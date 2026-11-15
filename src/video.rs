//! Video pipeline: damage-driven capture -> I420 -> x264 -> clients, with ack-based flow control.

use crate::capture::{Band, Capturer};
use crate::convert::I420;
use crate::encoder::{Encoded, Encoder, EncoderConfig, Fps, Pictures, Planes, Preset, Quality};
use crate::env::{Debugging, Marker};
use crate::hub::wait_any;
use crate::hub::{Backlog, Client, Hub, Out, ServerMessage, Takes, Viewer};
use crate::path::Kbps;
use crate::rate::{Bounds, FrameBudget, RateCtl, RTT_GUESS_MS};
use crate::timeline::{CaptureMarks, Coded};
use bytes::Bytes;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct VideoConfig {
    pub display: Option<String>,
    #[cfg(target_os = "macos")]
    pub enable_resize: bool,
    pub fps: Fps,
    pub preset: Preset,
    pub threads: u32,
    pub convert_threads: usize,
    pub crf: f32,
    /// What the stream is steered between (`--min-kbps`, `--max-kbps`).
    pub bounds: Bounds,
    pub paintover_crf: f32,
    pub paintover_delay_ms: u64,
    pub coalesce: Duration,
    pub stripes: u32,
    /// The debugging variables (`VOXL_*`), read when voxl started.
    pub debug: Debugging,
}

/// What a video message says before its stripe's H.264. The encode loop writes it; the
/// recorder, web/src/protocol.ts and bench/linkviewer.py read it, and each checks its
/// reader against web/src/video-header.json: messages, and the fields they mean.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Header {
    /// This stripe is a keyframe.
    pub key: bool,
    /// The frame sharpens what it codes (the bench's "paint-over").
    pub sharpens: bool,
    /// The frame's number: every stripe of a frame carries it, and a viewer acks it.
    pub frame: u32,
    pub pipe: Pipe,
    /// This stripe, of `stripes`.
    pub stripe: u8,
    pub stripes: u8,
    /// How many stripes' messages make up the frame.
    pub parts: u8,
    /// The stripe's top row.
    pub y: u16,
}

/// Capture to send, as a video message carries it: in tenths of a millisecond, at most
/// `u32::MAX` of them (119 hours).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pipe(u32);

impl Pipe {
    pub fn of(d: Duration) -> Pipe {
        Pipe((d.as_micros() / 100).min(u32::MAX as u128) as u32)
    }

    /// In whole milliseconds, rounded down.
    pub fn millis(self) -> u64 {
        self.0 as u64 / 10
    }
}

impl Header {
    /// A stripe's H.264 starts this far into its message.
    pub const LEN: usize = 16;
    /// A video message's type byte (`Bin.Video` in web/src/protocol.ts).
    const VIDEO: u8 = 0x01;
    /// The flags byte's bits.
    const KEY: u8 = 1;
    const SHARPENS: u8 = 2;

    /// Type, flags, frame, pipe, stripe, stripe count, parts in frame, a reserved 0, y.
    pub fn bytes(&self) -> [u8; Header::LEN] {
        let mut b = [0; Header::LEN];
        b[0] = Header::VIDEO;
        b[1] = if self.key { Header::KEY } else { 0 }
            | if self.sharpens { Header::SHARPENS } else { 0 };
        b[2..6].copy_from_slice(&self.frame.to_be_bytes());
        b[6..10].copy_from_slice(&self.pipe.0.to_be_bytes());
        b[10] = self.stripe;
        b[11] = self.stripes;
        b[12] = self.parts;
        b[14..16].copy_from_slice(&self.y.to_be_bytes());
        b
    }

    /// A video message's header. None for any other message, and for one with nothing
    /// after its header: the encode loop sends no stripe that coded nothing.
    pub fn read(m: &[u8]) -> Option<Header> {
        if m.len() <= Header::LEN || m[0] != Header::VIDEO {
            return None;
        }
        let u32_at = |i: usize| u32::from_be_bytes([m[i], m[i + 1], m[i + 2], m[i + 3]]);
        Some(Header {
            key: m[1] & Header::KEY != 0,
            sharpens: m[1] & Header::SHARPENS != 0,
            frame: u32_at(2),
            pipe: Pipe(u32_at(6)),
            stripe: m[10],
            stripes: m[11],
            parts: m[12],
            y: u16::from_be_bytes([m[14], m[15]]),
        })
    }
}

struct Job {
    buf: Box<I420>,
    frame: Frame,
    /// The bitrate the stream is steered to, and what the viewers' paths are known to
    /// carry (None: not yet), as the rate controller had them when the frame was captured.
    target: Kbps,
    capacity: Option<Kbps>,
    /// When the capture loop's part of it happened.
    captured: CaptureMarks,
    /// What the bench loadgen's marker said when the frame was captured, while
    /// `VOXL_DEBUG_MARKER` is set (see `marker_reading`).
    marker_ms: Option<u32>,
}

/// What a captured frame is for. Each kind carries what only it has.
enum Frame {
    /// A new viewer, or a decoder that lost its place: every stripe, from nothing.
    /// `blocks`: it went out in place of a sharpening pass, so it is coded at paint-over
    /// quality and sharpens every block, as that pass would have.
    /// `starts`: the viewers its plan promised it to, which it starts if it comes out whole.
    Key {
        blocks: Option<Sharpen>,
        starts: Vec<Arc<Client>>,
    },
    /// What changed on screen, and which blocks.
    Motion { band: Band, dirty: Arc<Vec<u8>> },
    /// The screen is still: re-encode `blocks` at paint-over quality.
    Sharpen { band: Band, blocks: Sharpen },
    /// Motion, plus blocks elsewhere that have gone still, at paint-over quality.
    MotionAndSharpen {
        band: Band,
        dirty: Arc<Vec<u8>>,
        blocks: Sharpen,
    },
}

impl Frame {
    /// Whether it encodes any of the rows `y..y + h`. A keyframe encodes them all.
    fn touches(&self, y: usize, h: usize) -> bool {
        match self {
            Frame::Key { .. } => true,
            Frame::Motion { band, .. }
            | Frame::Sharpen { band, .. }
            | Frame::MotionAndSharpen { band, .. } => {
                (band.y0 as usize) < y + h && (band.y1 as usize) > y
            }
        }
    }

    /// How a stripe codes it.
    fn coding(&self) -> Coding {
        match self {
            Frame::Key { blocks, .. } => Coding::Key {
                still: blocks.is_some(),
            },
            Frame::Motion { .. } => Coding::Motion,
            Frame::Sharpen { .. } | Frame::MotionAndSharpen { .. } => Coding::Sharpen,
        }
    }

    /// Whether it is coded at paint-over quality (a flag in each message's header).
    fn sharpens(&self) -> bool {
        match self {
            Frame::Key { blocks, .. } => blocks.is_some(),
            Frame::Motion { .. } => false,
            Frame::Sharpen { .. } | Frame::MotionAndSharpen { .. } => true,
        }
    }

    /// The blocks x264 analyses.
    fn mb_dirty(&self) -> Dirty {
        match self {
            Frame::Key { .. } => Dirty::All,
            Frame::Motion { dirty, .. } | Frame::MotionAndSharpen { dirty, .. } => {
                Dirty::Blocks(dirty.clone())
            }
            Frame::Sharpen { blocks, .. } => Dirty::Blocks(blocks.map.clone()),
        }
    }

    /// The blocks it sharpens, cleared once it came out sharp, and the viewers it starts.
    fn into_parts(self) -> (Option<Sharpen>, Vec<Arc<Client>>) {
        match self {
            Frame::Motion { .. } => (None, vec![]),
            Frame::Key { blocks, starts } => (blocks, starts),
            Frame::Sharpen { blocks, .. } | Frame::MotionAndSharpen { blocks, .. } => {
                (Some(blocks), vec![])
            }
        }
    }
}

/// The blocks a frame's encoders analyse.
#[derive(Clone)]
enum Dirty {
    /// Every block: a keyframe.
    All,
    /// Those the map marks (1 = changed), one byte per 16x16 block, row-major.
    Blocks(Arc<Vec<u8>>),
}

/// What a stripe worker is asked to code. The worker turns the kind into a CRF.
#[derive(Clone, Copy)]
enum Coding {
    /// An IDR, with a keyframe's allowance. `still`: in place of a sharpening pass, so at
    /// paint-over quality.
    Key { still: bool },
    /// Motion, at the configured CRF, which MotionQuality may lower.
    Motion,
    /// Sharpening, at the paint-over CRF.
    Sharpen,
}

/// The two CRFs a pipeline codes at, for its whole life.
#[derive(Clone, Copy)]
struct Crfs {
    configured: f32,
    paintover: f32,
}

impl Crfs {
    fn of(&self, coding: Coding) -> f32 {
        match coding {
            Coding::Key { still: false } | Coding::Motion => self.configured,
            Coding::Key { still: true } | Coding::Sharpen => self.paintover,
        }
    }
}

/// What a capture is sent as, decided before a buffer is taken for it.
enum Plan {
    /// A keyframe, and what it is to sharpening.
    Key(Keyframe),
    /// Something moved in this band.
    Motion(Band),
    /// The screen is still and a pass is due: these pending blocks.
    Sharpen { map: Vec<u8>, band: Band },
}

/// Blocks a frame sharpens, as the Refiner saw them when the frame was captured.
struct Sharpen {
    map: Arc<Vec<u8>>,
    /// When the frame was captured: blocks that changed after it stay pending.
    at: Instant,
}

/// A buffer back from the encoder, with the blocks it sharpened if it came out at
/// paint-over quality.
struct Returned {
    buf: Box<I420>,
    sharpened: Option<Sharpen>,
}

fn union(a: Option<Band>, b: Band) -> Band {
    match a {
        None => b,
        Some(a) => Band {
            y0: a.y0.min(b.y0),
            y1: a.y1.max(b.y1),
        },
    }
}

/// What the viewers watching are sent: the encoders, and the buffer captures are
/// converted into for them. There is one only while somebody watches.
///
/// One buffer, since a capture goes only to an idle encoder (`step`): a second, filled
/// while the encoder codes the first, only queued a frame behind it, and converted every
/// band twice, once into each.
struct Stream {
    /// Dropped first (fields drop in order), which ends the encode loop once it has sent
    /// what it was given.
    job_tx: mpsc::SyncSender<Job>,
    ret_rx: mpsc::Receiver<Returned>,
    /// The threads a capture is converted on.
    rows: crate::convert::Rows,
    /// None while the encoder holds it.
    buf: Option<Box<I420>>,
    /// The rows that changed on screen since the buffer was last converted into: the
    /// whole screen, until the first capture.
    pending: Option<Band>,
    /// Dropped last: waits until the encode loop has stopped.
    _encode: Stopped,
}

/// A thread that is waited for when this is dropped. The thread holds the sending side
/// of a channel nothing is sent on, which closes when the thread's work returns.
struct Stopped(mpsc::Receiver<std::convert::Infallible>);

impl Stopped {
    /// `work` on a thread named `name`.
    fn spawn(name: &str, work: impl FnOnce() + Send + 'static) -> Stopped {
        let (running, stopped) = mpsc::channel();
        std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let _running = running;
                work()
            })
            .unwrap();
        Stopped(stopped)
    }
}

impl Drop for Stopped {
    fn drop(&mut self) {
        let _ = self.0.recv();
    }
}

impl Stream {
    /// The encoders for a screen of `w` x `h`, one for each stripe the hub's hello names,
    /// on their thread, and the buffer, which holds nothing of it yet.
    fn new(cfg: &VideoConfig, hub: &Arc<Hub>, w: u32, h: u32) -> Result<Self, String> {
        log::info!("streaming");
        let (job_tx, job_rx) = mpsc::sync_channel::<Job>(1);
        let (ret_tx, ret_rx) = mpsc::channel::<Returned>();
        let layout = hub.stripes.lock().clone();
        let mut stripes = Vec::new();
        for &(y, sh) in &layout {
            let ecfg = EncoderConfig {
                pictures: Pictures {
                    width: w,
                    height: sh,
                    fps: cfg.fps,
                    preset: cfg.preset.clone(),
                    threads: (cfg.threads / layout.len() as u32).max(1),
                    debug: cfg.debug,
                },
                crf: cfg.crf,
                max_kbps: cfg.bounds.ceiling(),
            };
            stripes.push(Stripe {
                y: y as usize,
                h: sh as usize,
                enc: Encoder::new(&ecfg)?,
                applied_kbps: 0,
                applied_crf: -1.0,
            });
        }
        let hub = hub.clone();
        let crfs = Crfs {
            configured: cfg.crf,
            paintover: cfg.paintover_crf,
        };
        let (fps, debug) = (cfg.fps, cfg.debug);
        let encode = Stopped::spawn("encode", move || {
            encode_loop(stripes, job_rx, ret_tx, hub, fps, crfs, debug)
        });
        Ok(Stream {
            job_tx,
            ret_rx,
            buf: Some(Box::new(I420::new(w as usize, h as usize))),
            pending: Some(Band { y0: 0, y1: h }),
            rows: crate::convert::Rows::new(cfg.convert_threads),
            _encode: encode,
        })
    }

    /// The buffer the encoder is done with: back home, and the blocks it sharpened are
    /// sharp.
    fn returned(&mut self, r: Returned, refiner: &mut Refiner) {
        self.buf = Some(r.buf);
        if let Some(sharp) = r.sharpened {
            refiner.sharpened(&sharp.map, sharp.at);
        }
    }

    /// Whether the encoder holds the buffer.
    fn encoding(&self) -> Encoding {
        match self.buf {
            Some(_) => Encoding::Idle,
            None => Encoding::Busy,
        }
    }
}

/// Runs the pipeline. With a `Screen` (dynamic resolution), a client's size request
/// ends the capture loop: the encoder drains, the screen is resized and the pipeline
/// is rebuilt at the new size, and clients get the new geometry and a keyframe.
pub fn spawn(
    cfg: VideoConfig,
    hub: Arc<Hub>,
    mut screen: Option<crate::screen::Screen>,
) -> Result<(), String> {
    if let Some(s) = &screen {
        *hub.resize_max.lock() = Some(s.max());
    }
    let mut cap = build(&cfg, &hub)?;
    std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || loop {
            if let Err(e) = capture_loop(&mut cap, cfg.clone(), hub.clone()) {
                #[cfg(target_os = "linux")]
                let resized = cap.geometry_changed();
                #[cfg(not(target_os = "linux"))]
                let resized = false;
                if resized {
                    log::info!("capture geometry changed: {e}");
                } else {
                    log::error!("capture loop ended: {e}");
                    std::process::exit(1);
                }
            }
            drop(cap);
            #[cfg(target_os = "macos")]
            let display_request = hub.display_req.lock().take();
            #[cfg(target_os = "macos")]
            if let Some(request) = display_request {
                // A resize for the old monitor must not be applied to the new one.
                hub.resize_req.lock().take();
                cap = switch_display(request, &cfg, &hub, &mut screen).unwrap_or_else(|e| {
                    log::error!("display switch failed: {e}");
                    std::process::exit(1);
                });
                continue;
            }
            let req = hub.resize_req.lock().take();
            if let (Some(s), Some((w, h))) = (screen.as_mut(), req) {
                if let Err(e) = s.resize(w, h) {
                    log::warn!("resize to {w}x{h} failed: {e}");
                }
            }
            cap = match build(&cfg, &hub) {
                Ok(cap) => cap,
                Err(e) => {
                    log::error!("video restart failed: {e}");
                    std::process::exit(1);
                }
            };
        })
        .unwrap();
    Ok(())
}

#[cfg(target_os = "macos")]
fn switch_display(
    request: crate::mac::DisplayRequest,
    cfg: &VideoConfig,
    hub: &Arc<Hub>,
    screen: &mut Option<crate::screen::Screen>,
) -> Result<Capturer, String> {
    let previous = crate::mac::display();
    let mut open = |id| {
        // Best effort: an app-only mode change is undone by macOS when voxl exits.
        if let Some(mut old) = screen.take() {
            if let Err(e) = old.restore() {
                log::warn!("could not restore the previous display mode: {e}");
            }
        }
        crate::mac::select_display(id)?;
        *screen = if cfg.enable_resize {
            crate::screen::Screen::open(None)
                .inspect_err(|e| {
                    log::warn!("dynamic resolution unavailable: {e}");
                })
                .ok()
        } else {
            None
        };
        *hub.resize_max.lock() = screen.as_ref().map(|s| s.max());
        build(cfg, hub)
    };
    let attempt = request
        .ready
        .recv_timeout(Duration::from_secs(2))
        .map_err(|e| e.to_string())
        .and_then(|_| open(request.id));
    let cap = attempt.or_else(|e| {
        log::warn!("display switch: {e}; restoring the previous display");
        open(previous)
    })?;
    *hub.input.lock() = Some(request.sender);
    Ok(cap)
}

