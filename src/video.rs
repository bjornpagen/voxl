//! Video pipeline: damage-driven capture -> I420 -> x264 -> clients, with ack-based flow control.

use crate::capture::{Band, Capturer};
use crate::convert::I420;
use crate::encoder::{Encoded, Encoder, EncoderConfig, Pictures, Planes, Preset, Quality};
use crate::env::{Debugging, Marker};
use crate::hub::{wait_any, wait_event};
use crate::hub::{Client, Hub, Out, ServerMessage, Takes, Viewer};
use crate::path::{Capacity, Kbps};
use crate::rate::{Bounds, FrameBudget, RateCtl, RTT_GUESS_MS};
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
    pub fps: u32,
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
    idx: usize,
    frame: Frame,
    /// The bitrate the stream is steered to, and what the viewers' paths can carry,
    /// as the rate controller had them when the frame was captured.
    target: Kbps,
    capacity: Capacity,
    captured_at: Instant,
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

    /// The blocks x264 analyses (None: all of them).
    fn mb_dirty(&self) -> Option<Arc<Vec<u8>>> {
        match self {
            Frame::Key { .. } => None,
            Frame::Motion { dirty, .. } | Frame::MotionAndSharpen { dirty, .. } => {
                Some(dirty.clone())
            }
            Frame::Sharpen { blocks, .. } => Some(blocks.map.clone()),
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
    idx: usize,
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

/// What the viewers watching are sent: the encoders, and the two buffers captures are
/// converted into for them. There is one only while somebody watches.
struct Stream {
    /// Dropped first (fields drop in order), which ends the encode loop once it has sent
    /// what it was given.
    job_tx: mpsc::SyncSender<Job>,
    ret_rx: mpsc::Receiver<Returned>,
    slots: [Slot; 2],
    /// The slot the next capture is converted into.
    next: usize,
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

/// One of the two buffers a capture is converted into: the encoder reads one while the
/// next capture is converted into the other.
struct Slot {
    /// None while the encoder holds it.
    buf: Option<Box<I420>>,
    /// The rows that changed on screen since it was last converted into.
    pending: Option<Band>,
}

impl Stream {
    /// The encoders for a screen of `w` x `h`, one for each stripe the hub's hello names,
    /// on their thread, and two buffers that hold nothing of it yet.
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
        let slot = || Slot {
            buf: Some(Box::new(I420::new(w as usize, h as usize))),
            pending: Some(Band { y0: 0, y1: h }),
        };
        Ok(Stream {
            job_tx,
            ret_rx,
            slots: [slot(), slot()],
            next: 0,
            _encode: encode,
        })
    }

    /// A buffer the encoder is done with; the blocks it sharpened, if it did.
    fn returned(&mut self, r: Returned) -> Option<Sharpen> {
        self.slots[r.idx].buf = Some(r.buf);
        r.sharpened
    }

    /// Whether the encoder holds a buffer.
    fn encoding(&self) -> Encoding {
        match self.slots.iter().all(|slot| slot.buf.is_some()) {
            true => Encoding::Idle,
            false => Encoding::Busy,
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
        cfg.fps,
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

fn flow_ok(hub: &Hub, interval_ms: f64) -> bool {
    for (c, _) in hub.viewers() {
        let (n, age) = c.backlog();
        if n == 0 {
            continue;
        }
        let rtt = c.rtt_ms().unwrap_or(RTT_GUESS_MS) as f64;
        // Enough frames in flight to cover one RTT plus decode, but never more than ~rtt+120ms of queue.
        let max_n = (((rtt + 40.0) / interval_ms).ceil() as usize + 2).clamp(3, 40);
        let max_age = rtt as u128 + 120;
        if age > 4000 {
            // Stalled client (hidden tab, dead socket): stop letting it gate everyone.
            log::warn!(
                "client {} stalled ({} frames, {} ms); resyncing",
                c.id,
                n,
                age
            );
            c.start_over();
            continue;
        }
        if n >= max_n || age > max_age {
            return false;
        }
    }
    true
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
#[derive(Debug, PartialEq)]
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

/// Whether the encoder holds a frame: one of the capture loop's two buffers is out.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Encoding {
    Idle,
    Busy,
}

/// When the capture loop looks again, with nothing moved and no keyframe wanted.
#[derive(Debug, PartialEq)]
enum Look {
    /// A pass is due and the encoder idle: capture it now.
    Now,
    /// When the next pass is due, or sooner if something happens.
    In(Duration),
    /// When something happens. Everything the loop answers wakes it: damage, a viewer's
    /// command or ack, a viewer leaving, a screenshot or recording asked for, a buffer
    /// coming home from the encoder. So nothing needs a tick.
    OnWake,
}

/// Nothing moved and no keyframe is wanted: when to look again. A pass goes only to an
/// idle encoder. Queued behind a frame being encoded it would hold the next keystroke
/// back by two encodes; this way motion that comes meanwhile takes the free buffer and
/// waits for one at most.
fn still_wait(due: &Due, encoding: Encoding) -> Look {
    match (due, encoding) {
        (Due::Now, Encoding::Idle) => Look::Now,
        (Due::Now, Encoding::Busy) | (Due::Never, Encoding::Idle | Encoding::Busy) => Look::OnWake,
        (Due::In(wait), Encoding::Idle | Encoding::Busy) => Look::In(*wait),
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
    let interval = Duration::from_secs_f64(1.0 / cfg.fps as f64);
    // Allow slight early capture so source jitter doesn't alias into dropped frames.
    let min_interval = interval.mul_f64(0.85);
    let interval_ms = interval.as_secs_f64() * 1000.0;
    let mut last_capture = Instant::now() - interval;
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
        if let Some(s) = stream.as_mut() {
            while let Ok(r) = s.ret_rx.try_recv() {
                if let Some(sharp) = s.returned(r) {
                    refiner.sharpened(&sharp.map, sharp.at);
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
                cap.damaged = true;
                refiner.mark_all(Instant::now());
                Some(s)
            }
        };
        let Some(s) = stream.as_mut() else {
            // The target stands: a viewer that hid its tab comes back over the same
            // path, and one on a new path starts from what its own acks say.
            wait_event(&[cap.fd(), hub.waker.waitable()]);
            continue;
        };
        let capacity = rc.update(&hub, &viewers);
        let want_key = viewers.iter().any(|(_, v)| *v == Viewer::Waiting);
        // One reading of the clock for every decision before the capture; the capture
        // itself is stamped `t0` below, after any wait for a buffer.
        let now = Instant::now();
        let due = sharpening.due(now);
        if !cap.damaged && !want_key {
            match still_wait(&due, s.encoding()) {
                Look::Now => {}
                Look::In(wait) => {
                    let ms = wait.as_micros().div_ceil(1000) as i32;
                    wait_any(&[cap.fd(), hub.waker.waitable()], ms);
                    continue;
                }
                Look::OnWake => {
                    wait_event(&[cap.fd(), hub.waker.waitable()]);
                    continue;
                }
            }
        }
        let since = now.saturating_duration_since(last_capture);
        if since < min_interval {
            let rem = min_interval - since;
            wait_any(
                &[hub.waker.waitable()],
                (rem.as_micros() as i32 + 999) / 1000,
            );
            continue;
        }
        // Apps present a frame as a burst of puts; give the burst a moment to land so
        // the capture holds one whole frame instead of its first tiles.
        if let Some(t) = cap.damaged_at {
            let el = now.saturating_duration_since(t);
            if el < cfg.coalesce {
                let rem = cfg.coalesce - el;
                std::thread::sleep(rem);
                continue;
            }
        }
        if !want_key && !flow_ok(&hub, interval_ms) {
            hub.stats.flow_blocked.fetch_add(1, Ordering::Relaxed);
            wait_any(&[cap.fd(), hub.waker.waitable()], 4);
            continue;
        }
        // Need a free buffer (the encoder may still hold the other).
        if s.slots[s.next].buf.is_none() {
            match s.ret_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(r) => {
                    if let Some(sharp) = s.returned(r) {
                        refiner.sharpened(&sharp.map, sharp.at);
                    }
                }
                Err(_) => continue,
            }
            if s.slots[s.next].buf.is_none() {
                continue;
            }
        }

        let t0 = Instant::now();
        let origin = cap.damaged_at.unwrap_or(t0);
        hub.stats
            .damage_wait_us
            .fetch_add((t0 - origin).as_micros() as u64, Ordering::Relaxed);
        let mut band = None;
        let mb_dirty = cap.take_mb_map();
        refiner.mark(&mb_dirty, t0);
        if cap.damaged {
            band = cap.take_damage()?;
            if let Some(b) = band {
                cap.grab(b)?;
            }
        }
        let t1 = Instant::now();
        if marker.is_some() {
            log::info!(
                "cap dt {:.1} ms band {:?} grab {} us",
                last_capture.elapsed().as_secs_f64() * 1000.0,
                band,
                (t1 - t0).as_micros()
            );
        }
        let marker_ms = marker_reading(cap.bgrx(), probe);
        // What this capture is sent as, decided once, before a buffer is taken.
        let plan = match (want_key, band, due) {
            (true, Some(_), Due::Now | Due::In(_) | Due::Never) => Plan::Key(Keyframe::Fresh),
            (true, None, Due::Now) => Plan::Key(Keyframe::Pass),
            (true, None, Due::In(_) | Due::Never) => Plan::Key(Keyframe::Fresh),
            (false, Some(moved), Due::Now | Due::In(_) | Due::Never) => Plan::Motion(moved),
            // Sharpen only what was sent at motion quality; nothing pending, nothing to send.
            (false, None, Due::Now) => match refiner.pending_map() {
                Some((map, band)) => Plan::Sharpen { map, band },
                None => {
                    sharpening.on_nothing_pending();
                    continue;
                }
            },
            // The damage was empty after all, and no pass is due: nothing to send.
            (false, None, Due::In(_) | Due::Never) => continue,
        };
        let idx = s.next;
        let slot = &mut s.slots[idx];
        let mut buf = slot.buf.take().unwrap();
        let todo = match (slot.pending.take(), band) {
            (p, Some(b)) => Some(union(p, b)),
            (p, None) => p,
        };
        if let Some(b) = band {
            let other = &mut s.slots[1 - idx].pending;
            *other = Some(union(*other, b));
        }
        if let Some(t) = todo {
            cap.convert(&mut buf, t.y0 as usize, t.y1 as usize, cfg.convert_threads);
        }
        let t2 = Instant::now();
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
        last_capture = t0;
        s.next = 1 - idx;
        if s.job_tx
            .send(Job {
                buf,
                idx,
                frame,
                target: rc.target,
                capacity,
                captured_at: origin,
                marker_ms,
            })
            .is_err()
        {
            return Err("encoder gone".into());
        }
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
    mb_dirty: Option<Arc<Vec<u8>>>,
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
    fps: u32,
    crfs: Crfs,
    debug: bool,
) {
    let mut motion = MotionQuality::default();
    while let Ok(job) = rx.recv() {
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
            s.enc.set_rate_buf(asked, boost, boost / fps.max(1) * 8);
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
        let mut info = job.mb_dirty.as_ref().map(|d| {
            let rows = s.h.div_ceil(16);
            let start = (s.y / 16) * job.mb_cols;
            d[start..start + rows * job.mb_cols]
                .iter()
                .map(|&x| (x == 0) as u8)
                .collect::<Vec<u8>>()
        });
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
    fps: u32,
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
                idx: job.idx,
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
            .a_frame_at(fps)
            .get() as u32;
        let share = (frame.saturating_mul(2) / n as u32).clamp(1, frame.max(1));
        let ts = job.captured_at.duration_since(epoch).as_millis() as i64;
        // Persistent per-stripe workers keep each encoder's state warm in one core's cache.
        let buf_ptr = SendPtr(&*job.buf as *const I420);
        let mut pending = 0;
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
            idx: job.idx,
            buf: job.buf,
            sharpened: sharpened.filter(|_| sharp),
        });
        hub.wake();
        let parts: Vec<(usize, bool, Vec<u8>)> = results
            .into_iter()
            .enumerate()
            .filter_map(|(i, r)| match r {
                StripeOut::Sent { key, bytes, .. } => {
                    (bytes.len() > Header::LEN).then_some((i, key, bytes))
                }
                StripeOut::Failed => None,
            })
            .collect();
        let all_key = parts.len() == n && parts.iter().all(|p| p.1);
        // The viewers this keyframe was promised to start on it, or wait for another.
        for c in &starts {
            c.keyframe_sent(all_key);
        }
        if parts.is_empty() {
            continue;
        }
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
        let pipe = Pipe::of(job.captured_at.elapsed());
        let nparts = parts.len() as u8;
        let msgs: Vec<Bytes> = parts
            .into_iter()
            .map(|(i, key, mut out)| {
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
        for (c, _) in hub.viewers() {
            match c.takes(frame_id, now, total, HOPELESSLY_BEHIND) {
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
            job.captured_at.elapsed().as_micros() as u64,
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
                fps: 60,
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
                    60,
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
                    idx: 0,
                    frame,
                    target: Kbps::new(8000.0),
                    capacity: Capacity::Unknown,
                    captured_at: Instant::now(),
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
                        fps: 60,
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
                    for (idx, frame) in [frame, next].into_iter().enumerate() {
                        s.job_tx
                            .send(Job {
                                buf: Box::new(I420::new(w as usize, h as usize)),
                                idx,
                                frame,
                                target: Kbps::new(8000.0),
                                capacity: Capacity::Unknown,
                                captured_at: Instant::now(),
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
                        fps: 60,
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
        assert_eq!(still_wait(&Due::Now, Encoding::Idle), Look::Now);
        assert_eq!(still_wait(&Due::Now, Encoding::Busy), Look::OnWake);
        assert_eq!(still_wait(&Due::Never, Encoding::Idle), Look::OnWake);
        assert_eq!(
            still_wait(&Due::In(ms(40)), Encoding::Busy),
            Look::In(ms(40))
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
                fps: 60,
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
                    60,
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
            idx: 0,
            frame: super::Frame::Key {
                blocks: None,
                starts,
            },
            target: Kbps::new(8000.0),
            capacity: Capacity::Unknown,
            captured_at: Instant::now(),
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
}
