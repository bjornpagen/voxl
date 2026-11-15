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
use bytes::{BufMut, Bytes, BytesMut};
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

    /// What message `m` is to a reader of video.
    pub fn read(m: &[u8]) -> Result<Video<'_>, NotVideo> {
        match m.split_first_chunk::<{ Header::LEN }>() {
            _ if m.first() != Some(&Header::VIDEO) => Err(NotVideo::Other),
            None => Err(NotVideo::Short),
            Some((b, [])) => Ok(Video::Empty(Header::of(b))),
            Some((b, picture)) => Ok(Video::Stripe(Header::of(b), picture)),
        }
    }

    fn of(b: &[u8; Header::LEN]) -> Header {
        let u32_at = |i: usize| u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        Header {
            key: b[1] & Header::KEY != 0,
            sharpens: b[1] & Header::SHARPENS != 0,
            frame: u32_at(2),
            pipe: Pipe(u32_at(6)),
            stripe: b[10],
            stripes: b[11],
            parts: b[12],
            y: u16::from_be_bytes([b[14], b[15]]),
        }
    }
}

/// A video message, read.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Video<'a> {
    /// A stripe's header, and its H.264 after it.
    Stripe(Header, &'a [u8]),
    /// A header with nothing after it. The encode loop sends no stripe that coded nothing.
    Empty(Header),
}

/// A message that is not a video message.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NotVideo {
    /// Another kind: its first byte is not video's type byte, or it has no byte at all.
    Other,
    /// Video's type byte, and fewer bytes than a header.
    Short,
}

struct Job {
    /// The picture, shared with the stripe workers while they code it, and the capture
    /// loop's alone again once it comes back.
    buf: Arc<I420>,
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
    buf: Arc<I420>,
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
    /// None while the encoder holds it.
    buf: Option<Arc<I420>>,
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
        // One picture goes round, so no more than one comes back at once.
        let (ret_tx, ret_rx) = mpsc::sync_channel::<Returned>(1);
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
                applied: None,
            });
        }
        let hub = hub.clone();
        let crfs = Crfs {
            configured: cfg.crf,
            paintover: cfg.paintover_crf,
        };
        let (fps, debug) = (cfg.fps, cfg.debug);
        let encode = encoder_thread(hub.clone(), move || {
            encode_loop(stripes, job_rx, ret_tx, hub, fps, crfs, debug)
        });
        Ok(Stream {
            job_tx,
            ret_rx,
            buf: Some(Arc::new(I420::new(w as usize, h as usize))),
            pending: Some(Band { y0: 0, y1: h }),
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
/// A viewer whose oldest frame in flight is this old has stalled (a dead socket, a path
/// queued for seconds): 4 s. Upstream's guess (051bb63), unmeasured. Until then its frames
/// past `MOST_QUEUED_MS` hold every viewer's picture; from then it holds no other viewer's.
/// Nothing is done to it: on TCP its frames are late, not lost, and a keyframe, the largest
/// thing the stream sends, would reach it last. At half (2 s), a path whose round trip
/// swings by 2 s (a slow link's reached 2.7 s) stops holding the others at every swing; at
/// double (8 s), one stopped viewer freezes every other viewer's picture for 8 s. It is
/// also the longest a sharpening pass waits for one viewer's ack (`InFlight`): at double, a
/// viewer that answers nothing slows every other viewer's sharpening to a pass per 8 s.
pub const STALLED: Duration = Duration::from_millis(4000);

/// What one viewer's frames in flight say about capturing another now.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Flow {
    /// Room for one more.
    Room,
    /// As many in flight as its round trip covers, or its oldest past the queue it may
    /// have: the capture waits for its acks, or until it stalls, this long from now: the
    /// one change time alone brings.
    Full(Duration),
    /// Its oldest is `STALLED` old: it has no room, and holds no other viewer.
    Stalled,
}

impl Flow {
    /// From what it has in flight, its round trip, and the time between frames.
    fn of(backlog: Option<Backlog>, rtt_ms: u32, interval_ms: f64) -> Flow {
        let Some(b) = backlog else {
            return Flow::Room;
        };
        if b.oldest >= STALLED {
            return Flow::Stalled;
        }
        let age = b.oldest.as_millis();
        // Enough frames in flight to cover one round trip plus decode, and no more than
        // `MOST_QUEUED_MS` of queue past the round trip.
        let rtt = rtt_ms as f64;
        let room =
            (((rtt + DECODE_MS) / interval_ms).ceil() as usize + SPARE_FRAMES).min(MOST_IN_FLIGHT);
        if b.frames.get() >= room || age > rtt_ms as u128 + MOST_QUEUED_MS {
            Flow::Full(STALLED - b.oldest)
        } else {
            Flow::Room
        }
    }
}

impl Gated {
    /// Asks `viewers` at `now`, in the hub's order, up to the first that is full. It only
    /// reads. A stalled viewer holds no other: the gate opens while another has room.
    /// Alone, or beside only stalled ones, it holds the capture until its acks come, so a
    /// viewer alone is never sent more than its acks allow.
    fn ask(viewers: &[(Arc<Client>, Viewer)], interval_ms: f64, now: Instant) -> Gated {
        let mut gated = Gated::Held { until: None };
        for (c, _) in viewers {
            let rtt = c.rtt_ms().unwrap_or(RTT_GUESS_MS);
            match Flow::of(c.backlog(now), rtt, interval_ms) {
                Flow::Room => gated = Gated::Open,
                Flow::Full(stalls_in) => {
                    let until = Some(now + stalls_in);
                    return Gated::Held { until };
                }
                Flow::Stalled => {}
            }
        }
        gated
    }
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
    /// The sharpening pass due in this long, or damage or a wake: an ack that empties the
    /// viewers' paths sooner is one.
    Pass(Duration),
    /// The rest of the least interval since the capture that gates this one (`Captured`),
    /// or what the capture it holds back is woken by (`Next::interval_ends`).
    Interval { rest: Duration, next: Next },
    /// The rest of the coalesce window, whatever happens.
    Settle(Duration),
    /// The flow gate: an ack, a viewer going or a keyframe asked for (a wake), or damage
    /// to take in, or in this long, when the viewer it holds for will have stalled.
    Gate(Option<Duration>),
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
            Wait::Gate(stalls) => poll(On::Event, stalls),
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

/// Whether the flow gate lets a capture go: open while a viewer has room and none asked
/// is `Full`.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Gated {
    Open,
    /// Held by a full viewer until it will have stalled, or, with none, because no viewer
    /// has room (each has stalled). Before then only an ack, a viewer coming or going or
    /// a keyframe asked for can open the gate, and each of those wakes the hub.
    Held {
        until: Option<Instant>,
    },
}

/// Whether the encoder holds the capture loop's buffer.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Encoding {
    Idle,
    Busy,
}

/// What the viewers have yet to acknowledge, as a sharpening pass sees it. A pass goes out
/// only once they have acknowledged everything: on a path slower than what the encoder
/// makes of a page, passes sent one after another queue on it, and the keystroke after
/// them waits behind them all (a 0.3 to 0.66 Mbit/s path stayed busy with 743 passes for
/// 36 s after a 12 s scroll). Motion is not held: the first place on an empty path is
/// the next keystroke's.
///
/// A viewer holds a pass until it acknowledges the newest frame it was sent, or until that
/// frame has had `STALLED`. One that answers nothing (a dead socket) so slows the others'
/// sharpening to a pass per `STALLED`, and cannot stop it. One that answers but is seconds
/// behind (a slow path's queue) holds every pass until it has caught up: the age of its
/// oldest frame, which the flow gate reads, would let passes pour into that queue.
#[derive(Clone, Copy, Debug, PartialEq)]
enum InFlight {
    /// Every viewer has acknowledged all it was sent, or has had `STALLED` to acknowledge
    /// the newest of it.
    Nothing,
    /// A viewer has yet to acknowledge a frame sent less than `STALLED` ago: in this long
    /// the newest of them all will have had `STALLED`.
    Frames(Duration),
}

impl InFlight {
    /// From what each viewer has in flight.
    fn of(backlogs: impl IntoIterator<Item = Option<Backlog>>) -> InFlight {
        backlogs
            .into_iter()
            .flatten()
            .filter(|b| b.newest < STALLED)
            .map(|b| STALLED - b.newest)
            .max()
            .map_or(InFlight::Nothing, InFlight::Frames)
    }
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
    in_flight: InFlight,
}