/// The capture of the screen as it is now, and its geometry for the viewers.
fn build(cfg: &VideoConfig, hub: &Arc<Hub>) -> Result<Capturer, String> {
    let cap = Capturer::new(cfg.display.as_deref(), cfg.debug)?;
    let (w, h) = (cap.width, cap.height);
    hub.width.store(w, Ordering::Relaxed);
    hub.height.store(h, Ordering::Relaxed);
    hub.activity.reset(w, h);
    log::info!(
        "capturing {}x{} @ {} fps, preset {} threads {}",
        w,
        h,
        cfg.fps.get(),
        cfg.preset,
        cfg.threads
    );
    *hub.stripes.lock() = stripe_layout(h, cfg.stripes);
    #[cfg(target_os = "macos")]
    {
        let resizing = hub.resize_max.lock().is_some();
        let mut extra = hub.hello_extra.lock();
        extra.insert("displays".into(), crate::mac::display_info());
        if resizing {
            extra.insert(
                "resize".into(),
                serde_json::json!({ "scale": crate::mac::resize_scale() }),
            );
        } else {
            extra.remove("resize");
        }
    }
    // New geometry (and a keyframe) for clients already connected; no-op at startup.
    hub.restart_clients();
    Ok(cap)
}

// The flow gate's bounds. For a viewer sent a frame every frame time, each acknowledged
// its round trip plus some Y after it went (wire, decode, queue), the count holds the
// capture once Y passes `DECODE_MS` plus two to three frame times (by where the round
// trip falls on them), and the age once Y passes `MOST_QUEUED_MS` plus up to one. At 60
// fps the count holds from Y = 73 to 90 ms, the age from 121 to 137 ms.

/// What a viewer is taken to keep a frame for between its arrival and the ack (decode and
/// draw), in the frames it may have in flight: 40 ms. Upstream's guess (051bb63, unchanged
/// since), unmeasured. At 60 fps the count holds from Y = 73 to 90 ms; at half (20 ms)
/// from 53 to 70, at double (80 ms) from 113 to 130, where the age holds anyway.
const DECODE_MS: f64 = 40.0;
/// Frames a viewer may have in flight beyond those its round trip and decode cover: 2.
/// Upstream's guess (051bb63), unmeasured. At 60 fps the count holds from Y = 73 to
/// 90 ms; with 1 from 57 to 73, with 3 from 90 to 107. The round trip and decode cover
/// at least one frame time, so a viewer may always have 3: upstream's floor of 3 on the
/// count (`clamp(3, 40)`) never bound, and is gone.
const SPARE_FRAMES: usize = 2;
/// The most frames a viewer may have in flight, whatever its round trip: 40. Upstream's
/// guess (051bb63), unmeasured. It binds where the round trip and decode cover 38 frame
/// times, a round trip over 593 ms at 60 fps, and there the stream sends at most 40
/// frames a round trip. At 39 or 41 that is one frame either way; at half (20) it binds
/// from a 260 ms round trip, and holds a 400 ms path to about 49 fps; at double (80) only
/// past 1.26 s.
const MOST_IN_FLIGHT: usize = 40;
/// How long past its round trip a viewer's oldest frame may go unacknowledged before
/// the capture waits for it: 120 ms, of wire time, decode and queue. Upstream's guess
/// (051bb63), unmeasured. The count holds first (or with it) from 25 fps, this below 25
/// fps and past `MOST_IN_FLIGHT`. At half (60 ms) this holds first up to 86 fps, at 60
/// fps from Y = 61 to 77 ms; at double (240 ms) only below 10 fps.
const MOST_QUEUED_MS: u128 = 120;
/// A viewer whose oldest frame in flight is this old has stalled (a dead socket, a viewer
/// that stopped acknowledging): 4 s. Upstream's guess (051bb63), unmeasured. Until
/// then it holds every viewer's picture, its frames being past `MOST_QUEUED_MS`; then it
/// starts over at a keyframe and holds nobody. At half (2 s) a path whose round trip and
/// queue reach 2 s is resynced, a keyframe to everyone, every 2 s; at double (8 s) one
/// stopped viewer freezes every other viewer's picture for 8 s.
pub const STALLED: Duration = Duration::from_millis(4000);

/// What one viewer's frames in flight say about capturing another now, and, while it has
/// any and has not stalled, how long until it has: the one change time alone brings.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Flow {
    /// Room for one more.
    Room(Option<Duration>),
    /// As many in flight as its round trip covers, or its oldest past the queue it may
    /// have: the capture waits for its acks.
    Full(Duration),
    /// Its oldest is `STALLED` old: no wait would help.
    Stalled(Backlog),
}

impl Flow {
    /// From what it has in flight, its round trip, and the time between frames.
    fn of(backlog: Option<Backlog>, rtt_ms: u32, interval_ms: f64) -> Flow {
        let Some(b) = backlog else {
            return Flow::Room(None);
        };
        if b.oldest >= STALLED {
            return Flow::Stalled(b);
        }
        let stalls_in = STALLED - b.oldest;
        let age = b.oldest.as_millis();
        // Enough frames in flight to cover one round trip plus decode, and no more than
        // `MOST_QUEUED_MS` of queue past the round trip.
        let rtt = rtt_ms as f64;
        let room =
            (((rtt + DECODE_MS) / interval_ms).ceil() as usize + SPARE_FRAMES).min(MOST_IN_FLIGHT);
        if b.frames.get() >= room || age > rtt_ms as u128 + MOST_QUEUED_MS {
            Flow::Full(stalls_in)
        } else {
            Flow::Room(Some(stalls_in))
        }
    }
}

/// What the flow gate finds at one moment: whether a frame may be captured, and which
/// viewers have stalled.
struct Gate {
    gated: Gated,
    /// The stalled viewers asked: all of them if the gate is open, and those before the
    /// first full one if not.
    stalled: Vec<(Arc<Client>, Backlog)>,
}

impl Gate {
    /// Asks `viewers` at `now`, in the hub's order, up to the first that is full. It
    /// only reads: what is done to a stalled viewer is `resync`'s.
    fn ask(viewers: &[(Arc<Client>, Viewer)], interval_ms: f64, now: Instant) -> Gate {
        let mut stalled = Vec::new();
        // How soon the first of the viewers asked so far stalls, if one has frames in
        // flight: one before the full one may stall first.
        let mut soonest: Option<Duration> = None;
        for (c, _) in viewers {
            let rtt = c.rtt_ms().unwrap_or(RTT_GUESS_MS);
            match Flow::of(c.backlog(now), rtt, interval_ms) {
                Flow::Room(stalls_in) => soonest = soonest.into_iter().chain(stalls_in).min(),
                Flow::Full(stalls_in) => {
                    let until = now + soonest.into_iter().fold(stalls_in, Duration::min);
                    return Gate {
                        gated: Gated::Held { until },
                        stalled,
                    };
                }
                Flow::Stalled(b) => stalled.push((c.clone(), b)),
            }
        }
        Gate {
            gated: Gated::Open,
            stalled,
        }
    }
}

/// A stalled viewer starts over: what it has in flight is forgotten, and it waits for a
/// keyframe. The one place the flow gate changes a viewer.
fn resync(c: &Client, b: Backlog) {
    log::warn!(
        "client {} stalled ({} frames, {} ms); resyncing",
        c.id,
        b.frames,
        b.oldest.as_millis()
    );
    c.start_over();
}

/// Regional sharpening. Blocks encoded while they changed carry motion-level quality;
/// once a block has been still for a while it is re-encoded at paint-over quality,
/// even while other parts of the screen keep moving (a playing video would otherwise
/// keep the frame-level paint-over from ever running). A block stays pending until a
/// frame that re-encoded it came out at paint-over quality (`sharpened`): one the rate
/// cap squeezed leaves it for the next pass, which motion elsewhere does not put off.
struct Refiner {
    blocks: Vec<Block>,
    last_refine: Instant,
    cols: usize,
}

/// A macroblock, as sharpening sees it.
#[derive(Clone, Copy)]
enum Block {
    Sharp,
    /// Sent at motion quality, or by a pass the cap squeezed, last changed at `changed`.
    Pending {
        changed: Instant,
    },
}

const REFINE_STILL: Duration = Duration::from_millis(300);
const REFINE_EVERY: Duration = Duration::from_millis(250);

impl Refiner {
    fn new(blocks: usize, cols: usize, now: Instant) -> Self {
        Refiner {
            blocks: vec![Block::Sharp; blocks],
            last_refine: now,
            cols,
        }
    }
    fn mark(&mut self, dirty: &[u8], now: Instant) {
        for (b, &d) in self.blocks.iter_mut().zip(dirty) {
            if d != 0 {
                *b = Block::Pending { changed: now };
            }
        }
    }
    fn mark_all(&mut self, now: Instant) {
        self.blocks.fill(Block::Pending { changed: now });
    }
    /// Every block, as the blocks a frame captured at `now` sharpens: a keyframe codes
    /// them all.
    fn every_block(&self, now: Instant) -> Sharpen {
        Sharpen {
            map: Arc::new(vec![1; self.blocks.len()]),
            at: now,
        }
    }
    /// A frame captured at `at` coded `map`'s blocks at paint-over quality: they are
    /// sharp now, unless they changed again since.
    fn sharpened(&mut self, map: &[u8], at: Instant) {
        for (b, &m) in self.blocks.iter_mut().zip(map) {
            if let Block::Pending { changed } = *b {
                if m != 0 && changed <= at {
                    *b = Block::Sharp;
                }
            }
        }
    }
    /// The rows of 16 px the blocks `map` marks span, if it marks any.
    fn band(&self, map: &[u8]) -> Option<Band> {
        let mut rows = map
            .iter()
            .enumerate()
            .filter(|&(_, &m)| m != 0)
            .map(|(i, _)| i / self.cols);
        let first = rows.next()?;
        let last = rows.next_back().unwrap_or(first);
        Some(Band {
            y0: (first * 16) as u32,
            y1: ((last + 1) * 16) as u32,
        })
    }
    /// Every block not yet sharpened (1 = encode) and the row band they span.
    fn pending_map(&self) -> Option<(Vec<u8>, Band)> {
        let map: Vec<u8> = self
            .blocks
            .iter()
            .map(|b| matches!(b, Block::Pending { .. }) as u8)
            .collect();
        self.band(&map).map(|band| (map, band))
    }
    /// Blocks due for sharpening on a frame that carries motion (1 = refine): pending and
    /// still for `REFINE_STILL`. At most every `REFINE_EVERY`, so a block whose pass came
    /// back squeezed goes again that long after it, until one comes back sharp.
    fn due(&mut self, now: Instant) -> Option<(Vec<u8>, Band)> {
        if now.saturating_duration_since(self.last_refine) < REFINE_EVERY {
            return None;
        }
        let map: Vec<u8> = self
            .blocks
            .iter()
            .map(|b| match *b {
                Block::Pending { changed }
                    if now.saturating_duration_since(changed) >= REFINE_STILL =>
                {
                    1
                }
                Block::Sharp | Block::Pending { .. } => 0,
            })
            .collect();
        let band = self.band(&map)?;
        self.last_refine = now;
        Some((map, band))
    }
}

/// When the next sharpening pass may go: once the screen has been still for `delay`, and
/// from then on with every capture, until nothing is pending. A pass spends what the frame
/// budget allows, as any frame does, so passes one after another send what the target
/// carries and no more.
#[derive(Debug, PartialEq)]
struct Sharpening {
    /// Stillness before the first pass (`--paintover-delay-ms`).
    delay: Duration,
    state: Schedule,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Schedule {
    Idle,
    /// Something moved: passes are due from this instant on.
    From(Instant),
}

/// What a keyframe is to sharpening.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Keyframe {
    /// For a viewer or a decoder that needs one: every block at keyframe quality, to be
    /// sharpened once the screen has been still for the delay.
    Fresh,
    /// In place of a due pass: coded at paint-over quality and sharpening every block,
    /// and passes go on.
    Pass,
}

/// When a sharpening pass is due.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Due {
    Now,
    In(Duration),
    Never,
}

impl Sharpening {
    fn new(delay: Duration) -> Self {
        Sharpening {
            delay,
            state: Schedule::Idle,
        }
    }
    /// Something moved: passes once the screen has been still for `delay`.
    fn on_motion(&mut self, now: Instant) {
        self.state = Schedule::From(now + self.delay);
    }
    /// A keyframe went out, every block of it at keyframe quality. In place of a due
    /// pass it is that pass, and passes go on. Otherwise it is, to the schedule, what
    /// motion is: the Refiner counts every block as changed at `now`, so passes follow
    /// once the screen has been still for `delay`, though nothing on it moved.
    fn on_key(&mut self, key: Keyframe, now: Instant) {
        match key {
            Keyframe::Pass => {}
            Keyframe::Fresh => self.on_motion(now),
        }
    }
    fn on_nothing_pending(&mut self) {
        self.state = Schedule::Idle;
    }
    fn due(&self, now: Instant) -> Due {
        match self.state {
            Schedule::Idle => Due::Never,
            Schedule::From(at) if now >= at => Due::Now,
            Schedule::From(at) => Due::In(at - now),
        }
    }
}

/// Captures may come this share of the frame interval apart, so that a source's jitter
/// does not alias into dropped frames. Upstream's guess, unmeasured. It caps the capture
/// rate at the frame rate over it (70.6 a second at 60): at 1, a frame that lands a
/// little early waits a whole interval; at 0.7, up to 86 captures a second.
const MIN_INTERVAL_SHARE: f64 = 0.85;

/// What the capture loop does next.
#[derive(Debug, PartialEq)]
enum Step {
    Wait(Wait),
    /// Capture a change or a pass into the buffer that is home.
    Capture(Next),
}

/// What the capture loop waits for before it looks again. Damage wakes it through the
/// capturer; the hub's waker, through a viewer's command or ack, a viewer leaving, a
/// screenshot or recording asked for, or a buffer coming home from the encoder.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Wait {
    /// Damage or a wake: nothing is due.
    Change,
    /// The sharpening pass due in this long, or damage or a wake.
    Pass(Duration),
    /// The rest of the least interval since the capture that gates this one (`Captured`),
    /// or what the capture it holds back is woken by (`Next::interval_ends`).
    Interval { rest: Duration, next: Next },
    /// The rest of the coalesce window, whatever happens.
    Settle(Duration),
    /// The flow gate: an ack, a viewer going or a keyframe asked for (a wake), or damage
    /// to take in, or in this long, when the first viewer it holds for will have stalled.
    Gate(Duration),
    /// A buffer back from the encoder, which wakes the waker as it hands it back; damage
    /// waits for the capture this leads to.
    Buffer,
}

/// What a wait wakes on besides its time.
#[derive(Clone, Copy, Debug, PartialEq)]
enum On {
    /// Damage or the hub's waker.
    Event,
    /// The hub's waker only.
    Wake,
}

impl Wait {
    fn wait(self, cap: &Capturer, hub: &Hub) {
        let poll = |on, timeout| match on {
            On::Event => wait_any([cap.fd(), hub.waker.waitable()], timeout),
            On::Wake => wait_any([hub.waker.waitable()], timeout),
        };
        self.through(poll, std::thread::sleep);
    }

    /// The wait, made through `poll` (until its time, or with no limit) or `sleep`.
    fn through(self, poll: impl FnOnce(On, Option<Duration>), sleep: impl FnOnce(Duration)) {
        match self {
            Wait::Change => poll(On::Event, None),
            Wait::Pass(due) => poll(On::Event, Some(due)),
            Wait::Interval { rest, next } => poll(next.interval_ends(), Some(rest)),
            Wait::Settle(rest) => sleep(rest),
            Wait::Gate(stalls) => poll(On::Event, Some(stalls)),
            Wait::Buffer => poll(On::Wake, None),
        }
    }
}

/// Whether a viewer waits for a keyframe.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Key {
    Wanted,
    NotWanted,
}

/// What changed on screen since the last capture, as each platform's capturer holds it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Damage {
    None,
    /// To be captured though no area was reported: a new capturer, or the screen grabbed
    /// whole for a new stream.
    Unreported,
    /// Rows `y0..y1` drawn, the first of them at `since`.
    Rows {
        since: Instant,
        y0: u32,
        y1: u32,
    },
}

impl Damage {
    /// Rows `top..bot` were drawn at `now`.
    pub(crate) fn add(&mut self, top: u32, bot: u32, now: Instant) {
        *self = match *self {
            Damage::Rows { since, y0, y1 } => Damage::Rows {
                since,
                y0: y0.min(top),
                y1: y1.max(bot),
            },
            Damage::None | Damage::Unreported => Damage::Rows {
                since: now,
                y0: top,
                y1: bot,
            },
        };
    }

    /// To be captured whether or not anything is drawn.
    pub(crate) fn mark(&mut self) {
        if let Damage::None = self {
            *self = Damage::Unreported;
        }
    }

    /// The rows drawn since the last take, aligned to even rows for 4:2:0 within a
    /// picture `height` tall; nothing is left.
    pub(crate) fn take(&mut self, height: u32) -> Option<Band> {
        match std::mem::replace(self, Damage::None) {
            Damage::Rows { y0, y1, .. } => Some(Band {
                y0: y0 & !1,
                y1: ((y1 + 1) & !1).min(height),
            }),
            Damage::None | Damage::Unreported => None,
        }
    }
}

/// Whether the flow gate lets a capture go: open while no viewer asked is `Full`.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Gated {
    Open,
    /// Until the soonest a viewer asked will have stalled. Before that only an ack, a
    /// viewer going or a keyframe asked for can open the gate, and each of those wakes
    /// the hub.
    Held {
        until: Instant,
    },
}

/// Whether the encoder holds the capture loop's buffer.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Encoding {
    Idle,
    Busy,
}

/// What the capture loop knows when it looks, while somebody watches.
struct Look {
    key: Key,
    damage: Damage,
    due: Due,
    now: Instant,
    captured: Captured,
    /// The least time between two captures.
    min_interval: Duration,
    /// How long damage may settle before it is captured (`--coalesce-us`).
    coalesce: Duration,
    gate: Gated,
    encoding: Encoding,
}

