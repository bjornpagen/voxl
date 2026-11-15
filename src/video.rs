//! Video pipeline: damage-driven capture -> I420 -> x264 -> clients, with ack-based flow control.

use crate::capture::{Band, Capturer};
use crate::convert::I420;
use crate::encoder::{Encoded, Encoder, EncoderConfig, Pictures, Planes, Quality};
use crate::hub::wait_any;
use crate::hub::{Hub, Inflight, Out};
use crate::path::{Bits, Capacity, Kbps};
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
    pub preset: String,
    pub threads: u32,
    pub convert_threads: usize,
    pub crf: f32,
    pub max_kbps: u32,
    pub paintover_crf: f32,
    pub paintover_burst: u32,
    pub paintover_delay_ms: u64,
    pub min_kbps: u32,
    pub coalesce: Duration,
    pub stripes: u32,
    pub adaptive_threads: bool,
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
    marker_ms: i64,
}

/// What a captured frame is for. Each kind carries what only it has.
enum Frame {
    /// A new viewer, or a decoder that lost its place: every stripe, from nothing. `still`:
    /// it went out in place of a sharpening pass, so it is coded at paint-over quality.
    Key { still: bool },
    /// What changed on screen, and which blocks (None: analyse every block in the band).
    Motion {
        band: Band,
        dirty: Option<Arc<Vec<u8>>>,
    },
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
            Frame::Key { still } => Coding::Key { still: *still },
            Frame::Motion { .. } => Coding::Motion,
            Frame::Sharpen { .. } | Frame::MotionAndSharpen { .. } => Coding::Sharpen,
        }
    }

    /// Whether it is coded at paint-over quality (a flag in each message's header).
    fn sharpens(&self) -> bool {
        match self {
            Frame::Key { still } => *still,
            Frame::Motion { .. } => false,
            Frame::Sharpen { .. } | Frame::MotionAndSharpen { .. } => true,
        }
    }

    /// The blocks x264 analyses (None: all of them).
    fn mb_dirty(&self) -> Option<Arc<Vec<u8>>> {
        match self {
            Frame::Key { .. } => None,
            Frame::Motion { dirty, .. } => dirty.clone(),
            Frame::Sharpen { blocks, .. } => Some(blocks.map.clone()),
            Frame::MotionAndSharpen { dirty, .. } => Some(dirty.clone()),
        }
    }

    /// The blocks it sharpens, cleared once it came out sharp.
    fn into_sharpened(self) -> Option<Sharpen> {
        match self {
            Frame::Key { .. } | Frame::Motion { .. } => None,
            Frame::Sharpen { blocks, .. } | Frame::MotionAndSharpen { blocks, .. } => Some(blocks),
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
    /// A keyframe. `pass`: it goes out in place of a due sharpening pass.
    Key { pass: Option<Pass> },
    /// Something moved in this band.
    Motion(Band),
    /// The screen is still and a pass is due: these pending blocks.
    Sharpen {
        pass: Pass,
        map: Vec<u8>,
        band: Band,
    },
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

/// One capture/encode setup for the current screen size.
struct Pipeline {
    cap: Capturer,
    job_tx: mpsc::SyncSender<Job>,
    ret_rx: mpsc::Receiver<Returned>,
    encode: std::thread::JoinHandle<()>,
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
    let mut p = build(&cfg, &hub)?;
    std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || loop {
            let Pipeline {
                mut cap,
                job_tx,
                ret_rx,
                encode,
            } = p;
            if let Err(e) = capture_loop(&mut cap, cfg.clone(), hub.clone(), job_tx, ret_rx) {
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
            let _ = encode.join();
            #[cfg(target_os = "macos")]
            let display_request = hub.display_req.lock().take();
            #[cfg(target_os = "macos")]
            if let Some(request) = display_request {
                // A resize for the old monitor must not be applied to the new one.
                hub.resize_req.lock().take();
                p = switch_display(request, &cfg, &hub, &mut screen).unwrap_or_else(|e| {
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
            p = match build(&cfg, &hub) {
                Ok(p) => p,
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
) -> Result<Pipeline, String> {
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
    let pipeline = attempt.or_else(|e| {
        log::warn!("display switch: {e}; restoring the previous display");
        open(previous)
    })?;
    *hub.input.lock() = Some(request.sender);
    Ok(pipeline)
}

fn build(cfg: &VideoConfig, hub: &Arc<Hub>) -> Result<Pipeline, String> {
    let cap = Capturer::new(cfg.display.as_deref())?;
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

    let (job_tx, job_rx) = mpsc::sync_channel::<Job>(1);
    let (ret_tx, ret_rx) = mpsc::channel::<Returned>();
    let layout = stripe_layout(h, cfg.stripes);
    let mut stripes = Vec::new();
    for &(y, sh) in &layout {
        let fixed = if cfg.adaptive_threads {
            1
        } else {
            (cfg.threads / layout.len() as u32).max(1)
        };
        let ecfg = EncoderConfig {
            pictures: Pictures {
                width: w,
                height: sh,
                fps: cfg.fps,
                preset: cfg.preset.clone(),
                threads: fixed,
            },
            crf: cfg.crf,
            max_kbps: cfg.max_kbps,
        };
        let max_threads = if cfg.adaptive_threads {
            (cfg.threads / layout.len() as u32).max(1)
        } else {
            1
        };
        stripes.push(Stripe {
            y: y as usize,
            h: sh as usize,
            enc: Encoder::new(&ecfg)?,
            cfg: ecfg,
            max_threads,
            applied_kbps: 0,
            applied_crf: -1.0,
        });
    }
    *hub.stripes.lock() = layout;
    let hub2 = hub.clone();
    let (crf, po_crf) = (cfg.crf, cfg.paintover_crf);
    let encode = std::thread::Builder::new()
        .name("encode".into())
        .spawn(move || encode_loop(stripes, job_rx, ret_tx, hub2, crf, po_crf))
        .unwrap();
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
    Ok(Pipeline {
        cap,
        job_tx,
        ret_rx,
        encode,
    })
}

fn flow_ok(hub: &Hub, interval_ms: f64) -> bool {
    for c in hub.video_clients() {
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
            c.inflight.lock().clear();
            c.needs_keyframe.store(true, Ordering::Relaxed);
            c.started.store(false, Ordering::Relaxed);
            continue;
        }
        if n >= max_n || age > max_age {
            return false;
        }
    }
    true
}

/// What one client's acks say about its path now.
#[derive(Clone, Copy)]
struct PathNow {
    /// Delay queued on the path (ms).
    queue_ms: f64,
    /// What the path can carry, once its frames have told.
    capacity: Option<Kbps>,
    /// Its oldest unacknowledged frame is overdue.
    overdue: bool,
}

/// Delay-based bitrate adaptation. The path's capacity is measured by the frames
/// themselves (path.rs), so the target follows it directly: up to just under it while
/// nothing queues, down below it when something does. Nothing has to press against the
/// cap for the cap to rise, and an idle screen doesn't lower it.
struct RateCtl {
    target: Kbps,
    max: Kbps,
    min: Kbps,
    /// When it last decided, lowered the target for a queue, and raised it.
    last_eval: Option<Instant>,
    last_dec: Option<Instant>,
    last_inc: Option<Instant>,
}

/// The bitrate cap of a path nothing is known about yet.
const START: Kbps = Kbps::new(8000.0);
/// How much of the measured capacity the stream may fill. The rest is headroom for the
/// path's own variation, so a steady stream sits below the queue instead of in it.
const CAPACITY_SHARE: f64 = 0.9;
/// A path with more than this queued is congested; with no more than `QUEUE_CLEAR_MS`,
/// clear. Between the two the target stays where it is.
const QUEUE_CONGESTED_MS: f64 = 30.0;
const QUEUE_CLEAR_MS: f64 = 15.0;
/// A frame unacknowledged this long past the round trip is overdue.
const OVERDUE_MS: f64 = 80.0;
/// The round trip assumed for a client whose pings haven't been answered yet.
const RTT_GUESS_MS: u32 = 40;

impl RateCtl {
    /// Carries on from `resume`, the last pipeline's target: one rebuilt for a new
    /// screen size streams over the same paths as before.
    fn new(max: Kbps, min: Kbps, resume: Option<Kbps>) -> Self {
        RateCtl {
            target: resume.unwrap_or(START).max(min.min(max)).min(max),
            max,
            min,
            last_eval: None,
            last_dec: None,
            last_inc: None,
        }
    }

    /// Steers by what the clients' paths say now; what they can carry together.
    fn update(&mut self, hub: &Hub, clients: &[Arc<crate::hub::Client>]) -> Capacity {
        let paths: Vec<PathNow> = clients
            .iter()
            .filter(|c| c.started.load(Ordering::Relaxed))
            .map(|c| {
                let (_, age) = c.backlog();
                let rtt = c.rtt_ms().unwrap_or(RTT_GUESS_MS) as f64;
                PathNow {
                    queue_ms: c.queue_ms(),
                    capacity: c.capacity(),
                    overdue: age as f64 > rtt + OVERDUE_MS,
                }
            })
            .collect();
        self.step(Instant::now(), &paths);
        hub.set_target(self.target);
        Capacity::of(paths.iter().map(|p| p.capacity))
    }

    /// One decision, at most every 100 ms, from what every watched path says now.
    fn step(&mut self, now: Instant, paths: &[PathNow]) {
        let waited = |since: Option<Instant>, time: Duration| {
            since.is_none_or(|t| now.saturating_duration_since(t) >= time)
        };
        if !waited(self.last_eval, Duration::from_millis(100)) {
            return;
        }
        self.last_eval = Some(now);
        // The most the target may be, and what it may rise to.
        let (ceiling, goal) = match Capacity::of(paths.iter().map(|p| p.capacity)) {
            Capacity::NoViewers => return,
            // An unknown path gets the cautious start, whatever was reached before it.
            Capacity::Unknown => (START.min(self.max), None),
            // A path that got slower is followed down at once, queue or no queue.
            Capacity::Known(c) => {
                let share = c.times(CAPACITY_SHARE);
                (share.max(self.min), Some(share.min(self.max)))
            }
        };
        self.target = self.target.min(ceiling);
        let congested = paths
            .iter()
            .any(|p| p.queue_ms > QUEUE_CONGESTED_MS || p.overdue);
        let clear = paths.iter().all(|p| p.queue_ms <= QUEUE_CLEAR_MS);
        if congested {
            if waited(self.last_dec, Duration::from_millis(300)) {
                self.target = self.target.times(0.85).max(self.min);
                self.last_dec = Some(now);
                log::debug!("rate: congested, target {:.0} kbps", self.target.get());
            }
        } else if clear
            && waited(self.last_dec, Duration::from_secs(1))
            && waited(self.last_inc, Duration::from_millis(200))
        {
            if let Some(goal) = goal.filter(|&goal| self.target < goal) {
                self.target = Kbps::new(self.target.get() * 1.25 + 100.0).min(goal);
                self.last_inc = Some(now);
            }
        }
    }
}

/// How long a frame may take to cross the path: what a burst after stillness may cost
/// in delay, once.
const LATENCY_BUDGET: Duration = Duration::from_millis(60);
/// A frame in debt may still spend this share of the target's part of one frame time:
/// the stream keeps moving while it pays back, and pays back because it is under one.
const DEBT_SHARE: f64 = 0.5;

/// The bits a frame may spend. Tokens come in at the target rate by the time that
/// really passed, and pile up while the screen is still to what the path carries in
/// `LATENCY_BUDGET`: so the first frames of a scroll after stillness go out sharp.
/// Everything sent is taken off, into debt when a frame took more than there was (a
/// keyframe's boost, several stripes moving at once), so over any stretch the stream
/// sends what the target carries in it, give or take one depth.
struct FrameBudget {
    tokens: Bits,
    at: Instant,
}

impl FrameBudget {
    fn new(now: Instant) -> Self {
        FrameBudget {
            tokens: Bits::ZERO,
            at: now,
        }
    }

    /// What a frame encoded at `now` may spend.
    fn allow(&mut self, now: Instant, target: Kbps, capacity: Capacity, fps: u32) -> Bits {
        let steady = target.over(Duration::from_secs_f64(1.0 / fps.max(1) as f64));
        let depth = match capacity {
            Capacity::Known(c) => c.over(LATENCY_BUDGET).max(steady.times(2.0)),
            // A path nothing is known about gets two frame times of the target at most.
            Capacity::Unknown | Capacity::NoViewers => steady.times(2.0),
        };
        let passed = now.saturating_duration_since(self.at);
        self.at = now;
        self.tokens = (self.tokens + target.over(passed)).min(depth);
        self.tokens.max(steady.times(DEBT_SHARE))
    }

    /// A frame of `bytes` was sent.
    fn spend(&mut self, bytes: usize) {
        self.tokens = self.tokens - Bits::of_bytes(bytes);
    }
}

/// Regional sharpening. Blocks encoded while they changed carry motion-level quality;
/// once a block has been still for a while it is re-encoded at paint-over quality,
/// even while other parts of the screen keep moving (a playing video would otherwise
/// keep the frame-level paint-over from ever running). A block stays pending until a
/// frame that re-encoded it came out at paint-over quality (`sharpened`): one the rate
/// cap squeezed leaves it for the next pass.
struct Refiner {
    blocks: Vec<Block>,
    last_refine: Instant,
    cols: usize,
}

/// A macroblock, as sharpening sees it.
#[derive(Clone, Copy)]
enum Block {
    Sharp,
    /// Sent at motion quality, last changed at `changed`. `tried`: sharpened since on a
    /// frame that also carried motion; once is enough while things move, the rest waits
    /// for the screen to go still.
    Pending {
        changed: Instant,
        tried: bool,
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
                *b = Block::Pending {
                    changed: now,
                    tried: false,
                };
            }
        }
    }
    fn mark_all(&mut self, now: Instant) {
        self.blocks.fill(Block::Pending {
            changed: now,
            tried: false,
        });
    }
    /// A frame captured at `at` coded `map`'s blocks at paint-over quality: they are
    /// sharp now, unless they changed again since.
    fn sharpened(&mut self, map: &[u8], at: Instant) {
        for (b, &m) in self.blocks.iter_mut().zip(map) {
            if let Block::Pending { changed, .. } = *b {
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
    /// Blocks due for sharpening on a frame that carries motion (1 = refine): pending,
    /// still for `REFINE_STILL` and not tried since they changed. At most every
    /// `REFINE_EVERY`.
    fn due(&mut self, now: Instant) -> Option<(Vec<u8>, Band)> {
        if now.saturating_duration_since(self.last_refine) < REFINE_EVERY {
            return None;
        }
        let map: Vec<u8> = self
            .blocks
            .iter_mut()
            .map(|b| match *b {
                Block::Pending {
                    changed,
                    tried: false,
                } if now.saturating_duration_since(changed) >= REFINE_STILL => {
                    *b = Block::Pending {
                        changed,
                        tried: true,
                    };
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

/// When the next sharpening pass may go. Passes come in bursts of `burst`, the first once
/// the screen has been still for `delay`, the next that long after a burst that left
/// blocks pending, until none are.
#[derive(Debug, PartialEq)]
struct Sharpening {
    /// Passes in a burst (`--paintover-burst`; 0 turns sharpening off).
    burst: u32,
    /// Stillness before a burst (`--paintover-delay-ms`).
    delay: Duration,
    state: Schedule,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Schedule {
    Idle,
    /// `left` passes of this burst remain; the next is due at `at`.
    Burst {
        left: u32,
        at: Instant,
    },
}

/// A sharpening pass that is due. Only `Sharpening::due` makes one, so going out is
/// always a pass of a burst.
#[derive(Debug, PartialEq)]
struct Pass {
    left: u32,
    at: Instant,
}

/// When a sharpening pass is due.
#[derive(Debug, PartialEq)]
enum Due {
    Now(Pass),
    In(Duration),
    Never,
}

impl Sharpening {
    fn new(burst: u32, delay: Duration) -> Self {
        Sharpening {
            burst,
            delay,
            state: Schedule::Idle,
        }
    }
    /// Something moved: a burst once the screen has been still for `delay`.
    fn on_motion(&mut self, now: Instant) {
        self.state = match self.burst {
            0 => Schedule::Idle,
            left => Schedule::Burst {
                left,
                at: now + self.delay,
            },
        };
    }
    /// A due pass went out. The last of a burst schedules the next burst `delay` later:
    /// if that one finds nothing pending, it ends there.
    fn on_pass(&mut self, pass: Pass, now: Instant) {
        if pass.left > 1 {
            self.state = Schedule::Burst {
                left: pass.left - 1,
                at: pass.at,
            };
        } else {
            self.on_motion(now);
        }
    }
    /// A keyframe went out, every block of it at keyframe quality. In place of a due
    /// pass it is that pass. Otherwise it is, to the schedule, what motion is: the
    /// Refiner counts every block as changed at `now`, so a burst follows once the
    /// screen has been still for `delay`, though nothing on it moved.
    fn on_key(&mut self, pass: Option<Pass>, now: Instant) {
        match pass {
            Some(pass) => self.on_pass(pass, now),
            None => self.on_motion(now),
        }
    }
    fn on_nothing_pending(&mut self) {
        self.state = Schedule::Idle;
    }
    fn due(&self, now: Instant) -> Due {
        match self.state {
            Schedule::Idle => Due::Never,
            Schedule::Burst { left, at } if now >= at => Due::Now(Pass { left, at }),
            Schedule::Burst { at, .. } => Due::In(at - now),
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

fn capture_loop(
    cap: &mut Capturer,
    cfg: VideoConfig,
    hub: Arc<Hub>,
    job_tx: mpsc::SyncSender<Job>,
    ret_rx: mpsc::Receiver<Returned>,
) -> Result<(), String> {
    let (w, h) = (cap.width as usize, cap.height as usize);
    let mut pool: Vec<Option<Box<I420>>> = vec![
        Some(Box::new(I420::new(w, h))),
        Some(Box::new(I420::new(w, h))),
    ];
    let mut pending: [Option<Band>; 2] = [Some(cap.full()), Some(cap.full())];
    let mut next_idx = 0usize;
    let interval = Duration::from_secs_f64(1.0 / cfg.fps as f64);
    // Allow slight early capture so source jitter doesn't alias into dropped frames.
    let min_interval = interval.mul_f64(0.85);
    let interval_ms = interval.as_secs_f64() * 1000.0;
    let mut last_capture = Instant::now() - interval;
    let paintover_delay = Duration::from_millis(cfg.paintover_delay_ms);
    let mut sharpening = Sharpening::new(cfg.paintover_burst, paintover_delay);
    let mut rc = RateCtl::new(
        Kbps::new(cfg.max_kbps as f64),
        Kbps::new(cfg.min_kbps as f64),
        hub.target(),
    );
    hub.set_target(rc.target);
    let mut had_clients = false;
    let mut full_next = true;
    let mut refiner = Refiner::new(
        w.div_ceil(16) * h.div_ceil(16),
        w.div_ceil(16),
        Instant::now(),
    );
    let debug_marker = std::env::var("VOXL_DEBUG_MARKER").is_ok();

    loop {
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
        hub.drain_wake();
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
        while let Ok(r) = ret_rx.try_recv() {
            pool[r.idx] = Some(r.buf);
            if let Some(s) = r.sharpened {
                refiner.sharpened(&s.map, s.at);
            }
        }
        let clients = hub.video_clients();
        if clients.is_empty() {
            // The target stands: a viewer that hid its tab comes back over the same
            // path, and one on a new path starts from what its own acks say.
            had_clients = false;
            wait_any(&[cap.fd(), hub.waker.waitable()], 500);
            continue;
        }
        let capacity = rc.update(&hub, &clients);
        let want_key = clients
            .iter()
            .any(|c| c.needs_keyframe.load(Ordering::Relaxed));
        if !had_clients {
            had_clients = true;
            // Our shm copy may be stale: re-grab everything once.
            cap.grab(cap.full())?;
            pending = [Some(cap.full()), Some(cap.full())];
            cap.damaged = true;
            full_next = true;
        }
        // One reading of the clock for every decision before the capture; the capture
        // itself is stamped `t0` below, after any wait for a buffer.
        let now = Instant::now();
        let due = sharpening.due(now);
        if !cap.damaged && !want_key {
            match &due {
                Due::Now(_) => {}
                Due::In(wait) => {
                    wait_any(
                        &[cap.fd(), hub.waker.waitable()],
                        wait.as_millis() as i32 + 1,
                    );
                    continue;
                }
                Due::Never => {
                    wait_any(&[cap.fd(), hub.waker.waitable()], 100);
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
        if pool[next_idx].is_none() {
            match ret_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(r) => {
                    pool[r.idx] = Some(r.buf);
                    if let Some(s) = r.sharpened {
                        refiner.sharpened(&s.map, s.at);
                    }
                }
                Err(_) => continue,
            }
            if pool[next_idx].is_none() {
                continue;
            }
        }

        let t0 = Instant::now();
        let origin = cap.damaged_at.unwrap_or(t0);
        hub.stats
            .damage_wait_us
            .fetch_add((t0 - origin).as_micros() as u64, Ordering::Relaxed);
        let mut band = None;
        let mut mb_dirty = (!full_next).then(|| cap.take_mb_map());
        if full_next {
            cap.take_mb_map();
            full_next = false;
        }
        match &mb_dirty {
            Some(m) => refiner.mark(m, t0),
            None => refiner.mark_all(t0),
        }
        if cap.damaged {
            band = cap.take_damage()?;
            if let Some(b) = band {
                cap.grab(b)?;
            }
        }
        let t1 = Instant::now();
        if debug_marker {
            log::info!(
                "cap dt {:.1} ms band {:?} grab {} us",
                last_capture.elapsed().as_secs_f64() * 1000.0,
                band,
                (t1 - t0).as_micros()
            );
        }
        let marker_ms = match cap.bgrx() {
            Some((px, stride)) if debug_marker => marker_probe(px, stride, "grab"),
            _ => -1,
        };
        // What this capture is sent as, decided once, before a buffer is taken.
        let plan = match (want_key, band, due) {
            (true, Some(_), Due::Now(_) | Due::In(_) | Due::Never) => Plan::Key { pass: None },
            (true, None, Due::Now(pass)) => Plan::Key { pass: Some(pass) },
            (true, None, Due::In(_) | Due::Never) => Plan::Key { pass: None },
            (false, Some(moved), Due::Now(_) | Due::In(_) | Due::Never) => Plan::Motion(moved),
            // Sharpen only what was sent at motion quality; nothing pending, nothing to send.
            (false, None, Due::Now(pass)) => match refiner.pending_map() {
                Some((map, band)) => Plan::Sharpen { pass, map, band },
                None => {
                    sharpening.on_nothing_pending();
                    continue;
                }
            },
            // The damage was empty after all, and no pass is due: nothing to send.
            (false, None, Due::In(_) | Due::Never) => continue,
        };
        let idx = next_idx;
        let mut buf = pool[idx].take().unwrap();
        let todo = match (pending[idx], band) {
            (p, Some(b)) => Some(union(p, b)),
            (p, None) => p,
        };
        if let Some(b) = band {
            pending[1 - idx] = Some(union(pending[1 - idx], b));
        }
        pending[idx] = None;
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
            Plan::Key { pass } => {
                let still = pass.is_some();
                if still {
                    hub.stats.paintovers.fetch_add(1, Ordering::Relaxed);
                }
                sharpening.on_key(pass, t0);
                for c in &clients {
                    c.needs_keyframe.store(false, Ordering::Relaxed);
                }
                // A keyframe is squeezed into the rate cap; sharpen all of it once still.
                refiner.mark_all(t0);
                Frame::Key { still }
            }
            Plan::Sharpen { pass, map, band } => {
                sharpening.on_pass(pass, t0);
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
                        let mut m = mb_dirty.take().unwrap_or_else(|| vec![1; rmap.len()]);
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
                        dirty: mb_dirty.take().map(Arc::new),
                    },
                }
            }
        };
        last_capture = t0;
        next_idx = 1 - idx;
        if job_tx
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
    cfg: EncoderConfig,
    /// Sliced threads to fan out to under sustained heavy motion (1 = never).
    max_threads: u32,
    applied_kbps: u32,
    applied_crf: f32,
}

const HDR: usize = 16;

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

/// x264's sliced threading costs ~3x the CPU of one thread on desktop content (and on
/// small updates is even slower), so a stripe encodes single-threaded and only fans out
/// while sustained heavy motion would otherwise blow the frame budget.
struct ThreadGovernor {
    threads: u32,
    ewma_ms: f64,
    heavy: u32,
    light_since: Option<Instant>,
    budget_ms: f64,
    /// One-thread encode time as a function of changed macroblocks, learned from
    /// single-threaded frames, to tell when one thread would fit again.
    fit: CostFit,
    /// Predicted one-thread time while threaded.
    predicted_ms: f64,
}

/// Exponentially weighted least squares fit of encode ms = a + b * changed MBs.
#[derive(Default)]
struct CostFit {
    w: f64,
    x: f64,
    y: f64,
    xx: f64,
    xy: f64,
}

impl CostFit {
    fn add(&mut self, x: f64, y: f64) {
        let k = 0.995;
        self.w = self.w * k + 1.0;
        self.x = self.x * k + x;
        self.y = self.y * k + y;
        self.xx = self.xx * k + x * x;
        self.xy = self.xy * k + x * y;
    }
    fn predict(&self, x: f64) -> Option<f64> {
        if self.w < 30.0 {
            return None;
        }
        let var = self.xx * self.w - self.x * self.x;
        if var <= f64::EPSILON * self.w * self.w {
            return Some(self.y / self.w);
        }
        let b = (self.xy * self.w - self.x * self.y) / var;
        let a = (self.y - b * self.x) / self.w;
        Some(a + b * x)
    }
}

impl ThreadGovernor {
    fn want(&mut self, enc_ms: f64, changed_mbs: f64, max: u32) -> Option<u32> {
        self.ewma_ms = self.ewma_ms * 0.8 + enc_ms * 0.2;
        if self.threads == 1 {
            self.fit.add(changed_mbs, enc_ms);
            // Before one thread runs out of frame budget: encode times vary from frame to
            // frame, and at ~90% of the budget on average enough frames miss it to drop
            // the frame rate.
            self.heavy = if self.ewma_ms > self.budget_ms * 0.65 {
                self.heavy + 1
            } else {
                0
            };
            if self.heavy >= 20 {
                self.predicted_ms = self.ewma_ms;
                return Some(max);
            }
            return None;
        }
        // Without enough one-thread history, assume one thread is no faster than these.
        let one = self.fit.predict(changed_mbs).unwrap_or(enc_ms);
        self.predicted_ms = self.predicted_ms * 0.8 + one * 0.2;
        if self.predicted_ms < self.budget_ms * 0.5 {
            // One thread would fit with room to spare (switching reopens the encoder,
            // which costs a keyframe, so not on a brief lull).
            let since = *self.light_since.get_or_insert_with(Instant::now);
            (since.elapsed() >= Duration::from_secs(2)).then_some(1)
        } else {
            self.light_since = None;
            None
        }
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

fn stripe_worker(
    i: usize,
    mut s: Stripe,
    rx: mpsc::Receiver<StripeJob>,
    done: mpsc::Sender<StripeResult>,
    crfs: Crfs,
    debug: bool,
) {
    let mut gov = ThreadGovernor {
        threads: 1,
        ewma_ms: 0.0,
        heavy: 0,
        light_since: None,
        budget_ms: 1000.0 / s.cfg.pictures.fps as f64,
        fit: CostFit::default(),
        predicted_ms: 0.0,
    };
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
            s.enc
                .set_rate_buf(asked, boost, boost / s.cfg.pictures.fps.max(1) * 8);
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
        out.resize(HDR, 0);
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
        if s.max_threads > 1 {
            if let Some(t) = gov.want(
                te.elapsed().as_secs_f64() * 1000.0,
                changed as f64,
                s.max_threads,
            ) {
                let mut cfg = s.cfg.clone();
                cfg.pictures.threads = t;
                match Encoder::new(&cfg) {
                    Ok(e) => {
                        log::info!(
                            "stripe {i}: encoder now {t} thread(s) (encode {:.1} ms)",
                            gov.ewma_ms
                        );
                        // A fresh encoder opens with an IDR on its next frame, and
                        // its rate control starts over.
                        s.enc = e;
                        motion = MotionQuality::default();
                        s.applied_kbps = 0;
                        gov.threads = t;
                        gov.heavy = 0;
                        gov.light_since = None;
                    }
                    Err(e) => log::error!("encoder reopen failed: {e}"),
                }
            }
        }
        let result = match r {
            Ok(e) => {
                let size = matches!(job.coding, Coding::Motion).then(|| MotionSize {
                    bits: (out.len() - HDR) * 8,
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

fn encode_loop(
    stripes_in: Vec<Stripe>,
    rx: mpsc::Receiver<Job>,
    ret: mpsc::Sender<Returned>,
    hub: Arc<Hub>,
    crf: f32,
    po_crf: f32,
) {
    let epoch = Instant::now();
    let mut frame_id: u32 = 0;
    let n = stripes_in.len();
    let fps = stripes_in.first().map_or(60, |s| s.cfg.pictures.fps);
    let crfs = Crfs {
        configured: crf,
        paintover: po_crf,
    };
    let mut budget = FrameBudget::new(epoch);
    let debug_stripes = std::env::var("VOXL_DEBUG_STRIPES").is_ok();
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
                .spawn(move || stripe_worker(i, s, rx, done, crfs, debug_stripes))
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
        let _ = ret.send(Returned {
            idx: job.idx,
            buf: job.buf,
            sharpened: job.frame.into_sharpened().filter(|_| sharp),
        });
        let parts: Vec<(usize, bool, Vec<u8>)> = results
            .into_iter()
            .enumerate()
            .filter_map(|(i, r)| match r {
                StripeOut::Sent { key, bytes, .. } => {
                    (bytes.len() > HDR).then_some((i, key, bytes))
                }
                StripeOut::Failed => None,
            })
            .collect();
        if parts.is_empty() {
            continue;
        }
        let enc_us = t0.elapsed().as_micros() as u64;
        let all_key = parts.len() == n && parts.iter().all(|p| p.1);
        let total: usize = parts.iter().map(|p| p.2.len()).sum();
        hub.stats.encode_us.fetch_add(enc_us, Ordering::Relaxed);
        hub.stats.encoded.fetch_add(1, Ordering::Relaxed);
        hub.stats.bytes.fetch_add(total as u64, Ordering::Relaxed);
        budget.spend(total);
        if all_key {
            hub.stats.keyframes.fetch_add(1, Ordering::Relaxed);
        }
        frame_id = frame_id.wrapping_add(1);
        // Server-side pipeline latency (capture -> sent) in 1/10 ms, for client stats.
        let mut pipe = (job.captured_at.elapsed().as_micros() / 100).min(u32::MAX as u128) as u32;
        if job.marker_ms >= 0 {
            // Debug: wall-clock send time instead of pipeline latency.
            pipe = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u32;
        }
        let nparts = parts.len() as u8;
        let msgs: Vec<Bytes> = parts
            .into_iter()
            .map(|(i, key, mut out)| {
                // type, flags, frame id, pipe, stripe, stripe count, parts in frame, reserved, y
                out[0] = 0x01;
                out[1] = (key as u8) | if sharpens { 2 } else { 0 };
                out[2..6].copy_from_slice(&frame_id.to_be_bytes());
                out[6..10].copy_from_slice(&pipe.to_be_bytes());
                out[10] = i as u8;
                out[11] = n as u8;
                out[12] = nparts;
                out[13] = 0;
                out[14..16].copy_from_slice(&(stripes[i].0 as u16).to_be_bytes());
                Bytes::from(out)
            })
            .collect();
        let now = Instant::now();
        for c in hub.video_clients() {
            if !c.started.load(Ordering::Relaxed) {
                if !all_key {
                    continue;
                }
                c.started.store(true, Ordering::Relaxed);
                c.inflight.lock().clear();
            }
            {
                let mut q = c.inflight.lock();
                if q.len() > 600 {
                    log::warn!("client {} hopelessly behind; dropping", c.id);
                    drop(q);
                    c.send(Out::Text("{\"t\":\"close\"}".into()));
                    continue;
                }
                let alone = q.is_empty();
                q.push_back(Inflight {
                    id: frame_id,
                    sent: now,
                    bytes: total,
                    alone,
                });
            }
            for m in &msgs {
                c.send(Out::Bin(m.clone()));
            }
        }
        hub.stats.send_lat_us.fetch_add(
            job.captured_at.elapsed().as_micros() as u64,
            Ordering::Relaxed,
        );
        if job.marker_ms >= 0 {
            log::info!(
                "marker sent age {} ms (encode {} us)",
                marker_age(job.marker_ms),
                enc_us
            );
        }
    }
}

/// Debug: decode the bench loadgen marker at (300,150) and log its age.
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

fn marker_probe(buf: &[u8], stride: usize, what: &str) -> i64 {
    let mut v: u64 = 0;
    let (mx, my) = std::env::var("VOXL_DEBUG_MARKER")
        .ok()
        .and_then(|s| {
            s.split_once(',')
                .and_then(|(a, b)| Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?)))
        })
        .unwrap_or((300, 150));
    for b in 0..40 {
        let x = mx + (b % 20) * 24 + 12;
        let y = my + (b / 20) * 24 + 12;
        let i = y * stride + x * 4;
        if buf[i + 2] > 128 {
            v |= 1 << b;
        }
    }
    let ms = (v & 0xffffff) as i64;
    let now = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64)
        & 0xffffff;
    log::info!("marker {} frame {} age {} ms", what, v >> 24, now - ms);
    ms
}

fn marker_age(ms: i64) -> i64 {
    let now = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64)
        & 0xffffff;
    now - ms
}

#[cfg(test)]
mod tests {
    use super::*;

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
    /// (0: none), with or without MotionQuality.
    struct Run {
        enc: Encoder,
        motion: MotionQuality,
        correct: bool,
        crf: f32,
        max_kbps: u32,
        applied: f32,
        y: Vec<u8>,
        n: usize,
    }

    impl Run {
        fn new(correct: bool, crf: f32, max_kbps: u32) -> Run {
            Run {
                enc: Encoder::new(&EncoderConfig {
                    pictures: Pictures {
                        width: W as u32,
                        height: H as u32,
                        fps: 60,
                        preset: "ultrafast".into(),
                        threads: 1,
                    },
                    crf,
                    max_kbps,
                })
                .unwrap(),
                motion: MotionQuality::default(),
                correct,
                crf,
                max_kbps,
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
                self.enc.set_rate(c, self.max_kbps);
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
        let plain = mean(Run::new(false, crf, 0).motion(Scene::Drag).iter().map(qp));
        let corrected = mean(Run::new(true, crf, 0).motion(Scene::Drag).iter().map(qp));
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
        let plain = Run::new(false, crf, 0).motion(Scene::Drag);
        let complexity = mean(plain.iter().map(|f| qp(f) - f.crf));
        let capped = Run::new(true, crf, 20_000).motion(Scene::Drag);
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
        let plain = mean(Run::new(false, crf, 0).motion(Scene::Noise).iter().map(qp));
        let corrected = Run::new(true, crf, 0).motion(Scene::Noise);
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

    /// On frames that carry motion, a block still for `REFINE_STILL` is tried once per
    /// change, at most every `REFINE_EVERY`.
    #[test]
    fn a_moving_screen_tries_each_still_block_once() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut r = Refiner::new(2, 2, t0);
        r.mark(&[1, 1], ms(0));
        assert!(r.due(ms(299)).is_none(), "too soon after the change");
        assert_eq!(r.due(ms(300)).map(|d| d.0), Some(vec![1, 1]));
        assert!(r.due(ms(600)).is_none(), "already tried");
        r.mark(&[0, 1], ms(700));
        assert!(r.due(ms(900)).is_none(), "block 1 still for 200 ms only");
        assert_eq!(r.due(ms(1000)).map(|d| d.0), Some(vec![0, 1]));
        // Tried, not sharp: both stay pending for the still-screen passes.
        assert_eq!(r.pending_map().map(|p| p.0), Some(vec![1, 1]));
    }

    /// The sharpening schedule against the counter and instant it replaces, over a long
    /// run of motion, passes and empty pending maps at stated times: the same pass is
    /// due at the same moment in both, and the three rules hold.
    #[test]
    fn sharpening_comes_in_bursts_until_nothing_is_pending() {
        struct Model {
            left: u32,
            at: Instant,
        }
        let (burst, delay) = (3, Duration::from_millis(150));
        let t0 = Instant::now();
        let mut s = Sharpening::new(burst, delay);
        let mut m = Model { left: 0, at: t0 };
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
            let model_due = m.left > 0 && now >= m.at;
            assert_eq!(
                matches!(s.due(now), Due::Now(_)),
                model_due,
                "due at {:?}",
                now - t0
            );
            match next(10) {
                0..=2 => {
                    s.on_motion(now);
                    (m.left, m.at) = (burst, now + delay);
                    // Motion always restarts the wait.
                    assert_eq!(s.due(now), Due::In(delay));
                }
                3..=7 if model_due => {
                    let Due::Now(pass) = s.due(now) else {
                        panic!("the model says a pass is due at {:?}", now - t0)
                    };
                    s.on_pass(pass, now);
                    m.left -= 1;
                    if m.left == 0 {
                        (m.left, m.at) = (burst, now + delay);
                        // A burst that ends is followed by another one delay later.
                        assert_eq!(s.due(now), Due::In(delay));
                    }
                }
                8 if model_due => {
                    s.on_nothing_pending();
                    m.left = 0;
                    // Nothing pending always ends in Idle.
                    assert_eq!(s.due(now), Due::Never);
                }
                _ => {}
            }
        }
    }

    /// A viewer joining a still screen gets a keyframe, every block at keyframe quality,
    /// with nothing on screen moving: a burst sharpens it the paint-over delay after the
    /// keyframe, as after motion. A keyframe in place of a due pass is that pass.
    #[test]
    fn a_keyframe_on_a_still_screen_is_sharpened() {
        let (burst, delay) = (3, Duration::from_millis(150));
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut s = Sharpening::new(burst, delay);
        assert_eq!(s.due(ms(0)), Due::Never, "a still screen, all of it sharp");
        s.on_key(None, ms(1000));
        assert_eq!(s.due(ms(1149)), Due::In(Duration::from_millis(1)));
        let Due::Now(pass) = s.due(ms(1150)) else {
            panic!("no pass due at the keyframe plus the delay")
        };
        assert_eq!(
            pass,
            Pass {
                left: burst,
                at: ms(1150)
            }
        );
        // A keyframe goes out in place of that pass: the burst goes on at once.
        s.on_key(Some(pass), ms(1160));
        assert_eq!(
            s.due(ms(1160)),
            Due::Now(Pass {
                left: burst - 1,
                at: ms(1150)
            })
        );
    }

    /// Once the screen goes still the paint-overs show x264 forgetting the drag, so the
    /// next motion frame isn't handed the drag's correction.
    #[test]
    fn paintovers_wind_the_correction_down() {
        let crf = 24.0;
        let mut run = Run::new(true, crf, 0);
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
}

/// The rate controller, the frame budget and the path estimate together, against a
/// link that is the oracle: a rate, a round trip, and a queue that serves in order.
#[cfg(test)]
mod rate_tests {
    use super::*;
    use crate::path::PathEst;
    use std::collections::VecDeque;

    const FPS: u32 = 60;

    struct Rng(u64);
    impl Rng {
        fn range(&mut self, lo: u64, hi: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            lo + self.0 % (hi - lo)
        }
    }

    /// One viewer on one link, driven a frame time at a tick.
    struct Sim {
        now: Instant,
        /// The link's rate (bit/s) and round trip.
        rate: f64,
        rtt: Duration,
        /// When the link is free again.
        free: Instant,
        /// (acked at, sent at, bytes, sent with nothing ahead of it) of frames on their way.
        flying: VecDeque<(Instant, Instant, usize, bool)>,
        est: PathEst,
        rc: RateCtl,
        budget: FrameBudget,
        /// Worst time a frame waited behind others, since `reset`.
        worst_wait: Duration,
        /// Smallest share of what it wanted that a frame got, since `reset`.
        worst_share: f64,
        sent_bytes: usize,
    }

    impl Sim {
        fn new(rate: f64, rtt_ms: u64, max_kbps: f64) -> Self {
            let now = Instant::now();
            Sim {
                now,
                rate,
                rtt: Duration::from_millis(rtt_ms),
                free: now,
                flying: VecDeque::new(),
                est: PathEst::new(now),
                rc: RateCtl::new(Kbps::new(max_kbps), Kbps::new(2000.0), None),
                budget: FrameBudget::new(now),
                worst_wait: Duration::ZERO,
                worst_share: 1.0,
                sent_bytes: 0,
            }
        }

        fn reset(&mut self) {
            self.worst_wait = Duration::ZERO;
            self.worst_share = 1.0;
            self.sent_bytes = 0;
        }

        /// One frame time passes; the screen wants to send `want` bytes (0: it is still).
        fn tick(&mut self, want: usize) {
            self.now += Duration::from_secs_f64(1.0 / FPS as f64);
            while self.flying.front().is_some_and(|f| f.0 <= self.now) {
                let (acked, sent, bytes, alone) = self.flying.pop_front().unwrap();
                self.est.on_ack(acked, sent, bytes, alone);
            }
            let oldest = self
                .flying
                .front()
                .map_or(Duration::ZERO, |f| self.now.duration_since(f.1));
            let path = PathNow {
                queue_ms: self.est.queue_ms(self.now, self.flying.is_empty()),
                capacity: self.est.capacity(),
                overdue: oldest > self.rtt + Duration::from_millis(80),
            };
            self.rc.step(self.now, &[path]);
            // The capture loop's flow control: no new frame while the oldest is this late.
            if want == 0 || oldest > self.rtt + Duration::from_millis(120) {
                return;
            }
            // x264 under the rate cap: a frame holds at most what the budget allows.
            let room = self
                .budget
                .allow(self.now, self.rc.target, Capacity::of([path.capacity]), FPS)
                .bytes() as usize;
            let bytes = want.min(room).max(200);
            self.budget.spend(bytes);
            let start = self.free.max(self.now);
            self.worst_wait = self.worst_wait.max(start - self.now);
            self.worst_share = self.worst_share.min(bytes as f64 / want as f64);
            self.free = start + Duration::from_secs_f64(bytes as f64 * 8.0 / self.rate);
            let alone = self.flying.is_empty();
            self.flying
                .push_back((self.free + self.rtt, self.now, bytes, alone));
            self.sent_bytes += bytes;
        }

        fn run(&mut self, ticks: u32, mut want: impl FnMut(u32) -> usize) {
            for i in 0..ticks {
                self.tick(want(i));
            }
        }
    }

    /// A desktop at rest: a cursor and a clock, a few hundred bytes now and then.
    fn rest(i: u32) -> usize {
        if i.is_multiple_of(7) {
            600
        } else {
            0
        }
    }

    #[test]
    fn a_scroll_after_stillness_is_sharp_on_a_link_with_room() {
        // The case this exists for: a desktop that is mostly still, on a link faster
        // than the scroll needs (60 KB a frame at 60 fps is 29 Mbit/s), scrolled now and
        // then. Every scroll frame gets what it asks for, from the first one, and
        // nothing queues.
        let mut rng = Rng(0x2545F4914F6CDD1D);
        for _ in 0..40 {
            let rate = rng.range(36_000_000, 80_000_000) as f64;
            let mut sim = Sim::new(rate, rng.range(20, 150), 30_000.0);
            // Still, then one scroll that teaches the path, then stillness again.
            sim.run(120, rest);
            sim.run(40, |_| 45_000);
            sim.run(60 * rng.range(1, 20) as u32, rest);
            sim.reset();
            let want = rng.range(30_000, 60_000) as usize;
            sim.run(45, |_| want);
            assert!(
                sim.worst_share >= 0.999,
                "rate {rate}: a scroll frame got {:.0}% of what it wanted",
                sim.worst_share * 100.0
            );
            assert!(
                sim.worst_wait <= LATENCY_BUDGET,
                "rate {rate}: a frame waited {:?}",
                sim.worst_wait
            );
        }
    }

    #[test]
    fn the_first_frames_of_a_scroll_are_whole_on_a_link_the_scroll_overfills() {
        // A slower link can't carry the whole scroll at full quality, but what piled up
        // during the stillness pays for its first frame in full, within the latency
        // budget; the frames after it get what the link sustains.
        let mut rng = Rng(0x94D049BB133111EB);
        for _ in 0..40 {
            let rate = rng.range(8_000_000, 20_000_000) as f64;
            let mut sim = Sim::new(rate, rng.range(20, 150), 30_000.0);
            sim.run(120, rest);
            sim.run(40, |_| 45_000);
            sim.run(60 * rng.range(2, 20) as u32, rest);
            sim.reset();
            sim.run(1, |_| 45_000);
            assert!(
                sim.worst_share >= 0.999,
                "rate {rate}: the first scroll frame got {:.0}% of what it wanted",
                sim.worst_share * 100.0
            );
            sim.run(120, |_| 45_000);
            assert!(
                sim.worst_wait <= LATENCY_BUDGET + Duration::from_millis(40),
                "rate {rate}: a frame waited {:?}",
                sim.worst_wait
            );
        }
    }

    #[test]
    fn stillness_does_not_lower_the_target() {
        // Minutes of a still screen with a big frame now and then: the path is as fast
        // as it was, and the target says so.
        let mut sim = Sim::new(40_000_000.0, 100, 30_000.0);
        sim.run(120, rest);
        sim.run(40, |_| 45_000);
        sim.run(600, rest);
        let learned = sim.rc.target.get();
        assert!(
            learned >= 0.85 * 30_000.0,
            "target {learned} after one scroll"
        );
        for _ in 0..20 {
            sim.run(600, rest);
            sim.run(3, |_| 80_000);
        }
        assert!(
            sim.rc.target.get() >= learned,
            "target fell from {learned} to {}",
            sim.rc.target.get()
        );
    }

    #[test]
    fn sustained_motion_settles_under_a_slow_link() {
        // Video-like motion that always wants more than the link carries: the stream
        // settles under the link's rate, uses most of it, and the queue stays short.
        let mut rng = Rng(0x9E6C63D0876A9A47);
        for _ in 0..40 {
            let rate = rng.range(2_500_000, 12_000_000) as f64;
            let rtt = rng.range(20, 150);
            let mut sim = Sim::new(rate, rtt, 30_000.0);
            sim.run(120, rest);
            sim.run(60 * 20, |_| 120_000);
            sim.reset();
            sim.run(60 * 20, |_| 120_000);
            let sent = sim.sent_bytes as f64 * 8.0 / 20.0;
            assert!(
                sent <= rate * 1.0 && sent >= rate * 0.6,
                "rate {rate}, rtt {rtt}: sent {sent} bit/s"
            );
            assert!(
                sim.worst_wait <= Duration::from_millis(250),
                "rate {rate}, rtt {rtt}: a frame waited {:?}",
                sim.worst_wait
            );
        }
    }

    #[test]
    fn a_link_that_slows_is_followed_down() {
        // The path changes under a running stream (another network, a busy link).
        let mut sim = Sim::new(40_000_000.0, 60, 30_000.0);
        sim.run(120, rest);
        sim.run(60 * 5, |_| 60_000);
        assert!(
            sim.rc.target.get() > 20_000.0,
            "target {} on 40 Mbit/s",
            sim.rc.target.get()
        );
        sim.rate = 4_000_000.0;
        sim.run(60 * 15, |_| 60_000);
        sim.reset();
        sim.run(60 * 10, |_| 60_000);
        let sent = sim.sent_bytes as f64 * 8.0 / 10.0;
        assert!(sent <= 4_000_000.0, "sent {sent} bit/s on a 4 Mbit/s link");
        assert!(
            sim.worst_wait <= Duration::from_millis(250),
            "a frame waited {:?} after the link slowed",
            sim.worst_wait
        );
    }

    #[test]
    fn a_frame_gets_its_steady_share_and_no_more_than_the_latency_budget() {
        // Whatever the tokens, a frame may spend at least its share in debt of the
        // target's part of one frame time, and at most what the path carries in the
        // latency budget.
        let mut rng = Rng(0xBF58476D1CE4E5B9);
        let t0 = Instant::now();
        let frame_time = Duration::from_secs_f64(1.0 / FPS as f64);
        for _ in 0..500 {
            let mut b = FrameBudget::new(t0);
            let target = Kbps::new(rng.range(500, 30_000) as f64);
            let capacity = match rng.range(0, 3) {
                0 => Capacity::Unknown,
                _ => Capacity::Known(Kbps::new(rng.range(500, 100_000) as f64)),
            };
            let mut now = t0;
            for _ in 0..200 {
                now += Duration::from_micros(rng.range(100, 3_000_000));
                let may = b.allow(now, target, capacity, FPS);
                let steady = target.over(frame_time);
                let least = steady.times(DEBT_SHARE);
                assert!(may >= least, "{may:?} under the share in debt {least:?}");
                let most = match capacity {
                    Capacity::Known(c) => c.over(LATENCY_BUDGET).max(steady.times(2.0)),
                    Capacity::Unknown | Capacity::NoViewers => steady.times(2.0),
                };
                assert!(may <= most, "{may:?} over the latency budget {most:?}");
                b.spend(rng.range(0, 400_000) as usize);
            }
        }
    }

    /// Over any stretch, what frames spend is at most what the target carries in it,
    /// plus the depth, plus one frame's floor: at the capture loop's own pace (0.85 of
    /// a frame time apart, its `min_interval`), and with two stripes that each take
    /// their share of twice the frame's budget. What a frame takes beyond the tokens is
    /// a debt, and the frames after it pay it.
    #[test]
    fn a_long_run_of_frames_averages_the_target() {
        let t0 = Instant::now();
        let target = Kbps::new(9_000.0);
        let capacity = Capacity::Known(Kbps::new(10_000.0));
        let frame_time = Duration::from_secs_f64(1.0 / FPS as f64);
        let steady = target.over(frame_time);
        let depth = Kbps::new(10_000.0)
            .over(LATENCY_BUDGET)
            .max(steady.times(2.0));
        let mut over = vec![];
        // (frame times between frames, what a frame spends over what it may)
        for (pace, spends) in [(0.85, 1.0), (1.0, 2.0)] {
            let mut b = FrameBudget::new(t0);
            let (mut now, mut sent) = (t0, Bits::ZERO);
            for _ in 0..600 {
                now += frame_time.mul_f64(pace);
                let may = b.allow(now, target, capacity, FPS);
                let bytes = (may.bytes() * spends) as usize;
                b.spend(bytes);
                sent = sent + Bits::of_bytes(bytes);
            }
            let most = target.over(now - t0) + depth + steady;
            if sent > most {
                over.push((pace, spends, sent.bytes() / most.bytes()));
            }
        }
        assert!(
            over.is_empty(),
            "(pace, spends, sent over the most): {over:?}"
        );
    }
}