/// What the loop does next, from what it knows.
fn step(look: &Look) -> Step {
    let next = match (look.damage, look.key) {
        // Nothing moved and no keyframe is wanted. A pass goes only to an idle encoder,
        // as every capture does (below); until then a change may come, and with it the
        // capture is motion's.
        (Damage::None, Key::NotWanted) => match (look.due, look.encoding, look.in_flight) {
            (Due::Now, Encoding::Idle, InFlight::Nothing) => Next::Pass,
            // Nor onto a path that has frames on it: behind them, it would hold the next
            // keystroke back by all of them.
            (Due::Now, Encoding::Idle, InFlight::Frames(last)) => {
                return Step::Wait(Wait::Pass(last))
            }
            (Due::Now, Encoding::Busy, _) | (Due::Never, _, _) => return Step::Wait(Wait::Change),
            (Due::In(due), _, _) => return Step::Wait(Wait::Pass(due)),
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
        return Step::Wait(Wait::Gate(until.map(|until| until - look.now)));
    }
    // A capture goes only to an idle encoder. One made while it codes the frame before
    // would wait for it, a whole encode older by the time it is coded; made once it is
    // done, it holds what changed meanwhile, and goes at once.
    match look.encoding {
        Encoding::Idle => Step::Capture(next),
        Encoding::Busy => Step::Wait(Wait::Buffer),
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
    /// Before a pass, damage too: it makes the capture a change, which the pass does not
    /// hold back.
    fn interval_ends(self) -> On {
        match self {
            Next::Change => On::Wake,
            Next::Pass => On::Event,
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
        let look = Look {
            key,
            damage: cap.damage,
            due,
            now,
            captured,
            min_interval,
            coalesce: cfg.coalesce,
            gate: Gated::ask(&viewers, interval_ms, now),
            encoding: s.encoding(),
            in_flight: InFlight::of(viewers.iter().map(|(c, _)| c.backlog(now))),
        };
        let step = step(&look);
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
            let pic = Arc::get_mut(&mut buf).expect("a picture comes back from every stripe");
            cap.convert(pic, t.y0 as usize, t.y1 as usize, cfg.convert_threads);
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
    match Header::read(m) {
        Ok(Video::Stripe(header, _) | Video::Empty(header)) => Some(header.frame),
        Err(NotVideo::Other | NotVideo::Short) => None,
    }
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
    /// The rate the encoder was last set to for a frame that isn't a keyframe, while it
    /// holds: none before the first such frame, or after a keyframe's boost or a fresh
    /// encoder, so the next one sets it.
    applied: Option<Applied>,
}

/// The CRF and cap a stripe's encoder was set to.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Applied {
    crf: f32,
    kbps: u32,
}

/// How far a stripe's cap may move, as a share of the one set, before x264 is told:
/// each change reconfigures the encoder. Upstream's guess, unmeasured. At none, every
/// wobble of the frame budget reconfigures x264; at twice this, a frame may be coded
/// under a cap 8% off its share of the budget.
const CAP_SLACK: f64 = 0.04;

impl Applied {
    /// The rate to set before a frame at `crf` under `cap`, unless the one set (`was`)
    /// still holds: the same CRF, and a cap within CAP_SLACK of it.
    fn next(was: Option<Applied>, crf: f32, cap: u32) -> Option<Applied> {
        let holds = |a: Applied| {
            a.crf == crf && (cap as f64 - a.kbps as f64).abs() <= a.kbps as f64 * CAP_SLACK
        };
        match was {
            Some(a) if holds(a) => None,
            Some(_) | None => Some(Applied { crf, kbps: cap }),
        }
    }
}

#[derive(Clone)]
struct StripeJob {
    /// The frame's picture, lent to every stripe that codes it.
    buf: Arc<I420>,
    ts: i64,
    coding: Coding,
    cap: u32,
    mb_dirty: Dirty,
    mb_cols: usize,
}

/// What a stripe worker sends back for its part of a frame.
struct StripeResult {
    out: StripeOut,
    /// The picture it was lent, handed back with the result, so that once every result
    /// is in the capture loop has it alone again.
    buf: Arc<I420>,
}

/// What codes a frame's stripes. The encode loop codes stripe 0 itself, so a frame of one
/// stripe is coded on the thread that received it, with no stage that only hands it on.
/// The others have a worker thread each, with its stripe's encoder, reached by two
/// channels of its own: the encode loop holds one end of each and nothing else does, so
/// a worker that is gone has dropped the other ends, a job handed to it is refused and a
/// wait for its result ends at once. Each channel holds one message, since a worker has
/// one job at a time.
struct Crew {
    own: Box<dyn FnMut(StripeJob) -> StripeResult + Send>,
    /// Stripe 1's worker onwards.
    hands: Vec<Hand>,
    /// What came of the last frame: kept, so a frame allocates nothing for it.
    out: FrameOut,
}

/// The encode loop's ends of one stripe worker's channels.
struct Hand {
    jobs: mpsc::SyncSender<StripeJob>,
    done: mpsc::Receiver<StripeResult>,
}

/// A stripe worker is gone, so its stripe can't be coded again: the pipeline ends, as it
/// does when the encode loop is gone.
#[derive(Debug)]
struct Gone(usize);

impl Crew {
    /// `own` codes stripe 0; each of the others gets a thread, where `work` codes the jobs
    /// that arrive on its receiver and answers each on its sender.
    fn spawn<W>(
        own: impl FnMut(StripeJob) -> StripeResult + Send + 'static,
        workers: impl IntoIterator<Item = W>,
    ) -> Crew
    where
        W: FnOnce(mpsc::Receiver<StripeJob>, mpsc::SyncSender<StripeResult>) + Send + 'static,
    {
        let hands = workers
            .into_iter()
            .enumerate()
            .map(|(i, work)| {
                let (jobs, rx) = mpsc::sync_channel(1);
                let (answer, done) = mpsc::sync_channel(1);
                std::thread::Builder::new()
                    .name(format!("enc-{}", i + 1))
                    .spawn(move || work(rx, answer))
                    .unwrap();
                Hand { jobs, done }
            })
            .collect();
        Crew {
            own: Box::new(own),
            hands,
            out: FrameOut(Vec::new()),
        }
    }

    /// Lends the job's picture to the workers of the selected stripes, codes stripe 0
    /// while they code theirs, and waits for their results. The picture comes back with
    /// them, no longer shared.
    fn run(
        &mut self,
        selected: &[bool],
        job: StripeJob,
    ) -> Result<(&mut FrameOut, Arc<I420>), Gone> {
        let asked = || {
            self.hands
                .iter()
                .zip(&selected[1..])
                .enumerate()
                .filter(|(_, (_, &on))| on)
                .map(|(j, (hand, _))| (j + 1, hand))
        };
        for (i, hand) in asked() {
            hand.jobs.send(job.clone()).map_err(|_| Gone(i))?;
        }
        let results = &mut self.out.0;
        results.clear();
        results.extend((0..=self.hands.len()).map(|_| StripeOut::Skipped));
        if selected[0] {
            results[0] = (self.own)(job.clone()).out;
        }
        for (i, hand) in asked() {
            let StripeResult { out, buf } = hand.done.recv().map_err(|_| Gone(i))?;
            results[i] = out;
            drop(buf);
        }
        Ok((&mut self.out, job.buf))
    }
}

/// What came of a frame in one stripe.
enum StripeOut {
    /// The frame didn't touch the stripe, so its worker wasn't asked.
    Skipped,
    /// The encode failed.
    Failed,
    /// x264 put out nothing for the picture. With no B-frames, lookahead or frame
    /// threads it holds no picture back, so this doesn't come.
    Empty,
    /// The stripe's part of the frame: a header's room, then what x264 put out.
    Sent {
        key: bool,
        bytes: BytesMut,
        quality: Quality,
        /// When the worker took the stripe, handed it to x264 and had it back.
        coded: Coded,
    },
}

impl StripeOut {
    /// What x264 made of a stripe's picture, asked for at `crf`; `bytes` is a header's
    /// room and what it put out.
    fn coded(e: &Encoded, crf: f32, bytes: BytesMut, coded: Coded) -> StripeOut {
        match bytes.len() {
            Header::LEN => StripeOut::Empty,
            _ => StripeOut::Sent {
                key: e.keyframe,
                quality: e.quality(crf),
                bytes,
                coded,
            },
        }
    }
}

/// What came of a frame in each stripe, in stripe order.
struct FrameOut(Vec<StripeOut>);

impl FrameOut {
    /// Whether the frame's blocks came out sharp: every stripe it touched came back as
    /// asked.
    fn sharp(&self) -> bool {
        self.0.iter().all(|o| match o {
            StripeOut::Skipped => true,
            StripeOut::Sent { quality, .. } => *quality == Quality::AsAsked,
            StripeOut::Failed => false,
            // As the picture x264 didn't fill in read: a CRF average of 0, so as asked.
            StripeOut::Empty => true,
        })
    }

    /// Whether a viewer can start on the frame: every stripe is a keyframe.
    fn all_key(&self) -> bool {
        self.0.iter().all(|o| match o {
            StripeOut::Sent { key, .. } => *key,
            StripeOut::Skipped | StripeOut::Failed | StripeOut::Empty => false,
        })
    }

    /// When the stripe coded last was coded, if any stripe has a part to send: the frame
    /// waits for it.
    fn coded(&self) -> Option<Coded> {
        self.0
            .iter()
            .filter_map(|o| match o {
                StripeOut::Sent { coded, .. } => Some(*coded),
                StripeOut::Skipped | StripeOut::Failed | StripeOut::Empty => None,
            })
            .reduce(Coded::later)
    }

    /// How many parts go to the viewers.
    fn parts(&self) -> usize {
        self.0
            .iter()
            .filter(|o| match o {
                StripeOut::Sent { .. } => true,
                StripeOut::Skipped | StripeOut::Failed | StripeOut::Empty => false,
            })
            .count()
    }

    /// The parts that go to the viewers, taken out: each stripe that has some, whether
    /// it is a keyframe, and its bytes.
    fn take_parts(&mut self) -> impl Iterator<Item = (usize, bool, BytesMut)> + '_ {
        self.0.drain(..).enumerate().filter_map(|(i, o)| match o {
            StripeOut::Sent { key, bytes, .. } => Some((i, key, bytes)),
            StripeOut::Skipped | StripeOut::Failed | StripeOut::Empty => None,
        })
    }
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

/// Codes one stripe's part of each frame: its encoder, and what it has learned of the
/// frames it coded (how cheap their motion is).
struct Coder {
    s: Stripe,
    fps: Fps,
    crfs: Crfs,
    debug: bool,
    motion: MotionQuality,
    /// What x264 put out for the last frame: kept, so a frame allocates nothing for it.
    out: Vec<u8>,
    /// x264's hint for each macroblock of the stripe (1: unchanged), kept likewise.
    hints: Vec<u8>,
    arenas: Arenas,
}

/// The bytes a stripe's messages are cut from, in two arenas taken in turn. A message
/// is the next stretch of the arena in use; when that one is full the other is taken
/// back, once every message cut from it has been sent and dropped. So a stream whose
/// viewers keep up allocates nothing for its messages.
struct Arenas {
    two: [BytesMut; 2],
    at: usize,
}

/// What each of a stripe's two arenas holds. Sized from the four numbers' scroll scene:
/// a message there is about 11 KB, so an arena holds some 90 frames' messages, and is
/// taken back after the other has held as many, long after a viewer that keeps up has
/// been sent them; a keyframe of 300 KB fits three times. Half would still hold 45
/// frames; one message bigger than an arena gets an arena of its own size. Unmeasured
/// beyond that.
const ARENA_BYTES: usize = 1 << 20;

impl Arenas {
    fn new() -> Arenas {
        // Cutting from an arena the first time allocates bytes' count of who shares it:
        // done here, once, and not on some later frame.
        let shared = || {
            let mut arena = BytesMut::with_capacity(ARENA_BYTES);
            drop(arena.split());
            arena
        };
        Arenas {
            two: [shared(), shared()],
            at: 0,
        }
    }

    /// A message: a header's room, then `payload`.
    fn message(&mut self, payload: &[u8]) -> BytesMut {
        let len = Header::LEN + payload.len();
        if !self.two[self.at].try_reclaim(len) {
            self.at = 1 - self.at;
            if !self.two[self.at].try_reclaim(len) {
                self.two[self.at] = BytesMut::with_capacity(ARENA_BYTES.max(len));
            }
        }
        let arena = &mut self.two[self.at];
        arena.put_bytes(0, Header::LEN);
        arena.put_slice(payload);
        arena.split()
    }
}

impl Coder {
    fn new(s: Stripe, fps: Fps, crfs: Crfs, debug: bool) -> Coder {
        Coder {
            s,
            fps,
            crfs,
            debug,
            motion: MotionQuality::default(),
            out: Vec::new(),
            hints: Vec::new(),
            arenas: Arenas::new(),
        }
    }

    /// Codes the stripe's part of the job's frame, and hands the picture back with it.
    fn code(&mut self, job: StripeJob) -> StripeResult {
        let woke = Instant::now();
        let Coder {
            s,
            fps,
            crfs,
            debug,
            motion,
            out,
            hints,
            arenas,
        } = self;
        let buf = &*job.buf;
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
            s.applied = None;
        } else if let Some(rate) = Applied::next(s.applied, crf, job.cap) {
            s.enc.set_rate(rate.crf, rate.kbps);
            s.applied = Some(rate);
        }
        let suv = buf.stride_uv();
        let planes = Planes {
            y: &buf.y[s.y * buf.width..],
            u: &buf.u[(s.y / 2) * suv..],
            v: &buf.v[(s.y / 2) * suv..],
            stride_y: buf.width,
            stride_uv: suv,
        };
        out.clear();
        // x264 wants X264_MBINFO_CONSTANT (1) on unchanged macroblocks of this stripe.
        let info = match &job.mb_dirty {
            Dirty::All => None,
            Dirty::Blocks(d) => {
                let rows = s.h.div_ceil(16);
                let start = (s.y / 16) * job.mb_cols;
                hints.clear();
                hints.extend(
                    d[start..start + rows * job.mb_cols]
                        .iter()
                        .map(|&x| (x == 0) as u8),
                );
                Some(&mut hints[..])
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
            info,
            out,
        );
        let encoded = Instant::now();
        if *debug {
            let cpu_now = thread_cpu_us();
            log::info!(
                "stripe y{} encode {} us wall, thread cpu total {} us, {} bytes",
                s.y,
                te.elapsed().as_micros(),
                cpu_now,
                Header::LEN + out.len()
            );
        }
        let result = match r {
            Ok(e) => {
                let size = matches!(job.coding, Coding::Motion).then(|| MotionSize {
                    bits: out.len() * 8,
                    changed,
                    mbs: stripe_mbs,
                    crf: asked,
                });
                motion.note(&e, crf, size);
                StripeOut::coded(
                    &e,
                    crf,
                    arenas.message(out),
                    Coded {
                        woke,
                        x264: te,
                        encoded,
                    },
                )
            }
            Err(e) => {
                log::error!("{e}");
                StripeOut::Failed
            }
        };
        StripeResult {
            out: result,
            buf: job.buf,
        }
    }
}

/// A stripe worker: codes each job that arrives, on its own thread, and answers with it.
fn stripe_worker(
    mut coder: Coder,
    rx: mpsc::Receiver<StripeJob>,
    done: mpsc::SyncSender<StripeResult>,
) {
    while let Ok(job) = rx.recv() {
        if done.send(coder.code(job)).is_err() {
            return;
        }
    }
}

/// More frames than this in flight to a viewer, and it is closed (it stops watching, and is
/// sent the close once): upstream's guess (its first commit), never measured; 10 s of
/// frames at 60 a second. The flow check keeps a viewer that holds the capture to at most
/// 40 frames in flight, two more in the pipeline, and a viewer alone always holds it. A
/// viewer gets past that two ways. A keyframe passes the flow check: while a client asks
/// for one at every capture (the page asks at most every 500 ms), the flow check never
/// runs, and a viewer that acks nothing is sent one keyframe a capture until this bound
/// closes it, after 10 s at 60 a second, with 36 to 108 MB of keyframes (60 to 180 KB on a
/// terminal at 3024x1898) queued for a socket that may not be read. And a viewer whose
/// oldest frame is 4 s old holds no other, so beside one with room it is sent every frame,
/// and is closed once it has stayed so for about 9 s of frames at 60 a second. At half
/// these are 5 s and 4 s; at double 20 s and 19 s, with twice that queued.
const HOPELESSLY_BEHIND: usize = 600;

/// Starts the encode loop's thread with `run`, which owns the way home for the capture
/// loop's pictures. However `run` ends, by returning or by a panic, it has closed that
/// way by then, and the capture loop is woken to find it closed: it ends on "encoder
/// gone", and never waits for a picture that cannot come. Dropped, it waits for `run` to
/// end.
fn encoder_thread(hub: Arc<Hub>, run: impl FnOnce() + Send + 'static) -> Stopped {
    let woken = Woken(hub);
    Stopped::spawn("encode", move || {
        let _woken = woken;
        run()
    })
}

/// Wakes the hub when it is dropped.
struct Woken(Arc<Hub>);

impl Drop for Woken {
    fn drop(&mut self) {
        self.0.wake();
    }
}

fn encode_loop(
    stripes_in: Vec<Stripe>,
    rx: mpsc::Receiver<Job>,
    ret: mpsc::SyncSender<Returned>,
    hub: Arc<Hub>,
    fps: Fps,
    crfs: Crfs,
    debug: Debugging,
) {
    let debug_stripes = debug.stripes;
    // Geometry stays here; encoders move to their workers.
    let stripes: Vec<(usize, usize)> = stripes_in.iter().map(|s| (s.y, s.h)).collect();
    // Persistent per-stripe workers keep each encoder's state warm in one core's cache.
    let mut coders = stripes_in
        .into_iter()
        .map(|s| Coder::new(s, fps, crfs, debug_stripes));
    let mut own = coders.next().expect("a screen has a stripe");
    let mut crew = Crew::spawn(
        move |job| own.code(job),
        coders.map(|c| move |rx, done| stripe_worker(c, rx, done)),
    );
    if let Err(Gone(i)) = encode_frames(&mut crew, &stripes, fps, rx, ret, &hub) {
        log::error!("stripe {i}'s encoder is gone");
    }
}

/// Codes each frame the capture loop sends with the crew, and sends it to the viewers,
/// until the capture loop is gone or a stripe worker is.
fn encode_frames(
    crew: &mut Crew,
    stripes: &[(usize, usize)],
    fps: Fps,
    rx: mpsc::Receiver<Job>,
    ret: mpsc::SyncSender<Returned>,
    hub: &Hub,
) -> Result<(), Gone> {
    let epoch = Instant::now();
    let mut frame_id: u32 = 0;
    let n = stripes.len();
    let mut budget = FrameBudget::new(epoch);
    // Kept from frame to frame, so a frame allocates nothing for them.
    let mut selected: Vec<bool> = Vec::with_capacity(n);
    let mut msgs: Vec<Bytes> = Vec::with_capacity(n);
    let mut viewers = Vec::new();
    // A picture goes home only once its frame is in flight to its viewers: the capture
    // loop, woken by it, asks the flow gate next, and the gate must count that frame.
    let home = |r: Returned| {
        let _ = ret.send(r);
        hub.wake();
    };
    while let Ok(job) = rx.recv() {
        let t0 = Instant::now();
        // A sharpening frame's band covers the blocks it sharpens.
        selected.clear();
        selected.extend(stripes.iter().map(|&(y, h)| job.frame.touches(y, h)));
        if !selected.iter().any(|&x| x) {
            home(Returned {
                buf: job.buf,
                sharpened: None,
            });
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
        let mb_cols = job.buf.width.div_ceil(16);
        let dispatched = Instant::now();
        // Sharpening spends from the same budget as motion: what the budget doesn't
        // cover now is left pending for a later pass.
        let (out, buf) = crew.run(
            &selected,
            StripeJob {
                buf: job.buf,
                ts,
                coding,
                cap: share,
                mb_dirty,
                mb_cols,
            },
        )?;
        let collected = Instant::now();
        let sharp = out.sharp();
        let all_key = out.all_key();
        let sharpens = job.frame.sharpens();
        let (sharpened, starts) = job.frame.into_parts();
        let back = Returned {
            buf,
            sharpened: sharpened.filter(|_| sharp),
        };
        // The viewers this keyframe was promised to start on it, or wait for another.
        for c in &starts {
            c.keyframe_sent(all_key);
        }
        // A frame waits for the stripe coded last; one with nothing to send is not sent.
        let Some(coded) = out.coded() else {
            home(back);
            continue;
        };
        let nparts = out.parts() as u8;
        let enc_us = t0.elapsed().as_micros() as u64;
        frame_id = frame_id.wrapping_add(1);
        let pipe = Pipe::of(job.captured.damage.elapsed());
        msgs.extend(out.take_parts().map(|(i, key, mut out)| {
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
            out.freeze()
        }));
        let total: usize = msgs.iter().map(Bytes::len).sum();
        hub.stats.encode_us.fetch_add(enc_us, Ordering::Relaxed);
        hub.stats.encoded.fetch_add(1, Ordering::Relaxed);
        hub.stats.bytes.fetch_add(total as u64, Ordering::Relaxed);
        budget.spend(total);
        if all_key {
            hub.stats.keyframes.fetch_add(1, Ordering::Relaxed);
        }
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
        hub.viewers_into(&mut viewers);
        for (c, _) in &viewers {
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
        home(back);
        // What the viewers were sent is theirs now: dropped here, each stripe's arena is
        // taken back once they are done with it.
        msgs.clear();
        viewers.clear();
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
    Ok(())
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
                debug: Debugging::read(|_| None).0,
            },
            crf: 23.0,
            max_kbps: None,
        };
        let stripe = Stripe {
            y: 0,
            h,
            enc: Encoder::new(&ecfg).unwrap(),
            applied: None,
        };
        let (job_tx, job_rx) = mpsc::sync_channel::<Job>(1);
        // The way home holds one picture: the capture loop's part, taking each home, is
        // played by a thread of its own.
        let (ret_tx, ret_rx) = mpsc::sync_channel::<Returned>(1);
        let home = std::thread::spawn(move || ret_rx.iter().count());
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
                    Debugging::read(|_| None).0,
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
                    buf: Arc::new(I420::new(w, h)),
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
        assert_eq!(home.join().unwrap(), HOPELESSLY_BEHIND + 3);
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
                        debug: Debugging::read(|_| None).0,
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
                                buf: Arc::new(I420::new(w as usize, h as usize)),
                                frame,
                                target: Kbps::new(8000.0),
                                capacity: None,
                                captured: CaptureMarks::at(Instant::now()),
                                marker_ms: None,
                            })
                            .unwrap();
                        // Its picture comes home before the next is sent, as the capture
                        // loop's one picture does: the way home holds one.
                        s.ret_rx.recv().unwrap();
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
                        debug: Debugging::read(|_| None).0,
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
                in_flight: InFlight::Nothing,
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

    /// A tiny xorshift generator, for properties over generated cases.
    struct Gen(u64);
    impl Gen {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }
        fn bytes(&mut self, len: usize) -> Vec<u8> {
            (0..len).map(|_| self.below(256) as u8).collect()
        }
    }

    /// A fake stripe worker's answer for the rows `y..y + h` of `buf`: a digest of every
    /// byte of them, of the job, and of how many jobs it served before this one.
    #[allow(clippy::too_many_arguments)]
    fn fake_answer(
        (y, h): (usize, usize),
        buf: &I420,
        ts: i64,
        coding: Coding,
        cap: u32,
        mb_dirty: &Dirty,
        mb_cols: usize,
        served: u64,
    ) -> StripeOut {
        let mut d = 0xcbf29ce484222325u64;
        let mut eat = |bytes: &[u8]| {
            for &b in bytes {
                d = (d ^ b as u64).wrapping_mul(0x100000001b3);
            }
        };
        let suv = buf.stride_uv();
        eat(&buf.y[y * buf.width..(y + h) * buf.width]);
        eat(&buf.u[y / 2 * suv..(y + h).div_ceil(2) * suv]);
        eat(&buf.v[y / 2 * suv..(y + h).div_ceil(2) * suv]);
        eat(&ts.to_be_bytes());
        eat(&[match coding {
            Coding::Key { still: false } => 0,
            Coding::Key { still: true } => 1,
            Coding::Motion => 2,
            Coding::Sharpen => 3,
        }]);
        eat(&cap.to_be_bytes());
        match mb_dirty {
            Dirty::All => eat(&[2]),
            Dirty::Blocks(m) => {
                let start = (y / 16) * mb_cols;
                eat(&m[start..start + h.div_ceil(16) * mb_cols]);
            }
        }
        eat(&served.to_be_bytes());
        let at = Instant::now();
        StripeOut::Sent {
            key: d & 1 == 1,
            bytes: BytesMut::from(&d.to_be_bytes()[..]),
            quality: if d & 2 == 2 {
                Quality::AsAsked
            } else {
                Quality::Squeezed
            },
            coded: Coded {
                woke: at,
                x264: at,
                encoded: at,
            },
        }
    }

    fn shown(outs: &[StripeOut]) -> Vec<String> {
        outs.iter()
            .map(|o| match o {
                StripeOut::Skipped => "skipped".to_string(),
                StripeOut::Failed => "failed".to_string(),
                StripeOut::Empty => "empty".to_string(),
                StripeOut::Sent {
                    key,
                    bytes,
                    quality,
                    ..
                } => format!("{key} {bytes:?} {quality:?}"),
            })
            .collect()
    }

    /// The picture lent to the stripe workers comes back whole: over generated pictures,
    /// stripe layouts, selections and jobs, every selected stripe's worker reads its rows
    /// of the picture and answers for them, and the very picture lent comes back, each
    /// plane where it was and as it was, and no longer shared, so the capture loop can
    /// write it again.
    #[test]
    fn a_lent_picture_comes_back_as_it_went() {
        let mut g = Gen(0x5851F42D4C957F2D);
        for case in 0..300 {
            let width = 16 + g.below(80) as usize;
            let height = 16 + g.below(200) as usize;
            let layout: Vec<(usize, usize)> = stripe_layout(height as u32, 1 + g.below(6) as u32)
                .into_iter()
                .map(|(y, h)| (y as usize, h as usize))
                .collect();
            let n = layout.len();
            // A fake coder for the stripe at `rows`, counting the jobs it served.
            let coder = |rows: (usize, usize)| {
                let mut served = 0;
                move |job: StripeJob| {
                    let out = fake_answer(
                        rows,
                        &job.buf,
                        job.ts,
                        job.coding,
                        job.cap,
                        &job.mb_dirty,
                        job.mb_cols,
                        served,
                    );
                    served += 1;
                    StripeResult { out, buf: job.buf }
                }
            };
            let mut crew = Crew::spawn(
                coder(layout[0]),
                layout[1..].iter().map(|&rows| {
                    move |rx: mpsc::Receiver<StripeJob>, done: mpsc::SyncSender<StripeResult>| {
                        let mut code = coder(rows);
                        while let Ok(job) = rx.recv() {
                            if done.send(code(job)).is_err() {
                                return;
                            }
                        }
                    }
                }),
            );
            let mut served = vec![0; n];

            // Two buffers, as the capture loop has, written afresh before each frame.
            let mut pool = [
                Some(Arc::new(I420::new(width, height))),
                Some(Arc::new(I420::new(width, height))),
            ];
            for frame in 0..12 {
                let idx = frame % 2;
                let mut buf = pool[idx].take().unwrap();
                let pic = Arc::get_mut(&mut buf).expect("the picture came back alone");
                let (y, u, v) = (
                    g.bytes(pic.y.len()),
                    g.bytes(pic.u.len()),
                    g.bytes(pic.v.len()),
                );
                pic.y.copy_from_slice(&y);
                pic.u.copy_from_slice(&u);
                pic.v.copy_from_slice(&v);
                let at = (pic.y.as_ptr(), pic.u.as_ptr(), pic.v.as_ptr());
                let selected: Vec<bool> = (0..n).map(|_| g.below(3) != 0).collect();
                let ts = g.below(1 << 40) as i64;
                let coding = match g.below(4) {
                    0 => Coding::Key { still: false },
                    1 => Coding::Key { still: true },
                    2 => Coding::Motion,
                    _ => Coding::Sharpen,
                };
                let share = 1 + g.below(50_000) as u32;
                let mb_cols = width.div_ceil(16);
                let mb_dirty = match g.below(2) {
                    0 => Dirty::All,
                    _ => Dirty::Blocks(Arc::new(g.bytes(mb_cols * height.div_ceil(16)))),
                };
                let want: Vec<StripeOut> = (0..n)
                    .map(|i| {
                        if !selected[i] {
                            return StripeOut::Skipped;
                        }
                        served[i] += 1;
                        fake_answer(
                            layout[i],
                            &buf,
                            ts,
                            coding,
                            share,
                            &mb_dirty,
                            mb_cols,
                            served[i] - 1,
                        )
                    })
                    .collect();

                let (outs, mut back) = crew
                    .run(
                        &selected,
                        StripeJob {
                            buf,
                            ts,
                            coding,
                            cap: share,
                            mb_dirty,
                            mb_cols,
                        },
                    )
                    .expect("no worker dies");
                assert_eq!(shown(&outs.0), shown(&want), "case {case} frame {frame}");
                assert_eq!(
                    (back.y.as_ptr(), back.u.as_ptr(), back.v.as_ptr()),
                    at,
                    "case {case} frame {frame}: the planes came back moved"
                );
                assert!(
                    back.y == y && back.u == u && back.v == v && back.width == width,
                    "case {case} frame {frame}: the picture came back changed"
                );
                assert!(
                    Arc::get_mut(&mut back).is_some(),
                    "case {case} frame {frame}: the picture came back still shared"
                );
                pool[idx] = Some(back);
            }
        }
    }

    /// What came of a frame, from what came of it in each stripe: over generated frames
    /// of 1-8 stripes, each untouched, failed, or coded by x264 (a keyframe or not, at a
    /// CRF average either side of as asked, with bytes or none, a picture x264 put
    /// nothing out for reading as one it didn't fill in), the frame is sharp when every
    /// stripe it touched was coded as asked or put nothing out, a whole keyframe when
    /// every stripe put out a keyframe, and its parts are the stripes that put out bytes,
    /// in order.
    #[test]
    fn a_frame_comes_out_as_its_stripes_did() {
        let mut g = Gen(0xD1B54A32D192ED03);
        // Frames that came out sharp, not sharp, a whole keyframe, and with an empty stripe.
        let mut reached = [0; 4];
        for case in 0..20_000 {
            let n = 1 + g.below(8) as usize;
            let mut outs = vec![];
            let (mut sharp, mut all_key, mut parts, mut empty) = (true, true, vec![], false);
            for i in 0..n {
                let crf = g.below(52) as f32;
                match g.below(5) {
                    0 => {
                        outs.push(StripeOut::Skipped);
                        all_key = false;
                    }
                    1 => {
                        outs.push(StripeOut::Failed);
                        (sharp, all_key) = (false, false);
                    }
                    _ => {
                        let len = match g.below(4) {
                            0 => 0,
                            _ => 1 + g.below(64) as usize,
                        };
                        let e = match len {
                            // What x264 leaves of a picture it put nothing out for.
                            0 => Encoded {
                                keyframe: false,
                                qp: None,
                                crf_avg: 0.0,
                            },
                            _ => Encoded {
                                keyframe: g.below(3) != 0,
                                qp: Some(crf),
                                crf_avg: crf + (g.below(40) as f32 - 10.0) / 10.0,
                            },
                        };
                        let mut bytes = BytesMut::zeroed(Header::LEN);
                        bytes.extend(g.bytes(len));
                        sharp &= len == 0 || e.crf_avg < crf + 0.5;
                        all_key &= len > 0 && e.keyframe;
                        empty |= len == 0;
                        if len > 0 {
                            parts.push((i, e.keyframe, bytes.clone()));
                        }
                        let at = Instant::now();
                        let coded = Coded {
                            woke: at,
                            x264: at,
                            encoded: at,
                        };
                        outs.push(StripeOut::coded(&e, crf, bytes, coded));
                    }
                }
            }
            let mut out = FrameOut(outs);
            let (sharp_got, all_key_got, count) = (out.sharp(), out.all_key(), out.parts());
            let got = (sharp_got, all_key_got, out.take_parts().collect::<Vec<_>>());
            assert_eq!(count, got.2.len(), "case {case}");
            reached[usize::from(!sharp)] += 1;
            reached[2] += usize::from(all_key);
            reached[3] += usize::from(empty && sharp);
            assert!(got == (sharp, all_key, parts), "case {case}");
        }
        assert!(
            reached.iter().all(|&r| r >= 200),
            "too few cases reached: {reached:?}"
        );
    }

    /// A stripe sets x264's rate when it moves: over generated runs of keyframes, frames
    /// at a CRF (asked, or lowered for motion) under a cap of at least 1 kbit/s (the
    /// share is never less), and reopened encoders, a frame sets its CRF and cap when
    /// nothing set holds (at the start, after a keyframe or a reopen), when its CRF is
    /// not the one set, or when its cap is more than CAP_SLACK of the one set away, and
    /// otherwise sets nothing.
    #[test]
    fn a_stripe_sets_its_rate_when_it_moves() {
        let mut g = Gen(0x94D049BB133111EB);
        // Frames that kept a rate, that set one, and that moved the cap onto the slack's edge.
        let mut reached = [0; 3];
        for case in 0..2_000 {
            // The rate set, if any holds.
            let mut set: Option<(f32, u32)> = None;
            let mut applied: Option<Applied> = None;
            let mut cap = 1 + g.below(40_000) as u32;
            let mut crf = 24.0;
            for frame in 0..200 {
                match g.below(20) {
                    // A keyframe's boost, or a reopened encoder: nothing set holds.
                    0 | 1 => {
                        set = None;
                        applied = None;
                    }
                    _ => {
                        // Caps wobble with the budget, and sometimes jump; CRFs step.
                        cap = match g.below(6) {
                            0 => 1 + g.below(40_000) as u32,
                            // Onto the edge of the slack, or one past it.
                            1 if set.is_some_and(|(_, kbps)| kbps % 25 == 0) => {
                                let kbps = set.map_or(cap, |(_, kbps)| kbps);
                                let edge = kbps / 25 + g.below(2) as u32;
                                match g.below(2) {
                                    0 => kbps + edge,
                                    _ => kbps - edge.min(kbps - 1),
                                }
                            }
                            _ => {
                                let step = g.below(cap as u64 / 10 + 2) as u32;
                                match g.below(2) {
                                    0 => cap + step,
                                    _ => cap - step.min(cap - 1),
                                }
                            }
                        };
                        if g.below(8) == 0 {
                            crf = 12.0 + g.below(20) as f32;
                        }
                        let holds = set.is_some_and(|(c, kbps)| {
                            c == crf && cap.abs_diff(kbps) as f64 <= kbps as f64 * CAP_SLACK
                        });
                        let on_edge = set.is_some_and(|(_, kbps)| {
                            cap.abs_diff(kbps) as f64 == kbps as f64 * CAP_SLACK
                        });
                        let want = (!holds).then_some((crf, cap));
                        if let Some(rate) = want {
                            set = Some(rate);
                        }
                        let new = Applied::next(applied, crf, cap).map(|rate| {
                            applied = Some(rate);
                            (rate.crf, rate.kbps)
                        });
                        assert_eq!(new, want, "case {case} frame {frame}");
                        reached[usize::from(new.is_some())] += 1;
                        reached[2] += usize::from(on_edge && holds);
                    }
                }
            }
        }
        assert!(
            reached.iter().all(|&r| r >= 100),
            "too few cases reached: {reached:?}"
        );
    }

    /// The crew codes each stripe as the stripe's coder alone would, wherever it runs it:
    /// stripe 0 on the encode loop's own thread, the others on their workers. Over
    /// generated screens of 1-4 stripes and runs of frames (each picture new, any
    /// selection, every coding, caps that move, block maps or none), real x264 encoders
    /// on one thread each put out, stripe by stripe, the bytes, keyframes and quality
    /// their coders put out when handed the same jobs one by one on this thread, and a
    /// stripe not selected is skipped. And once the crew is done with a frame nothing it
    /// ran holds the frame's block map, so the capture loop may write it again.
    #[test]
    fn a_stripe_coded_where_its_frame_is_comes_out_as_on_a_worker() {
        let crfs = Crfs {
            configured: 24.0,
            paintover: 16.0,
        };
        let coders = |width: usize, layout: &[(usize, usize)], cap: Option<Kbps>| {
            layout
                .iter()
                .map(|&(y, h)| {
                    let cfg = EncoderConfig {
                        pictures: Pictures {
                            width: width as u32,
                            height: h as u32,
                            fps: Fps::new(60).unwrap(),
                            preset: Preset::parse("ultrafast").unwrap(),
                            threads: 1,
                            debug: Debugging::read(|_| None).0,
                        },
                        crf: 24.0,
                        max_kbps: cap,
                    };
                    let s = Stripe {
                        y,
                        h,
                        enc: Encoder::new(&cfg).unwrap(),
                        applied: None,
                    };
                    Coder::new(s, Fps::new(60).unwrap(), crfs, false)
                })
                .collect::<Vec<Coder>>()
        };
        let mut g = Gen(0xBF58476D1CE4E5B9);
        let mut sent = 0;
        for case in 0..40 {
            let width = 16 * (2 + g.below(10) as usize);
            let height = 16 * (2 + g.below(10) as usize);
            let layout: Vec<(usize, usize)> = stripe_layout(height as u32, 1 + g.below(4) as u32)
                .into_iter()
                .map(|(y, h)| (y as usize, h as usize))
                .collect();
            let n = layout.len();
            let cap = (g.below(2) == 0).then(|| Kbps::new(2000.0 + g.below(20_000) as f64));
            let mut alone = coders(width, &layout, cap);
            let mut ours = coders(width, &layout, cap).into_iter();
            let mut own = ours.next().unwrap();
            let mut new = Crew::spawn(
                move |job| own.code(job),
                ours.map(|c| move |rx, done| stripe_worker(c, rx, done)),
            );
            let mut buf = Arc::new(I420::new(width, height));
            for frame in 0..10 {
                let pic = Arc::get_mut(&mut buf).unwrap();
                // Text-like rows over a gradient, moved a little each frame, or noise.
                let noise = g.below(4) == 0;
                let shift = g.below(8) as usize;
                for (k, p) in pic.y.iter_mut().enumerate() {
                    let (x, y) = (k % width, k / width);
                    *p = match noise {
                        true => g.below(256) as u8,
                        false => match ((x + shift) / 3 + y / 7) % 5 {
                            0 => 30,
                            _ => (40 + y * 2) as u8,
                        },
                    };
                }
                let selected: Vec<bool> = match frame {
                    0 => vec![true; n],
                    _ => (0..n).map(|_| g.below(3) != 0).collect(),
                };
                let coding = match (frame, g.below(5)) {
                    (0, _) | (_, 0) => Coding::Key { still: false },
                    (_, 1) => Coding::Key { still: true },
                    (_, 2) => Coding::Sharpen,
                    _ => Coding::Motion,
                };
                let mb_cols = width.div_ceil(16);
                let job = StripeJob {
                    buf,
                    ts: frame as i64 * 16,
                    coding,
                    cap: 200 + g.below(30_000) as u32,
                    mb_dirty: match g.below(2) {
                        0 => Dirty::All,
                        _ => Dirty::Blocks(Arc::new(
                            (0..mb_cols * height.div_ceil(16))
                                .map(|_| (g.below(3) != 0) as u8)
                                .collect(),
                        )),
                    },
                    mb_cols,
                };
                let want: Vec<StripeOut> = alone
                    .iter_mut()
                    .zip(&selected)
                    .map(|(coder, &on)| match on {
                        true => coder.code(job.clone()).out,
                        false => StripeOut::Skipped,
                    })
                    .collect();
                let map = match &job.mb_dirty {
                    Dirty::Blocks(map) => Some(Arc::downgrade(map)),
                    Dirty::All => None,
                };
                let (got, back) = new.run(&selected, job).unwrap();
                let want = shown(&want);
                assert_eq!(shown(&got.0), want, "case {case} frame {frame}");
                assert!(
                    map.is_none_or(|map| map.upgrade().is_none()),
                    "case {case} frame {frame}: the block map is still held"
                );
                sent += want.iter().filter(|w| w.len() > 20).count();
                buf = back;
            }
        }
        assert!(sent >= 300, "only {sent} stripes coded");
    }

    /// A frame in steady state allocates nothing on the encode loop's thread or a stripe
    /// worker's, whatever it is. The oracle is the allocator itself: it counts every
    /// allocation the process makes while frames go through.
    ///
    /// The encode loop runs on a thread of its own, as the stream runs it, with real
    /// encoders for 1 to 3 stripes of a 1024x768 screen and one viewer. This test's
    /// thread plays the capture loop (it draws into the picture that came back and hands
    /// it over) and the viewer's socket task (acks each frame as it arrives, and holds
    /// what was sent until the next frame, as a socket that takes its time); what it
    /// allocates is theirs and not counted. First every kind of frame goes through over
    /// every band, of noise and then of text, which brings every buffer the encode loop
    /// and its workers keep to its size (x264's output vector to the largest part it put
    /// out); then a generated run of frames of text is counted. What x264 allocates
    /// goes through C's malloc, which this allocator does not see.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_encode_loop_and_its_workers_allocate_nothing_a_frame() {
        if !in_a_process_of_its_own(
            "video::tests::the_encode_loop_and_its_workers_allocate_nothing_a_frame",
        ) {
            return;
        }
        let (w, h) = (1024usize, 768usize);
        let (cols, rows) = (w.div_ceil(16), h.div_ceil(16));
        let crfs = Crfs {
            configured: 24.0,
            paintover: 16.0,
        };
        let mut rng = 0x2545F4914F6CDD1Du64;
        let mut next = |n: usize| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng as usize % n
        };
        for stripes in 1..=3 {
            let layout = stripe_layout(h as u32, stripes);
            let mut coders = layout.iter().map(|&(y, sh)| {
                let cfg = EncoderConfig {
                    pictures: Pictures {
                        width: w as u32,
                        height: sh,
                        fps: Fps::new(60).unwrap(),
                        preset: Preset::parse("ultrafast").unwrap(),
                        threads: (4 / layout.len() as u32).max(1),
                        debug: Debugging::read(|_| None).0,
                    },
                    crf: 24.0,
                    max_kbps: Some(Kbps::new(30_000.0)),
                };
                let s = Stripe {
                    y: y as usize,
                    h: sh as usize,
                    enc: Encoder::new(&cfg).unwrap(),
                    applied: None,
                };
                Coder::new(s, Fps::new(60).unwrap(), crfs, false)
            });
            let mut own = coders.next().unwrap();
            let crew = Crew::spawn(
                move |job| own.code(job),
                coders.map(|c| move |rx, done| stripe_worker(c, rx, done)),
            );
            let geometry: Vec<(usize, usize)> = layout
                .iter()
                .map(|&(y, sh)| (y as usize, sh as usize))
                .collect();
            let hub = Hub::new();
            let (tx, mut socket) = tokio::sync::mpsc::unbounded_channel();
            let viewer = hub.add_client(tx);
            viewer.watch();
            // The capture loop planned its keyframe: the first frame played is one.
            viewer.keyframe_planned();
            let (job_tx, job_rx) = mpsc::sync_channel::<Job>(1);
            // One picture goes round (capture-when-idle).
            let (ret_tx, ret_rx) = mpsc::sync_channel::<Returned>(1);
            let encoding = hub.clone();
            let encode = std::thread::Builder::new()
                .name("encode".into())
                .spawn(move || {
                    let mut crew = crew;
                    let fps = Fps::new(60).unwrap();
                    encode_frames(&mut crew, &geometry, fps, job_rx, ret_tx, &encoding).unwrap()
                })
                .unwrap();
            // Bands of block rows, each with a map of the blocks in it that changed,
            // made before anything is counted.
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
            let mut home = Some(Arc::new(I420::new(w, h)));
            // The viewer the first keyframe was planned for; it starts on it.
            let mut starts = vec![viewer.clone()];
            let mut writing: Vec<Bytes> = vec![];
            let mut frame = |kind: usize, which: usize, n: usize, noise: bool| {
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
                // The picture comes home before the next frame is captured into it.
                while home.is_none() {
                    home = Some(ret_rx.recv().unwrap().buf);
                }
                let mut buf = home.take().unwrap();
                // What moved: lines of 9 px glyphs scrolled 3 px a frame through the band;
                // or noise over the whole picture, which wants more bytes than any text.
                let pic = Arc::get_mut(&mut buf).unwrap();
                if noise {
                    let mut state = 0x9E37_79B9_7F4A_7C15u64 ^ n as u64;
                    for p in pic.y.iter_mut() {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        *p = state as u8;
                    }
                }
                let (y0, y1) = match noise {
                    true => (0, 0),
                    false => (band.y0 as usize, band.y1 as usize),
                };
                for (y, row) in pic.y.chunks_exact_mut(w).enumerate().take(y1).skip(y0) {
                    let (line, within) = ((y + 3 * n) / 18, (y + 3 * n) % 18);
                    for (x, glyph) in row.chunks_mut(9).enumerate() {
                        let ink = within < 12 && (x * 2654435761 + line * 40503) >> 7 & 1 == 1;
                        glyph.fill(if ink { 30 } else { 235 });
                    }
                }
                job_tx
                    .send(Job {
                        buf,
                        frame,
                        target: Kbps::new(6000.0),
                        capacity: None,
                        captured: CaptureMarks::at(Instant::now()),
                        marker_ms: None,
                    })
                    .unwrap();
                // The viewer: every frame is acknowledged as it arrives, and what was
                // sent to it is written out a frame later, as a socket that takes its time.
                let mut arrived = vec![];
                while let Ok(m) = socket.try_recv() {
                    if let Out::Bin(m) = m {
                        let id = u32::from_be_bytes([m[2], m[3], m[4], m[5]]);
                        viewer.on_ack(Instant::now(), id, crate::hub::Hold::Unsaid);
                        arrived.push(m);
                    }
                }
                writing = arrived;
            };
            let mut n = 0;
            for noise in [true, false] {
                for kind in 0..6 {
                    for which in 0..maps.len() {
                        frame(kind, which, n, noise);
                        n += 1;
                    }
                }
            }
            const COUNTED: usize = 120;
            counting::arm();
            for _ in 0..COUNTED {
                // A keyframe now and then, as a viewer joining asks for one.
                let kind = match next(30) {
                    0 => next(2),
                    _ => 2 + next(4),
                };
                frame(kind, next(maps.len()), n, false);
                n += 1;
            }
            let (threads, others) = counting::disarm();
            drop(job_tx);
            encode.join().unwrap();
            let ours: Vec<String> = threads
                .iter()
                .map(|t| {
                    (
                        String::from_utf8_lossy(&t.name)
                            .trim_end_matches('\0')
                            .to_owned(),
                        t,
                    )
                })
                .filter(|(name, _)| name == "encode" || name.starts_with("enc-"))
                .map(|(name, t)| {
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
                ours.is_empty() && others == 0,
                "{stripes} stripes: {COUNTED} frames allocated, by thread:\n{}\nand {others} \
                 times on threads past the table",
                ours.join("\n")
            );
        }
    }

    /// How a fake stripe worker ends.
    #[derive(Clone, Copy, Debug)]
    enum End {
        /// It answers every job it is given.
        Never,
        /// It answers `n` jobs, then returns before taking another.
        Returns(u32),
        /// It answers `n` jobs, takes the next one and returns without answering it.
        Drops(u32),
        /// It answers `n` jobs, takes the next one and panics.
        Panics(u32),
    }

    impl End {
        /// How many jobs it answers, if not all.
        fn answers(self) -> Option<u32> {
            match self {
                End::Never => None,
                End::Returns(n) | End::Drops(n) | End::Panics(n) => Some(n),
            }
        }
    }

    /// A fake stripe worker for stripe `i`: it answers each job with a header's room, its
    /// stripe and how many jobs it answered before, until its `end`. Once it has dropped
    /// both of its channels' ends, by returning or by unwinding, it says `i` on `ended`.
    fn mortal(
        i: usize,
        end: End,
        ended: mpsc::Sender<usize>,
    ) -> impl FnOnce(mpsc::Receiver<StripeJob>, mpsc::SyncSender<StripeResult>) + Send + 'static
    {
        struct Ended(usize, mpsc::Sender<usize>);
        impl Drop for Ended {
            fn drop(&mut self) {
                let _ = self.1.send(self.0);
            }
        }
        move |rx, done| {
            // Declared first, so dropped after both ends.
            let _ended = Ended(i, ended);
            let (rx, done) = (rx, done);
            for served in 0u32.. {
                if let End::Returns(n) = end {
                    if n == served {
                        return;
                    }
                }
                let Ok(job) = rx.recv() else { return };
                match end {
                    End::Drops(n) if n == served => return,
                    End::Panics(n) if n == served => panic!("stripe {i} dies on its job {n}"),
                    End::Never | End::Returns(_) | End::Drops(_) | End::Panics(_) => {}
                }
                let mut bytes = BytesMut::zeroed(Header::LEN);
                bytes.extend([i as u8, served as u8]);
                let at = Instant::now();
                let out = StripeOut::Sent {
                    key: true,
                    bytes,
                    quality: Quality::AsAsked,
                    coded: Coded {
                        woke: at,
                        x264: at,
                        encoded: at,
                    },
                };
                if done.send(StripeResult { out, buf: job.buf }).is_err() {
                    return;
                }
            }
        }
    }

    fn a_job(n: usize) -> StripeJob {
        StripeJob {
            buf: Arc::new(I420::new(16, 16 * n)),
            ts: 0,
            coding: Coding::Motion,
            cap: 1,
            mb_dirty: Dirty::All,
            mb_cols: 1,
        }
    }

    /// A stripe worker that is gone leaves the encode loop nothing to wait on: once it
    /// has dropped its ends, by returning before a job, after taking one, or by
    /// panicking, its job channel refuses and its result channel reads disconnected,
    /// for any crew and any of its workers gone, while the others' stay open.
    #[test]
    fn a_gone_worker_closes_both_its_channels() {
        let mut g = Gen(0x2545F4914F6CDD1D);
        for case in 0..300 {
            let n = 1 + g.below(8) as usize;
            let ends: Vec<End> = (0..n)
                .map(|_| match g.below(4) {
                    0 => End::Returns(0),
                    1 => End::Drops(0),
                    2 => End::Panics(0),
                    _ => End::Never,
                })
                .collect();
            let (ended_tx, ended) = mpsc::channel();
            let crew = Crew::spawn(
                |job: StripeJob| StripeResult {
                    out: StripeOut::Failed,
                    buf: job.buf,
                },
                (0..n).map(|i| mortal(i, ends[i], ended_tx.clone())),
            );
            for hand in &crew.hands {
                let _ = hand.jobs.send(a_job(n));
            }
            let mut gone: Vec<usize> = (0..n).filter(|&i| ends[i].answers().is_some()).collect();
            let mut seen: Vec<usize> = (0..gone.len()).map(|_| ended.recv().unwrap()).collect();
            seen.sort();
            gone.sort();
            assert_eq!(seen, gone, "case {case}: {ends:?}");
            for (i, hand) in crew.hands.iter().enumerate() {
                let what = format!("case {case}: stripe {i}, {:?}", ends[i]);
                match ends[i] {
                    End::Never => {
                        assert!(hand.done.recv().is_ok(), "{what}");
                        assert!(
                            matches!(hand.done.try_recv(), Err(mpsc::TryRecvError::Empty)),
                            "{what}: there, and its results are closed"
                        );
                        assert!(hand.jobs.send(a_job(n)).is_ok(), "{what}");
                    }
                    End::Returns(_) | End::Drops(_) | End::Panics(_) => {
                        assert!(
                            matches!(hand.done.try_recv(), Err(mpsc::TryRecvError::Disconnected)),
                            "{what}: gone, and its results are open"
                        );
                        assert!(
                            hand.jobs.send(a_job(n)).is_err(),
                            "{what}: gone, and it takes jobs"
                        );
                    }
                }
            }
        }
    }

    /// Any stripe worker may die at any point, and the pipeline then ends with it gone
    /// and never waits for it. Over generated stripe layouts, deaths (returning before
    /// a job, taking one and returning, taking one and panicking, after any number of
    /// jobs) and runs of frames over any band of rows, fed as the capture loop feeds
    /// them: every frame before the first one that needs a dead worker comes back with
    /// the picture it was given, and no other does; the encode loop ends with a worker
    /// that frame needed gone; the capture loop, waiting for its pictures as it does (on
    /// the hub's waker, with no limit), is woken to find the way home closed, which it
    /// ends on ("encoder gone"); and its frame after the next is refused, as a closed
    /// encoder refuses it. Stripe 0, coded on the encode loop's own thread, dies by
    /// panicking there, which ends the same way. Every wait here is on a channel or the
    /// waker, none on a clock.
    #[test]
    fn a_dead_worker_ends_the_pipeline() {
        let mut g = Gen(0x9E3779B97F4A7C15);
        // Cases that ended with each way of dying, that ended well, that refused, and
        // that ended with stripe 0 panicking on the encode loop's thread.
        let mut reached = [0; 6];
        for case in 0..400 {
            let width = 16 + g.below(40) as usize;
            let height = 16 + g.below(160) as usize;
            let stripes: Vec<(usize, usize)> = stripe_layout(height as u32, 1 + g.below(6) as u32)
                .into_iter()
                .map(|(y, h)| (y as usize, h as usize))
                .collect();
            let n = stripes.len();
            // Stripe 0 is coded on the encode loop's own thread: it has no worker to
            // lose, and dies only by panicking there.
            let ends: Vec<End> = (0..n)
                .map(|i| {
                    let after = g.below(5) as u32;
                    match (i, g.below(6)) {
                        (0, 2) => End::Panics(after),
                        (0, _) => End::Never,
                        (_, 0) => End::Returns(after),
                        (_, 1) => End::Drops(after),
                        (_, 2) => End::Panics(after),
                        (_, _) => End::Never,
                    }
                })
                .collect();
            let (ended_tx, _ended) = mpsc::channel();
            let (mut served0, end0) = (0u32, ends[0]);
            let crew = Crew::spawn(
                move |job: StripeJob| {
                    if end0.answers() == Some(served0) {
                        panic!("stripe 0 dies on its job {served0}");
                    }
                    let mut bytes = BytesMut::zeroed(Header::LEN);
                    bytes.extend([0, served0 as u8]);
                    served0 += 1;
                    let at = Instant::now();
                    let out = StripeOut::Sent {
                        key: true,
                        bytes,
                        quality: Quality::AsAsked,
                        coded: Coded {
                            woke: at,
                            x264: at,
                            encoded: at,
                        },
                    };
                    StripeResult { out, buf: job.buf }
                },
                (1..n).map(|i| mortal(i, ends[i], ended_tx.clone())),
            );
            // Each frame is a band of rows; one past the picture needs no stripe.
            let frames: Vec<Band> = (0..1 + g.below(16))
                .map(|_| {
                    let y0 = g.below(height as u64 + 8) as u32;
                    let y1 = y0 + 1 + g.below(height as u64) as u32;
                    Band { y0, y1 }
                })
                .collect();
            // The model: how many frames come back, and the stripes dead at the first
            // one that doesn't.
            let mut served = vec![0u32; n];
            let mut back = 0;
            let mut dying = vec![];
            for band in &frames {
                let needs: Vec<usize> = (0..n)
                    .filter(|&i| {
                        let (y, h) = stripes[i];
                        (band.y0 as usize) < y + h && (band.y1 as usize) > y
                    })
                    .collect();
                dying = needs
                    .iter()
                    .copied()
                    .filter(|&i| ends[i].answers() == Some(served[i]))
                    .collect();
                if !dying.is_empty() {
                    break;
                }
                for &i in &needs {
                    served[i] += 1;
                }
                back += 1;
            }
            // The capture loop can hand over one frame past the one the encode loop
            // ends on before it is refused, since the job channel holds one.
            let refused = !dying.is_empty() && frames.len() > back + 2;

            let (job_tx, rx) = mpsc::sync_channel::<Job>(1);
            let (ret, ret_rx) = mpsc::sync_channel(2);
            let hub = Hub::new();
            let (ended_tx, ended) = mpsc::channel();
            let encode = encoder_thread(hub.clone(), {
                let hub = hub.clone();
                let fps = Fps::new(60).unwrap();
                move || {
                    let mut crew = crew;
                    let r = encode_frames(&mut crew, &stripes, fps, rx, ret, &hub);
                    drop(crew);
                    let _ = ended_tx.send(r);
                }
            });
            let frames_n = frames.len();
            let pictures: Vec<Arc<I420>> = (0..frames.len())
                .map(|_| Arc::new(I420::new(width, height)))
                .collect();
            let at: Vec<*const I420> = pictures.iter().map(Arc::as_ptr).collect();
            // The capture loop: it hands over each frame, and stops at one refused.
            let capture = std::thread::spawn(move || {
                for (band, buf) in frames.into_iter().zip(pictures) {
                    let job = Job {
                        buf,
                        frame: super::Frame::Motion {
                            band,
                            dirty: Arc::new(vec![]),
                        },
                        target: Kbps::new(1000.0),
                        capacity: None,
                        captured: CaptureMarks::at(Instant::now()),
                        marker_ms: None,
                    };
                    if job_tx.send(job).is_err() {
                        return true;
                    }
                }
                false
            });
            let what = format!("case {case}: {n} stripes, {ends:?}, {dying:?} dying");
            // The capture loop's wait for its pictures. Once every frame is in, a capture
            // loop that ended well has closed the stream and waits for nothing.
            let mut home = 0;
            while !(dying.is_empty() && home == frames_n) {
                hub.drain_wake();
                match ret_rx.try_recv() {
                    Ok(r) => {
                        assert!(home < back, "{what}: frame {home} came back");
                        assert_eq!(Arc::as_ptr(&r.buf), at[home], "{what}: frame {home}");
                        home += 1;
                    }
                    Err(mpsc::TryRecvError::Empty) => wait_any([hub.waker.waitable()], None),
                    Err(mpsc::TryRecvError::Disconnected) => break,
                }
            }
            assert_eq!(home, back, "{what}");
            drop(encode);
            match ended.recv().unwrap_or(Err(Gone(0))) {
                Ok(()) => {
                    assert!(dying.is_empty(), "{what}: ended well");
                    reached[3] += 1;
                }
                Err(Gone(0)) => {
                    assert!(dying.contains(&0), "{what}: the encode loop panicked");
                    reached[5] += 1;
                }
                Err(Gone(i)) => {
                    assert!(dying.contains(&i), "{what}: ended with {i} gone");
                    reached[match ends[i] {
                        End::Returns(_) => 0,
                        End::Drops(_) => 1,
                        End::Panics(_) => 2,
                        End::Never => unreachable!("{what}: {i} never dies"),
                    }] += 1;
                }
            }
            if refused {
                assert!(capture.join().unwrap(), "{what}: no frame was refused");
                reached[4] += 1;
            } else {
                capture.join().unwrap();
            }
        }
        assert!(
            reached.iter().all(|&r| r >= 20),
            "too few cases reached: {reached:?}"
        );
    }

    /// A frame is in flight to its viewers before its picture goes home. The capture loop,
    /// woken by the picture, asks the flow gate next, and the gate counts what is in
    /// flight: a frame not yet counted would let it capture one frame past the gate's room.
    /// Over generated screens of 1-4 stripes and runs of frames over any band of rows
    /// (some touching no stripe), each stripe's part put out or failed, and a viewer that
    /// has started and acks nothing: the way home holds no picture (a rendezvous), so the
    /// test takes each one the moment the encode loop hands it over, and then the viewer
    /// has in flight every frame so far that put out a part, and no other.
    #[test]
    fn a_frame_is_in_flight_before_its_picture_goes_home() {
        let mut g = Gen(0x2545F4914F6CDD1D);
        // Frames counted in flight, frames every stripe of which failed, frames that
        // touched no stripe.
        let mut reached = [0; 3];
        for case in 0..300 {
            let height = 16 * (1 + g.below(12) as usize);
            let stripes: Vec<(usize, usize)> = stripe_layout(height as u32, 1 + g.below(4) as u32)
                .into_iter()
                .map(|(y, h)| (y as usize, h as usize))
                .collect();
            let n = stripes.len();
            let frames: Vec<Band> = (0..1 + g.below(20))
                .map(|_| {
                    let y0 = g.below(height as u64 + 8) as u32;
                    Band {
                        y0,
                        y1: y0 + 1 + g.below(height as u64) as u32,
                    }
                })
                .collect();
            // Whether each stripe's k-th part fails.
            let fails: Vec<Vec<bool>> = (0..n)
                .map(|_| (0..frames.len()).map(|_| g.below(3) == 0).collect())
                .collect();
            let coder = |i: usize| {
                let (fails, mut k) = (fails[i].clone(), 0);
                move |job: StripeJob| {
                    let out = match fails[k] {
                        true => StripeOut::Failed,
                        false => {
                            let mut bytes = BytesMut::zeroed(Header::LEN);
                            bytes.extend([i as u8]);
                            let at = Instant::now();
                            StripeOut::Sent {
                                key: false,
                                bytes,
                                quality: Quality::AsAsked,
                                coded: Coded {
                                    woke: at,
                                    x264: at,
                                    encoded: at,
                                },
                            }
                        }
                    };
                    k += 1;
                    StripeResult { out, buf: job.buf }
                }
            };
            let crew = Crew::spawn(
                coder(0),
                (1..n).map(|i| {
                    let mut code = coder(i);
                    move |rx: mpsc::Receiver<StripeJob>, done: mpsc::SyncSender<StripeResult>| {
                        while let Ok(job) = rx.recv() {
                            if done.send(code(job)).is_err() {
                                return;
                            }
                        }
                    }
                }),
            );
            let hub = Hub::new();
            let (tx, _socket) = tokio::sync::mpsc::unbounded_channel();
            let viewer = hub.add_client(tx);
            // A viewer that has started: watching, a keyframe planned and sent it whole.
            viewer.watch();
            viewer.keyframe_planned();
            viewer.keyframe_sent(true);
            let (job_tx, rx) = mpsc::sync_channel::<Job>(1);
            let (ret, ret_rx) = mpsc::sync_channel(0);
            let (ended_tx, ended) = mpsc::channel();
            let encode = encoder_thread(hub.clone(), {
                let (hub, stripes) = (hub.clone(), stripes.clone());
                let fps = Fps::new(60).unwrap();
                move || {
                    let mut crew = crew;
                    let _ = ended_tx.send(encode_frames(&mut crew, &stripes, fps, rx, ret, &hub));
                }
            });
            // The model: each stripe's parts so far, and the frames that put one out.
            let mut served = vec![0; n];
            let mut in_flight = 0;
            for (idx, band) in frames.into_iter().enumerate() {
                let touched: Vec<usize> = (0..n)
                    .filter(|&i| {
                        let (y, h) = stripes[i];
                        (band.y0 as usize) < y + h && (band.y1 as usize) > y
                    })
                    .collect();
                let parts = touched.iter().filter(|&&i| !fails[i][served[i]]).count();
                for &i in &touched {
                    served[i] += 1;
                }
                match (touched.len(), parts) {
                    (0, _) => reached[2] += 1,
                    (_, 0) => reached[1] += 1,
                    _ => {
                        in_flight += 1;
                        reached[0] += 1;
                    }
                }
                job_tx
                    .send(Job {
                        buf: Arc::new(I420::new(16, height)),
                        frame: super::Frame::Motion {
                            band,
                            dirty: Arc::new(vec![]),
                        },
                        target: Kbps::new(1000.0),
                        capacity: None,
                        captured: CaptureMarks::at(Instant::now()),
                        marker_ms: None,
                    })
                    .unwrap();
                let home = ret_rx.recv().unwrap();
                assert_eq!(
                    viewer.inflight().len(),
                    in_flight,
                    "case {case}: frame {idx} over {band:?}, {n} stripes, its picture home"
                );
                drop(home);
            }
            drop(job_tx);
            drop(encode);
            ended.recv().unwrap().unwrap();
        }
        assert!(
            reached.iter().all(|&r| r >= 100),
            "too few cases reached: {reached:?}"
        );
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
            assert_eq!(Header::read(&message), Ok(Video::Stripe(header, &picture)));
        }
    }

    #[test]
    fn a_header_alone_is_empty_less_is_short_and_another_type_is_not_video() {
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        for _ in 0..100_000 {
            let header = random_header(&mut rng);
            let bytes = header.bytes();
            assert_eq!(Header::read(&bytes), Ok(Video::Empty(header)));
            let cut = 1 + (rng.next() % (Header::LEN as u64 - 1)) as usize;
            assert_eq!(Header::read(&bytes[..cut]), Err(NotVideo::Short));
            // Another message's type byte, or no byte, with anything after it.
            let mut other: Vec<u8> = (0..rng.next() % 64).map(|_| rng.next() as u8).collect();
            if other.first() == Some(&Header::VIDEO) {
                other[0] = Header::VIDEO.wrapping_add(1 + (rng.next() % 255) as u8);
            }
            assert_eq!(Header::read(&other), Err(NotVideo::Other));
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
            let (bytes, picture) = (hex(&c.header), hex(&c.picture));
            let message = [&bytes[..], &picture].concat();
            assert_eq!(
                Header::read(&message),
                Ok(Video::Stripe(header, &picture)),
                "{}",
                c.header
            );
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
                debug: Debugging::read(|_| None).0,
            },
            crf: 23.0,
            max_kbps: None,
        };
        let stripe = Stripe {
            y: 0,
            h,
            enc: Encoder::new(&ecfg).unwrap(),
            applied: None,
        };
        let (job_tx, job_rx) = mpsc::sync_channel::<Job>(1);
        let (ret_tx, _ret_rx) = mpsc::sync_channel::<Returned>(1);
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
                    Debugging::read(|_| None).0,
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
            buf: Arc::new(I420::new(w, h)),
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
    #[ignore = "frames allocate on main: the conversion's threads (split_rows), started per \
                capture; 40 B once in 120 frames on the test's own thread, which plays the \
                capture loop and the viewer"]
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
            debug: Debugging::read(|_| None).0,
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
            let pic = Arc::get_mut(&mut buf).unwrap();
            crate::convert::convert_band(&screen, w * 4, pic, y0, y1, cfg.convert_threads);
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

    /// Xorshift from `seed`: each call gives 0 to `n` less one, the same cases every run.
    fn generator(mut seed: u64) -> impl FnMut(u64) -> u64 {
        move |n| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        }
    }

    /// A held gate names the soonest moment time alone can open it: the viewer that holds
    /// it stalls. Over generated viewers (watching or not, a round trip of up to 4 s or
    /// none, up to 45 frames in flight whose oldest is about its round trip old,
    /// about to stall, or anything up to 5 s, to the microsecond) at 1 to 240 fps: asked
    /// again at any moment before it, with no ack, no viewer coming or going and no
    /// keyframe asked for, the gate is held, so a wait until then misses nothing; that
    /// moment is when a viewer asked stalls, so the wait is no shorter; and asked at it,
    /// the gate opens, or names a later moment or none. Held with none (no viewer has
    /// room), it is held whenever it is asked again: time alone never opens it. And what
    /// it answers is the rule, whatever the viewers' order: held while a watching viewer
    /// is full, to when the first of them stalls; else open while one has room.
    #[test]
    fn a_held_flow_gate_waits_until_a_viewer_stalls() {
        let mut next = generator(0x9E37_79B9_7F4A_7C15);
        let now = Instant::now() + Duration::from_secs(10);
        // Held to a stall, held with every watching viewer stalled, and open.
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
            let at = format!("case {case}: {fps} fps");
            let viewers = hub.viewers();
            let gate = Gated::ask(&viewers, interval, now);
            // Each viewer asked alone: room, full until it stalls, or stalled.
            let alone: Vec<Gated> = viewers
                .iter()
                .map(|c| Gated::ask(std::slice::from_ref(c), interval, now))
                .collect();
            let full = alone.iter().find_map(|g| match g {
                Gated::Held { until: Some(until) } => Some(*until),
                Gated::Open | Gated::Held { until: None } => None,
            });
            let rule = match (full, alone.contains(&Gated::Open)) {
                (Some(until), _) => Gated::Held { until: Some(until) },
                (None, true) => Gated::Open,
                (None, false) => Gated::Held { until: None },
            };
            assert_eq!(gate, rule, "{at}");
            let until = match gate {
                Gated::Open => {
                    seen[2] += 1;
                    continue;
                }
                Gated::Held { until: None } => {
                    for t in [1, 1_000, 10_000_000].map(|us| now + Duration::from_micros(us)) {
                        let again = Gated::ask(&viewers, interval, t);
                        assert_eq!(again, Gated::Held { until: None }, "{at}");
                    }
                    seen[1] += u32::from(!oldest.is_empty());
                    continue;
                }
                Gated::Held { until: Some(until) } => until,
            };
            let at = format!("{at}, held until {:?}", until - now);
            assert!(
                oldest.iter().any(|&sent| sent + STALLED == until),
                "not when a viewer stalls: {at}"
            );
            for t in [
                now,
                now + (until - now) / 2,
                until - Duration::from_micros(1),
            ] {
                let again = Gated::ask(&viewers, interval, t);
                assert!(
                    matches!(again, Gated::Held { until: Some(_) }),
                    "{again:?} at {:?}: {at}",
                    t - now
                );
            }
            let then = Gated::ask(&viewers, interval, until);
            assert!(
                match then {
                    Gated::Open | Gated::Held { until: None } => true,
                    Gated::Held { until: Some(later) } => later > until,
                },
                "{then:?}: {at}"
            );
            seen[0] += 1;
        }
        assert!(seen.iter().all(|&n| n > 100), "unreached: {seen:?}");
    }

    /// A pass waits for every viewer to acknowledge all it was sent, each until the newest
    /// frame it was sent has had `STALLED`: nothing holds it once every viewer has done one
    /// or the other, and else it waits as long as the latest of them has left. Over none to
    /// three viewers, each with none to two frames in flight, sent in order at ages either
    /// side of nothing and of `STALLED`; read from the viewers as the capture loop reads
    /// them, against the instants the frames were sent.
    #[test]
    fn a_pass_waits_for_the_viewers_to_acknowledge_what_they_were_sent() {
        let ns = Duration::from_nanos;
        let now = Instant::now() + Duration::from_secs(60);
        let ages = [
            ns(0),
            ns(1),
            Duration::from_millis(250),
            STALLED - ns(1),
            STALLED,
            STALLED + ns(1),
            Duration::from_secs(30),
        ];
        // What one viewer has in flight, oldest first: nothing, one frame, or two.
        let queues: Vec<Vec<Duration>> = std::iter::once(vec![])
            .chain(ages.map(|a| vec![a]))
            .chain(ages.iter().flat_map(|&a| {
                ages.iter()
                    .filter(move |&&b| b <= a)
                    .map(move |&b| vec![a, b])
            }))
            .collect();
        let hub = Hub::new();
        let viewers: Vec<Arc<Client>> = (0..3)
            .map(|_| hub.add_client(tokio::sync::mpsc::unbounded_channel().0))
            .collect();
        let mut seen = [0; 2];
        for n in 0..=viewers.len() as u32 {
            for pick in 0..queues.len().pow(n) {
                let picked: Vec<&[Duration]> = (0..n)
                    .map(|i| &queues[pick / queues.len().pow(i) % queues.len()][..])
                    .collect();
                for (c, ages) in viewers.iter().zip(&picked) {
                    let mut q = c.inflight();
                    q.clear();
                    q.extend(
                        ages.iter()
                            .enumerate()
                            .map(|(id, &age)| crate::hub::Inflight {
                                id: id as u32,
                                sent: now - age,
                                bytes: 1000,
                                alone: id == 0,
                            }),
                    );
                }
                let got = InFlight::of(viewers[..n as usize].iter().map(|c| c.backlog(now)));
                // Each viewer holds the pass until its newest frame has had `STALLED`.
                let until = picked
                    .iter()
                    .filter_map(|ages| ages.last())
                    .map(|&age| now - age + STALLED)
                    .filter(|&until| until > now)
                    .max();
                let want = match until {
                    None => InFlight::Nothing,
                    Some(until) => InFlight::Frames(until - now),
                };
                assert_eq!(got, want, "in flight, by age: {picked:?}");
                seen[until.is_some() as usize] += 1;
            }
        }
        assert!(seen.iter().all(|&n| n > 1000), "unreached: {seen:?}");
    }

    /// On a path that carries less than the encoder makes, a keystroke waits behind one
    /// pass at most, and sharpening goes on: a pass goes only once the path is empty, and
    /// at most a frame's least interval after. The loop's own `step`, with a pass due at
    /// every look (a page whose passes all come back squeezed, as on a slow path), drives a
    /// modelled stream: an encoder that codes one capture at a time, a path of a rate and
    /// a round trip that serves in order, and a viewer that acknowledges each frame as it
    /// arrives; keystrokes come at generated instants. Over 300 generated paths from
    /// 100 kbit/s to 40 Mbit/s and round trips to 600 ms, passes to 40 KB, keystrokes'
    /// frames to 4 KB, encodes of 1 to 30 ms and coalesce windows to 4 ms, 30 s each.
    #[test]
    fn a_keystroke_waits_behind_one_pass_at_most() {
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut next = |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let us = Duration::from_micros;
        // A frame on its way: when it went, when its last byte left the path's queue,
        // when its ack comes back, and whether it is a pass.
        struct Sent {
            at: Instant,
            out: Instant,
            acked: Instant,
            pass: bool,
        }
        let (mut keys_behind_a_pass, mut passes) = (0, 0);
        for _ in 0..300 {
            let bps = 100_000 + next(40_000_000);
            let rtt = us(1000 + next(600_000));
            let pass_bytes = 200 + next(40_000);
            let key_bytes = 200 + next(4000);
            let encode = us(1000 + next(29_000));
            let coalesce = us(next(4000));
            let min_interval = Duration::from_secs_f64(1.0 / 60.0).mul_f64(MIN_INTERVAL_SHARE);
            let crossing = |bytes: u64| us(bytes * 8_000_000 / bps);
            let t0 = Instant::now() + Duration::from_secs(10);
            let end = t0 + Duration::from_secs(30);
            let mut keys = std::collections::VecDeque::new();
            let mut k = t0;
            while k < end {
                k += us(50_000 + next(3_000_000));
                keys.push_back(k);
            }
            let (mut now, mut captured) = (t0, Captured::new(t0 - Duration::from_secs(1)));
            let mut damage = Damage::None;
            // The capture the encoder holds: when it is coded, and whether it is a pass.
            let mut encoder: Option<(Instant, bool)> = None;
            let (mut sent, mut path_free) = (Vec::<Sent>::new(), t0);
            // Since when the path has been empty, the encoder idle and nothing damaged.
            let mut clear: Option<Instant> = None;
            while now < end {
                if let Some((done, pass)) = encoder.filter(|e| e.0 <= now) {
                    encoder = None;
                    let out =
                        done.max(path_free) + crossing(if pass { pass_bytes } else { key_bytes });
                    path_free = out;
                    if !pass {
                        let ahead = sent.iter().filter(|f| f.pass && f.out > done).count();
                        assert!(ahead <= 1, "a keystroke's frame behind {ahead} passes");
                        keys_behind_a_pass += ahead;
                    }
                    sent.push(Sent {
                        at: done,
                        out,
                        acked: out + rtt,
                        pass,
                    });
                }
                while let Some(key) = keys.pop_front() {
                    if key > now {
                        keys.push_front(key);
                        break;
                    }
                    if damage == Damage::None {
                        damage = Damage::Rows {
                            since: key,
                            y0: 0,
                            y1: 16,
                        };
                    }
                }
                let unacked: Vec<&Sent> = sent.iter().filter(|f| f.acked > now).collect();
                let backlog = unacked.first().zip(unacked.last()).map(|(o, n)| Backlog {
                    frames: std::num::NonZeroUsize::new(unacked.len()).unwrap(),
                    oldest: now - o.at,
                    newest: now - n.at,
                });
                clear = match (backlog, encoder, damage) {
                    (None, None, Damage::None) => clear.or(Some(now)),
                    _ => None,
                };
                let look = Look {
                    key: Key::NotWanted,
                    damage,
                    due: Due::Now,
                    now,
                    captured,
                    min_interval,
                    coalesce,
                    gate: Gated::Open,
                    encoding: match encoder {
                        None => Encoding::Idle,
                        Some(_) => Encoding::Busy,
                    },
                    in_flight: InFlight::of([backlog]),
                };
                let wait = match step(&look) {
                    Step::Capture(what) => {
                        let pass = what == Next::Pass;
                        if pass {
                            let since = clear.expect("a pass went onto a path in use");
                            assert!(
                                now - since <= min_interval,
                                "a pass {:?} after the path emptied",
                                now - since
                            );
                            passes += 1;
                        }
                        damage = Damage::None;
                        encoder = Some((now + encode, pass));
                        captured.on_capture(what, now);
                        continue;
                    }
                    Step::Wait(wait) => wait,
                };
                // Each wait ends as `Wait::through` makes it: on damage, on the hub's waker
                // (a buffer home, an ack), on its time.
                let woken = encoder
                    .map(|e| e.0)
                    .into_iter()
                    .chain(sent.iter().map(|f| f.acked).filter(|&a| a > now));
                let damaged = keys.front().copied();
                now = match wait {
                    Wait::Change => woken.chain(damaged).min(),
                    Wait::Pass(due) | Wait::Gate(Some(due)) => {
                        woken.chain(damaged).chain([now + due]).min()
                    }
                    Wait::Gate(None) => woken.chain(damaged).min(),
                    Wait::Interval { rest, next } => match next.interval_ends() {
                        On::Event => woken.chain(damaged).chain([now + rest]).min(),
                        On::Wake => woken.chain([now + rest]).min(),
                    },
                    Wait::Settle(rest) => Some(now + rest),
                    Wait::Buffer => woken.min(),
                }
                .unwrap_or(end);
            }
        }
        assert!(
            passes > 10_000 && keys_behind_a_pass > 100,
            "unreached: {passes} passes, {keys_behind_a_pass} keystrokes behind one"
        );
    }

    /// One look, as the capture loop reads it: the capturer's damage, whether a viewer
    /// wants a keyframe, the pass due, the last capture, the pacing, the
    /// gate, whether the buffer is home, what the viewers have in flight, the area the
    /// damage comes to when taken, and the blocks pending.
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
        in_flight: InFlight,
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
                in_flight: self.in_flight,
            }
        }
    }

    /// How many looks `each_case` makes: four frame rates, three coalesce windows, six
    /// damages, 28 pairs of times since the last change and the last capture, nine dues,
    /// seven answers of the gate, and 32 ways for the rest.
    const CASES: usize = 4 * 3 * 6 * 28 * 9 * 7 * 32;

    /// Every look over a discretised domain: each state the capturer's damage takes, at
    /// four times since it came either side of three coalesce windows; four frame rates,
    /// at seven times since the last change and since the last capture (no longer ago than
    /// the change), either side of the least interval and its rounding to whole
    /// milliseconds; seven waits for a pass, a pass due and none; a
    /// keyframe wanted or not; the gate open, holding until a viewer stalls now, either
    /// side of a millisecond or seconds away, or holding with no viewer to stall; the
    /// buffer home or out; nothing in flight, or frames whose newest will have had
    /// `STALLED` in a nanosecond under it; damage that came to an area or to none; and
    /// blocks pending or none.
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
        .map(|d| Gated::Held {
            until: Some(now + d),
        });
        let gates = [
            Gated::Open,
            gates[0],
            gates[1],
            gates[3],
            gates[4],
            gates[5],
            Gated::Held { until: None },
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
                    for (case, gate) in (0..32).flat_map(|case| gates.map(move |g| (case, g))) {
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
                            in_flight: match bit(4) {
                                false => InFlight::Nothing,
                                true => InFlight::Frames(STALLED - ns(1)),
                            },
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
    /// encoder holds the buffer or a viewer has frames to acknowledge), for the pass due,
    /// or for the viewers' acks until the newest frame has had `STALLED`, or else for a
    /// change; the rest of the least interval
    /// since the last capture; the rest of the coalesce window; the flow gate. The gate
    /// is asked only when nothing before it holds the capture and no keyframe is wanted.
    /// Checked over every look of `each_case`.
    #[test]
    fn a_capture_waits_only_for_what_holds_it() {
        let cases = each_case(|c| {
            let got = step(&c.look());
            // A due pass goes only to an idle encoder, and onto empty paths.
            let pass_goes = c.due == Due::Now && c.home && c.in_flight == InFlight::Nothing;
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
            let want = match (still, early, settling) {
                (true, _, _) => Step::Wait(match (c.due, c.home, c.in_flight) {
                    (Due::In(due), _, _) => Wait::Pass(due),
                    (Due::Now, true, InFlight::Frames(last)) => Wait::Pass(last),
                    (Due::Now, true, InFlight::Nothing) => unreachable!("a pass that goes"),
                    (Due::Now, false, _) | (Due::Never, _, _) => Wait::Change,
                }),
                (false, Some(rest), _) => Step::Wait(Wait::Interval { rest, next }),
                (false, None, Some(rest)) => Step::Wait(Wait::Settle(rest)),
                (false, None, None) => match (gate_asked, c.gate, c.home) {
                    (true, Gated::Held { until }, _) => {
                        Step::Wait(Wait::Gate(until.map(|until| until - c.now)))
                    }
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
    /// a wake, with no limit; a pass on damage or a wake, or once due; the interval before
    /// a change only on a wake, before a pass on damage too, or once over; the gate on
    /// damage or a wake, or once the viewer it holds for will have stalled, with no limit
    /// when every viewer has; a buffer only on a wake, with no limit; the coalesce window
    /// as a sleep of exactly its rest. Over every wait `step` makes in `each_case`.
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
                Wait::Interval {
                    rest,
                    next: Next::Change,
                } => (Some((On::Wake, Some(rest))), None),
                Wait::Interval {
                    rest,
                    next: Next::Pass,
                } => (Some((On::Event, Some(rest))), None),
                Wait::Settle(rest) => (None, Some(rest)),
                Wait::Gate(stalls) => (Some((On::Event, stalls)), None),
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

    /// When a viewer's acks for a frame come, from the frame's going.
    #[derive(Clone, Copy, Debug)]
    enum Acks {
        /// Its round trip (the one its pings show) and up to `w_us` more: it may fill,
        /// never stall.
        OnTime { rtt_ms: u32, w_us: u64 },
        /// Each anywhere from `lo_us` to `hi_us`.
        Late { lo_us: u64, hi_us: u64 },
    }

    /// A viewer in the model of the capture loop's flow.
    struct Watcher {
        c: Arc<crate::hub::Client>,
        acks: Acks,
        /// Its acks on their way: when each arrives, in order, and for which frame.
        coming: std::collections::VecDeque<(Instant, u32)>,
        keyframes: u32,
        frames: u32,
    }

    /// A viewer whose acks come late (5 s, or anything up to 8 s) is sent no keyframe but
    /// the one it starts on, is sent every frame after it, and once it has stalled holds
    /// no other viewer; alone, it holds the capture, and is never sent more frames than
    /// its acks allow. Nothing is done to it: what it has in flight is every frame sent to
    /// it and not acknowledged. A model of the capture loop's flow over 30 s, one frame
    /// time at a time with the screen always changing: the acks due arrive, in order as
    /// TCP brings them; then a keyframe if one is wanted, else the gate is asked; and a
    /// frame goes to every watching viewer as the encode loop sends one. Over the late
    /// viewer alone or beside 1 to 3 on time (round trips of 1 to 300 ms, plus up to
    /// 150 ms, so they hold the capture now and then), placed anywhere among them,
    /// its acks each 5 s late or anywhere in a range of 0 to 8 s, its round trip known
    /// or not, at 10 to 120 fps. Every viewer joins at the start.
    #[test]
    fn a_late_viewer_is_sent_no_keyframe_and_holds_nobody_else() {
        let mut next = generator(0x2545_F491_4F6C_DD1D);
        let start = Instant::now() + Duration::from_secs(10);
        // Steps where the late viewer had stalled: beside viewers the gate opened for,
        // beside viewers one of which held it, and alone; and late viewers that came back.
        let mut seen = [0u32; 4];
        for case in 0..400 {
            let fps = 10 + next(111);
            let interval = Duration::from_secs_f64(1.0 / fps as f64);
            let interval_ms = interval.as_secs_f64() * 1000.0;
            let on_time = next(4) as usize;
            let late_at = next(on_time as u64 + 1) as usize;
            let late = match next(4) {
                0 => Acks::Late {
                    lo_us: 5_000_000,
                    hi_us: 5_000_001,
                },
                _ => {
                    let lo_us = next(5_000_000);
                    Acks::Late {
                        lo_us,
                        hi_us: lo_us + 1 + next(3_000_000),
                    }
                }
            };
            let hub = Hub::new();
            // The same viewers but the late one.
            let mut others = Vec::new();
            let mut viewers = Vec::new();
            for i in 0..=on_time {
                let acks = match i == late_at {
                    true => late,
                    false => Acks::OnTime {
                        rtt_ms: 1 + next(300) as u32,
                        w_us: next(150_001),
                    },
                };
                let c = hub.add_client(tokio::sync::mpsc::unbounded_channel().0);
                c.watch();
                let rtt_ms = match acks {
                    Acks::OnTime { rtt_ms, .. } => rtt_ms,
                    Acks::Late { .. } => next(2) as u32 * (1 + next(300) as u32),
                };
                c.min_rtt_ms.store(rtt_ms, Ordering::Relaxed);
                if i != late_at {
                    others.push(c.clone());
                }
                viewers.push(Watcher {
                    c,
                    acks,
                    coming: Default::default(),
                    keyframes: 0,
                    frames: 0,
                });
            }
            let at = format!("case {case}: {fps} fps, {on_time} on time, late {late:?}");
            let mut stalled_before = false;
            let mut id = 0u32;
            let mut t = start;
            while t < start + Duration::from_secs(30) {
                for v in &mut viewers {
                    while let Some(&(when, id)) = v.coming.front().filter(|a| a.0 <= t) {
                        v.c.on_ack(when, id, crate::hub::Hold::Unsaid);
                        v.coming.pop_front();
                    }
                }
                let stalled = viewers[late_at]
                    .c
                    .backlog(t)
                    .is_some_and(|b| b.oldest >= STALLED);
                seen[3] += u32::from(stalled_before && !stalled);
                stalled_before = stalled;
                let key = viewers
                    .iter()
                    .any(|v| v.c.viewer() == crate::hub::Viewer::Waiting);
                let go = key || {
                    let gate = Gated::ask(&hub.viewers(), interval_ms, t);
                    if stalled {
                        let others: Vec<_> =
                            others.iter().map(|c| (c.clone(), c.viewer())).collect();
                        assert_eq!(gate, Gated::ask(&others, interval_ms, t), "{at}");
                        seen[match (on_time, gate) {
                            (0, _) => 2,
                            (_, Gated::Open) => 0,
                            (_, Gated::Held { .. }) => 1,
                        }] += 1;
                    }
                    gate == Gated::Open
                };
                if go {
                    // A keyframe is planned for the viewers waiting, and sent whole: it
                    // starts them.
                    if key {
                        for v in &viewers {
                            v.c.keyframe_planned();
                        }
                        for v in &viewers {
                            v.c.keyframe_sent(true);
                        }
                    }
                    for v in &mut viewers {
                        if v.c.viewer() != crate::hub::Viewer::Started {
                            continue;
                        }
                        let mut q = v.c.inflight();
                        let alone = q.is_empty();
                        q.push_back(crate::hub::Inflight {
                            id,
                            sent: t,
                            bytes: 1000,
                            alone,
                        });
                        drop(q);
                        v.keyframes += u32::from(key);
                        v.frames += 1;
                        let after = match v.acks {
                            Acks::OnTime { rtt_ms, w_us } => {
                                Duration::from_millis(rtt_ms as u64)
                                    + Duration::from_micros(next(w_us + 1))
                            }
                            Acks::Late { lo_us, hi_us } => {
                                Duration::from_micros(lo_us + next(hi_us - lo_us))
                            }
                        };
                        // In order: an ack waits for the one before it.
                        let when = v.coming.back().map_or(t + after, |a| a.0.max(t + after));
                        v.coming.push_back((when, id));
                    }
                    id += 1;
                }
                let v = &viewers[late_at];
                assert_eq!(v.keyframes, 1, "{at}");
                assert_eq!(v.c.viewer(), crate::hub::Viewer::Started, "{at}");
                assert_eq!(v.c.inflight().len(), v.coming.len(), "{at}");
                match on_time {
                    0 => assert!(v.coming.len() <= MOST_IN_FLIGHT, "{at}"),
                    _ => assert!(viewers.iter().all(|o| o.frames == v.frames), "{at}"),
                }
                t += interval;
            }
        }
        assert!(seen.iter().all(|&n| n > 100), "unreached: {seen:?}");
    }
}