/// What the loop does next, from what it knows.
fn step(look: &Look) -> Step {
    let next = match (look.damage, look.key) {
        // Nothing moved and no keyframe is wanted. A pass goes only to an idle encoder,
        // as every capture does (below); until then a change may come, and with it the
        // capture is motion's.
        (Damage::None, Key::NotWanted) => match (look.due, look.encoding) {
            (Due::Now, Encoding::Idle) => Next::Pass,
            (Due::Now, Encoding::Busy) | (Due::Never, _) => return Step::Wait(Wait::Change),
            (Due::In(due), _) => return Step::Wait(Wait::Pass(due)),
        },
        (Damage::Unreported | Damage::Rows { .. }, Key::Wanted | Key::NotWanted)
        | (Damage::None, Key::Wanted) => Next::Change,
    };
    let since = look.now.saturating_duration_since(look.captured.gate(next));
    if since < look.min_interval {
        let rest = look.min_interval - since;
        return Step::Wait(Wait::Interval { rest, next });
    }
    // Apps present a frame as a burst of puts; give the burst a moment to land so
    // the capture holds one whole frame instead of its first tiles.
    if let Damage::Rows { since, .. } = look.damage {
        let settled = look.now.saturating_duration_since(since);
        if settled < look.coalesce {
            return Step::Wait(Wait::Settle(look.coalesce - settled));
        }
    }
    // A keyframe is not held by the gate.
    if let (Key::NotWanted, Gated::Held { until }) = (look.key, look.gate) {
        return Step::Wait(Wait::Gate(until - look.now));
    }
    // A capture goes only to an idle encoder. One made while it codes the frame before
    // would wait for it, a whole encode older by the time it is coded; made once it is
    // done, it holds what changed meanwhile, and goes at once.
    match look.encoding {
        Encoding::Idle => Step::Capture(next),
        Encoding::Busy => Step::Wait(Wait::Buffer),
    }
}

impl Step {
    /// Whether the flow gate had its say in this step: it does once nothing before it
    /// holds the capture, while no keyframe is wanted. Then the viewers it found stalled
    /// start over (`resync`), as they did when the gate was asked only there.
    fn asked_gate(&self, key: Key) -> bool {
        matches!(
            (self, key),
            (
                Step::Capture(_) | Step::Wait(Wait::Gate(_) | Wait::Buffer),
                Key::NotWanted
            )
        )
    }
}

/// What a capture is sent as, or why nothing is.
enum Planned {
    Send(Plan),
    /// A pass was due and nothing is pending: the screen is sharp.
    Sharp,
    /// The damage held no area after all, and no pass is due.
    Empty,
}

/// What a capture whose damage came to `band` is sent as.
fn plan(key: Key, band: Option<Band>, due: Due, refiner: &Refiner) -> Planned {
    match (key, band, due) {
        (Key::Wanted, Some(_), Due::Now | Due::In(_) | Due::Never) => {
            Planned::Send(Plan::Key(Keyframe::Fresh))
        }
        (Key::Wanted, None, Due::Now) => Planned::Send(Plan::Key(Keyframe::Pass)),
        (Key::Wanted, None, Due::In(_) | Due::Never) => Planned::Send(Plan::Key(Keyframe::Fresh)),
        (Key::NotWanted, Some(moved), Due::Now | Due::In(_) | Due::Never) => {
            Planned::Send(Plan::Motion(moved))
        }
        // Sharpen only what was sent at motion quality; nothing pending, nothing to send.
        (Key::NotWanted, None, Due::Now) => match refiner.pending_map() {
            Some((map, band)) => Planned::Send(Plan::Sharpen { map, band }),
            None => Planned::Sharp,
        },
        (Key::NotWanted, None, Due::In(_) | Due::Never) => Planned::Empty,
    }
}

/// What the capture loop is about to capture.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Next {
    /// Something moved, or a viewer wants a keyframe.
    Change,
    /// Nothing moved and a pass is due.
    Pass,
}

impl Next {
    /// What ends the wait for the frame interval before capturing this, besides its time.
    fn interval_ends(self) -> On {
        match self {
            Next::Change | Next::Pass => On::Wake,
        }
    }
}

/// When the last frames were captured: what holds the next one to the frame rate.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Captured {
    /// Any frame, a pass included.
    any: Instant,
    /// The last change: motion or a keyframe.
    change: Instant,
}

impl Captured {
    fn new(at: Instant) -> Self {
        Captured {
            any: at,
            change: at,
        }
    }
    /// The capture the next frame waits a frame interval after. A pass waits after any
    /// frame, so passes go at most at the frame rate and never right behind a change. A
    /// change waits only after the last change: a keystroke that comes just after a pass
    /// is captured at once and waits for that pass's encode at most, not for a frame.
    fn gate(&self, next: Next) -> Instant {
        match next {
            Next::Pass => self.any,
            Next::Change => self.change,
        }
    }
    fn on_capture(&mut self, next: Next, at: Instant) {
        self.any = at;
        match next {
            Next::Pass => {}
            Next::Change => self.change = at,
        }
    }
}

/// Notes the damage the capturer has taken for /wait, whether or not anyone is watching.
fn note_changes(cap: &mut Capturer, hub: &Hub) {
    if !cap.changes.is_empty() {
        hub.activity.note(&cap.changes);
        cap.changes.clear();
    }
}

fn capture_loop(cap: &mut Capturer, cfg: VideoConfig, hub: Arc<Hub>) -> Result<(), String> {
    let (w, h) = (cap.width as usize, cap.height as usize);
    let interval = Duration::from_secs_f64(1.0 / cfg.fps.get() as f64);
    let min_interval = interval.mul_f64(MIN_INTERVAL_SHARE);
    let interval_ms = interval.as_secs_f64() * 1000.0;
    let mut captured = Captured::new(Instant::now() - interval);
    let paintover_delay = Duration::from_millis(cfg.paintover_delay_ms);
    let mut sharpening = Sharpening::new(paintover_delay);
    let mut rc = RateCtl::new(cfg.bounds, hub.target());
    hub.set_target(rc.target);
    let mut refiner = Refiner::new(
        w.div_ceil(16) * h.div_ceil(16),
        w.div_ceil(16),
        Instant::now(),
    );
    let marker = cfg.debug.marker;
    // The marker is read only where all of it fits this capture (a resize builds a new
    // loop): past an edge its squares are outside the picture or in the next row.
    let probe = marker.and_then(|at| {
        let probe = MarkerProbe::fit(at, w, h);
        if probe.is_none() {
            log::error!(
                "VOXL_DEBUG_MARKER={},{}: the marker does not fit the {w}x{h} capture; \
                 it is not read",
                at.x,
                at.y
            );
        }
        probe
    });
    // The encoders, while somebody watches. Every way out of the loop drops it, and with it
    // waits for the encoder to send what it holds.
    let mut stream: Option<Stream> = None;

    loop {
        // The wake is taken before anything it may stand for is read: a request that
        // lands after this line wakes the next wait, and one that landed before it is
        // seen below. Read first and drained after, a resize asked for in between lost
        // its wake, and the waits below have no time limit.
        hub.drain_wake();
        #[cfg(target_os = "macos")]
        if hub.display_req.lock().is_some() {
            return Ok(());
        }
        if hub.resize_req.lock().is_some() || cap.needs_rebuild() {
            return Ok(());
        }
        cap.pump()?;
        if cap.needs_rebuild() {
            return Ok(());
        }
        note_changes(cap, &hub);
        let snaps = std::mem::take(&mut *hub.snapshots.lock());
        if !snaps.is_empty() {
            // The whole current screen, whether or not anyone is watching. Damage
            // stays pending, so the stream still sends what changed. Only the pixels
            // are copied here (the framebuffer is live); the PNG is made off this thread.
            // The capturer takes the damage for exactly these pixels with them, so the
            // screenshot's seq covers what it shows and nothing drawn after it.
            let bgrx = cap.snapshot()?;
            note_changes(cap, &hub);
            let shot = Arc::new(crate::record::Snapshot {
                width: w,
                height: h,
                bgrx,
                seq: hub.activity.seq(),
            });
            for tx in snaps {
                let _ = tx.send(shot.clone());
            }
        }
        // The encoder holds the sender for as long as it runs: once it is gone, no buffer
        // will come back to wake the loop.
        if let Some(s) = stream.as_mut() {
            loop {
                match s.ret_rx.try_recv() {
                    Ok(r) => s.returned(r, &mut refiner),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => return Err("encoder gone".into()),
                }
            }
        }
        // One read of the viewers: the stream, the rate, the keyframe and whom it is promised
        // to are all decided from it.
        let viewers = hub.viewers();
        stream = match (stream.take(), viewers.is_empty()) {
            // Nobody watches: a stream is dropped here, once its encoder has sent what it
            // holds.
            (None | Some(_), true) => None,
            (Some(s), false) => Some(s),
            // A new stream starts from the whole screen: our shm copy may be stale, and
            // none of it has been sent, so every block is pending.
            (None, false) => {
                let s = Stream::new(&cfg, &hub, cap.width, cap.height)?;
                cap.grab(cap.full())?;
                // The whole screen is grabbed and pending: it holds what was drawn while
                // nobody watched, so that damage is taken with it, and the stream's first
                // frame counts from its capture, not from damage no frame took.
                let _ = cap.damage.take(cap.height);
                cap.damage.mark();
                refiner.mark_all(Instant::now());
                Some(s)
            }
        };
        let Some(s) = stream.as_mut() else {
            // The target stands: a viewer that hid its tab comes back over the same
            // path, and one on a new path starts from what its own acks say.
            Wait::Change.wait(cap, &hub);
            continue;
        };
        let capacity = rc.update(&hub, &viewers);
        let key = match viewers.iter().any(|(_, v)| *v == Viewer::Waiting) {
            true => Key::Wanted,
            false => Key::NotWanted,
        };
        // One reading of the clock for every decision before the capture; the capture
        // itself is stamped `t0` below, after any wait for a buffer.
        let now = Instant::now();
        let due = sharpening.due(now);
        let gate = Gate::ask(&viewers, interval_ms, now);
        let look = Look {
            key,
            damage: cap.damage,
            due,
            now,
            captured,
            min_interval,
            coalesce: cfg.coalesce,
            gate: gate.gated,
            encoding: s.encoding(),
        };
        let step = step(&look);
        if step.asked_gate(key) {
            for (c, b) in gate.stalled {
                resync(&c, b);
            }
        }
        let next = match step {
            Step::Wait(wait) => {
                if let Wait::Gate(_) = wait {
                    hub.stats.flow_blocked.fetch_add(1, Ordering::Relaxed);
                }
                wait.wait(cap, &hub);
                continue;
            }
            Step::Capture(next) => next,
        };

        let t0 = Instant::now();
        let origin = match cap.damage {
            Damage::Rows { since, .. } => since,
            Damage::None | Damage::Unreported => t0,
        };
        hub.stats
            .damage_wait_us
            .fetch_add((t0 - origin).as_micros() as u64, Ordering::Relaxed);
        let mb_dirty = cap.take_mb_map();
        refiner.mark(&mb_dirty, t0);
        let band = cap.take_damage()?;
        if let Some(b) = band {
            cap.grab(b)?;
        }
        let t1 = Instant::now();
        if marker.is_some() {
            log::info!(
                "cap dt {:.1} ms band {:?} grab {} us",
                captured.any.elapsed().as_secs_f64() * 1000.0,
                band,
                (t1 - t0).as_micros()
            );
        }
        let marker_ms = marker_reading(cap.bgrx(), probe);
        // What this capture is sent as, decided once, before a buffer is taken.
        let plan = match plan(key, band, due, &refiner) {
            Planned::Send(plan) => plan,
            Planned::Sharp => {
                sharpening.on_nothing_pending();
                continue;
            }
            Planned::Empty => continue,
        };
        let mut buf = s.buf.take().unwrap();
        let todo = match (s.pending.take(), band) {
            (p, Some(b)) => Some(union(p, b)),
            (p, None) => p,
        };
        if let Some(t) = todo {
            cap.convert(&mut buf, t.y0 as usize, t.y1 as usize, &s.rows);
        }
        let t2 = Instant::now();
        let rows = todo.map_or(0, |t| t.y1 - t.y0);
        hub.stats
            .grab_us
            .fetch_add((t1 - t0).as_micros() as u64, Ordering::Relaxed);
        hub.stats
            .convert_us
            .fetch_add((t2 - t1).as_micros() as u64, Ordering::Relaxed);
        hub.stats.captured.fetch_add(1, Ordering::Relaxed);

        // Each plan is one input to the sharpening schedule.
        let frame = match plan {
            Plan::Key(keyframe) => {
                // Every block goes out at keyframe quality, which the rate cap squeezes:
                // all of it is pending. In place of a pass the keyframe is coded at
                // paint-over quality, and what comes back as asked is sharp.
                refiner.mark_all(t0);
                let blocks = match keyframe {
                    Keyframe::Pass => {
                        hub.stats.paintovers.fetch_add(1, Ordering::Relaxed);
                        Some(refiner.every_block(t0))
                    }
                    Keyframe::Fresh => None,
                };
                sharpening.on_key(keyframe, t0);
                let starts = viewers
                    .iter()
                    .filter(|(c, v)| *v == Viewer::Waiting && c.keyframe_planned())
                    .map(|(c, _)| c.clone())
                    .collect();
                Frame::Key { blocks, starts }
            }
            Plan::Sharpen { map, band } => {
                hub.stats.paintovers.fetch_add(1, Ordering::Relaxed);
                Frame::Sharpen {
                    band,
                    blocks: Sharpen {
                        map: Arc::new(map),
                        at: t0,
                    },
                }
            }
            Plan::Motion(moved) => {
                sharpening.on_motion(t0);
                // Piggyback sharpening of blocks that have gone still on this frame.
                match refiner.due(t0) {
                    // What moved plus what is due; everything else stays skipped.
                    Some((rmap, rband)) => {
                        let mut m = mb_dirty;
                        for (d, r) in m.iter_mut().zip(&rmap) {
                            *d |= r;
                        }
                        hub.stats.refines.fetch_add(1, Ordering::Relaxed);
                        Frame::MotionAndSharpen {
                            band: union(Some(moved), rband),
                            dirty: Arc::new(m),
                            blocks: Sharpen {
                                map: Arc::new(rmap),
                                at: t0,
                            },
                        }
                    }
                    None => Frame::Motion {
                        band: moved,
                        dirty: Arc::new(mb_dirty),
                    },
                }
            }
        };
        captured.on_capture(next, t0);
        if s.job_tx
            .send(Job {
                buf,
                frame,
                target: rc.target,
                capacity,
                captured: CaptureMarks {
                    damage: origin,
                    capture: t0,
                    grabbed: t1,
                    converted: t2,
                    queued: Instant::now(),
                    rows,
                },
                marker_ms,
            })
            .is_err()
        {
            return Err("encoder gone".into());
        }
    }
}

/// The number of the frame a video message is part of; None for any other message.
pub fn frame_of(m: &[u8]) -> Option<u32> {
    Header::read(m).map(|header| header.frame)
}

/// Split the screen into horizontal stripes, each an independent H.264 stream.
/// x264 pays ~9 ms of CPU per 2560x1600 frame even when nothing changed, and sliced
/// threading triples that; independent single-threaded stripes are encoded in
/// parallel (low latency) and only when they changed (low CPU).
fn stripe_layout(height: u32, n: u32) -> Vec<(u32, u32)> {
    let n = n.clamp(1, 16);
    let each = (height.div_ceil(n) + 15) & !15;
    let mut v = Vec::new();
    let mut y = 0;
    while y < height {
        let h = each.min(height - y);
        v.push((y, h));
        y += h;
    }
    v
}

struct Stripe {
    y: usize,
    h: usize,
    enc: Encoder,
    applied_kbps: u32,
    applied_crf: f32,
}

struct SendPtr<T>(*const T);
unsafe impl<T> Send for SendPtr<T> {}
impl<T> Clone for SendPtr<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for SendPtr<T> {}

struct StripeJob {
    /// Valid until the worker replies: the encode loop waits for every stripe.
    buf: SendPtr<I420>,
    ts: i64,
    coding: Coding,
    cap: u32,
    mb_dirty: Dirty,
    mb_cols: usize,
}

/// What a stripe worker sends back for its part of a frame.
struct StripeResult {
    stripe: usize,
    out: StripeOut,
}

enum StripeOut {
    /// The encode failed, or the worker never answered.
    Failed,
    Sent {
        key: bool,
        bytes: Vec<u8>,
        quality: Quality,
        /// When the worker took the stripe, handed it to x264 and had it back.
        coded: Coded,
    },
}

/// x264's CRF raises the QP of frames it judges complex, and its quick half-resolution
/// estimate can't follow a dragged window or a fast scroll: the moved area counts as new
/// pixels and the QP lands 7-10 steps above `--crf`, although the real search codes the
/// move as cheap motion vectors (a few KB a frame). Big moves went blocky, with nothing
/// saved that the bitrate cap needed. Motion frames that come out that cheap get a CRF
/// lowered by what the estimate adds, so they stay at the configured quality. Video and
/// new content really are complex, and x264's higher QP there saves real bits, so those
/// are left alone, as are the lower QPs x264 picks for small updates (typing) and
/// paint-overs. VBV still raises the QP when the link is short.
#[derive(Default)]
struct MotionQuality {
    /// QP steps x264's complexity estimate adds to the next frame (negative for simple
    /// ones). x264 blurs it over the frames it encodes, paint-overs included, so each
    /// frame's is a good guess at the next one's.
    complexity: f32,
    /// log2 of the bits per changed macroblock of recent motion frames, scaled to the
    /// configured CRF. Averaged in the log domain: one frame of new content (a menu
    /// opening mid-drag) shouldn't outweigh the run of cheap frames around it.
    mb_bits_log2: f32,
    /// QP steps taken off the CRF of motion frames now.
    applied: f32,
}

/// Most QP steps taken off a motion frame's CRF.
const MAX_MOTION_EXCESS: f32 = 12.0;

/// Bits per changed macroblock (at the configured CRF) under which motion counts as
/// cheap: a dragged window or a scrolled page costs 10-30, video and new content 50 and
/// more.
const CHEAP_MB_BITS: f32 = 40.0;

impl MotionQuality {
    /// The CRF to encode a motion frame with, for the configured `crf`.
    fn crf(&mut self, crf: f32) -> f32 {
        let want = if self.mb_bits_log2 < CHEAP_MB_BITS.log2() {
            self.complexity.clamp(0.0, MAX_MOTION_EXCESS)
        } else {
            0.0
        };
        // Whole steps, and only once the estimate has moved a full step: every change
        // reconfigures x264.
        if (want - self.applied).abs() >= 1.0 {
            self.applied = want.round();
        }
        (crf - self.applied).max(0.0)
    }

    /// Notes what x264 made of a frame it was given `crf` for. `size` is set for a
    /// motion frame (not a paint-over).
    fn note(&mut self, e: &Encoded, crf: f32, size: Option<MotionSize>) {
        // A keyframe's QP isn't picked from the complexity estimate.
        let (false, Some(qp)) = (e.keyframe, e.qp) else {
            return;
        };
        self.observe(crf, qp, e);
        // Small updates cost more per macroblock (headers, a cursor's edges) and never
        // get x264's QP up; only big ones tell what kind of motion this is.
        if let Some(m) = size.filter(|m| m.changed * 16 >= m.mbs) {
            // Halving the quantizer step (6 QP) about doubles the bits.
            let b = (m.bits.max(1) as f32 / m.changed.max(1) as f32).log2() + (qp - m.crf) / 6.0;
            self.mb_bits_log2 = self.mb_bits_log2 * 0.7 + b * 0.3;
        }
    }

    /// x264 starts from QP0 = `crf` + complexity. VBV may raise that before the frame
    /// (to `qp`) and row by row within it (to an average x264 doesn't report), never
    /// below QP0 or, with a VBV buffer over 1.1 frames, below the frame's starting QP,
    /// and `e.crf_avg` is `crf` + average - QP0. So the complexity lies between
    /// `qp - e.crf_avg` (no row raised) and `qp - crf` (nothing raised before the
    /// frame), give or take the rounding of `qp`: one value when VBV stayed out, a range
    /// when it didn't. Then the last estimate is kept while it fits, else moved to the
    /// nearest end, so a frame VBV squeezed doesn't throw away what earlier frames showed.
    fn observe(&mut self, crf: f32, qp: f32, e: &Encoded) {
        let lo = qp - 0.5 - e.crf_avg;
        let hi = qp + 0.5 - crf;
        self.complexity = if hi - lo < 1.25 {
            (lo + hi) / 2.0
        } else {
            self.complexity.max(lo).min(hi)
        };
    }
}

/// How big a motion frame came out, for MotionQuality.
#[derive(Clone, Copy)]
struct MotionSize {
    bits: usize,
    /// Macroblocks that changed, of the stripe's `mbs`.
    changed: usize,
    mbs: usize,
    /// The configured CRF.
    crf: f32,
}

fn stripe_worker(
    i: usize,
    mut s: Stripe,
    rx: mpsc::Receiver<StripeJob>,
    done: mpsc::Sender<StripeResult>,
    fps: Fps,
    crfs: Crfs,
    debug: bool,
) {
    let mut motion = MotionQuality::default();
    while let Ok(job) = rx.recv() {
        let woke = Instant::now();
        let buf = unsafe { &*job.buf.0 };
        // The CRF the job asks for, and the one x264 is given: MotionQuality may lower
        // a motion frame's.
        let asked = crfs.of(job.coding);
        let crf = match job.coding {
            Coding::Key { .. } | Coding::Sharpen => asked,
            Coding::Motion => motion.crf(asked),
        };
        if let Coding::Key { .. } = job.coding {
            // A keyframe is what a new viewer sees first; don't starve it to the
            // steady-state frame budget (up to ~8 frames' worth at 4x the cap).
            let boost = job.cap.saturating_mul(4).max(16_000);
            s.enc.set_rate_buf(asked, boost, boost / fps.get() * 8);
            s.applied_kbps = 0;
        } else {
            let moved = s.applied_kbps == 0
                || (job.cap as f64 - s.applied_kbps as f64).abs() > s.applied_kbps as f64 * 0.04;
            if crf != s.applied_crf || moved {
                s.enc.set_rate(crf, job.cap);
                s.applied_crf = crf;
                s.applied_kbps = job.cap;
            }
        }
        let suv = buf.stride_uv();
        let planes = Planes {
            y: &buf.y[s.y * buf.width..],
            u: &buf.u[(s.y / 2) * suv..],
            v: &buf.v[(s.y / 2) * suv..],
            stride_y: buf.width,
            stride_uv: suv,
        };
        let mut out = Vec::with_capacity(64 << 10);
        out.resize(Header::LEN, 0);
        // x264 wants X264_MBINFO_CONSTANT (1) on unchanged macroblocks of this stripe.
        let mut info = match &job.mb_dirty {
            Dirty::All => None,
            Dirty::Blocks(d) => {
                let rows = s.h.div_ceil(16);
                let start = (s.y / 16) * job.mb_cols;
                Some(
                    d[start..start + rows * job.mb_cols]
                        .iter()
                        .map(|&x| (x == 0) as u8)
                        .collect::<Vec<u8>>(),
                )
            }
        };
        // Changed macroblocks in this stripe (all of them without a damage map).
        let stripe_mbs = s.h.div_ceil(16) * job.mb_cols;
        let changed = info
            .as_ref()
            .map_or(stripe_mbs, |m| m.iter().filter(|&&c| c == 0).count());
        let te = Instant::now();
        let r = s.enc.encode(
            &planes,
            job.ts,
            matches!(job.coding, Coding::Key { .. }),
            None,
            info.as_deref_mut(),
            &mut out,
        );
        let encoded = Instant::now();
        if debug {
            let cpu_now = thread_cpu_us();
            log::info!(
                "stripe y{} encode {} us wall, thread cpu total {} us, {} bytes",
                s.y,
                te.elapsed().as_micros(),
                cpu_now,
                out.len()
            );
        }
        let result = match r {
            Ok(e) => {
                let size = matches!(job.coding, Coding::Motion).then(|| MotionSize {
                    bits: (out.len() - Header::LEN) * 8,
                    changed,
                    mbs: stripe_mbs,
                    crf: asked,
                });
                motion.note(&e, crf, size);
                StripeOut::Sent {
                    key: e.keyframe,
                    quality: e.quality(crf),
                    bytes: out,
                    coded: Coded {
                        woke,
                        x264: te,
                        encoded,
                    },
                }
            }
            Err(e) => {
                log::error!("{e}");
                StripeOut::Failed
            }
        };
        if done
            .send(StripeResult {
                stripe: i,
                out: result,
            })
            .is_err()
        {
            return;
        }
    }
}

/// More frames than this in flight to a viewer, and it is closed (it stops watching, and is
/// sent the close once): upstream's guess (its
/// first commit), never measured; 10 s of frames at 60 a second. The flow check keeps a
/// viewer to at most 40 frames in flight, two more in the pipeline, and starts it over
/// once its oldest frame is 4 s old, but a keyframe passes the flow check. So a viewer
/// gets this far only on keyframes: while a client asks for one at every capture (the
/// page asks at most every 500 ms), the flow check never runs, and a viewer that acks
/// nothing is sent one keyframe a capture until this bound closes it, after 10 s at 60
/// a second, with 36 to 108 MB of keyframes (60 to 180 KB on a terminal at 3024x1898)
/// queued for a socket that may not be read. At half it closes after 5 s; at double after
/// 20 s, with twice that queued. A page reaches neither.
const HOPELESSLY_BEHIND: usize = 600;

fn encode_loop(
    stripes_in: Vec<Stripe>,
    rx: mpsc::Receiver<Job>,
    ret: mpsc::Sender<Returned>,
    hub: Arc<Hub>,
    fps: Fps,
    crfs: Crfs,
    debug: Debugging,
) {
    let epoch = Instant::now();
    let mut frame_id: u32 = 0;
    let n = stripes_in.len();
    let mut budget = FrameBudget::new(epoch);
    let debug_stripes = debug.stripes;
    // Geometry stays here; encoders move to their workers.
    let stripes: Vec<(usize, usize)> = stripes_in.iter().map(|s| (s.y, s.h)).collect();
    let (done_tx, done_rx) = mpsc::channel::<StripeResult>();
    let workers: Vec<(mpsc::Sender<StripeJob>, std::thread::JoinHandle<()>)> = stripes_in
        .into_iter()
        .enumerate()
        .map(|(i, s)| {
            let (tx, rx) = mpsc::channel();
            let done = done_tx.clone();
            let h = std::thread::Builder::new()
                .name(format!("enc-{i}"))
                .spawn(move || stripe_worker(i, s, rx, done, fps, crfs, debug_stripes))
                .unwrap();
            (tx, h)
        })
        .collect();
    while let Ok(job) = rx.recv() {
        let t0 = Instant::now();
        // A sharpening frame's band covers the blocks it sharpens.
        let selected: Vec<bool> = stripes
            .iter()
            .map(|&(y, h)| job.frame.touches(y, h))
            .collect();
        if !selected.iter().any(|&x| x) {
            let _ = ret.send(Returned {
                buf: job.buf,
                sharpened: None,
            });
            hub.wake();
            continue;
        }
        let coding = job.frame.coding();
        let mb_dirty = job.frame.mb_dirty();
        // Each stripe may use up to twice its area share of the frame's budget, and
        // never more than all of it: what was sent is taken off the budget afterwards,
        // so several stripes moving at once is paid for by the frames that follow.
        let frame = budget
            .allow(t0, job.target, job.capacity, fps)
            .a_frame_at(fps.get())
            .get() as u32;
        let share = (frame.saturating_mul(2) / n as u32).clamp(1, frame.max(1));
        let ts = job.captured.damage.duration_since(epoch).as_millis() as i64;
        // Persistent per-stripe workers keep each encoder's state warm in one core's cache.
        let buf_ptr = SendPtr(&*job.buf as *const I420);
        let mut pending = 0;
        let dispatched = Instant::now();
        for (i, &on) in selected.iter().enumerate() {
            if on {
                // Sharpening spends from the same budget as motion: what the budget
                // doesn't cover now is left pending for a later pass.
                let _ = workers[i].0.send(StripeJob {
                    buf: buf_ptr,
                    ts,
                    coding,
                    cap: share,
                    mb_dirty: mb_dirty.clone(),
                    mb_cols: job.buf.width.div_ceil(16),
                });
                pending += 1;
            }
        }
        let mut results: Vec<StripeOut> = (0..n).map(|_| StripeOut::Failed).collect();
        for _ in 0..pending {
            if let Ok(r) = done_rx.recv() {
                results[r.stripe] = r.out;
            }
        }
        let collected = Instant::now();
        // A frame's blocks are sharp when every stripe it touched came back as asked.
        let sharp = selected.iter().zip(&results).all(|(&on, r)| {
            !on || matches!(
                r,
                StripeOut::Sent {
                    quality: Quality::AsAsked,
                    ..
                }
            )
        });
        let sharpens = job.frame.sharpens();
        let (sharpened, starts) = job.frame.into_parts();
        let _ = ret.send(Returned {
            buf: job.buf,
            sharpened: sharpened.filter(|_| sharp),
        });
        hub.wake();
        let parts: Vec<(usize, bool, Vec<u8>, Coded)> = results
            .into_iter()
            .enumerate()
            .filter_map(|(i, r)| match r {
                StripeOut::Sent {
                    key, bytes, coded, ..
                } => (bytes.len() > Header::LEN).then_some((i, key, bytes, coded)),
                StripeOut::Failed => None,
            })
            .collect();
        let all_key = parts.len() == n && parts.iter().all(|p| p.1);
        // The viewers this keyframe was promised to start on it, or wait for another.
        for c in &starts {
            c.keyframe_sent(all_key);
        }
        // A frame waits for the stripe coded last; one with nothing to send is not sent.
        let Some(coded) = parts.iter().map(|p| p.3).reduce(Coded::later) else {
            continue;
        };
        let enc_us = t0.elapsed().as_micros() as u64;
        let total: usize = parts.iter().map(|p| p.2.len()).sum();
        hub.stats.encode_us.fetch_add(enc_us, Ordering::Relaxed);
        hub.stats.encoded.fetch_add(1, Ordering::Relaxed);
        hub.stats.bytes.fetch_add(total as u64, Ordering::Relaxed);
        budget.spend(total);
        if all_key {
            hub.stats.keyframes.fetch_add(1, Ordering::Relaxed);
        }
        frame_id = frame_id.wrapping_add(1);
        let pipe = Pipe::of(job.captured.damage.elapsed());
        let nparts = parts.len() as u8;
        let msgs: Vec<Bytes> = parts
            .into_iter()
            .map(|(i, key, mut out, _)| {
                let header = Header {
                    key,
                    sharpens,
                    frame: frame_id,
                    pipe,
                    stripe: i as u8,
                    stripes: n as u8,
                    parts: nparts,
                    y: stripes[i].0 as u16,
                };
                out[..Header::LEN].copy_from_slice(&header.bytes());
                Bytes::from(out)
            })
            .collect();
        let now = Instant::now();
        hub.timeline.record(
            frame_id,
            &crate::timeline::Frame {
                captured: job.captured,
                taken: t0,
                dispatched,
                coded,
                collected,
                sent: now,
                bytes: total,
            },
        );
        for (c, _) in hub.viewers() {
            match c.takes(frame_id, now, total, all_key, HOPELESSLY_BEHIND) {
                Takes::Nothing => {}
                Takes::Frame => {
                    for m in &msgs {
                        c.send(Out::Bin(m.clone()));
                    }
                }
                Takes::Close => {
                    log::warn!("client {} hopelessly behind; dropping", c.id);
                    c.send(Out::Text(ServerMessage::Close));
                }
            }
        }
        hub.stats.send_lat_us.fetch_add(
            job.captured.damage.elapsed().as_micros() as u64,
            Ordering::Relaxed,
        );
        if let Some(ms) = job.marker_ms {
            log::info!(
                "marker sent age {} ms (encode {} us)",
                marker_age(ms),
                enc_us
            );
        }
    }
}

/// CPU time this thread has used, in microseconds.
#[cfg(unix)]
fn thread_cpu_us() -> i64 {
    use nix::time::{clock_gettime, ClockId};
    clock_gettime(ClockId::CLOCK_THREAD_CPUTIME_ID)
        .map(|t| t.tv_sec() * 1_000_000 + t.tv_nsec() / 1000)
        .unwrap_or(0)
}

#[cfg(windows)]
fn thread_cpu_us() -> i64 {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
    let (mut c, mut e, mut k, mut u) = Default::default();
    let ft = |f: FILETIME| ((f.dwHighDateTime as i64) << 32 | f.dwLowDateTime as i64) / 10;
    match unsafe { GetThreadTimes(GetCurrentThread(), &mut c, &mut e, &mut k, &mut u) } {
        Ok(()) => ft(k) + ft(u),
        Err(_) => 0,
    }
}

/// The bench loadgen's marker as `bench/loadgen.c` draws it: 40 bits (the wall clock's
/// milliseconds in the low 24, the frame in the next 16), each a square of 24 px, 20 to
/// a row, read at each square's centre. These are loadgen's numbers, not chosen here: any
/// other value reads pixels it did not draw.
const MARKER_BITS: usize = 40;
const MARKER_ROW: usize = 20;
const MARKER_SQUARE: usize = 24;

/// A `VOXL_DEBUG_MARKER` place on a capture the whole marker fits, so every square it
/// reads is a pixel of the picture in the square's own row.
#[derive(Clone, Copy, Debug)]
struct MarkerProbe(Marker);

impl MarkerProbe {
    /// The marker at `at` on a `width` x `height` capture, if the centre of its last
    /// square across and its last square down are inside it.
    fn fit(at: Marker, width: usize, height: usize) -> Option<MarkerProbe> {
        let last = |from: usize, squares: usize| {
            from.checked_add((squares - 1) * MARKER_SQUARE + MARKER_SQUARE / 2)
        };
        (last(at.x, MARKER_ROW)? < width && last(at.y, MARKER_BITS / MARKER_ROW)? < height)
            .then_some(MarkerProbe(at))
    }
}

/// What the bench loadgen's marker says on this capture: none unless `VOXL_DEBUG_MARKER`
/// placed it where it fits and the capture has pixels to read it from.
fn marker_reading(bgrx: Option<(&[u8], usize)>, probe: Option<MarkerProbe>) -> Option<u32> {
    match (bgrx, probe) {
        (Some((px, stride)), Some(at)) => Some(marker_probe(px, stride, at, "grab")),
        _ => None,
    }
}

/// Debug: decode the bench loadgen marker at `at` and log its age.
fn marker_probe(buf: &[u8], stride: usize, MarkerProbe(at): MarkerProbe, what: &str) -> u32 {
    let mut v: u64 = 0;
    for b in 0..MARKER_BITS {
        let x = at.x + (b % MARKER_ROW) * MARKER_SQUARE + MARKER_SQUARE / 2;
        let y = at.y + (b / MARKER_ROW) * MARKER_SQUARE + MARKER_SQUARE / 2;
        let i = y * stride + x * 4;
        if buf[i + 2] > 128 {
            v |= 1 << b;
        }
    }
    let ms = (v & 0xffffff) as u32;
    let now = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64)
        & 0xffffff;
    log::info!(
        "marker {} frame {} age {} ms",
        what,
        v >> 24,
        now - i64::from(ms)
    );
    ms
}

fn marker_age(ms: u32) -> i64 {
    let now = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64)
        & 0xffffff;
    now - i64::from(ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A marker is read where all of it fits the capture, as the time painted on it, and
    /// nowhere else: a place past an edge (by a pixel, by far, or so far the sum overflows)
    /// reads none and never leaves its square's row or the picture. Captures of random sizes
    /// and padding, either side of the marker's own, with random pixels (a fixed seed); the
    /// places are drawn around the last one that fits, anywhere, and near the largest. The
    /// model is the 40 squares' centres as loadgen draws them.
    #[test]
    fn a_marker_is_read_where_it_fits_and_nowhere_else() {
        let mut state = 0u64;
        let mut next = || {
            // splitmix64
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            (z ^ (z >> 31)) as usize
        };
        // Places that fit, that do not, and one pixel either side of the last that fits.
        let (mut fits, mut not, mut edge) = (0, 0, 0);
        for _ in 0..4000 {
            let (w, h) = (440 + next() % 100, 30 + next() % 40);
            let stride = w * 4 + next() % 3 * 4;
            let mut buf = vec![0u8; stride * h];
            for chunk in buf.chunks_mut(8) {
                chunk.copy_from_slice(&next().to_le_bytes()[..chunk.len()]);
            }
            let mut place = |room: usize| match next() % 8 {
                0..=2 => room.saturating_sub(4) + next() % 8,
                3..=5 => next() % (room + 8),
                6 => usize::MAX - next() % 600,
                _ => next(),
            };
            let at = Marker {
                x: place(w.saturating_sub(468)),
                y: place(h.saturating_sub(36)),
            };
            let centres: Vec<_> = (0..40)
                .map(|b| {
                    let x = at.x.checked_add(b % 20 * 24 + 12).filter(|&x| x < w)?;
                    let y = at.y.checked_add(b / 20 * 24 + 12).filter(|&y| y < h)?;
                    Some(y * stride + x * 4)
                })
                .collect::<Option<_>>()
                .unwrap_or_default();
            let painted = match next() % 4 {
                0 => next() & !0xff_ffff,
                1 => next() | 0xff_ffff,
                _ => next(),
            } & 0xff_ffff_ffff;
            for (b, &i) in centres.iter().enumerate() {
                let r = next() as u8 % 127;
                buf[i + 2] = if painted >> b & 1 == 1 { 129 + r } else { r };
            }
            let probe = MarkerProbe::fit(at, w, h);
            assert_eq!(probe.is_some(), !centres.is_empty(), "{at:?} on {w}x{h}");
            if probe.is_some() {
                fits += 1;
            } else {
                not += 1;
            }
            if [469, 468].contains(&w.wrapping_sub(at.x))
                || [37, 36].contains(&h.wrapping_sub(at.y))
            {
                edge += 1;
            }
            assert_eq!(
                marker_reading(Some((&buf[..], stride)), probe),
                probe.map(|_| (painted & 0xff_ffff) as u32),
                "{at:?} on {w}x{h} {painted:#x}"
            );
            assert_eq!(marker_reading(None, probe), None);
        }
        assert!(
            fits > 300 && not > 300 && edge > 300,
            "{fits} fit, {not} do not, {edge} at an edge"
        );
    }

    /// Dropping a `Stopped` waits for its work: whatever the work did before it returned
    /// is done once the drop returns, for work of every length up to 64 steps.
    #[test]
    fn a_stopped_thread_has_done_its_work_once_dropped() {
        for steps in 0..64u32 {
            let (tx, rx) = mpsc::channel();
            drop(Stopped::spawn("work", move || {
                for i in 0..steps {
                    tx.send(i).unwrap();
                }
            }));
            assert_eq!(rx.try_iter().count(), steps as usize, "{steps} steps");
        }
    }

    /// A viewer that acks nothing is sent frames until more than `HOPELESSLY_BEHIND` of them
    /// are in flight, and is then closed, once, by the real encode loop: it is sent
    /// `HOPELESSLY_BEHIND + 1` frames, then one close, then nothing, whatever comes after.
    #[test]
    fn a_viewer_too_far_behind_is_closed() {
        let hub = Hub::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let viewer = hub.add_client(tx);
        viewer.watch();
        viewer.keyframe_planned();
        let (w, h) = (64, 64);
        let ecfg = EncoderConfig {
            pictures: Pictures {
                width: w as u32,
                height: h as u32,
                fps: Fps::new(60).unwrap(),
                preset: Preset::parse("ultrafast").unwrap(),
                threads: 1,
                debug: Debugging::read(|_| None).unwrap(),
            },
            crf: 23.0,
            max_kbps: None,
        };
        let stripe = Stripe {
            y: 0,
            h,
            enc: Encoder::new(&ecfg).unwrap(),
            applied_kbps: 0,
            applied_crf: -1.0,
        };
        let (job_tx, job_rx) = mpsc::sync_channel::<Job>(1);
        let (ret_tx, _ret_rx) = mpsc::channel::<Returned>();
        let encode = std::thread::spawn({
            let hub = hub.clone();
            move || {
                encode_loop(
                    vec![stripe],
                    job_rx,
                    ret_tx,
                    hub,
                    Fps::new(60).unwrap(),
                    Crfs {
                        configured: 23.0,
                        paintover: 18.0,
                    },
                    Debugging::read(|_| None).unwrap(),
                )
            }
        });
        for i in 0..HOPELESSLY_BEHIND + 3 {
            let frame = match i {
                0 => super::Frame::Key {
                    blocks: None,
                    starts: vec![viewer.clone()],
                },
                _ => super::Frame::Motion {
                    band: Band {
                        y0: 0,
                        y1: h as u32,
                    },
                    dirty: Arc::new(vec![1; (w / 16) * (h / 16)]),
                },
            };
            job_tx
                .send(Job {
                    buf: Box::new(I420::new(w, h)),
                    frame,
                    target: Kbps::new(8000.0),
                    capacity: None,
                    captured: CaptureMarks::at(Instant::now()),
                    marker_ms: None,
                })
                .unwrap();
        }
        drop(job_tx);
        encode.join().unwrap();
        let sent: Vec<Out> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        let frames = sent.iter().take_while(|m| matches!(m, Out::Bin(_))).count();
        assert_eq!(frames, HOPELESSLY_BEHIND + 1);
        let after: Vec<String> = sent[frames..]
            .iter()
            .map(|m| match m {
                Out::Text(t) => t.json(),
                Out::Bin(_) => "a frame".into(),
            })
            .collect();
        assert_eq!(after, ["{\"t\":\"close\"}"], "after the frames");
    }

    /// A stream is made when somebody starts watching, so its encoders are new: whatever
    /// its first frame was planned as, and whatever blocks it was told are unchanged, each
    /// stripe codes the first part it sends from nothing, and a viewer is sent nothing that
    /// depends on a frame from before the stream. For every kind of frame, with every block
    /// marked unchanged, over the whole screen and over its first rows, at 1 to 4 stripes,
    /// then a motion frame with every block changed: each stripe's first part is coded
    /// from nothing and its second is not. The stripes are the ones the hub's hello names.
    #[test]
    fn a_new_streams_first_frame_is_coded_from_nothing() {
        let (w, h) = (64u32, 128u32);
        let blocks = (w.div_ceil(16) * h.div_ceil(16)) as usize;
        let unchanged = || Arc::new(vec![0u8; blocks]);
        let sharpen = || Sharpen {
            map: Arc::new(vec![1u8; blocks]),
            at: Instant::now(),
        };
        let bands = [Band { y0: 0, y1: h }, Band { y0: 0, y1: 16 }];
        for stripes in 1..=4 {
            for band in bands {
                let frames = [
                    super::Frame::Key {
                        blocks: None,
                        starts: vec![],
                    },
                    super::Frame::Key {
                        blocks: Some(sharpen()),
                        starts: vec![],
                    },
                    super::Frame::Motion {
                        band,
                        dirty: unchanged(),
                    },
                    super::Frame::Sharpen {
                        band,
                        blocks: sharpen(),
                    },
                    super::Frame::MotionAndSharpen {
                        band,
                        dirty: unchanged(),
                        blocks: sharpen(),
                    },
                ];
                for frame in frames {
                    let what = format!("{stripes} stripes, {:?}, {}", band, frame_kind(&frame));
                    let hub = Hub::new();
                    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
                    let viewer = hub.add_client(tx);
                    // A viewer that has started, whatever the stream sends first.
                    viewer.watch();
                    assert!(viewer.keyframe_planned());
                    viewer.keyframe_sent(true);
                    let cfg = VideoConfig {
                        display: None,
                        #[cfg(target_os = "macos")]
                        enable_resize: false,
                        fps: Fps::new(60).unwrap(),
                        preset: Preset::parse("ultrafast").unwrap(),
                        threads: 1,
                        convert_threads: 1,
                        crf: 23.0,
                        bounds: Bounds::new(Kbps::new(500.0), None),
                        paintover_crf: 18.0,
                        paintover_delay_ms: 0,
                        coalesce: Duration::ZERO,
                        // The stream codes the stripes the hub's hello names, not these.
                        stripes: 1,
                        debug: Debugging::read(|_| None).unwrap(),
                    };
                    *hub.stripes.lock() = stripe_layout(h, stripes);
                    let s = Stream::new(&cfg, &hub, w, h).unwrap();
                    let next = super::Frame::Motion {
                        band: bands[0],
                        dirty: Arc::new(vec![1u8; blocks]),
                    };
                    for frame in [frame, next] {
                        s.job_tx
                            .send(Job {
                                buf: Box::new(I420::new(w as usize, h as usize)),
                                frame,
                                target: Kbps::new(8000.0),
                                capacity: None,
                                captured: CaptureMarks::at(Instant::now()),
                                marker_ms: None,
                            })
                            .unwrap();
                    }
                    drop(s);
                    // Whether each part a stripe sent was coded from nothing, in order.
                    let mut sent = vec![vec![]; hub.stripes.lock().len()];
                    while let Ok(m) = rx.try_recv() {
                        if let Out::Bin(b) = m {
                            sent[b[10] as usize].push(b[1] & 1 == 1);
                        }
                    }
                    for (i, parts) in sent.iter().enumerate() {
                        assert_eq!(parts.first(), Some(&true), "stripe {i}: {what}");
                        assert!(parts[1..].iter().all(|&key| !key), "stripe {i}: {what}");
                    }
                }
            }
        }
    }

    fn frame_kind(f: &super::Frame) -> &'static str {
        match f {
            super::Frame::Key { blocks: None, .. } => "key",
            super::Frame::Key {
                blocks: Some(_), ..
            } => "key in place of a pass",
            super::Frame::Motion { .. } => "motion",
            super::Frame::Sharpen { .. } => "sharpen",
            super::Frame::MotionAndSharpen { .. } => "motion and sharpen",
        }
    }

    const W: usize = 1280;
    const H: usize = 768;

    /// What the screen shows in a test sequence.
    #[derive(Clone, Copy)]
    enum Scene {
        /// A window of text-like detail dragged 48 px a frame across a gradient desktop.
        Drag,
        /// Noise over the whole screen, new every frame: content that really is complex.
        Noise,
    }

    fn draw(y: &mut [u8], scene: Scene, frame: usize) {
        match scene {
            Scene::Drag => {
                let (ww, wh) = (W / 2, H / 2);
                let x0 = 48 * frame % (W - ww);
                let y0 = H / 4;
                for row in 0..H {
                    let line = &mut y[row * W..(row + 1) * W];
                    line.fill((40 + row * 120 / H) as u8);
                    if (y0..y0 + wh).contains(&row) {
                        let r = row - y0;
                        for (x, p) in line[x0..x0 + ww].iter_mut().enumerate() {
                            // Lines of "glyphs": dark strokes on white, a gap between lines.
                            let glyph =
                                ((x / 9) as u64 * 2654435761 + (r / 18) as u64 * 40503) >> 7;
                            let ink = r % 18 < 12 && (glyph >> (x % 9 + r % 18 % 6)) & 1 == 1;
                            *p = if ink { 30 } else { 235 };
                        }
                    }
                }
            }
            Scene::Noise => {
                let mut state = (frame as u64 + 1).wrapping_mul(0x9E3779B97F4A7C15);
                for p in y.iter_mut() {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    *p = 64 + (state % 128) as u8;
                }
            }
        }
    }

    /// One frame as a stripe worker saw it: the CRF it handed x264 and what came back.
    struct Frame {
        crf: f32,
        e: Encoded,
        complexity: f32,
    }

    /// Encodes frames the way a stripe worker does, under a bitrate cap of `max_kbps`
    /// if there is one, with or without MotionQuality.
    struct Run {
        enc: Encoder,
        motion: MotionQuality,
        correct: bool,
        crf: f32,
        applied: f32,
        y: Vec<u8>,
        n: usize,
    }

    impl Run {
        fn new(correct: bool, crf: f32, max_kbps: Option<Kbps>) -> Run {
            Run {
                enc: Encoder::new(&EncoderConfig {
                    pictures: Pictures {
                        width: W as u32,
                        height: H as u32,
                        fps: Fps::new(60).unwrap(),
                        preset: Preset::parse("ultrafast").unwrap(),
                        threads: 1,
                        debug: Debugging::read(|_| None).unwrap(),
                    },
                    crf,
                    max_kbps,
                })
                .unwrap(),
                motion: MotionQuality::default(),
                correct,
                crf,
                applied: crf,
                y: vec![0; W * H],
                n: 0,
            }
        }

        /// Encodes the next frame of `scene` as a motion frame, or the current picture
        /// again as a paint-over at `paintover_crf`.
        fn frame(&mut self, scene: Scene, paintover_crf: Option<f32>) -> Frame {
            let motion = paintover_crf.is_none();
            if motion {
                draw(&mut self.y, scene, self.n);
            }
            let c = match paintover_crf {
                Some(c) => c,
                None if self.correct => self.motion.crf(self.crf),
                None => self.crf,
            };
            if c != self.applied {
                self.enc.set_crf(c);
                self.applied = c;
            }
            let (u, v) = (vec![128u8; W * H / 4], vec![128u8; W * H / 4]);
            let planes = Planes {
                y: &self.y,
                u: &u,
                v: &v,
                stride_y: W,
                stride_uv: W / 2,
            };
            let mut out = Vec::new();
            let e = self
                .enc
                .encode(
                    &planes,
                    self.n as i64 * 16,
                    self.n == 0,
                    None,
                    None,
                    &mut out,
                )
                .unwrap();
            let mbs = W / 16 * H / 16;
            let size = motion.then_some(MotionSize {
                bits: out.len() * 8,
                changed: mbs,
                mbs,
                crf: self.crf,
            });
            self.motion.note(&e, c, size);
            self.n += 1;
            Frame {
                crf: c,
                complexity: self.motion.complexity,
                e,
            }
        }

        /// 40 motion frames of `scene`; returns the last 30, once rate control settled.
        fn motion(mut self, scene: Scene) -> Vec<Frame> {
            (0..40).map(|_| self.frame(scene, None)).skip(10).collect()
        }
    }

    fn mean(v: impl Iterator<Item = f32>) -> f32 {
        let (sum, n) = v.fold((0.0, 0), |(s, n), x| (s + x, n + 1));
        sum / n as f32
    }

    fn qp(f: &Frame) -> f32 {
        f.e.qp.unwrap()
    }

    #[test]
    fn dragged_window_stays_at_crf() {
        let crf = 24.0;
        let plain = mean(
            Run::new(false, crf, None)
                .motion(Scene::Drag)
                .iter()
                .map(qp),
        );
        let corrected = mean(Run::new(true, crf, None).motion(Scene::Drag).iter().map(qp));
        // The scene has to show the problem for the check below to mean anything.
        assert!(
            plain > crf + 3.0,
            "x264 coded the drag at QP {plain:.1} without the correction (CRF {crf})"
        );
        assert!(
            corrected < crf + 1.0,
            "drag frames at QP {corrected:.1} for CRF {crf}"
        );
    }

    /// Under a bitrate cap VBV raises rows within a frame, which the frame's starting QP
    /// doesn't show; the estimate must not take that for a simple frame and drop the
    /// correction.
    #[test]
    fn capped_drag_keeps_its_estimate() {
        let crf = 24.0;
        // Without a cap the complexity is exactly what x264 added to the QP.
        let plain = Run::new(false, crf, None).motion(Scene::Drag);
        let complexity = mean(plain.iter().map(|f| qp(f) - f.crf));
        let capped = Run::new(true, crf, Some(Kbps::new(20_000.0))).motion(Scene::Drag);
        let start_qp_says = capped
            .iter()
            .map(|f| qp(f) - f.e.crf_avg)
            .fold(f32::MAX, f32::min);
        assert!(
            start_qp_says < complexity - 4.0,
            "VBV never raised rows enough to matter (lowest {start_qp_says:.1}, complexity {complexity:.1})"
        );
        let lowest = capped.iter().map(|f| f.complexity).fold(f32::MAX, f32::min);
        assert!(
            lowest > complexity - 3.0,
            "estimate fell to {lowest:.1} under the cap, complexity {complexity:.1}"
        );
    }

    /// Content that really is complex keeps x264's QP: lowering it would only spend bits.
    #[test]
    fn complex_content_is_left_alone() {
        let crf = 24.0;
        let plain = mean(
            Run::new(false, crf, None)
                .motion(Scene::Noise)
                .iter()
                .map(qp),
        );
        let corrected = Run::new(true, crf, None).motion(Scene::Noise);
        assert!(
            plain > crf + 3.0,
            "x264 didn't raise the QP for noise ({plain:.1}, CRF {crf})"
        );
        let lowered = corrected.iter().filter(|f| f.crf != crf).count();
        assert_eq!(lowered, 0, "{lowered} noise frames got a lowered CRF");
    }

    /// A block stays pending until a frame that sharpened it comes back sharp, and one
    /// that changed again after that frame was captured stays pending even then.
    #[test]
    fn only_what_came_out_sharp_is_cleared() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut r = Refiner::new(4, 2, t0);
        r.mark(&[1, 1, 1, 0], ms(10));
        let (map, _) = r.pending_map().unwrap();
        // A pass captured at 20 ms went out squeezed: nothing came back, so the next
        // pass has it all.
        assert_eq!(r.pending_map().unwrap().0, map);
        r.mark(&[0, 1, 0, 0], ms(30));
        // Then it comes back sharp: block 1 changed after its capture and stays.
        r.sharpened(&map, ms(20));
        assert_eq!(r.pending_map().unwrap().0, vec![0, 1, 0, 0]);
        let (map, _) = r.pending_map().unwrap();
        r.sharpened(&map, ms(40));
        assert!(r.pending_map().is_none());
    }

    /// A keyframe sent in place of a due pass is coded at paint-over quality: when it
    /// comes back as asked, every block is sharp and nothing is left for the passes
    /// after it, except a block that changed after its capture. Any other keyframe
    /// leaves every block pending.
    #[test]
    fn a_keyframe_in_place_of_a_pass_sharpens_every_block() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut r = Refiner::new(4, 2, t0);
        r.mark(&[1, 0, 1, 0], ms(10));
        r.mark_all(ms(200));
        let key = super::Frame::Key {
            blocks: None,
            starts: vec![],
        };
        assert!(key.into_parts().0.is_none());
        assert_eq!(r.pending_map().map(|p| p.0), Some(vec![1, 1, 1, 1]));
        let key = super::Frame::Key {
            blocks: Some(r.every_block(ms(200))),
            starts: vec![],
        };
        assert!(key.sharpens());
        r.mark(&[0, 0, 0, 1], ms(210));
        let s = key.into_parts().0.expect("it sharpens what it codes");
        r.sharpened(&s.map, s.at);
        assert_eq!(r.pending_map().map(|p| p.0), Some(vec![0, 0, 0, 1]));
    }

    /// Typing beside an animation on a slow link: what the viewer is shown of blocks that
    /// stopped changing is sharp within a bounded time, however long the motion beside them
    /// goes on.
    ///
    /// One stripe: an animation's block changes on every frame, so the screen is never
    /// still and the text is sharpened only by passes that frames with motion carry. The
    /// text's blocks change in bursts of keystrokes, each burst followed by a pause. The
    /// link: the cap squeezes a pass that has much left to send, but what a squeezed pass
    /// sends stays, so a pass comes back as asked once every block it carries has had
    /// `needs` passes since it last changed (live at 6 Mbit/s a still page takes 25-45).
    /// The oracle is the model's own record of what the viewer is shown: a block at motion
    /// quality from its last change until a pass that carried it, captured after that
    /// change, comes back as asked.
    ///
    /// The bound, from the last keystroke: `REFINE_STILL` of stillness, then `needs`
    /// passes `REFINE_EVERY` apart, each waiting up to a frame for a capture, then a frame
    /// for the last one to come back. Throughout, a pass carries only blocks still for
    /// `REFINE_STILL`, never the animation's, passes stay `REFINE_EVERY` apart, and none
    /// goes once the text is shown sharp.
    #[test]
    fn still_blocks_are_sharpened_beside_motion() {
        // Block 0 is the animation's, the others the text's.
        const BLOCKS: usize = 8;
        const FRAME: Duration = Duration::from_nanos(1_000_000_000 / 60);
        #[derive(Clone, Copy, Debug, PartialEq)]
        enum Shown {
            AtMotionQuality,
            Sharp,
        }
        struct Model {
            r: Refiner,
            now: Instant,
            needs: u32,
            changed: [Instant; BLOCKS],
            /// Passes each block has had since it last changed.
            passes: [u32; BLOCKS],
            shown: [Shown; BLOCKS],
            /// A pass that came back as asked, taken in before the next capture.
            back: Option<(Vec<u8>, Instant)>,
            last_pass: Option<Instant>,
        }
        impl Model {
            /// One capture: the animation moved, and the block `typed`, if any.
            fn capture(&mut self, typed: Option<usize>) {
                self.now += FRAME;
                if let Some((map, at)) = self.back.take() {
                    for b in (0..BLOCKS).filter(|&b| map[b] != 0 && self.changed[b] <= at) {
                        self.shown[b] = Shown::Sharp;
                    }
                    self.r.sharpened(&map, at);
                }
                let mut dirty = [0u8; BLOCKS];
                dirty[0] = 1;
                if let Some(b) = typed {
                    dirty[b] = 1;
                }
                for b in (0..BLOCKS).filter(|&b| dirty[b] != 0) {
                    (self.changed[b], self.passes[b], self.shown[b]) =
                        (self.now, 0, Shown::AtMotionQuality);
                }
                self.r.mark(&dirty, self.now);
                let Some((map, _)) = self.r.due(self.now) else {
                    return;
                };
                assert_eq!(map[0], 0, "the animation's block went into a pass");
                for b in (0..BLOCKS).filter(|&b| map[b] != 0) {
                    let still = self.now - self.changed[b];
                    assert!(
                        still >= REFINE_STILL,
                        "block {b} in a pass, still {still:?}"
                    );
                }
                if let Some(last) = self.last_pass {
                    assert!(
                        self.now - last >= REFINE_EVERY,
                        "passes {:?} apart",
                        self.now - last
                    );
                }
                self.last_pass = Some(self.now);
                for (p, &m) in self.passes.iter_mut().zip(&map) {
                    *p += m as u32;
                }
                if map
                    .iter()
                    .zip(&self.passes)
                    .all(|(&m, &p)| m == 0 || p >= self.needs)
                {
                    self.back = Some((map, self.now));
                }
            }
            fn text_at_motion_quality(&self) -> Vec<usize> {
                (1..BLOCKS)
                    .filter(|&b| self.shown[b] == Shown::AtMotionQuality)
                    .collect()
            }
        }
        let t0 = Instant::now();
        let mut m = Model {
            r: Refiner::new(BLOCKS, BLOCKS, t0),
            now: t0,
            needs: 1,
            changed: [t0; BLOCKS],
            passes: [0; BLOCKS],
            shown: [Shown::Sharp; BLOCKS],
            back: None,
            last_pass: None,
        };
        let mut rng = 0x2545F4914F6CDD1Du64;
        let mut next = |n: u64| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % n
        };
        for _ in 0..300 {
            m.needs = 1 + next(8) as u32;
            let bound = REFINE_STILL + (REFINE_EVERY + FRAME) * m.needs + FRAME;
            // Keystrokes 1 to 20 frames apart.
            for _ in 0..1 + next(12) {
                for _ in 0..next(20) {
                    m.capture(None);
                }
                m.capture(Some(1 + next(BLOCKS as u64 - 1) as usize));
            }
            let typed = m.now;
            while m.now < typed + bound {
                m.capture(None);
            }
            assert_eq!(
                m.text_at_motion_quality(),
                Vec::<usize>::new(),
                "needing {} passes, text shown at motion quality {:?} after the last keystroke",
                m.needs,
                m.now - typed
            );
            let sharp = m.last_pass;
            for _ in 0..next(60) {
                m.capture(None);
            }
            assert_eq!(m.last_pass, sharp, "a pass once the text was shown sharp");
        }
    }

    /// The sharpening schedule against a model of it (when passes are due from, if they
    /// are), over a long run of motion, passes, keyframes and empty pending maps at stated
    /// times: a pass is due at the same moments in both, and the rules hold.
    #[test]
    fn sharpening_goes_on_until_nothing_is_pending() {
        let delay = Duration::from_millis(150);
        let t0 = Instant::now();
        let mut s = Sharpening::new(delay);
        let mut from: Option<Instant> = None;
        let mut rng = 0x2545F4914F6CDD1Du64;
        let mut next = |n: u64| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % n
        };
        let mut now = t0;
        for _ in 0..20_000 {
            now += Duration::from_millis(next(120));
            let model_due = from.is_some_and(|at| now >= at);
            assert_eq!(
                matches!(s.due(now), Due::Now),
                model_due,
                "due at {:?}",
                now - t0
            );
            match next(10) {
                0..=2 => {
                    s.on_motion(now);
                    from = Some(now + delay);
                    // Motion always restarts the wait.
                    assert_eq!(s.due(now), Due::In(delay));
                }
                3 => {
                    // A keyframe: in place of a due pass it is that pass, else motion.
                    let key = match s.due(now) {
                        Due::Now => Keyframe::Pass,
                        Due::In(_) | Due::Never => Keyframe::Fresh,
                    };
                    s.on_key(key, now);
                    if !model_due {
                        from = Some(now + delay);
                    }
                }
                4..=7 if model_due => {
                    // A pass went out: the next one is due at once.
                    assert_eq!(s.due(now), Due::Now);
                }
                8 if model_due => {
                    s.on_nothing_pending();
                    from = None;
                    // Nothing pending always ends in Idle.
                    assert_eq!(s.due(now), Due::Never);
                }
                _ => {}
            }
        }
    }

    /// A viewer joining a still screen gets a keyframe, every block at keyframe quality,
    /// with nothing on screen moving: passes sharpen it from the paint-over delay after
    /// the keyframe, as after motion. A keyframe in place of a due pass is that pass, and
    /// passes go on.
    #[test]
    fn a_keyframe_on_a_still_screen_is_sharpened() {
        let delay = Duration::from_millis(150);
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut s = Sharpening::new(delay);
        assert_eq!(s.due(ms(0)), Due::Never, "a still screen, all of it sharp");
        s.on_key(Keyframe::Fresh, ms(1000));
        assert_eq!(s.due(ms(1149)), Due::In(Duration::from_millis(1)));
        assert_eq!(
            s.due(ms(1150)),
            Due::Now,
            "a pass at the keyframe plus the delay"
        );
        // A keyframe goes out in place of that pass: passes go on at once.
        s.on_key(Keyframe::Pass, ms(1160));
        assert_eq!(s.due(ms(1160)), Due::Now);
    }

    /// With nothing moved, a due pass is captured only while the encoder holds nothing, so
    /// a keystroke that comes during it never waits behind a queued pass; with the encoder
    /// busy, or nothing due, the loop waits for a wake; a pass due later, until then.
    #[test]
    fn a_pass_goes_only_to_an_idle_encoder() {
        let ms = Duration::from_millis;
        let now = Instant::now() + Duration::from_secs(10);
        // A still screen, long since the last capture.
        let still = |due, encoding| {
            let look = Look {
                key: Key::NotWanted,
                damage: Damage::None,
                due,
                now,
                captured: Captured::new(now - Duration::from_secs(1)),
                min_interval: ms(14),
                coalesce: ms(2),
                gate: Gated::Open,
                encoding,
            };
            step(&look)
        };
        let on_wake = Step::Wait(Wait::Change);
        assert_eq!(still(Due::Now, Encoding::Idle), Step::Capture(Next::Pass));
        assert_eq!(still(Due::Now, Encoding::Busy), on_wake);
        assert_eq!(still(Due::Never, Encoding::Idle), on_wake);
        assert_eq!(
            still(Due::In(ms(40)), Encoding::Busy),
            Step::Wait(Wait::Pass(ms(40)))
        );
    }

    /// The frame interval as the capture loop applies it, over 20,000 random requests: a
    /// change asked for at `t` is captured at `t`, or a frame after the last change if that
    /// is later, and never later for a pass in between; a pass goes a frame after any
    /// capture, so passes never come faster than the frame rate or right behind a change.
    #[test]
    fn a_pass_never_holds_a_change_back() {
        let t0 = Instant::now();
        let frame = Duration::from_micros(14_167);
        let mut rng = 0x9E3779B97F4A7C15u64;
        let mut next = |n: u64| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % n
        };
        let mut captured = Captured::new(t0);
        let (mut now, mut last_any, mut last_change) = (t0, t0, t0);
        for _ in 0..20_000 {
            now += Duration::from_micros(next(30_000));
            let what = match next(3) {
                0 => Next::Change,
                _ => Next::Pass,
            };
            // The loop waits until a frame has passed since the capture that gates it.
            let at = now.max(captured.gate(what) + frame);
            match what {
                Next::Change => {
                    assert_eq!(at, now.max(last_change + frame), "a change held back");
                    last_change = at;
                }
                Next::Pass => assert!(at >= last_any + frame, "a pass within a frame"),
            }
            captured.on_capture(what, at);
            (now, last_any) = (at, at);
        }
    }

    /// Once the screen goes still the paint-overs show x264 forgetting the drag, so the
    /// next motion frame isn't handed the drag's correction.
    #[test]
    fn paintovers_wind_the_correction_down() {
        let crf = 24.0;
        let mut run = Run::new(true, crf, None);
        for _ in 0..40 {
            run.frame(Scene::Drag, None);
        }
        let during = crf - run.motion.crf(crf);
        for _ in 0..3 {
            run.frame(Scene::Drag, Some(16.0));
        }
        let after = crf - run.motion.crf(crf);
        assert!(
            after <= during / 2.0,
            "correction {after} after the paint-overs, {during} during the drag"
        );
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        /// A number of any size: a random one, shifted right by a random amount.
        fn any(&mut self) -> u64 {
            let shift = self.next() % 64;
            self.next() >> shift
        }
    }

    fn random_header(rng: &mut Rng) -> Header {
        Header {
            key: rng.next() & 1 == 1,
            sharpens: rng.next() & 1 == 1,
            frame: rng.any() as u32,
            pipe: Pipe(rng.any() as u32),
            stripe: rng.any() as u8,
            stripes: rng.any() as u8,
            parts: rng.any() as u8,
            y: rng.any() as u16,
        }
    }

    #[test]
    fn a_header_reads_back_as_it_was_written() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for _ in 0..100_000 {
            let header = random_header(&mut rng);
            let picture: Vec<u8> = (0..1 + rng.next() % 64).map(|_| rng.next() as u8).collect();
            let message = [&header.bytes()[..], &picture].concat();
            assert_eq!(Header::read(&message), Some(header));
            assert_eq!(&message[Header::LEN..], &picture[..]);
        }
    }

    #[test]
    fn only_a_video_message_with_a_picture_has_a_header() {
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        for _ in 0..100_000 {
            let header = random_header(&mut rng).bytes();
            // Its header alone, or any part of it.
            let cut = (rng.next() % (Header::LEN as u64 + 1)) as usize;
            assert_eq!(Header::read(&header[..cut]), None);
            // Another message's type byte, with anything after it.
            let mut other: Vec<u8> = (0..rng.next() % 64).map(|_| rng.next() as u8).collect();
            if other.first() == Some(&Header::VIDEO) {
                other[0] = Header::VIDEO.wrapping_add(1 + (rng.next() % 255) as u8);
            }
            assert_eq!(Header::read(&other), None);
        }
    }

    #[test]
    fn a_pipe_is_whole_tenths_of_a_millisecond() {
        let mut rng = Rng(0x6a09_e667_f3bc_c908);
        let tenth = Duration::from_micros(100);
        for _ in 0..100_000 {
            let d = Duration::from_nanos(rng.any());
            let Pipe(p) = Pipe::of(d);
            // As many whole tenths as fit, or as many as a header holds.
            assert!(tenth * p <= d, "{d:?} as {p}");
            if p < u32::MAX {
                assert!(d < tenth * (p + 1), "{d:?} as {p}");
            }
            let ms = Pipe(p).millis();
            assert!(ms * 10 <= p as u64 && p as u64 <= ms * 10 + 9);
        }
    }

    /// One case of web/src/video-header.json.
    #[derive(serde::Deserialize)]
    struct Case {
        header: String,
        picture: String,
        key: bool,
        sharpens: bool,
        frame: u32,
        pipe: u32,
        stripe: u8,
        stripes: u8,
        parts: u8,
        y: u16,
    }

    fn hex(s: &str) -> Vec<u8> {
        let digits: String = s.split_whitespace().collect();
        (0..digits.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&digits[i..i + 2], 16).unwrap())
            .collect()
    }

    /// The messages web/src/protocol.ts and bench/linkviewer.py check their readers against
    /// are what the server writes and reads.
    #[test]
    fn the_header_is_what_the_fixture_says() {
        let cases: Vec<Case> =
            serde_json::from_str(include_str!("../web/src/video-header.json")).unwrap();
        for c in cases {
            let header = Header {
                key: c.key,
                sharpens: c.sharpens,
                frame: c.frame,
                pipe: Pipe(c.pipe),
                stripe: c.stripe,
                stripes: c.stripes,
                parts: c.parts,
                y: c.y,
            };
            assert_eq!(header.bytes().to_vec(), hex(&c.header), "{}", c.header);
            let message = [hex(&c.header), hex(&c.picture)].concat();
            assert_eq!(Header::read(&message), Some(header), "{}", c.header);
        }
    }

    /// A viewer that joins while the stream is rebuilt (a resize, a display switch)
    /// starts on the rebuilt stream. The recorder needs it: it ends a recording at a
    /// hello once it holds a frame, so a frame of the old stream ahead of the rebuilt
    /// stream's hello is a recording of one frame, whatever length was asked for.
    /// Steps, the loops' own: a page joins and the capture loop plans its keyframe and
    /// clears its request; a resize ends the capture loop, and `encode.join` lets the
    /// real encode loop send that last job while a recording has just joined; then
    /// `build`'s `restart_clients` says hello.
    #[test]
    fn a_viewer_that_joins_while_the_stream_is_rebuilt_starts_on_the_rebuilt_stream() {
        let hub = Hub::new();
        let watch = || {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let c = hub.add_client(tx);
            c.watch();
            (c, rx)
        };
        let (w, h) = (64, 64);
        let ecfg = EncoderConfig {
            pictures: Pictures {
                width: w as u32,
                height: h as u32,
                fps: Fps::new(60).unwrap(),
                preset: Preset::parse("ultrafast").unwrap(),
                threads: 1,
                debug: Debugging::read(|_| None).unwrap(),
            },
            crf: 23.0,
            max_kbps: None,
        };
        let stripe = Stripe {
            y: 0,
            h,
            enc: Encoder::new(&ecfg).unwrap(),
            applied_kbps: 0,
            applied_crf: -1.0,
        };
        let (job_tx, job_rx) = mpsc::sync_channel::<Job>(1);
        let (ret_tx, _ret_rx) = mpsc::channel::<Returned>();
        let encode = std::thread::spawn({
            let hub = hub.clone();
            move || {
                encode_loop(
                    vec![stripe],
                    job_rx,
                    ret_tx,
                    hub,
                    Fps::new(60).unwrap(),
                    Crfs {
                        configured: 23.0,
                        paintover: 18.0,
                    },
                    Debugging::read(|_| None).unwrap(),
                )
            }
        });
        // A page joins; the capture loop plans its keyframe and clears its request.
        let (_page, _page_rx) = watch();
        let starts = hub
            .viewers()
            .into_iter()
            .map(|(c, _)| c)
            .filter(|c| c.keyframe_planned())
            .collect();
        let key = Job {
            buf: Box::new(I420::new(w, h)),
            frame: super::Frame::Key {
                blocks: None,
                starts,
            },
            target: Kbps::new(8000.0),
            capacity: None,
            captured: CaptureMarks::at(Instant::now()),
            marker_ms: None,
        };
        // A resize ends the capture loop, and a recording joins, before the encode loop
        // has coded that keyframe: it reads the viewers once the frame is coded.
        let (_recording, mut recording_rx) = watch();
        job_tx.send(key).unwrap();
        drop(job_tx);
        encode.join().unwrap();
        // build(): the rebuilt stream's hello.
        hub.restart_clients();
        assert!(
            matches!(recording_rx.try_recv().unwrap(), Out::Text(_)),
            "the recording held a frame of the old stream before the rebuilt stream's \
             hello, and ends there"
        );
    }

    /// The test binary's allocator on Linux: the system's, counting by thread what is
    /// allocated while it is armed.
    #[cfg(target_os = "linux")]
    #[global_allocator]
    static COUNTING: counting::Counting = counting::Counting;

    #[cfg(target_os = "linux")]
    mod counting {
        use std::alloc::{GlobalAlloc, Layout, System};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Mutex, MutexGuard};

        pub struct Counting;

        /// What one thread allocated while the count was armed.
        #[derive(Clone, Copy, Debug)]
        pub struct Thread {
            /// As the kernel has it: 15 bytes at most.
            pub name: [u8; 16],
            pub allocations: usize,
            /// How many of each size it allocated, which tells where they come from: the
            /// first 16 sizes it allocated, by size.
            pub sizes: [Option<(usize, usize)>; 16],
        }

        /// Every thread that allocated while armed: a fixed table, since the allocator
        /// cannot allocate to keep it.
        struct Seen {
            threads: [Option<Thread>; 32],
            /// Allocations by threads after the table's 32.
            others: usize,
        }

        static ARMED: AtomicBool = AtomicBool::new(false);
        static SEEN: Mutex<Seen> = Mutex::new(Seen {
            threads: [None; 32],
            others: 0,
        });

        fn seen() -> MutexGuard<'static, Seen> {
            SEEN.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
        }

        /// The calling thread's name, as the kernel has it.
        pub fn thread_name() -> [u8; 16] {
            let mut name = [0u8; 16];
            unsafe { libc::prctl(libc::PR_GET_NAME, name.as_mut_ptr()) };
            name
        }

        fn note(size: usize) {
            if !ARMED.load(Ordering::Relaxed) {
                return;
            }
            let name = thread_name();
            let mut seen = seen();
            let slot = seen
                .threads
                .iter()
                .position(|t| t.is_none_or(|t| t.name == name));
            let Some(i) = slot else {
                seen.others += 1;
                return;
            };
            let t = seen.threads[i].get_or_insert(Thread {
                name,
                allocations: 0,
                sizes: [None; 16],
            });
            t.allocations += 1;
            let same = t
                .sizes
                .iter()
                .position(|s| s.is_none_or(|(s, _)| s == size));
            if let Some(s) = same {
                t.sizes[s].get_or_insert((size, 0)).1 += 1;
            }
        }

        /// Counts every allocation from now on, by every thread of the process.
        pub fn arm() {
            *seen() = Seen {
                threads: [None; 32],
                others: 0,
            };
            ARMED.store(true, Ordering::SeqCst);
        }

        /// Stops counting: what each thread allocated since `arm`, and how many
        /// allocations came from threads past the table.
        pub fn disarm() -> (Vec<Thread>, usize) {
            ARMED.store(false, Ordering::SeqCst);
            let seen = seen();
            (
                seen.threads.iter().flatten().copied().collect(),
                seen.others,
            )
        }

        unsafe impl GlobalAlloc for Counting {
            unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                note(layout.size());
                System.alloc(layout)
            }
            unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
                note(layout.size());
                System.alloc_zeroed(layout)
            }
            unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
                note(size);
                System.realloc(ptr, layout, size)
            }
            unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
                System.dealloc(ptr, layout)
            }
        }
    }

    /// The counting allocator counts the whole process, where other tests allocate as
    /// they please: so `name` runs in a process of its own, the test binary started again
    /// with only that test selected. True in that process; in the first, starts it and
    /// checks that it ran the test and the test passed.
    #[cfg(target_os = "linux")]
    fn in_a_process_of_its_own(name: &str) -> bool {
        const ALONE: &str = "VOXL_TEST_IN_A_PROCESS_OF_ITS_OWN";
        if std::env::var_os(ALONE).is_some() {
            return true;
        }
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([name, "--exact", "--include-ignored", "--test-threads=1"])
            .env(ALONE, "1")
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && said.contains("1 passed"),
            "{said}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        false
    }

    /// A frame in steady state allocates nothing, whatever it is: the capture loop's
    /// conversion of its band into a buffer, the encode loop, the stripe workers and the
    /// hand-off of its messages to a viewer. The oracle is the allocator itself: it
    /// counts every allocation the process makes while frames go through.
    ///
    /// The stream is the one a viewer starts, at the defaults, with one viewer; the test
    /// plays the capture loop (with no X server it cannot grab, so it draws) and the
    /// viewer's socket task. First every kind of frame goes through, which brings every
    /// buffer the stream keeps to its size. Then a generated run of frames (each kind,
    /// bands of any height, block maps of any shape, keyframes among them) is counted.
    /// The maps the test hands the encode loop are made before the count, so what is
    /// counted is the pipeline's. What the capture loop does before it converts (taking
    /// the damage and the block map, sharpening's bookkeeping, the flow gate and the rate)
    /// needs an X server and is not run here. x264 and libyuv are C: what they allocate
    /// goes through C's malloc, which this allocator does not see.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "frames allocate on main: the encode loop's selected, results, parts and \
                msgs, its viewer list and each message's Bytes header; the stripe worker's \
                output and hint vectors; the blocks of the unbounded channels between the \
                loops"]
    fn a_steady_state_frame_allocates_nothing() {
        if !in_a_process_of_its_own("video::tests::a_steady_state_frame_allocates_nothing") {
            return;
        }
        let (w, h) = (1024usize, 768usize);
        let cfg = VideoConfig {
            display: None,
            fps: Fps::new(60).unwrap(),
            preset: Preset::parse("ultrafast").unwrap(),
            threads: 4,
            convert_threads: 4,
            crf: 24.0,
            bounds: Bounds::new(Kbps::new(2000.0), Some(Kbps::new(30000.0))),
            paintover_crf: 16.0,
            paintover_delay_ms: 150,
            coalesce: Duration::ZERO,
            stripes: 1,
            debug: Debugging::read(|_| None).unwrap(),
        };
        let hub = Hub::new();
        let (tx, mut socket) = tokio::sync::mpsc::unbounded_channel();
        let viewer = hub.add_client(tx);
        viewer.watch();
        // The capture loop planned its keyframe: the first frame played is one.
        viewer.keyframe_planned();
        *hub.stripes.lock() = stripe_layout(h as u32, cfg.stripes);
        let mut stream = Stream::new(&cfg, &hub, w as u32, h as u32).unwrap();
        let (cols, rows) = (w.div_ceil(16), h.div_ceil(16));
        let mut refiner = Refiner::new(cols * rows, cols, Instant::now());
        let mut rng = 0x2545F4914F6CDD1Du64;
        let mut next = |n: usize| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng as usize % n
        };
        // Bands of block rows, each with a map of the blocks in it that changed.
        let maps: Vec<(Band, Arc<Vec<u8>>)> = (0..8)
            .map(|_| {
                let first = next(rows);
                let last = first + next(rows - first);
                let map = (0..cols * rows)
                    .map(|b| ((first..=last).contains(&(b / cols)) && next(3) > 0) as u8)
                    .collect();
                let band = Band {
                    y0: (first * 16) as u32,
                    y1: ((last + 1) * 16).min(h) as u32,
                };
                (band, Arc::new(map))
            })
            .collect();
        let every = Arc::new(vec![1u8; cols * rows]);
        let sharpen = |map: &Arc<Vec<u8>>| Sharpen {
            map: map.clone(),
            at: Instant::now(),
        };
        let mut screen = vec![0u8; w * h * 4];
        // The viewer the first keyframe was planned for; it starts on it.
        let mut starts = vec![viewer.clone()];
        let mut capture = |kind: usize, which: usize, n: usize| {
            let (band, map) = &maps[which];
            let frame = match kind {
                0 => super::Frame::Key {
                    blocks: None,
                    starts: std::mem::take(&mut starts),
                },
                1 => super::Frame::Key {
                    blocks: Some(sharpen(&every)),
                    starts: std::mem::take(&mut starts),
                },
                2 => super::Frame::Motion {
                    band: *band,
                    dirty: every.clone(),
                },
                3 => super::Frame::Motion {
                    band: *band,
                    dirty: map.clone(),
                },
                4 => super::Frame::Sharpen {
                    band: *band,
                    blocks: sharpen(map),
                },
                _ => super::Frame::MotionAndSharpen {
                    band: *band,
                    dirty: map.clone(),
                    blocks: sharpen(map),
                },
            };
            // What moved: lines of 9 px glyphs scrolled 3 px a frame through the band.
            let (y0, y1) = match kind {
                0 | 1 => (0, h),
                _ => (band.y0 as usize, band.y1 as usize),
            };
            for (y, row) in screen.chunks_exact_mut(w * 4).enumerate().take(y1).skip(y0) {
                let (line, within) = ((y + 3 * n) / 18, (y + 3 * n) % 18);
                for (x, glyph) in row.chunks_mut(9 * 4).enumerate() {
                    let ink = within < 12 && (x * 2654435761 + line * 40503) >> 7 & 1 == 1;
                    glyph.fill(if ink { 30 } else { 235 });
                }
            }
            while stream.buf.is_none() {
                let r = stream.ret_rx.recv().unwrap();
                stream.returned(r, &mut refiner);
            }
            let mut buf = stream.buf.take().unwrap();
            crate::convert::convert_band(&screen, w * 4, &mut buf, y0, y1, &stream.rows);
            stream
                .job_tx
                .send(Job {
                    buf,
                    frame,
                    target: Kbps::new(6000.0),
                    capacity: None,
                    captured: CaptureMarks::at(Instant::now()),
                    marker_ms: None,
                })
                .unwrap();
            // The viewer: its socket task writes what was handed to it, and every frame is
            // acknowledged as it arrives.
            while let Ok(m) = socket.try_recv() {
                let Some(id) = (match &m {
                    Out::Bin(b) => frame_of(b),
                    Out::Text(_) => None,
                }) else {
                    continue;
                };
                hub.timeline.written(id, Instant::now());
                viewer.on_ack(Instant::now(), id, crate::hub::Hold::Unsaid);
            }
        };
        // Every kind of frame, over every band, twice: every buffer the stream keeps has
        // then been as large as these frames make it.
        let mut n = 0;
        for _ in 0..2 {
            for kind in 0..6 {
                for which in 0..maps.len() {
                    capture(kind, which, n);
                    n += 1;
                }
            }
        }
        // Each kind about thirty times, keyframes four.
        const COUNTED: usize = 120;
        let test = counting::thread_name();
        counting::arm();
        for _ in 0..COUNTED {
            // A keyframe now and then, as a viewer joining asks for one.
            let kind = match next(30) {
                0 => next(2),
                _ => 2 + next(4),
            };
            capture(kind, next(maps.len()), n);
            n += 1;
        }
        let (threads, others) = counting::disarm();
        drop(stream);
        let allocated: Vec<String> = threads
            .iter()
            .map(|t| {
                let name = match t.name == test {
                    true => "the capture loop and the viewer (this test's thread)".into(),
                    false => String::from_utf8_lossy(&t.name)
                        .trim_end_matches('\0')
                        .to_owned(),
                };
                let sizes: Vec<String> = t
                    .sizes
                    .iter()
                    .flatten()
                    .map(|(size, n)| format!("{size} B x {n}"))
                    .collect();
                format!(
                    "{name}: {:.2} a frame ({})",
                    t.allocations as f64 / COUNTED as f64,
                    sizes.join(", ")
                )
            })
            .collect();
        assert!(
            allocated.is_empty() && others == 0,
            "{COUNTED} frames allocated, by thread:\n{}\nand {others} times on threads past the table",
            allocated.join("\n")
        );
    }

    /// Recording a viewer's frames and acks allocates nothing once it has joined: its trace
    /// is made then, with room for every frame it keeps. The oracle is the counting
    /// allocator. The viewer is sent three rings' worth of frames, numbered from 1 again
    /// every 1000 as streams start over, as the encode loop records them; acks come as
    /// on_ack records them: for the frame just sent or up to three before it (several at
    /// once, out of order, twice), now and then for a number never sent, and none for a
    /// spell longer than the ring, after which an ack scans a whole ring of open frames;
    /// with every kind of hold.
    #[cfg(target_os = "linux")]
    #[test]
    fn recording_a_viewers_frames_and_acks_allocates_nothing() {
        use crate::hub::{Hold, Inflight};
        use crate::timeline::FRAMES_KEPT;
        if !in_a_process_of_its_own(
            "video::tests::recording_a_viewers_frames_and_acks_allocates_nothing",
        ) {
            return;
        }
        let hub = Hub::new();
        let viewer = hub.add_client(tokio::sync::mpsc::unbounded_channel().0);
        let t0 = Instant::now();
        // A fixed scramble of a frame's place in the run: what it is and how it is acked.
        let mix = |k: u64| k.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32;
        let deaf = FRAMES_KEPT as u64..FRAMES_KEPT as u64 * 9 / 4;
        counting::arm();
        for k in 0..3 * FRAMES_KEPT as u64 {
            let at = t0 + Duration::from_millis(k * 16);
            let id = (k % 1000) as u32 + 1;
            let frame = Inflight {
                id,
                sent: at,
                bytes: (mix(k) % 100_000) as usize,
                alone: mix(k) % 2 == 0,
            };
            viewer.trace.lock().sent(frame, mix(k) % 30 == 0);
            if deaf.contains(&k) {
                continue;
            }
            let hold = match mix(k) % 3 {
                0 => Hold::Unsaid,
                1 => Hold::Said(Duration::from_micros(mix(k) % 10_000)),
                _ => Hold::Unreadable,
            };
            let named = match mix(k) % 50 {
                0 => id + 5000,
                n => id.wrapping_sub((n % 4) as u32),
            };
            let acked = at + Duration::from_millis(mix(k) % 400);
            viewer.trace.lock().acked(acked, named, hold);
        }
        let (threads, others) = counting::disarm();
        assert!(
            threads.is_empty() && others == 0,
            "recording allocated: {threads:?}, and {others} times on threads past the table"
        );
    }

    /// A held gate names the soonest moment time alone can change its answer: a viewer it
    /// asked, with frames in flight, stalls. Over generated viewers (watching or not,
    /// a round trip of up to 4 s or none, up to 45 frames in flight whose oldest is about
    /// its round trip old, about to stall, or anything up to 5 s, to the microsecond) at 1
    /// to 240 fps, once what the gate found stalled
    /// is resynced as the capture loop does: asked again at any moment before it, with no
    /// ack, no viewer coming or going and no keyframe asked for, the gate is held and finds
    /// nobody stalled, so a wait until then misses nothing; that moment is when a viewer
    /// asked stalls, so the wait is no shorter; and asked at it, the gate names a later
    /// one.
    #[test]
    fn a_held_flow_gate_waits_until_a_viewer_stalls() {
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let now = Instant::now() + Duration::from_secs(10);
        // Held with a viewer before the full one stalling first, held by the full one's
        // own stall, and open.
        let mut seen = [0u32; 3];
        for case in 0..20_000 {
            let fps = 1 + next(240);
            let interval = Duration::from_secs_f64(1.0 / fps as f64).as_secs_f64() * 1000.0;
            let hub = Hub::new();
            // Each viewer's oldest frame in flight, if it watches and has any.
            let mut oldest = Vec::new();
            for _ in 0..1 + next(4) {
                let c = hub.add_client(tokio::sync::mpsc::unbounded_channel().0);
                let watching = next(5) > 0;
                if watching {
                    c.watch();
                }
                let rtt = next(4000);
                c.min_rtt_ms.store(rtt as u32, Ordering::Relaxed);
                // About its round trip old, about stalling, or anything up to 5 s.
                let age = Duration::from_micros(match next(3) {
                    0 => rtt * 1000 + next(200_000),
                    1 => 3_990_000 + next(20_000),
                    _ => next(5_000_000),
                });
                let frames = next(46);
                let mut q = c.inflight();
                for i in 0..frames {
                    q.push_back(crate::hub::Inflight {
                        id: i as u32,
                        sent: now - age.saturating_sub(Duration::from_micros(i * next(30_000))),
                        bytes: 1000,
                        alone: i == 0,
                    });
                }
                if watching {
                    oldest.extend(q.front().map(|f| f.sent));
                }
            }
            let gate = Gate::ask(&hub.viewers(), interval, now);
            for (c, b) in gate.stalled {
                resync(&c, b);
            }
            let Gated::Held { until } = gate.gated else {
                seen[2] += 1;
                continue;
            };
            let at = format!("case {case}: {fps} fps, held until {:?}", until - now);
            assert!(
                oldest.iter().any(|&sent| sent + STALLED == until),
                "not when a viewer stalls: {at}"
            );
            for t in [
                now,
                now + (until - now) / 2,
                until - Duration::from_micros(1),
            ] {
                let again = Gate::ask(&hub.viewers(), interval, t);
                assert!(
                    matches!(again.gated, Gated::Held { .. }) && again.stalled.is_empty(),
                    "changed at {:?}: {at}",
                    t - now
                );
            }
            assert_ne!(
                Gate::ask(&hub.viewers(), interval, until).gated,
                Gated::Held { until },
                "{at}"
            );
            // Whether the full one's own stall sets it, or one asked before it stalls first.
            let full = hub.viewers().into_iter().find_map(|(c, _)| {
                let rtt = c.rtt_ms().unwrap_or(RTT_GUESS_MS);
                match Flow::of(c.backlog(now), rtt, interval) {
                    Flow::Full(stalls_in) => Some(now + stalls_in),
                    Flow::Room(_) | Flow::Stalled(_) => None,
                }
            });
            seen[usize::from(full == Some(until))] += 1;
        }
        assert!(seen.iter().all(|&n| n > 100), "unreached: {seen:?}");
    }

    /// One look, as the capture loop reads it: the capturer's damage, whether a viewer
    /// wants a keyframe, the pass due, the last capture, the pacing, the
    /// gate, whether the buffer is home, the area the damage comes to when taken, and the
    /// blocks pending.
    #[derive(Clone, Copy)]
    struct StepCase<'a> {
        damage: Damage,
        want_key: bool,
        now: Instant,
        due: Due,
        captured: Captured,
        min_interval: Duration,
        coalesce: Duration,
        gate: Gated,
        home: bool,
        band: Option<Band>,
        refiner: &'a Refiner,
    }

    impl StepCase<'_> {
        fn key(&self) -> Key {
            match self.want_key {
                true => Key::Wanted,
                false => Key::NotWanted,
            }
        }
        fn look(&self) -> Look {
            Look {
                key: self.key(),
                damage: self.damage,
                due: self.due,
                now: self.now,
                captured: self.captured,
                min_interval: self.min_interval,
                coalesce: self.coalesce,
                gate: self.gate,
                encoding: match self.home {
                    true => Encoding::Idle,
                    false => Encoding::Busy,
                },
            }
        }
    }

    /// How many looks `each_case` makes: four frame rates, three coalesce windows, six
    /// damages, 28 pairs of times since the last change and the last capture, nine dues,
    /// six answers of the gate, and 16 ways for the rest.
    const CASES: usize = 4 * 3 * 6 * 28 * 9 * 6 * 16;

    /// Every look over a discretised domain: each state the capturer's damage takes, at
    /// four times since it came either side of three coalesce windows; four frame rates,
    /// at seven times since the last change and since the last capture (no longer ago than
    /// the change), either side of the least interval and its rounding to whole
    /// milliseconds; seven waits for a pass, a pass due and none; a
    /// keyframe wanted or not; the gate open, or holding until a viewer stalls now, either
    /// side of a millisecond or seconds away; the buffer home or out; damage that came to
    /// an area or to none; and blocks pending or none.
    fn each_case(mut check: impl FnMut(StepCase)) -> usize {
        let ns = Duration::from_nanos;
        let us = Duration::from_micros;
        let ms = Duration::from_millis;
        let now = Instant::now() + Duration::from_secs(10);
        let sharp = Refiner::new(12, 4, now);
        let mut pending = Refiner::new(12, 4, now);
        pending.mark(&[0, 0, 0, 0, 0, 1, 1, 0, 0, 0, 0, 1], now - ms(500));
        let ins = [
            ns(0),
            ns(1),
            ns(999_999),
            ms(1),
            ms(1) + ns(1),
            ms(40),
            ms(1500),
        ];
        let dues = ins.map(Due::In).into_iter().chain([Due::Never, Due::Now]);
        let gates = [
            ns(0),
            ns(1),
            us(999),
            ms(1),
            ms(1) + ns(1),
            Duration::from_secs(4),
        ]
        .map(|d| Gated::Held { until: now + d });
        let gates = [
            Gated::Open,
            gates[0],
            gates[1],
            gates[3],
            gates[4],
            gates[5],
        ];
        let mut cases = 0;
        for fps in [1, 30, 60, 144] {
            let interval = Duration::from_secs_f64(1.0 / fps as f64);
            let min_interval = interval.mul_f64(MIN_INTERVAL_SHARE);
            let sinces = [
                ns(0),
                min_interval - us(1000),
                min_interval - us(999),
                min_interval - ns(1),
                min_interval,
                min_interval + ns(1),
                Duration::from_secs(5),
            ];
            for coalesce in [ns(0), ms(2), ms(8)] {
                let settled = [
                    ns(0),
                    coalesce.saturating_sub(ns(1)),
                    coalesce,
                    coalesce + ms(1),
                ];
                let damages = [Damage::None, Damage::Unreported]
                    .into_iter()
                    .chain(settled.map(|e| Damage::Rows {
                        since: now - e,
                        y0: 16,
                        y1: 48,
                    }));
                // The last capture of any kind is the last change or later.
                let captures = sinces.iter().enumerate().flat_map(|(i, &change)| {
                    sinces[..=i].iter().map(move |&any| Captured {
                        any: now - any,
                        change: now - change,
                    })
                });
                for (damage, captured, due) in damages.flat_map(|d| {
                    let dues = dues.clone();
                    captures
                        .clone()
                        .flat_map(move |c| dues.clone().map(move |due| (d, c, due)))
                }) {
                    for (case, gate) in (0..16).flat_map(|case| gates.map(move |g| (case, g))) {
                        let bit = |i: u32| case >> i & 1 == 1;
                        check(StepCase {
                            damage,
                            want_key: bit(0),
                            now,
                            due,
                            captured,
                            min_interval,
                            coalesce,
                            gate,
                            home: bit(1),
                            band: bit(2).then_some(Band { y0: 16, y1: 48 }),
                            refiner: if bit(3) { &pending } else { &sharp },
                        });
                        cases += 1;
                    }
                }
            }
        }
        cases
    }

    /// A look waits for the first of these that holds it, and only for that, else it
    /// captures if the encoder is idle, or waits for the buffer to come home: a still
    /// screen (nothing damaged, no keyframe wanted, and no pass due, or one due while the
    /// encoder holds the buffer),
    /// for the pass due or, with none due, for a change; the rest of the least interval
    /// since the last capture; the rest of the coalesce window; the flow gate. The gate
    /// is asked only when nothing before it holds the capture and no keyframe is wanted.
    /// Checked over every look of `each_case`.
    #[test]
    fn a_capture_waits_only_for_what_holds_it() {
        let cases = each_case(|c| {
            let got = step(&c.look());
            let asked = got.asked_gate(c.key());
            // A due pass goes only to an idle encoder.
            let pass_goes = c.due == Due::Now && c.home;
            let still = !(c.damage != Damage::None || c.want_key || pass_goes);
            // A change waits a frame after the last change, a pass after any capture.
            let (next, after) = match c.damage != Damage::None || c.want_key {
                true => (Next::Change, c.captured.change),
                false => (Next::Pass, c.captured.any),
            };
            let since = c.now - after;
            let early = (since < c.min_interval).then(|| c.min_interval - since);
            let settling = match c.damage {
                Damage::Rows { since, .. } => Some(c.now - since),
                Damage::None | Damage::Unreported => None,
            }
            .filter(|&settled| settled < c.coalesce)
            .map(|settled| c.coalesce - settled);
            let gate_asked = !still && early.is_none() && settling.is_none() && !c.want_key;
            assert_eq!(asked, gate_asked, "the gate asked");
            let want = match (still, early, settling) {
                (true, _, _) => Step::Wait(match c.due {
                    Due::In(due) => Wait::Pass(due),
                    Due::Now | Due::Never => Wait::Change,
                }),
                (false, Some(rest), _) => Step::Wait(Wait::Interval { rest, next }),
                (false, None, Some(rest)) => Step::Wait(Wait::Settle(rest)),
                (false, None, None) => match (gate_asked, c.gate, c.home) {
                    (true, Gated::Held { until }, _) => Step::Wait(Wait::Gate(until - c.now)),
                    (_, _, true) => Step::Capture(next),
                    (_, _, false) => Step::Wait(Wait::Buffer),
                },
            };
            assert_eq!(
                got, want,
                "still {still}, early {early:?}, settling {settling:?}, gate asked \
                 {gate_asked} and {:?}, buffer home {}",
                c.gate, c.home
            );
        });
        assert_eq!(cases, CASES);
    }

    /// Each wait wakes on what it waits for, and ends on its time: a change on damage or
    /// a wake, with no limit; a pass on damage or a wake, or once due; the interval only on
    /// a wake, or once over; the gate on damage or a wake, or once a viewer will have
    /// stalled; a buffer only on a wake, with no limit; the coalesce window as a sleep of
    /// exactly its rest. Over every wait `step` makes in `each_case`.
    #[test]
    fn a_wait_wakes_on_what_it_waits_for() {
        let mut waits = 0;
        each_case(|c| {
            let Step::Wait(wait) = step(&c.look()) else {
                return;
            };
            waits += 1;
            let (mut polled, mut slept) = (None, None);
            wait.through(|on, t| polled = Some((on, t)), |t| slept = Some(t));
            let want = match wait {
                Wait::Change => (Some((On::Event, None)), None),
                Wait::Pass(due) => (Some((On::Event, Some(due))), None),
                Wait::Interval { rest, .. } => (Some((On::Wake, Some(rest))), None),
                Wait::Settle(rest) => (None, Some(rest)),
                Wait::Gate(stalls) => (Some((On::Event, Some(stalls))), None),
                Wait::Buffer => (Some((On::Wake, None)), None),
            };
            assert_eq!((polled, slept), want, "{wait:?}");
        });
        assert!(waits > 0);
    }

    /// A capture is a keyframe while one is wanted, in place of a due pass when nothing
    /// moved; else what moved, if anything did; else a due pass sharpens every pending
    /// block, or finds the screen sharp; else nothing is sent. Over every look of
    /// `each_case`.
    #[test]
    fn a_capture_is_sent_as_what_it_holds() {
        let cases = each_case(|c| {
            let pending = c.refiner.pending_map();
            let shown = format!("key {}, band {:?}, due {:?}", c.want_key, c.band, c.due);
            match (plan(c.key(), c.band, c.due, c.refiner), c.due) {
                (Planned::Send(Plan::Key(Keyframe::Pass)), Due::Now) => {
                    assert!(c.want_key && c.band.is_none(), "{shown}")
                }
                (Planned::Send(Plan::Key(Keyframe::Fresh)), due) => {
                    assert!(
                        c.want_key && (c.band.is_some() || due != Due::Now),
                        "{shown}"
                    )
                }
                (Planned::Send(Plan::Motion(moved)), _) => {
                    assert!(!c.want_key, "{shown}");
                    assert_eq!(Some((moved.y0, moved.y1)), c.band.map(|b| (b.y0, b.y1)));
                }
                (Planned::Send(Plan::Sharpen { map, band }), Due::Now) => {
                    assert!(!c.want_key && c.band.is_none(), "{shown}");
                    let (want, want_band) = pending.expect("a pass sharpens what is pending");
                    assert_eq!(map, want);
                    assert_eq!((band.y0, band.y1), (want_band.y0, want_band.y1));
                }
                (Planned::Sharp, Due::Now) => {
                    assert!(
                        !c.want_key && c.band.is_none() && pending.is_none(),
                        "{shown}"
                    )
                }
                (Planned::Empty, Due::In(_) | Due::Never) => {
                    assert!(!c.want_key && c.band.is_none(), "{shown}")
                }
                (_, due) => panic!("{shown}: planned with {due:?}"),
            }
        });
        assert_eq!(cases, CASES);
    }

    /// The capturer's damage, against what was drawn: over every sequence of five events
    /// from a new capturer (rows drawn at three places, the screen marked to capture, a
    /// take), damage is due exactly when something was drawn or marked since the last
    /// take; it has rows exactly when rows were drawn, since the first of them, spanning
    /// them all; and a take returns those rows widened to even rows within the picture,
    /// and leaves nothing.
    #[test]
    fn damage_is_what_was_drawn_since_the_take() {
        let t0 = Instant::now();
        let height = 101;
        let rows = [(0, 1), (17, 48), (90, 101)];
        for seq in 0..5u32.pow(5) {
            let mut damage = Damage::Unreported;
            // Since the last take: when rows were first drawn and which, and whether
            // anything at all is due (a new capturer is).
            let mut drawn: Option<(Instant, u32, u32)> = None;
            let mut due: bool;
            for i in 0..5 {
                let now = t0 + Duration::from_millis(i as u64);
                match seq / 5u32.pow(i) % 5 {
                    r @ 0..=2 => {
                        let (top, bot) = rows[r as usize];
                        damage.add(top, bot, now);
                        drawn = Some(match drawn {
                            Some((since, a, b)) => (since, a.min(top), b.max(bot)),
                            None => (now, top, bot),
                        });
                        due = true;
                    }
                    3 => {
                        damage.mark();
                        due = true;
                    }
                    _ => {
                        let band = damage.take(height).map(|b| (b.y0, b.y1));
                        let want = drawn.map(|(_, a, b)| (a & !1, ((b + 1) & !1).min(height)));
                        assert_eq!(band, want, "sequence {seq}, event {i}");
                        assert_eq!(damage, Damage::None);
                        (drawn, due) = (None, false);
                    }
                }
                let want = match (drawn, due) {
                    (Some((since, y0, y1)), _) => Damage::Rows { since, y0, y1 },
                    (None, true) => Damage::Unreported,
                    (None, false) => Damage::None,
                };
                assert_eq!(damage, want, "sequence {seq}, event {i}");
            }
        }
    }

    /// Damage that comes while the loop waits out the frame interval for a pass ends the
    /// wait: it makes the capture a change, which the pass does not hold back. Waiting out
    /// the interval for a change, more damage changes nothing, and only a wake ends it
    /// early. Over every interval wait `step` makes in `each_case`.
    #[test]
    #[ignore = "fails until the next commit: a pass's interval wait wakes on the waker alone"]
    fn damage_ends_the_wait_before_a_pass() {
        let mut waits = [0; 2];
        each_case(|c| {
            let Step::Wait(wait @ Wait::Interval { .. }) = step(&c.look()) else {
                return;
            };
            // What the capture held back would be, as the look says.
            let (next, on) = match c.damage != Damage::None || c.want_key {
                true => (Next::Change, On::Wake),
                false => (Next::Pass, On::Event),
            };
            waits[usize::from(next == Next::Pass)] += 1;
            let mut polled = None;
            wait.through(|on, _| polled = Some(on), |_| {});
            assert_eq!(polled, Some(on), "waiting out the interval for {next:?}");
        });
        assert!(waits.iter().all(|&n| n > 0), "unreached: {waits:?}");
    }
}
