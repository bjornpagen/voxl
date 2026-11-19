//! Video pipeline: damage-driven capture -> I420 -> AV1 -> each session's lane, a capture
//! held while a session's link says too much is on the way, or a frame late.
//!
//! One stream codes the screen for every viewer, from its shape's first watcher until the
//! screen changes shape. It starts with a keyframe for the viewers it starts, and codes
//! every later frame from the frames before it, which every viewer's lane carried in order;
//! a keyframe is coded only for a viewer starting (`hub::Starts`). While nobody watches,
//! nothing is captured and the encoder waits as it was: a viewer that comes back holding
//! the stream's newest frame goes on with the next delta (`hub::Holds`). Kept so, libaom
//! holds 202 MB resident for a 3024x1898 stream on 8 threads and 289 MB for 3840x2400 on
//! 16 tile columns, after a minute of scrolling (libaom 3.15.1 configured as `av1::Encoder`
//! configures it, on an M2 Max, 2026-11-19), against the daemon unit's 4 GB memory cap.
//! Every frame is coded whole, at one quality: a slow or stalled path lowers the frame
//! rate, through the gate (`session::link`), and the screen as it then is goes next.

use crate::av1;
#[cfg(target_os = "linux")]
use crate::capture::Capturer;
use crate::convert::I420;
use crate::env::{Debugging, Marker};
#[cfg(target_os = "linux")]
use crate::hub::wait_any;
#[cfg(target_os = "linux")]
use crate::hub::Waitable;
use crate::hub::{Hub, Starts};
use crate::hub::{Seen, Viewer};
#[cfg(target_os = "linux")]
use crate::session::link::Gate;
use crate::wire::{Coding, Header, Pipe};
use bytes::{BufMut, BytesMut};
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
#[derive(Clone)]
pub struct VideoConfig {
    /// The X display.
    pub display: String,
    /// The cores the encoder may use: `av1::Stream::new` derives its threads from them.
    pub cores: std::num::NonZeroUsize,
    /// The threads that convert a capture to I420.
    pub convert_threads: usize,
    /// The debugging variables (`VOXL_*`), read when voxl started.
    pub debug: Debugging,
}

/// The most frames a second. Provisional: the refresh of the common display, which is
/// the most a viewer's screen shows of most desktops; what would measure it is the
/// bench's glass-to-glass frame rate and CPU at 60 against 120 on a 120 Hz viewer.
pub const FRAMERATE: u32 = 60;

#[cfg(target_os = "linux")]
/// The threads converting a capture to I420: libyuv's row converters each take a band of
/// rows. Provisional: upstream's choice; what would measure it is the bench's convert
/// time per frame at 1, 2, 4 and 8.
const CONVERT_THREADS: usize = 4;

#[cfg(target_os = "linux")]
impl VideoConfig {
    /// The daemon's: every value above, and the `cores` the process may run on.
    pub fn new(display: String, debug: Debugging, cores: std::num::NonZeroUsize) -> Self {
        VideoConfig {
            display,
            cores,
            convert_threads: CONVERT_THREADS,
            debug,
        }
    }
}

/// What a capture is sent as.
enum Plan<S> {
    /// A keyframe, for the viewers it starts: the only way one is made.
    Key(S),
    /// What changed on screen.
    Motion,
}

/// A capture, as the encode loop is given it.
struct Job {
    /// The picture: the encode loop's while it codes it, and the capture loop's again once
    /// it comes back.
    buf: Box<I420>,
    frame: Plan<Starts>,
    /// When the screen first changed for it: its timestamp, and the start of its pipeline
    /// time (`Pipe`).
    damage: Instant,
    /// What the bench loadgen's marker said when the frame was captured, while
    /// `VOXL_DEBUG_MARKER` is set (see `marker_reading`).
    marker_ms: Option<u32>,
}

/// Rows `y0..y1` of the screen.
#[derive(Clone, Copy, Debug)]
pub struct Band {
    pub y0: u32,
    pub y1: u32,
}

/// What the viewers watching are sent: the encoder, on its thread. There is one from the
/// first time somebody watches a stream of this shape, and it starts with a keyframe. One
/// picture goes round between the capture loop and the encoder (the loop holds where it
/// is, `Buffer`), since a capture goes only to an idle encoder (`step`): a second, filled
/// while the encoder codes the first, only queued a frame behind it, and converted every
/// band twice, once into each.
struct Stream {
    /// Dropped first (fields drop in order), which ends the encode loop once it has sent
    /// what it was given.
    job_tx: mpsc::SyncSender<Job>,
    /// The picture back from the encoder, once its frame is on its viewers' lanes.
    ret_rx: mpsc::Receiver<Box<I420>>,
    /// Dropped last: waits until the encode loop has stopped.
    _encode: Stopped,
}

/// A thread that is waited for when this is dropped. The thread holds the sending side
/// of a channel nothing is sent on, which closes when the thread's work returns.
struct Stopped(mpsc::Receiver<std::convert::Infallible>);

impl Stopped {
    /// `work` on a thread named `name`; a thread that cannot be started is the error.
    fn spawn(name: &str, work: impl FnOnce() + Send + 'static) -> Result<Stopped, String> {
        let (running, stopped) = mpsc::channel();
        std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let _running = running;
                work()
            })
            .map_err(|e| format!("no {name} thread: {e}"))?;
        Ok(Stopped(stopped))
    }
}

impl Drop for Stopped {
    fn drop(&mut self) {
        // Nothing is sent on it: it ends, empty, when the thread's work has returned.
        if let Ok(never) = self.0.recv() {
            match never {}
        }
    }
}

impl Stream {
    /// The stream of `coded`, its encoder on its thread, which codes `first` (a keyframe of
    /// the whole screen) first.
    fn start(
        hub: &Arc<Hub>,
        coded: av1::Stream,
        first: Job,
        debug: Debugging,
    ) -> Result<Self, String> {
        log::info!("streaming");
        let (job_tx, job_rx) = mpsc::sync_channel::<Job>(1);
        // One picture goes round, so no more than one comes back at once.
        let (ret_tx, ret_rx) = mpsc::sync_channel::<Box<I420>>(1);
        let hub = hub.clone();
        let encode = encoder_thread(hub.clone(), move || {
            encode_loop(coded, first, job_rx, ret_tx, hub, debug)
        })?;
        Ok(Stream {
            job_tx,
            ret_rx,
            _encode: encode,
        })
    }
}

#[cfg(target_os = "linux")]
/// Runs the pipeline. The capture loop gives a client's size request to `screen`; only if
/// the screen's size really changes does the loop end: the encoder drains, the pipeline is
/// rebuilt at the new size, and clients get a keyframe of it.
pub fn spawn(
    cfg: VideoConfig,
    hub: Arc<Hub>,
    mut screen: crate::screen::Screen,
) -> Result<(), String> {
    let (mut cap, mut coded) = build(&cfg, &hub)?;
    std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || loop {
            let looped = capture_loop(&mut cap, coded, cfg.clone(), hub.clone(), &mut screen);
            if let Err(e) = looped {
                match cap.geometry_changed() {
                    Ok(true) => log::info!("capture geometry changed: {e}"),
                    Ok(false) => {
                        log::error!("capture loop ended: {e}");
                        crate::logging::exit(1);
                    }
                    Err(asked) => {
                        log::error!("capture loop ended: {e}; its geometry: {asked}");
                        crate::logging::exit(1);
                    }
                }
            }
            drop(cap);
            (cap, coded) = match build(&cfg, &hub) {
                Ok(built) => built,
                Err(e) => {
                    log::error!("video restart failed: {e}");
                    crate::logging::exit(1);
                }
            };
        })
        .map_err(|e| format!("no capture thread: {e}"))?;
    Ok(())
}

#[cfg(target_os = "linux")]
/// The capture of the screen as it is now, and how its stream is coded, for the viewers.
fn build(cfg: &VideoConfig, hub: &Arc<Hub>) -> Result<(Capturer, av1::Stream), String> {
    let cap = Capturer::new(&cfg.display, cfg.debug, cfg.convert_threads)?;
    let (w, h) = (cap.width, cap.height);
    let shape = av1::Shape {
        width: w,
        height: h,
    };
    let coded =
        av1::Stream::new(shape, cfg.cores).map_err(|e| format!("no AV1 level holds {:?}", e.0))?;
    log::info!(
        "capturing {w}x{h} @ {FRAMERATE} fps as {}, {} threads",
        coded.codec(),
        coded.threads
    );
    // Clients already connected start again on a keyframe of the new size; no-op at startup.
    hub.restart_clients();
    Ok((cap, coded))
}

/// Whether the viewers' links let a capture start: open while every one's does.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Gated {
    Open,
    /// Held by a viewer whose link is full or late: only an update told taken, a lane
    /// ending, or a viewer coming or going can open the gate, and each of those wakes the
    /// hub.
    Held,
}

impl Gated {
    #[cfg(target_os = "linux")]
    /// What the look at the viewers saw of their links: held by any that is not open. A
    /// recording holds nothing.
    fn of(seen: &[(Arc<crate::hub::Client>, Seen)]) -> Gated {
        match seen.iter().all(|(_, v)| v.gate == Gate::Open) {
            true => Gated::Open,
            false => Gated::Held,
        }
    }
}

/// Captures may come this share of the frame interval apart, so that a source's jitter
/// does not alias into dropped frames. Upstream's guess, unmeasured. It caps the capture
/// rate at the frame rate over it (70.6 a second at 60): at 1, a frame that lands a
/// little early waits a whole interval; at 0.7, up to 86 captures a second.
const MIN_INTERVAL_SHARE: f64 = 0.85;

/// What the capture loop does next. A frame is coded only for a `Start` or a `Capture`:
/// both come from `step` alone, which lets neither go while a viewer's link holds the gate.
#[derive(Debug, PartialEq)]
enum Step<S, B> {
    /// Wait, the encoder as it was.
    Wait(Wait, Encoder<S, B>),
    /// Start a stream, with what a stopped encoder held: capture the whole screen, its
    /// first keyframe.
    Start(S),
    /// Capture into the buffer that was home.
    Capture(B),
}

/// What the capture loop waits for before it looks again. Damage wakes it through the
/// capturer; the hub's waker, through a viewer's command, an update told taken, a lane
/// ending, a viewer leaving, a screenshot or recording asked for, or a buffer coming home
/// from the encoder.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Wait {
    /// Damage or a wake: nothing is due.
    Change,
    /// The rest of the least interval since the last capture, or a wake: damage that comes
    /// meanwhile goes with the capture it holds back.
    Interval { rest: Duration },
    /// The links: an update told taken, a lane ending, a viewer going (a wake), or damage
    /// to take in. No time: a link opens only when it is told something, and with nothing
    /// told nothing goes.
    Gate,
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
    #[cfg(target_os = "linux")]
    fn wait(self, cap: &impl Capture, hub: &Hub) -> Result<(), String> {
        let mut waited = Ok(());
        let poll = |on, timeout| {
            waited = match on {
                On::Event => wait_any([cap.fd(), hub.waker.waitable()], timeout),
                On::Wake => wait_any([hub.waker.waitable()], timeout),
            }
        };
        self.through(poll);
        waited.map_err(|e| format!("the capture loop's wait: {e}"))
    }

    /// The wait, made through `poll`: until its time, or with no limit.
    fn through(self, poll: impl FnOnce(On, Option<Duration>)) {
        match self {
            Wait::Change => poll(On::Event, None),
            Wait::Interval { rest } => poll(On::Wake, Some(rest)),
            Wait::Gate => poll(On::Event, None),
            Wait::Buffer => poll(On::Wake, None),
        }
    }
}

/// Whether a viewer waits to start.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Key {
    /// One does: the next capture is a keyframe, for every viewer it starts.
    Wanted,
    NotWanted,
}

impl Key {
    /// What the look at the viewers saw: a keyframe is wanted if one waits to start.
    fn of(seen: &[(Arc<crate::hub::Client>, Seen)]) -> Key {
        match seen.iter().any(|(_, v)| v.viewer == Viewer::Waiting) {
            true => Key::Wanted,
            false => Key::NotWanted,
        }
    }
}

/// What changed on screen since the last capture, as each platform's capturer holds it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Damage {
    None,
    /// To be captured though no area was reported: a new capturer.
    Unreported,
    /// Rows `y0..y1` drawn, the first of them at `first`.
    Rows {
        first: Instant,
        y0: u32,
        y1: u32,
    },
}

impl Damage {
    /// Rows `top..bot` were drawn at `now`.
    pub(crate) fn add(&mut self, top: u32, bot: u32, now: Instant) {
        *self = match *self {
            Damage::Rows { first, y0, y1 } => Damage::Rows {
                first,
                y0: y0.min(top),
                y1: y1.max(bot),
            },
            Damage::None | Damage::Unreported => Damage::Rows {
                first: now,
                y0: top,
                y1: bot,
            },
        };
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

/// Where the capture loop's buffer is: home, with what is home (the buffer itself in the
/// loop; nothing in a test of what the loop decides), or away at the encoder.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Buffer<B> {
    Home(B),
    Away,
}

/// The encoder, as the capture loop holds it when it looks: what a stream is started with
/// (nothing, in the loop: a start makes its own buffer), or where the running stream's
/// buffer is. The loop knows which it holds, and says so in the types: a look at a stopped
/// encoder has no buffer to capture into, and one at a running encoder nothing to start.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Encoder<S, B> {
    /// None runs: the stream was just built, and nobody has watched it yet. The next
    /// capture starts one, on a keyframe of the whole screen.
    Stopped(S),
    /// One runs, the loop's buffer home or with it.
    Running(Buffer<B>),
}

/// What the capture loop knows when it looks, with somebody watching.
#[derive(Clone, Copy)]
struct Look {
    key: Key,
    damage: Damage,
    now: Instant,
    /// When the last capture went.
    captured: Instant,
    /// The least time between two captures.
    min_interval: Duration,
    gate: Gated,
}

/// What the loop does next, from what it knows: the one place a capture is let go, the
/// first of a stream as any other. A capture takes the buffer, so one is made only when it
/// is home; a wait leaves the encoder as it was.
fn step<S, B>(look: Look, encoder: Encoder<S, B>) -> Step<S, B> {
    let Look {
        key,
        damage,
        now,
        captured,
        min_interval,
        gate,
    } = look;
    match (key, damage, &encoder) {
        // Nobody waits to start, and nothing moved, or no stream runs to show it on (what
        // moves meanwhile is in the whole screen the next stream starts with): nothing
        // to capture.
        (Key::NotWanted, Damage::None, Encoder::Stopped(_) | Encoder::Running(_))
        | (Key::NotWanted, Damage::Unreported | Damage::Rows { .. }, Encoder::Stopped(_)) => {
            return Step::Wait(Wait::Change, encoder);
        }
        (
            Key::Wanted,
            Damage::None | Damage::Unreported | Damage::Rows { .. },
            Encoder::Stopped(_) | Encoder::Running(_),
        )
        | (Key::NotWanted, Damage::Unreported | Damage::Rows { .. }, Encoder::Running(_)) => {}
    }
    // The loop reads `now` after the capture it stamped `captured`: on one thread, in order.
    let since = now - captured;
    if since < min_interval {
        let rest = min_interval - since;
        return Step::Wait(Wait::Interval { rest }, encoder);
    }
    // A capture goes once every viewer's link lets one.
    if gate == Gated::Held {
        return Step::Wait(Wait::Gate, encoder);
    }
    match encoder {
        Encoder::Stopped(with) => Step::Start(with),
        // A capture goes only to an idle encoder. One made while it codes the frame before
        // would wait for it, a whole encode older by the time it is coded; made once it is
        // done, it holds what changed meanwhile, and goes at once.
        Encoder::Running(Buffer::Home(buf)) => Step::Capture(buf),
        Encoder::Running(Buffer::Away) => Step::Wait(Wait::Buffer, encoder),
    }
}

/// What a capture is sent as, or why nothing is.
enum Planned<S> {
    Send(Plan<S>),
    /// The damage held no area after all, and no keyframe was promised.
    Empty,
}

/// What a capture whose damage came to `band` is sent as: a keyframe for the viewers
/// `starts` if a keyframe was wanted and promised to any; else what moved, if anything did.
fn plan<S>(starts: Option<S>, band: Option<Band>) -> Planned<S> {
    match (starts, band) {
        (Some(s), Some(_) | None) => Planned::Send(Plan::Key(s)),
        (None, Some(_)) => Planned::Send(Plan::Motion),
        (None, None) => Planned::Empty,
    }
}

#[cfg(target_os = "linux")]
/// What the capture loop asks of the screen it captures: the platform's `Capturer`, or a
/// screen a test draws on.
trait Capture {
    /// Width and height, in pixels.
    fn size(&self) -> (u32, u32);
    /// Readable when the screen has something to report.
    fn fd(&self) -> Waitable<'_>;
    /// Takes in what the screen reported.
    fn pump(&mut self) -> Result<(), String>;
    /// The screen changed size.
    fn needs_rebuild(&self) -> bool;
    /// What changed since the last capture.
    fn damage(&mut self) -> &mut Damage;
    /// The rows damaged since the last take, if any were.
    fn take_damage(&mut self) -> Option<Band>;
    /// Every row.
    fn full(&self) -> Band;
    /// The whole screen as BGRX, for a screenshot.
    fn snapshot(&mut self) -> Result<Vec<u8>, String>;
    /// Reads `band` of the screen into the frame `convert` reads.
    fn grab(&mut self, band: Band) -> Result<(), String>;
    /// Converts rows [y0, y1) of the grabbed frame into `dst`, on the capturer's threads.
    fn convert(&self, dst: &mut I420, y0: usize, y1: usize) -> Result<(), String>;
    /// The grabbed frame as BGRX and its stride (the debug marker's probe).
    fn bgrx(&self) -> (&[u8], usize);
}

#[cfg(target_os = "linux")]
impl Capture for Capturer {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    fn fd(&self) -> Waitable<'_> {
        Capturer::fd(self)
    }
    fn pump(&mut self) -> Result<(), String> {
        Capturer::pump(self)
    }
    fn needs_rebuild(&self) -> bool {
        Capturer::needs_rebuild(self)
    }
    fn damage(&mut self) -> &mut Damage {
        &mut self.damage
    }
    fn take_damage(&mut self) -> Option<Band> {
        Capturer::take_damage(self)
    }
    fn full(&self) -> Band {
        Capturer::full(self)
    }
    fn snapshot(&mut self) -> Result<Vec<u8>, String> {
        Capturer::snapshot(self)
    }
    fn grab(&mut self, band: Band) -> Result<(), String> {
        Capturer::grab(self, band)
    }
    fn convert(&self, dst: &mut I420, y0: usize, y1: usize) -> Result<(), String> {
        Capturer::convert(self, dst, y0, y1)
    }
    fn bgrx(&self) -> (&[u8], usize) {
        Capturer::bgrx(self)
    }
}

/// What sets the screen's size for dynamic resolution: the platform's `screen::Screen`.
pub(crate) trait Resize {
    /// Asks for `w`x`h`: the size the screen has now.
    fn resize(&mut self, w: u32, h: u32) -> Result<(u32, u32), String>;
}

#[cfg(target_os = "linux")]
impl Resize for crate::screen::Screen {
    fn resize(&mut self, w: u32, h: u32) -> Result<(u32, u32), String> {
        crate::screen::Screen::resize(self, w, h)
    }
}

/// A size asked for, given to the screen: whether the stream, at `streamed`, is rebuilt.
/// Only a screen whose size really changed rebuilds it; an ask the screen refuses, or one
/// for the size it has, changes nothing: a rebuild for either would start every viewer's
/// stream over, on a keyframe of the whole screen, for nothing.
pub(crate) fn rebuilds_for(
    screen: &mut impl Resize,
    (w, h): (u32, u32),
    streamed: (u32, u32),
) -> bool {
    match screen.resize(w, h) {
        Ok(size) => size != streamed,
        Err(e) => {
            log::warn!("resize to {w}x{h} failed: {e}");
            false
        }
    }
}

#[cfg(target_os = "linux")]
fn capture_loop(
    cap: &mut impl Capture,
    coded: av1::Stream,
    cfg: VideoConfig,
    hub: Arc<Hub>,
    screen: &mut impl Resize,
) -> Result<(), String> {
    let (w, h) = cap.size();
    let (w, h) = (w as usize, h as usize);
    let interval = Duration::from_secs_f64(1.0 / f64::from(FRAMERATE));
    let min_interval = interval.mul_f64(MIN_INTERVAL_SHARE);
    // When the last capture went: an interval ago, so the first goes at once.
    let mut captured = Instant::now() - interval;
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
    // The encoder, from the first time somebody watches, and where the picture that goes
    // round is. Every way out of the loop drops it, and with it waits for the encoder to send
    // what it holds.
    let mut stream: Option<(Stream, Buffer<Box<I420>>)> = None;
    // What the loop saw of each watching viewer when it last looked: kept, so that a look
    // allocates nothing once there is room for every viewer.
    let mut seen: Vec<(Arc<crate::hub::Client>, Seen)> = Vec::new();

    loop {
        // The wake is taken before anything it may stand for is read: a request that
        // lands after this line wakes the next wait, and one that landed before it is
        // seen below. Read first and drained after, a resize asked for in between lost
        // its wake, and the waits below have no time limit.
        hub.drain_wake();
        if cap.needs_rebuild() {
            return Ok(());
        }
        let asked = hub.resize_req.lock().take();
        if let Some(want) = asked {
            if rebuilds_for(screen, want, cap.size()) {
                return Ok(());
            }
        }
        cap.pump()?;
        if cap.needs_rebuild() {
            return Ok(());
        }
        let snaps = std::mem::take(&mut *hub.snapshots.lock());
        if !snaps.is_empty() {
            // The whole current screen, whether or not anyone is watching. Damage stays
            // pending, so the stream still sends what changed. Only the pixels are copied
            // here (the framebuffer is live); the PNG is made off this thread.
            let shot = Arc::new(crate::record::Snapshot {
                width: w,
                height: h,
                bgrx: cap.snapshot()?,
            });
            for tx in snaps {
                if tx.send(shot.clone()).is_err() {
                    log::info!("a screenshot's asker left before its answer");
                }
            }
        }
        // The encoder holds the sender for as long as it runs: once it is gone, no buffer
        // will come back to wake the loop.
        if let Some((s, home)) = stream.as_mut() {
            loop {
                match s.ret_rx.try_recv() {
                    Ok(buf) => *home = Buffer::Home(buf),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => return Err("encoder gone".into()),
                }
            }
        }
        // One reading of the clock and one look at each viewer: the stream, the keyframe and
        // whom it starts and the gate are all decided from them. The capture itself
        // is stamped `t0` below, after any wait for a buffer.
        let now = Instant::now();
        hub.look_into(&mut seen);
        if seen.is_empty() {
            // Nobody watches: nothing is captured, and the encoder waits as it was, for a
            // viewer that holds its newest frame to go on from (`hub::Holds`). What changes
            // meanwhile is damage the next capture takes.
            Wait::Change.wait(cap, &hub)?;
            continue;
        }
        let look = Look {
            key: Key::of(&seen),
            damage: *cap.damage(),
            now,
            captured,
            min_interval,
            gate: Gated::of(&seen),
        };
        // What the look promises its keyframe to, if it wants one: asked once a capture
        // is let go.
        let promise = || match look.key {
            Key::Wanted => hub.promise(&seen),
            Key::NotWanted => None,
        };
        // The one decision, for a stream to start as for one that runs: nothing below
        // captures but what `step` let go.
        let Some((s, picture)) = stream.as_mut() else {
            match step(look, Encoder::<(), std::convert::Infallible>::Stopped(())) {
                Step::Wait(wait, _) => wait.wait(cap, &hub)?,
                Step::Capture(never) => match never {},
                // A viewer that has left since the look is promised nothing: with nobody
                // to start, the next look sees who is there.
                Step::Start(()) => {
                    if let Some(starts) = promise() {
                        let (cw, ch) = cap.size();
                        cap.grab(cap.full())?;
                        // The whole screen is grabbed: it holds what was drawn while
                        // nobody watched, so that damage is taken with it.
                        cap.damage().take(ch);
                        let t0 = Instant::now();
                        let mut buf = Box::new(I420::new(cw as usize, ch as usize));
                        cap.convert(&mut buf, 0, ch as usize)?;
                        hub.stats.captured.fetch_add(1, Ordering::Relaxed);
                        captured = t0;
                        let first = Job {
                            buf,
                            frame: Plan::Key(starts),
                            damage: t0,
                            marker_ms: None,
                        };
                        let started = Stream::start(&hub, coded, first, cfg.debug)?;
                        stream = Some((started, Buffer::Away));
                    }
                }
            }
            continue;
        };
        let home = std::mem::replace(picture, Buffer::Away);
        let mut buf = match step(look, Encoder::<std::convert::Infallible, _>::Running(home)) {
            Step::Wait(wait, Encoder::Running(buffer)) => {
                *picture = buffer;
                wait.wait(cap, &hub)?;
                continue;
            }
            Step::Wait(_, Encoder::Stopped(never)) | Step::Start(never) => match never {},
            Step::Capture(buf) => buf,
        };
        let t0 = Instant::now();
        let origin = match *cap.damage() {
            Damage::Rows { first, .. } => first,
            Damage::None | Damage::Unreported => t0,
        };
        hub.stats
            .damage_wait_us
            .fetch_add((t0 - origin).as_micros() as u64, Ordering::Relaxed);
        let band = cap.take_damage();
        if let Some(b) = band {
            cap.grab(b)?;
        }
        let t1 = Instant::now();
        if marker.is_some() {
            log::info!(
                "cap dt {:.1} ms band {:?} grab {} us",
                captured.elapsed().as_secs_f64() * 1000.0,
                band,
                (t1 - t0).as_micros()
            );
        }
        let marker_ms = marker_reading(cap.bgrx(), probe);
        // What this capture is sent as, decided once; with nothing to send, the buffer
        // goes back home.
        let frame = match plan(promise(), band) {
            Planned::Send(frame) => frame,
            Planned::Empty => {
                *picture = Buffer::Home(buf);
                continue;
            }
        };
        if let Some(b) = band {
            let pic: &mut I420 = &mut buf;
            cap.convert(pic, b.y0 as usize, b.y1 as usize)?;
        }
        let t2 = Instant::now();
        hub.stats
            .grab_us
            .fetch_add((t1 - t0).as_micros() as u64, Ordering::Relaxed);
        hub.stats
            .convert_us
            .fetch_add((t2 - t1).as_micros() as u64, Ordering::Relaxed);
        hub.stats.captured.fetch_add(1, Ordering::Relaxed);
        captured = t0;
        if s.job_tx
            .send(Job {
                buf,
                frame,
                damage: origin,
                marker_ms,
            })
            .is_err()
        {
            return Err("encoder gone".into());
        }
    }
}

/// A frame coded: what the encoder says it is, and its message, a header's room and then
/// the AV1 temporal unit.
struct Coded {
    coding: Coding,
    message: BytesMut,
}

/// Codes each frame: the encoder, and what it puts out.
struct Coder {
    enc: av1::Encoder,
    /// What libaom put out for the last frame: kept, so a frame allocates nothing.
    out: Vec<u8>,
    arenas: Arenas,
    debug: bool,
}

impl Coder {
    /// The stream `coded`'s coder, and its first frame, `first`, a keyframe: a stream starts
    /// with the viewers it starts.
    fn start(coded: av1::Stream, first: &I420, debug: bool) -> Result<(Coder, Coded), av1::Failed> {
        let mut out = Vec::new();
        let te = Instant::now();
        let enc = av1::Encoder::start(&coded, &planes(first), &mut out)?;
        let mut coder = Coder {
            enc,
            out,
            arenas: Arenas::new(),
            debug,
        };
        let coded = coder.coded(Coding::Key, te);
        Ok((coder, coded))
    }

    /// Codes `buf`, a keyframe if `coding` asks for one: a header's room, then the temporal
    /// unit, and what the encoder coded it as.
    fn code(&mut self, buf: &I420, coding: Coding) -> Result<Coded, av1::Failed> {
        self.out.clear();
        let te = Instant::now();
        let coding = self.enc.encode(&planes(buf), coding, &mut self.out)?;
        Ok(self.coded(coding, te))
    }

    /// What the encoder put out, begun at `te`, coded as `coding`: its message.
    fn coded(&mut self, coding: Coding, te: Instant) -> Coded {
        if self.debug {
            match thread_cpu_us() {
                Ok(cpu) => log::info!(
                    "encode {} us wall, thread cpu total {cpu} us, {} bytes",
                    te.elapsed().as_micros(),
                    Header::LEN + self.out.len()
                ),
                Err(e) => log::error!("this thread's CPU time: {e}"),
            }
        }
        Coded {
            coding,
            message: self.arenas.message(&self.out),
        }
    }
}

/// `buf`'s planes, as the encoder reads them.
fn planes(buf: &I420) -> av1::Planes<'_> {
    av1::Planes {
        y: &buf.y,
        u: &buf.u,
        v: &buf.v,
        stride_y: buf.width,
        stride_uv: buf.stride_uv(),
    }
}

/// The bytes frames are cut from, in two arenas taken in turn. A message
/// is the next stretch of the arena in use; when that one is full the other is taken
/// back, once every message cut from it has been sent and dropped. So a stream whose
/// viewers keep up allocates nothing for its messages.
struct Arenas {
    two: [BytesMut; 2],
    at: usize,
}

/// What each of the two arenas holds. Sized from the four numbers' scroll scene:
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

/// Starts the encode loop's thread with `run`, which owns the way home for the capture
/// loop's pictures. However `run` ends, by returning or by a panic, it has closed that
/// way by then, and the capture loop is woken to find it closed: it ends on "encoder
/// gone", and never waits for a picture that cannot come. Dropped, it waits for `run` to
/// end.
fn encoder_thread(hub: Arc<Hub>, run: impl FnOnce() + Send + 'static) -> Result<Stopped, String> {
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
    coded: av1::Stream,
    first: Job,
    rx: mpsc::Receiver<Job>,
    ret: mpsc::SyncSender<Box<I420>>,
    hub: Arc<Hub>,
    debug: Debugging,
) {
    if let Err(e) = encode_frames(coded, first, rx, ret, &hub, debug.encodes) {
        log::error!("the encoder is gone: {e}");
    }
}

/// Codes `first`, then each frame the capture loop sends, and sends each to the viewers,
/// until the capture loop is gone; a frame libaom fails is the error, which ends the stream.
fn encode_frames(
    coded: av1::Stream,
    first: Job,
    rx: mpsc::Receiver<Job>,
    ret: mpsc::SyncSender<Box<I420>>,
    hub: &Hub,
    debug: bool,
) -> Result<(), av1::Failed> {
    // Kept from frame to frame, so a frame allocates nothing for it.
    let mut viewers = Vec::new();
    // A picture goes home only once its frame is on its viewers' lanes: the capture loop,
    // woken by it, looks at the gate next, and the lanes must count that frame. A
    // capture loop gone has no home to send it to, and ends this loop at its next job.
    let home = |buf: Box<I420>| {
        if ret.send(buf).is_ok() {
            hub.wake();
        }
    };
    let t0 = Instant::now();
    let (mut coder, out) = Coder::start(coded, &first.buf, debug)?;
    let configured = coder.enc.configuration().clone();
    let mut sent = |job: Job, coded: Coded, t0: Instant| {
        let Coded {
            coding,
            message: mut bytes,
        } = coded;
        let enc_us = t0.elapsed().as_micros() as u64;
        let version = hub.next_version();
        let header = Header {
            coding,
            version,
            pipe: Pipe::of(job.damage.elapsed()),
        };
        bytes[..Header::LEN].copy_from_slice(&header.bytes());
        let bytes = bytes.freeze();
        hub.stats.encode_us.fetch_add(enc_us, Ordering::Relaxed);
        hub.stats.encoded.fetch_add(1, Ordering::Relaxed);
        hub.stats
            .bytes
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        // The stream's newest frame, before anyone is sent it; the viewers a keyframe
        // starts start on it, and are sent it.
        hub.coded(version);
        if let Plan::Key(starts) = &job.frame {
            hub.stats.keyframes.fetch_add(1, Ordering::Relaxed);
            starts.started(version, &configured);
        }
        hub.clients_into(&mut viewers);
        for c in &viewers {
            c.takes(&bytes, version);
        }
        home(job.buf);
        // What the viewers were sent is on their lanes now: dropped here, its arena is
        // taken back.
        drop(bytes);
        viewers.clear();
        hub.stats
            .send_lat_us
            .fetch_add(job.damage.elapsed().as_micros() as u64, Ordering::Relaxed);
        if let Some(ms) = job.marker_ms {
            match marker_age(ms) {
                Ok(age) => log::info!("marker sent age {age} ms (encode {enc_us} us)"),
                Err(e) => log::error!("marker sent: the wall clock: {e}"),
            }
        }
    };
    sent(first, out, t0);
    while let Ok(job) = rx.recv() {
        let t0 = Instant::now();
        let coding = match job.frame {
            Plan::Key(_) => Coding::Key,
            Plan::Motion => Coding::Delta,
        };
        let out = coder.code(&job.buf, coding)?;
        sent(job, out, t0);
    }
    Ok(())
}

/// CPU time this thread has used, in microseconds.
fn thread_cpu_us() -> nix::Result<i64> {
    use nix::time::{clock_gettime, ClockId};
    clock_gettime(ClockId::CLOCK_THREAD_CPUTIME_ID)
        .map(|t| t.tv_sec() * 1_000_000 + t.tv_nsec() / 1000)
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

/// What the bench loadgen's marker says on this capture, its pixels `bgrx` and their
/// stride: none unless `VOXL_DEBUG_MARKER` placed it where it fits.
fn marker_reading((px, stride): (&[u8], usize), probe: Option<MarkerProbe>) -> Option<u32> {
    probe.map(|at| marker_probe(px, stride, at, "grab"))
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
    match marker_age(ms) {
        Ok(age) => log::info!("marker {} frame {} age {age} ms", what, v >> 24),
        Err(e) => log::error!("marker {what}: the wall clock: {e}"),
    }
    ms
}

/// How long ago the loadgen drew a marker of `ms`, the low 24 bits of the wall clock's
/// milliseconds; a wall clock before 1970 is the error.
fn marker_age(ms: u32) -> Result<i64, std::time::SystemTimeError> {
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?;
    Ok((since.as_millis() as i64 & 0xffffff) - i64::from(ms))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::session::estimate::tests::Link;
    use crate::session::link::tests::Simulated;
    use crate::session::link::{Gate, Path};
    use crate::session::sender::Sender;
    use crate::xorshift::Xorshift;

    /// A session of `hub` with a lane, which no writer drains: a test reads what it holds
    /// (`updates`).
    fn viewer(hub: &Hub) -> Arc<crate::hub::Client> {
        let (sender, _writer) = Sender::with_lane();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        hub.add_client(tx, sender)
    }

    /// The video updates `c`'s lane holds, in order, taken off it as its socket would.
    fn updates(c: &crate::hub::Client) -> Vec<Vec<u8>> {
        c.lane(|s| s.updates())
    }

    /// The stream a test codes: `w` x `h` on one thread.
    fn coded(w: u32, h: u32) -> av1::Stream {
        let shape = av1::Shape {
            width: w,
            height: h,
        };
        av1::Stream::new(shape, std::num::NonZeroUsize::MIN).unwrap()
    }

    /// The viewers of `hub` waiting to start, promised a keyframe, as the capture loop
    /// promises them.
    fn promised(hub: &Hub) -> Starts {
        let mut seen = Vec::new();
        hub.look_into(&mut seen);
        hub.promise(&seen).expect("a viewer waiting to start")
    }

    /// A job of `frame` on a picture of `w` x `h` whose luma is random.
    fn job(w: u32, h: u32, frame: Plan<Starts>, rng: &mut Xorshift) -> Job {
        let mut buf = I420::new(w as usize, h as usize);
        for p in buf.y.iter_mut() {
            *p = rng.below(256) as u8;
        }
        Job {
            buf: Box::new(buf),
            frame,
            damage: Instant::now(),
            marker_ms: None,
        }
    }

    /// The top-level boxes of an MP4, each its type and body, if they tile it exactly.
    fn boxes(b: &[u8]) -> Option<Vec<([u8; 4], &[u8])>> {
        let mut out = Vec::new();
        let mut at = 0;
        while at < b.len() {
            let size = u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?) as usize;
            let name: [u8; 4] = b.get(at + 4..at + 8)?.try_into().ok()?;
            out.push((name, b.get(at + 8..at + size)?));
            at += size.max(8);
        }
        Some(out)
    }

    /// The body of the box at `path` inside `b`, through containers.
    fn inside<'a>(b: &'a [u8], path: &[&[u8; 4]]) -> Option<&'a [u8]> {
        let (first, rest) = path.split_first()?;
        let (_, body) = boxes(b)?.into_iter().find(|(n, _)| n == *first)?;
        match rest {
            [] => Some(body),
            [_, ..] => inside(body, rest),
        }
    }

    /// A recording is an MP4 of the stream's own frames: `ftyp`, then `moov`, then `mdat`,
    /// tiling the file; one track as large as the screen, an `av01` sample entry whose `av1C`
    /// names the stream's level; a sample for every frame the recorder took, each, in order,
    /// exactly the temporal unit a page beside it was sent without its temporal delimiter,
    /// the first a sync sample. Through the real encode loop with the recorder as a viewer
    /// beside a page, over generated sizes and runs of 1 to 20 frames after the keyframe that
    /// starts it.
    #[tokio::test]
    async fn a_recording_is_an_mp4_of_its_frames() {
        let mut rng = Xorshift::new(0x1f83_d9ab_fb41_bd6b);
        for run in 0..6 {
            let (w, h) = (2 * rng.range(32, 200) as u32, 2 * rng.range(32, 150) as u32);
            let hub = Hub::new(crate::hub::tests::hello()).unwrap();
            let page = viewer(&hub);
            page.watch();
            let (stop, stopped) = tokio::sync::oneshot::channel();
            let recording = tokio::spawn({
                let hub = hub.clone();
                async move {
                    let until = crate::record::Until::Stopped(stopped);
                    crate::record::record(&hub, until, Duration::from_secs(3600)).await
                }
            });
            while hub.clients.lock().len() < 2 {
                tokio::task::yield_now().await;
            }
            let first = job(w, h, Plan::Key(promised(&hub)), &mut rng);
            let s = Stream::start(&hub, coded(w, h), first, Debugging::read(|_| None).unwrap())
                .unwrap();
            s.ret_rx.recv().unwrap();
            let n = 1 + rng.below(20) as usize;
            for _ in 0..n {
                s.job_tx.send(job(w, h, Plan::Motion, &mut rng)).unwrap();
                s.ret_rx.recv().unwrap();
            }
            drop(s);
            stop.send(()).unwrap();
            let mp4 = recording.await.unwrap().unwrap();
            let top = boxes(&mp4).expect("the boxes tile the file");
            let names: Vec<[u8; 4]> = top.iter().map(|(n, _)| *n).collect();
            assert_eq!(names, [*b"ftyp", *b"moov", *b"mdat"]);
            let mdat_at = mp4.len() - top[2].1.len();
            let traks: Vec<&[u8]> = boxes(top[1].1)
                .unwrap()
                .into_iter()
                .filter(|(n, _)| n == b"trak")
                .map(|(_, b)| b)
                .collect();
            assert_eq!(traks.len(), 1, "one track");
            let u32_at = |b: &[u8], i: usize| u32::from_be_bytes(b[i..i + 4].try_into().unwrap());
            let tkhd = inside(traks[0], &[b"tkhd"]).unwrap();
            // Version and flags, times, id, reserved, duration, reserved (8), layer and
            // group, volume and reserved, the matrix (9), then width and height.
            let size = 4 + 20 + 8 + 4 + 4 + 9 * 4;
            assert_eq!(
                (u32_at(tkhd, size), u32_at(tkhd, size + 4)),
                (w << 16, h << 16)
            );
            let stbl = |name| inside(traks[0], &[b"mdia", b"minf", b"stbl", name]).unwrap();
            let entry = boxes(&stbl(b"stsd")[8..]).unwrap();
            assert_eq!(entry[0].0, *b"av01");
            // The visual sample entry's 78 bytes, then its boxes: `av1C`, whose second byte
            // is the Main profile and the level.
            let av1c = inside(&entry[0].1[78..], &[b"av1C"]).unwrap();
            let level = coded(w, h).codec()[7..9].parse::<u8>().unwrap();
            assert_eq!(av1c[1], level, "run {run}");
            // One sync sample, the first.
            assert_eq!(&stbl(b"stss")[4..12], &[0, 0, 0, 1, 0, 0, 0, 1]);
            let stsz = stbl(b"stsz");
            assert_eq!(u32_at(stsz, 8) as usize, n + 1, "samples against frames");
            let at = u32_at(stbl(b"stco"), 8) as usize;
            assert!(at >= mdat_at && at < mp4.len(), "a chunk outside mdat");
            let sent = page.lane(|s| s.updates());
            assert_eq!(sent.len(), n + 1);
            let mut at = at;
            for (f, update) in sent.iter().enumerate() {
                let len = u32_at(stsz, 12 + 4 * f) as usize;
                let unit = &update[Header::LEN..];
                assert_eq!(&unit[..2], &av1::TEMPORAL_DELIMITER, "run {run}, frame {f}");
                assert_eq!(mp4[at..at + len], unit[2..], "run {run}, sample {f}");
                at += len;
            }
        }
    }

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
                7.. => next(),
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
                2.. => next(),
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
                marker_reading((&buf[..], stride), probe),
                probe.map(|_| (painted & 0xff_ffff) as u32),
                "{at:?} on {w}x{h} {painted:#x}"
            );
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
            drop(
                Stopped::spawn("work", move || {
                    for i in 0..steps {
                        tx.send(i).unwrap();
                    }
                })
                .unwrap(),
            );
            assert_eq!(rx.try_iter().count(), steps as usize, "{steps} steps");
        }
    }

    /// Every viewer's lane carries the stream from the keyframe it starts on, through the
    /// real encode loop, a replaced lane included: over generated runs, a viewer
    /// watches from the start, a second joins partway, and a third's lane is replaced partway
    /// (the relay reopened it) and again later; a keyframe is coded whenever one waits to
    /// start, as the capture loop codes one. The first viewer's lane is the whole stream.
    /// Each lane's updates begin with a keyframe and are a run of the whole stream's, in its
    /// order, none left out: what a viewer is sent refers only to what it was sent before.
    #[test]
    fn every_lane_starts_on_a_keyframe_and_misses_nothing_after() {
        let (w, h) = (64u32, 64u32);
        let mut rng = Xorshift::new(0x510e_527f_ade6_82d1);
        let mut lanes_checked = 0;
        for case in 0..8 {
            let hub = Hub::new(crate::hub::tests::hello()).unwrap();
            let whole = viewer(&hub);
            whole.watch();
            let moved = viewer(&hub);
            moved.watch();
            let first = job(w, h, Plan::Key(promised(&hub)), &mut rng);
            let s = Stream::start(&hub, coded(w, h), first, Debugging::read(|_| None).unwrap())
                .unwrap();
            s.ret_rx.recv().unwrap();
            let (join, reopen) = (rng.range(5, 50), rng.range(5, 40));
            let again = reopen + rng.range(1, 20);
            // Each lane's updates, as its socket took them.
            let mut lanes: Vec<Vec<Vec<u8>>> = Vec::new();
            let mut joined = None;
            for f in 0..60 {
                if f == join {
                    let b = viewer(&hub);
                    b.watch();
                    joined = Some(b);
                }
                if f == reopen || f == again {
                    // The socket took what the lane held before it broke.
                    lanes.push(updates(&moved));
                    moved.attach().expect("an open session takes a lane");
                }
                let mut seen = Vec::new();
                hub.look_into(&mut seen);
                let frame = match Key::of(&seen) {
                    Key::Wanted => Plan::Key(promised(&hub)),
                    Key::NotWanted => Plan::Motion,
                };
                s.job_tx.send(job(w, h, frame, &mut rng)).unwrap();
                s.ret_rx.recv().unwrap();
            }
            drop(s);
            lanes.push(updates(&moved));
            lanes.extend(joined.map(|b| updates(&b)));
            let all = updates(&whole);
            for (l, got) in lanes.iter().enumerate() {
                assert!(!got.is_empty(), "case {case}: lane {l} carried nothing");
                assert!(
                    Header::read(&got[0]).is_ok_and(|v| v.header.coding == Coding::Key),
                    "case {case}: lane {l} starts on no keyframe"
                );
                // Where the lane's first update is in the whole stream: the same bytes.
                let at = all.iter().position(|u| *u == got[0]).unwrap();
                assert_eq!(got[..], all[at..at + got.len()], "case {case}: lane {l}");
                lanes_checked += 1;
            }
        }
        assert_eq!(lanes_checked, 8 * 4);
    }

    /// A viewer that joins while the stream is rebuilt (a resize, a display switch)
    /// starts on the rebuilt stream: no frame of the old stream reaches its lanes, whose
    /// decoder would take it as the start of the stream the rebuilt one then replaces.
    /// Steps, the loops' own: a page joins and the capture loop plans its keyframe; a
    /// resize ends the capture loop, and the real encode loop sends that keyframe while a
    /// second page has just joined; then `build`'s `restart_clients` starts every viewer
    /// over. The first page's lane has the keyframe, so the test is of the stream, not of
    /// a loop that sent nothing.
    #[test]
    fn a_viewer_that_joins_while_the_stream_is_rebuilt_starts_on_the_rebuilt_stream() {
        let hub = Hub::new(crate::hub::tests::hello()).unwrap();
        let (w, h) = (64, 64);
        let page = viewer(&hub);
        page.watch();
        let mut rng = Xorshift::new(0x6a09_e667_f3bc_c908);
        let first = job(w, h, Plan::Key(promised(&hub)), &mut rng);
        // A resize ends the capture loop, and a second page joins, before the encode loop
        // has coded that keyframe.
        let second = viewer(&hub);
        second.watch();
        let s =
            Stream::start(&hub, coded(w, h), first, Debugging::read(|_| None).unwrap()).unwrap();
        drop(s);
        // build(): every viewer starts over, on the rebuilt stream.
        hub.restart_clients();
        assert_eq!(updates(&page).len(), 1, "the first page had no keyframe");
        assert!(
            updates(&second).is_empty(),
            "the second page's lane had a frame of the old stream"
        );
    }

    /// The test binary's allocator: the system's, counting by thread what is allocated
    /// while it is armed.
    #[global_allocator]
    static COUNTING: counting::Counting = counting::Counting;

    pub(crate) mod counting {
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
            // SAFETY: each writes at most the 16 bytes it is given, a name and its NUL.
            #[cfg(target_os = "linux")]
            unsafe {
                libc::prctl(libc::PR_GET_NAME, name.as_mut_ptr())
            };
            #[cfg(not(target_os = "linux"))]
            unsafe {
                libc::pthread_getname_np(libc::pthread_self(), name.as_mut_ptr().cast(), 16)
            };
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
        #[cfg(target_os = "linux")]
        pub fn arm() {
            *seen() = Seen {
                threads: [None; 32],
                others: 0,
            };
            ARMED.store(true, Ordering::SeqCst);
        }

        /// Stops counting: what each thread allocated since `arm`, and how many
        /// allocations came from threads past the table.
        #[cfg(target_os = "linux")]
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
    pub(crate) fn in_a_process_of_its_own(name: &str) -> bool {
        const ALONE: &str = "VOXL_TEST_IN_A_PROCESS_OF_ITS_OWN";
        if std::env::var_os(ALONE).is_some() {
            return true;
        }
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                name,
                "--exact",
                "--include-ignored",
                "--test-threads=1",
                "--nocapture",
            ])
            .env(ALONE, "1")
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&out.stdout);
        // What it printed (a benchmark's numbers), here, where its runner sees it.
        print!("{said}");
        assert!(
            out.status.success() && said.contains("1 passed"),
            "{said}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        false
    }

    /// The screen a test draws on, shared by the test and the capture it hands the loop.
    #[cfg(target_os = "linux")]
    struct Canvas {
        w: u32,
        h: u32,
        drawn: parking_lot::Mutex<Drawn>,
        /// Readable when something was drawn: the capture's fd.
        news: crate::hub::Wake,
        /// The test is done: the capture loop ends, as at a change of the screen's size.
        done: std::sync::atomic::AtomicBool,
    }

    #[cfg(target_os = "linux")]
    struct Drawn {
        bgrx: Vec<u8>,
        /// The rows drawn since the capture last took in what was drawn.
        rows: Option<(u32, u32)>,
    }

    #[cfg(target_os = "linux")]
    impl Canvas {
        /// Text in rows `y0..y1` as it stands `n` steps into a scroll of 3 px a step.
        fn draw(&self, y0: u32, y1: u32, n: usize) {
            let mut d = self.drawn.lock();
            let w = self.w as usize;
            for (y, row) in d
                .bgrx
                .chunks_exact_mut(w * 4)
                .enumerate()
                .take(y1 as usize)
                .skip(y0 as usize)
            {
                let (line, within) = ((y + 3 * n) / 18, (y + 3 * n) % 18);
                for (x, glyph) in row.chunks_mut(9 * 4).enumerate() {
                    let ink = within < 12 && (x * 2654435761 + line * 40503) >> 7 & 1 == 1;
                    glyph.fill(if ink { 30 } else { 235 });
                }
            }
            d.rows = Some(match d.rows {
                None => (y0, y1),
                Some((a, b)) => (a.min(y0), b.max(y1)),
            });
            drop(d);
            self.news.signal();
        }
    }

    /// The loop's capture of a `Canvas`: it keeps damage as the X11 capturer does
    /// (capture.rs: `pump`, `take_damage`, the same lines, and the same
    /// `Damage`), over pixels the test draws, read in place as from Xvfb's framebuffer.
    #[cfg(target_os = "linux")]
    struct Drawing {
        canvas: Arc<Canvas>,
        damage: Damage,
        rows: crate::convert::Rows,
        /// The frame the last grab read, as the X server's shared memory holds it.
        frame: Vec<u8>,
    }

    #[cfg(target_os = "linux")]
    impl Capture for Drawing {
        fn size(&self) -> (u32, u32) {
            (self.canvas.w, self.canvas.h)
        }
        fn fd(&self) -> Waitable<'_> {
            self.canvas.news.waitable()
        }
        fn pump(&mut self) -> Result<(), String> {
            self.canvas.news.drain();
            let Some((top, bot)) = self.canvas.drawn.lock().rows.take() else {
                return Ok(());
            };
            self.damage.add(top, bot, Instant::now());
            Ok(())
        }
        fn needs_rebuild(&self) -> bool {
            self.canvas.done.load(Ordering::Relaxed)
        }
        fn damage(&mut self) -> &mut Damage {
            &mut self.damage
        }
        fn take_damage(&mut self) -> Option<Band> {
            self.damage.take(self.canvas.h)
        }
        fn full(&self) -> Band {
            Band {
                y0: 0,
                y1: self.canvas.h,
            }
        }
        fn grab(&mut self, _band: Band) -> Result<(), String> {
            self.frame.copy_from_slice(&self.canvas.drawn.lock().bgrx);
            Ok(())
        }
        fn convert(&self, dst: &mut I420, y0: usize, y1: usize) -> Result<(), String> {
            let stride = self.canvas.w as usize * 4;
            self.rows.convert(&self.frame, stride, dst, y0, y1)
        }
        fn snapshot(&mut self) -> Result<Vec<u8>, String> {
            Ok(self.canvas.drawn.lock().bgrx.clone())
        }
        fn bgrx(&self) -> (&[u8], usize) {
            (&self.frame, self.canvas.w as usize * 4)
        }
    }

    /// What the stream's threads allocated over a steady run, by thread.
    #[cfg(target_os = "linux")]
    struct Counted {
        threads: Vec<counting::Thread>,
        /// Allocations by threads past the counting table.
        others: usize,
        /// The frames the run sent.
        frames: usize,
        /// The test's own thread: the viewer, not counted.
        viewer: [u8; 16],
    }

    #[cfg(target_os = "linux")]
    impl Counted {
        /// The stream's threads named so.
        fn named(&self, name: impl Fn(&str) -> bool) -> impl Iterator<Item = &counting::Thread> {
            self.threads
                .iter()
                .filter(move |t| t.name != self.viewer && name(&thread(t)))
        }

        /// Every thread's allocations, a frame, with their sizes.
        fn said(&self) -> String {
            let said: Vec<String> = self
                .threads
                .iter()
                .map(|t| {
                    let sizes: Vec<String> = t
                        .sizes
                        .iter()
                        .flatten()
                        .map(|(size, n)| format!("{size} B x {n}"))
                        .collect();
                    format!(
                        "{}: {:.2} a frame ({})",
                        thread(t),
                        t.allocations as f64 / self.frames as f64,
                        sizes.join(", ")
                    )
                })
                .collect();
            format!(
                "{} frames; by thread (the viewer is this test's own):\n{}\nand {} times on \
                 threads past the table",
                self.frames,
                said.join("\n"),
                self.others
            )
        }
    }

    #[cfg(target_os = "linux")]
    fn thread(t: &counting::Thread) -> String {
        String::from_utf8_lossy(&t.name)
            .trim_end_matches('\0')
            .to_owned()
    }

    /// A thread parks on a parking_lot condition variable, surely: it says it is ready under
    /// the lock and waits, and is told to go only once the lock is free, which waiting frees.
    #[cfg(target_os = "linux")]
    fn parked_once() {
        let state = Arc::new((
            parking_lot::Mutex::new((false, false)),
            parking_lot::Condvar::new(),
        ));
        let waiter = {
            let state = state.clone();
            std::thread::spawn(move || {
                let (lock, cv) = &*state;
                let mut s = lock.lock();
                s.0 = true;
                while !s.1 {
                    cv.wait(&mut s);
                }
            })
        };
        let (lock, cv) = &*state;
        loop {
            let mut s = lock.lock();
            if s.0 {
                s.1 = true;
                cv.notify_one();
                break;
            }
        }
        waiter.join().unwrap();
    }

    /// A steady run of the capture loop, its stream and one viewer, counted: in a process
    /// of its own (see `in_a_process_of_its_own`), so None in the first.
    ///
    /// The loop runs as `spawn` runs it, on its own thread, over a 1024x768 screen the test
    /// draws on (a `Canvas`, captured by a `Drawing` that keeps damage as the X11 capturer
    /// does, so no X server is needed), at the defaults, with one viewer: the stream it
    /// makes for that viewer is the one a real viewer gets. The test draws text scrolling
    /// through rows of any height, and plays the viewer: its socket, taking each frame off
    /// its lane, and its relay, saying each taken. First each
    /// kind of step goes through 24 times, which brings every buffer the
    /// stream keeps to its size; then 60 generated steps are counted. The test's own thread
    /// is the viewer, so it is not the stream's.
    ///
    /// What it cannot see: libaom and libyuv are C, and allocate with C's malloc, which the
    /// counting allocator does not count; nor what the X11 capturer does over its socket.
    #[cfg(target_os = "linux")]
    fn steady_state(name: &str, convert_threads: usize) -> Option<Counted> {
        if !in_a_process_of_its_own(name) {
            return None;
        }
        let (w, h) = (1024u32, 768u32);
        let cfg = VideoConfig {
            convert_threads,
            ..VideoConfig::new(
                ":0".into(),
                Debugging::read(|_| None).unwrap(),
                std::num::NonZeroUsize::new(4).unwrap(),
            )
        };
        let hub = Hub::new(crate::hub::tests::hello()).unwrap();
        let client = viewer(&hub);
        client.watch();
        let canvas = Arc::new(Canvas {
            w,
            h,
            drawn: parking_lot::Mutex::new(Drawn {
                bgrx: vec![0; (w * h * 4) as usize],
                rows: None,
            }),
            news: crate::hub::Wake::new().unwrap(),
            done: std::sync::atomic::AtomicBool::new(false),
        });
        let mut drawing = Drawing {
            canvas: canvas.clone(),
            damage: Damage::Unreported,
            rows: crate::convert::Rows::new(cfg.convert_threads).unwrap(),
            frame: vec![0; (w * h * 4) as usize],
        };
        let capture = {
            let (cfg, hub) = (cfg.clone(), hub.clone());
            let coded = av1::Stream::new(
                av1::Shape {
                    width: w,
                    height: h,
                },
                cfg.cores,
            )
            .unwrap();
            // Nothing asks this test's screen for a size: it keeps the one it has.
            struct Kept((u32, u32));
            impl Resize for Kept {
                fn resize(&mut self, _: u32, _: u32) -> Result<(u32, u32), String> {
                    Ok(self.0)
                }
            }
            std::thread::Builder::new()
                .name("capture".into())
                .spawn(move || capture_loop(&mut drawing, coded, cfg, hub, &mut Kept((w, h))))
                .unwrap()
        };
        // The viewer: it takes frames off its lane until there is one, as its socket does,
        // and says each taken, as its relay does.
        struct Viewer {
            hub: Arc<Hub>,
            client: Arc<crate::hub::Client>,
            frames: usize,
        }
        impl Viewer {
            fn frame(&mut self) {
                loop {
                    let updates = self.client.lane(|s| s.delivered(Instant::now()));
                    if updates.is_empty() {
                        // Nothing yet: the stream's threads are coding it.
                        std::thread::yield_now();
                        continue;
                    }
                    self.hub.wake();
                    self.frames += updates.len();
                    return;
                }
            }
        }
        let mut viewer = Viewer {
            hub: hub.clone(),
            client,
            frames: 0,
        };
        let mut rng = Xorshift::new(0x2545F4914F6CDD1D);
        let mut next = move |n: u64| rng.below(n);
        // Text scrolls: a line, a block of lines, the whole screen.
        let mut step = |n: usize, viewer: &mut Viewer| {
            let y0 = next(h as u64) as u32;
            let y1 = match next(3) {
                0 => (y0 + 48).min(h),
                1 => y0 + 1 + next((h - y0) as u64) as u32,
                2.. => h,
            };
            canvas.draw(y0, y1, n);
            viewer.frame();
        };
        viewer.frame();
        let mut n = 0;
        for _ in 0..72 {
            n += 1;
            step(n, &mut viewer);
        }
        // parking_lot makes its table of parked threads (1 KB) the first time any thread
        // parks: a park now, so the count does not catch the process's first contended
        // lock.
        parked_once();
        let test = counting::thread_name();
        let before = viewer.frames;
        counting::arm();
        for _ in 0..60 {
            n += 1;
            step(n, &mut viewer);
        }
        let (threads, others) = counting::disarm();
        let counted = viewer.frames - before;
        canvas.done.store(true, Ordering::Relaxed);
        hub.wake();
        capture.join().unwrap().unwrap();
        Some(Counted {
            threads,
            others,
            frames: counted,
            viewer: test,
        })
    }

    /// A steady-state frame allocates nothing in the capture loop or its conversion
    /// threads: conversion runs on threads started with the capturer.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_steady_state_frame_allocates_nothing_capturing() {
        let Some(c) = steady_state(
            "video::tests::a_steady_state_frame_allocates_nothing_capturing",
            4,
        ) else {
            return;
        };
        let capturing = |t: &str| t == "capture" || t == "convert";
        let all: usize = c.named(capturing).map(|t| t.allocations).sum();
        assert_eq!(all, 0, "{}", c.said());
    }

    /// A steady-state frame allocates nothing in the encode loop, its lanes included.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_steady_state_frame_allocates_nothing_encoding() {
        let Some(c) = steady_state(
            "video::tests::a_steady_state_frame_allocates_nothing_encoding",
            4,
        ) else {
            return;
        };
        let all: usize = c.named(|t| t == "encode").map(|t| t.allocations).sum();
        assert_eq!(all, 0, "{}", c.said());
    }

    /// One look, as the capture loop reads it: the capturer's damage, whether a viewer
    /// waits to start, the last capture, the pacing, the gate, the encoder (none running,
    /// or one with the buffer home or away), and the area the damage comes to when taken.
    #[derive(Clone, Copy)]
    struct StepCase {
        damage: Damage,
        key: Key,
        now: Instant,
        captured: Instant,
        min_interval: Duration,
        gate: Gated,
        encoder: Encoder<(), ()>,
        band: Option<Band>,
    }

    impl StepCase {
        fn look(&self) -> Look {
            Look {
                key: self.key,
                damage: self.damage,
                now: self.now,
                captured: self.captured,
                min_interval: self.min_interval,
                gate: self.gate,
            }
        }
    }

    /// How many looks `each_case` makes: four frame rates, three damages, seven times since
    /// the last capture, the gate open or held, the encoder's three states, and 4 ways for
    /// the rest.
    const CASES: usize = 4 * 3 * 7 * 2 * 3 * 4;

    /// Every look over a discretised domain: each state the capturer's damage takes; four
    /// frame rates, at seven times since the last capture, either side of the least interval
    /// and its rounding to whole milliseconds; a viewer waiting to start or not; the gate
    /// open or held; no stream running, or one with the buffer home or out; damage that
    /// came to an area or to none.
    fn each_case(mut check: impl FnMut(StepCase)) -> usize {
        let ns = Duration::from_nanos;
        let us = Duration::from_micros;
        let ms = Duration::from_millis;
        let now = Instant::now() + Duration::from_secs(10);
        let damages = [
            Damage::None,
            Damage::Unreported,
            Damage::Rows {
                first: now - ms(3),
                y0: 16,
                y1: 48,
            },
        ];
        let encoders = [
            Encoder::Stopped(()),
            Encoder::Running(Buffer::Home(())),
            Encoder::Running(Buffer::Away),
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
            for (damage, since) in damages
                .into_iter()
                .flat_map(|d| sinces.map(move |s| (d, s)))
            {
                for (gate, encoder, case) in [Gated::Open, Gated::Held]
                    .into_iter()
                    .flat_map(|g| encoders.map(move |e| (g, e)))
                    .flat_map(|(g, e)| (0..4).map(move |c| (g, e, c)))
                {
                    let bit = |i: u32| case >> i & 1 == 1;
                    check(StepCase {
                        damage,
                        key: [Key::NotWanted, Key::Wanted][usize::from(bit(0))],
                        now,
                        captured: now - since,
                        min_interval,
                        gate,
                        encoder,
                        band: bit(1).then_some(Band { y0: 16, y1: 48 }),
                    });
                    cases += 1;
                }
            }
        }
        cases
    }

    /// A look waits for the first of these that holds it, and only for that: a still screen
    /// (nobody waiting to start, and nothing damaged or no stream running to show it on),
    /// for a change; the rest of the least interval since the last capture; a viewer's link
    /// that holds the gate, with no time. Else it starts a stream if none runs, captures
    /// if the encoder is idle, or waits for the buffer to come home. So no stream starts,
    /// and no capture goes, while the gate is held. Checked over every look of `each_case`.
    #[test]
    fn a_capture_waits_only_for_what_holds_it() {
        let cases = each_case(|c| {
            let got = step(c.look(), c.encoder);
            let nothing = c.damage == Damage::None || c.encoder == Encoder::Stopped(());
            let still = nothing && c.key == Key::NotWanted;
            let since = c.now - c.captured;
            let early = (since < c.min_interval).then(|| c.min_interval - since);
            // A wait leaves the encoder as it was; a capture takes the buffer.
            let wait = |w| Step::Wait(w, c.encoder);
            let want = match (still, early, c.gate, c.encoder) {
                (true, _, _, _) => wait(Wait::Change),
                (false, Some(rest), _, _) => wait(Wait::Interval { rest }),
                (false, None, Gated::Held, _) => wait(Wait::Gate),
                (false, None, Gated::Open, Encoder::Running(Buffer::Away)) => wait(Wait::Buffer),
                (false, None, Gated::Open, Encoder::Running(Buffer::Home(()))) => Step::Capture(()),
                (false, None, Gated::Open, Encoder::Stopped(())) => Step::Start(()),
            };
            assert_eq!(got, want, "still {still}, early {early:?}, {:?}", c.encoder);
        });
        assert_eq!(cases, CASES);
    }

    /// Each wait wakes on what it waits for, and ends on its time: a change on damage or a
    /// wake, with no limit; the interval only on a wake (damage meanwhile goes with the
    /// capture it holds back), or once over; the gate on damage or a wake (a lane emptied),
    /// with no limit: no time opens it; a buffer only on a wake, with no limit. Over every
    /// wait `step` makes in `each_case`.
    #[test]
    fn a_wait_wakes_on_what_it_waits_for() {
        let mut waits = 0;
        each_case(|c| {
            let Step::Wait(wait, _) = step(c.look(), c.encoder) else {
                return;
            };
            waits += 1;
            let mut polled = None;
            wait.through(|on, t| polled = Some((on, t)));
            let want = match wait {
                Wait::Change => (On::Event, None),
                Wait::Interval { rest } => (On::Wake, Some(rest)),
                Wait::Gate => (On::Event, None),
                Wait::Buffer => (On::Wake, None),
            };
            assert_eq!(polled, Some(want), "{wait:?}");
        });
        assert!(waits > 0);
    }

    /// A capture is a keyframe exactly when one was wanted and promised to a viewer: the
    /// only way one is planned; else what moved, if anything did; else nothing is sent. Over
    /// every look of `each_case`, with the keyframe promised or not.
    #[test]
    fn a_capture_is_sent_as_what_it_holds() {
        let mut cases = 0;
        each_case(|c| {
            for promised in [false, true] {
                let wanted = c.key == Key::Wanted;
                let starts = (wanted && promised).then_some(());
                let shown = format!("{:?}, band {:?}, {promised}", c.key, c.band);
                match plan(starts, c.band) {
                    Planned::Send(Plan::Key(())) => assert!(wanted && promised, "{shown}"),
                    Planned::Send(Plan::Motion) => {
                        assert!(starts.is_none() && c.band.is_some(), "{shown}")
                    }
                    Planned::Empty => assert!(starts.is_none() && c.band.is_none(), "{shown}"),
                }
                cases += 1;
            }
        });
        assert_eq!(cases, 2 * CASES);
    }

    /// The capturer's damage, against what was drawn: over every sequence of five events
    /// from a new capturer (rows drawn at three places, a take), damage is due exactly when
    /// the capturer is new or something was drawn since the last take; it has rows exactly
    /// when rows were drawn, stamped with the first of them, spanning them all; and a take
    /// returns those rows widened to even rows within the picture, and leaves nothing.
    #[test]
    fn damage_is_what_was_drawn_since_the_take() {
        let t0 = Instant::now();
        let height = 101;
        let rows = [(0, 1), (17, 48), (90, 101)];
        for seq in 0..4u32.pow(5) {
            let mut damage = Damage::Unreported;
            // Since the last take: when rows were first drawn and which, and
            // whether anything at all is due (a new capturer is).
            let mut drawn: Option<(Instant, u32, u32)> = None;
            let mut due: bool;
            for i in 0..5 {
                let now = t0 + Duration::from_millis(i as u64);
                match seq / 4u32.pow(i) % 4 {
                    r @ 0..=2 => {
                        let (top, bot) = rows[r as usize];
                        damage.add(top, bot, now);
                        drawn = Some(match drawn {
                            Some((first, a, b)) => (first, a.min(top), b.max(bot)),
                            None => (now, top, bot),
                        });
                        due = true;
                    }
                    3.. => {
                        let band = damage.take(height).map(|b| (b.y0, b.y1));
                        let want = drawn.map(|(_, a, b)| (a & !1, ((b + 1) & !1).min(height)));
                        assert_eq!(band, want, "sequence {seq}, event {i}");
                        assert_eq!(damage, Damage::None);
                        (drawn, due) = (None, false);
                    }
                }
                let want = match (drawn, due) {
                    (Some((first, y0, y1)), _) => Damage::Rows { first, y0, y1 },
                    (None, true) => Damage::Unreported,
                    (None, false) => Damage::None,
                };
                assert_eq!(damage, want, "sequence {seq}, event {i}");
            }
        }
    }

    /// A frame a simulated session showed: when its change was drawn (a keyframe with no
    /// change behind it, when it was let go), when it was sent and told taken, its bytes,
    /// what the link knew of the path when it was sent, and how long the path was silent
    /// while it was on the way.
    struct Shown {
        drawn: Instant,
        sent: Instant,
        took: Instant,
        bytes: u64,
        path: Path,
        silent: Duration,
        /// A keyframe is the whole screen, and starts its viewer.
        coding: Coding,
        /// The bytes sent before it and not yet told taken, when it was sent.
        ahead: u64,
        /// What the link let be on the way when its capture was let go: its window, or
        /// nothing while it knew nothing of the path.
        allowed: u64,
        /// The gate held its capture back, at a look before the one that let it go.
        held: bool,
    }

    impl Shown {
        /// The round trip the link knew when it was sent, once it knew the path.
        fn fastest(&self) -> Option<Duration> {
            match self.path {
                Path::Known(known) => Some(known.round_trip),
                Path::Unknown => None,
            }
        }

        /// The rate the link knew when it was sent, in bytes a second.
        fn rate(&self) -> Option<u64> {
            match self.path {
                Path::Known(known) => Some(known.rate.0),
                Path::Unknown => None,
            }
        }
    }

    /// What a simulated path changes to, mid-session.
    #[derive(Clone, Copy)]
    enum Change {
        RoundTrip(Duration),
        /// Bytes a second.
        Rate(u64),
    }

    /// The least and the most a simulated frame takes to code, in milliseconds: what the
    /// daemon's encoder was measured to take a frame at this quality.
    ///
    /// @limitKind measured
    /// @owner the daemon's encoder, as these tests simulate it
    /// @unit milliseconds a frame
    /// @scope the simulated sessions of this module's tests
    /// @evidence the daemon's own encode timings on its server (its `encode_us_total` over
    /// `encoded`, read by `voxl stats`) under its previous encoder; and the codec report's
    /// §3 table for the server, libaom speed 10 at 8 threads, not measured again here
    /// @observed 2026-11-18 and 2026-11-19
    /// @environment the daemon at this quality on its server, a 2912x1736 desktop; libaom
    /// 3.13.1 there at 3024x1896
    /// @workload scrolled text
    /// @result 15 to 27 ms a frame before; libaom deltas 15.1 to 18.7 ms at the median, 21.9
    /// to 33.0 ms at the 90th percentile, a forced keyframe 34.7 to 37.0 ms
    /// @decision the first range, a frame's time drawn evenly from it: libaom's medians fall
    /// inside it, so the session rates measured with it (`SHOWN_A_SECOND`,
    /// `AFTER_A_PAUSE_A_SECOND`) stand
    /// @revalidate `bench_hot_path` on the server's hardware under the daemon itself
    const CODING_MS: (u64, u64) = (15, 27);

    /// What a simulated session's viewer or its screen does, mid-session.
    #[derive(Clone, Copy, Debug)]
    enum Turn {
        /// The screen changes size: the stream is rebuilt, and the viewer starts over.
        Resize,
        /// `v,0`.
        StopWatching,
        /// `v,1`.
        Watch,
    }

    /// A frame at a simulated encoder: when it is coded, and what `Shown` says of it that
    /// its capture decided.
    #[derive(Clone, Copy)]
    struct Work {
        done: Instant,
        drawn: Instant,
        bytes: u64,
        coding: Coding,
        allowed: u64,
        held: bool,
    }

    /// A simulated session's stream: none, or one whose encoder is idle or codes a frame.
    #[derive(Clone, Copy)]
    enum Sim {
        Stopped,
        Idle,
        Busy(Work),
    }

    /// A session over `path`, on a virtual clock, from `start`: one viewer, with a lane,
    /// who watches from the first. Its desktop draws the changes of `draws` (when, and the
    /// bytes the screen then codes to; a whole screen, a keyframe, codes to `whole`); the
    /// capture loop's own decision (`step`) says what to do at each look, with the gate the
    /// session's link gives; a capture is coded for a time within `CODING_MS` and sent if
    /// its viewer has started, and the path says each frame taken. The screen as it is
    /// goes next: a change drawn while another waits replaces it, and a keyframe holds both.
    ///
    /// Around `step`, the session does what the capture loop and the hub do: a stream ends
    /// when its screen is resized, once its encoder has sent what it holds, and the loop
    /// looks at nothing until then; a rebuilt stream starts its viewer over; nobody watching
    /// is looked at by nobody, and a viewer that comes back holding the newest frame goes on
    /// from it; a viewer waiting to start is promised the next keyframe let go, and starts
    /// when that keyframe is coded, if it is still promised it.
    fn session(
        path: &mut Simulated,
        (start, whole): (Instant, u64),
        draws: &[(Instant, u64)],
        // The path changes: when, and to what.
        change: Option<(Instant, Change)>,
        turns: &[(Instant, Turn)],
        end: Instant,
    ) -> Vec<Shown> {
        let interval = Duration::from_secs_f64(1.0 / f64::from(FRAMERATE));
        let min_interval = interval.mul_f64(MIN_INTERVAL_SHARE);
        let mut link = Link::default();
        let (mut shown, mut next) = (Vec::new(), 0u32);
        let mut coding_times = Xorshift::new(0x1b87_3593_cc9e_2d51);
        // The change waiting to be captured: when the first of it was drawn, and its bytes.
        let mut drawn: Option<(Instant, u64)> = None;
        let (mut viewer, mut sim) = (Viewer::Waiting, Sim::Stopped);
        // The stream's newest frame, and the next frame's name.
        let (mut head, mut versions) = (None, 0u32);
        // The stream is to end, and its viewer to start over on the one rebuilt.
        let (mut ending, mut rebuilt) = (false, false);
        // The gate has held a capture back since the last one went.
        let mut held = false;
        let mut draws = draws.iter().copied().peekable();
        let mut turns = turns.iter().copied().peekable();
        let (mut now, mut captured) = (start, start - interval);
        while now < end {
            match change.filter(|(at, _)| *at <= now) {
                Some((_, Change::RoundTrip(delay))) => path.delay = delay,
                Some((_, Change::Rate(rate))) => path.rate = rate,
                None => {}
            }
            while let Some((at, bytes)) = draws.next_if(|(at, _)| *at <= now) {
                drawn = Some((drawn.map_or(at, |(first, _)| first), bytes));
            }
            while let Some((_, turn)) = turns.next_if(|(at, _)| *at <= now) {
                match turn {
                    Turn::Resize => (ending, rebuilt) = (true, true),
                    Turn::StopWatching => viewer = crate::hub::stopped(viewer),
                    Turn::Watch => viewer = crate::hub::watching(viewer, head),
                }
            }
            path.tell(now, |sequence, at| link.took(sequence, at));
            if let Sim::Busy(frame) = sim {
                if frame.done <= now {
                    sim = Sim::Idle;
                    // The frame is the stream's newest; the keyframe a viewer was promised
                    // starts it; only a viewer that has started is sent a frame, and holds it.
                    let version = crate::wire::Version(versions);
                    versions += 1;
                    head = Some(version);
                    viewer = match (viewer, frame.coding) {
                        (Viewer::Promised, Coding::Key)
                        | (Viewer::Started { .. }, Coding::Key | Coding::Delta) => {
                            Viewer::Started { through: version }
                        }
                        (Viewer::Promised, Coding::Delta)
                        | (Viewer::NotWatching(_) | Viewer::Waiting, _) => viewer,
                    };
                    if let Viewer::Started { .. } = viewer {
                        let Work {
                            done, drawn, bytes, ..
                        } = frame;
                        let stats = link.stats(done);
                        let sequence = crate::wire::Sequence(next);
                        next += 1;
                        link.sent(sequence, bytes, done);
                        let took = path.send(sequence, bytes, Duration::ZERO, done);
                        shown.push(Shown {
                            drawn,
                            sent: done,
                            took,
                            bytes,
                            path: stats.path,
                            silent: path.silent(done, took),
                            coding: frame.coding,
                            ahead: stats.outstanding_bytes,
                            allowed: frame.allowed,
                            held: frame.held,
                        });
                    }
                }
            }
            // With nobody watching, the stream waits as it was: only a resize ends it.
            let encoder = match (sim, ending) {
                // The stream's end waits for its encoder: nothing looks meanwhile.
                (Sim::Busy(_), true) => None,
                (Sim::Stopped | Sim::Idle, true) => {
                    (sim, ending) = (Sim::Stopped, false);
                    if std::mem::take(&mut rebuilt) {
                        viewer = crate::hub::restarted(viewer);
                        head = None;
                    }
                    Some(Encoder::Stopped(()))
                }
                (Sim::Stopped, false) => Some(Encoder::Stopped(())),
                (Sim::Idle, false) => Some(Encoder::Running(Buffer::Home(()))),
                (Sim::Busy(_), false) => Some(Encoder::Running(Buffer::Away)),
            };
            let key = match viewer {
                Viewer::Waiting => Key::Wanted,
                Viewer::NotWatching(_) | Viewer::Promised | Viewer::Started { .. } => {
                    Key::NotWanted
                }
            };
            let look = Look {
                key,
                damage: match drawn {
                    Some((first, _)) => Damage::Rows {
                        first,
                        y0: 0,
                        y1: 2,
                    },
                    None => Damage::None,
                },
                now,
                captured,
                min_interval,
                gate: match link.gate(now) {
                    Gate::Open => Gated::Open,
                    Gate::Full | Gate::Late => Gated::Held,
                },
            };
            // Nobody watching is looked at by nobody, as in the loop.
            let stepped = encoder
                .filter(|_| !matches!(viewer, Viewer::NotWatching(_)))
                .map(|encoder| step(look, encoder));
            let wait = match stepped {
                Some(Step::Start(()) | Step::Capture(())) => {
                    // A keyframe is the whole screen, and holds what was drawn; a frame
                    // that is not one is what was drawn.
                    let (first, bytes) = match (key, drawn.take()) {
                        (Key::Wanted, drawn) => (drawn.map_or(now, |(first, _)| first), whole),
                        (Key::NotWanted, Some(drawn)) => drawn,
                        (Key::NotWanted, None) => panic!("a capture with nothing to show"),
                    };
                    if key == Key::Wanted {
                        viewer = Viewer::Promised;
                    }
                    captured = now;
                    let (least, most) = CODING_MS;
                    let takes = Duration::from_millis(coding_times.range(least, most + 1));
                    sim = Sim::Busy(Work {
                        done: now + takes,
                        drawn: first,
                        bytes,
                        coding: match key {
                            Key::Wanted => Coding::Key,
                            Key::NotWanted => Coding::Delta,
                        },
                        allowed: match link.stats(now).path {
                            Path::Known(known) => known.window(),
                            Path::Unknown => 0,
                        },
                        held: std::mem::take(&mut held),
                    });
                    None
                }
                Some(Step::Wait(Wait::Interval { rest }, _)) => Some(now + rest),
                Some(Step::Wait(Wait::Gate, _)) => {
                    held = true;
                    None
                }
                Some(Step::Wait(Wait::Change | Wait::Buffer, _)) | None => None,
            };
            // The next thing that can change a look: a change drawn, a turn, a took, the
            // encoder done, the interval over, the path changed.
            let events = [
                draws.peek().map(|(at, _)| *at),
                turns.peek().map(|(at, _)| *at),
                path.tooks.front().map(|(at, _)| *at),
                match sim {
                    Sim::Busy(frame) => Some(frame.done),
                    Sim::Stopped | Sim::Idle => None,
                },
                wait,
                change.map(|(at, _)| at),
            ];
            match events.into_iter().flatten().filter(|at| *at > now).min() {
                Some(at) => now = at,
                None => break,
            }
        }
        shown
    }

    /// A value at a percentile of `values`, which it sorts.
    fn percentile(values: &mut [Duration], p: usize) -> Duration {
        values.sort();
        values[(values.len() - 1) * p / 100]
    }

    /// What the sessions of `sessions_over_the_measured_paths` were measured to show, which
    /// the test holds them to: the least frames a second of each, in the order the test
    /// runs them (the hotspot's scroll, typing, and typing with whole screens; then the
    /// office-like path's).
    ///
    /// @limitKind measured
    /// @owner this module's tests
    /// @unit frames a second, over a session of 30 s
    /// @scope `sessions_over_the_measured_paths`
    /// @evidence the test's own output (`--nocapture`): it prints every session's frames a
    /// second and the percentiles of its frames' time on the way
    /// @observed 2026-11-19, the measuring machine's local day
    /// @environment the path simulated in `session::link`'s tests, on a virtual clock, as
    /// the two paths were measured: one of 1,860 KB/s and a round trip of 236 ms that goes
    /// silent both ways for 0.43 s every 1.8 s; one of 2,400 KB/s, no silences, whose round
    /// trip steps from 106 to 243 ms halfway
    /// @workload a keyframe of 400 KB, then for 30 s: a scroll (a frame of 20 to 25 KB wanted
    /// every sixtieth of a second), typing (2 KB every 120 ms), and typing with a whole
    /// screen of 400 KB every 5 s; a frame coded in 15 to 27 ms (`CODING_MS`), which alone
    /// leaves a scroll 47.6 frames a second
    /// @result frames a second, and a frame's time from its send to its took at the median,
    /// the 90th percentile and the most. The silent path: scroll 36.70 (249, 638, 679 ms),
    /// of the 36.2 its silences leave of 47.6; typing 8.03 (237, 604, 615 ms); with whole
    /// screens 7.43 (237, 604, 881 ms). The other: scroll 45.57 (251, 253, 273 ms), the
    /// same before its round trip steps as after; typing 8.03 (244, 244, 273 ms); with
    /// whole screens 7.83 (244, 244, 410 ms)
    /// @decision each measured rate less a hundredth of a frame a second: a floor, not a
    /// goal
    /// @revalidate a change to the link's gate or estimators, or to the capture decision
    const SHOWN_A_SECOND: [f64; 6] = [36.69, 8.02, 7.42, 45.56, 8.02, 7.82];

    /// Sessions over the two paths as they were measured, the capture loop's own decision
    /// driving them: a scroll, typing, and typing with whole-screen changes, each after a
    /// keyframe. With the path's own word as the oracle:
    /// - every frame is told taken within twice the round trip the link knew when it was
    ///   sent, the response time, its own bytes' time on the path, and the time the path
    ///   was silent while it was on the way: no frame waits behind more than a window;
    /// - each session shows no fewer frames a second than it was measured to
    ///   (`SHOWN_A_SECOND`): the path's silences cost the frames drawn during them and no
    ///   others, and a round trip grown longer costs none.
    #[test]
    fn sessions_over_the_measured_paths() {
        use crate::session::link::RESPONSE;
        let start = Instant::now();
        let secs = Duration::from_secs_f64;
        let long = 30.0;
        let end = start + secs(long);
        let mut rng = Xorshift::new(0x0fc1_9dc6_8b8c_d5b5);
        // Each starts on a keyframe, a whole screen; then its changes from a second on.
        let key = (start, 400_000u64);
        let every = |gap: f64, bytes: &mut dyn FnMut() -> u64| -> Vec<(Instant, u64)> {
            let n = ((long - 1.0) / gap) as u32;
            (0..n)
                .map(|i| (start + secs(1.0 + f64::from(i) * gap), bytes()))
                .collect()
        };
        let scroll = every(1.0 / 60.0, &mut || 20_000 + rng.below(5_000));
        let typing = every(0.12, &mut || 2_000);
        let mut screens = typing.clone();
        screens.extend((1..6).map(|i| (start + secs(f64::from(i) * 5.0 + 0.05), 400_000)));
        screens.sort();
        let silences = Some((secs(1.8), secs(0.43)));
        let longer = Change::RoundTrip(Duration::from_millis(243));
        let step = Some((start + secs(long / 2.0), longer));
        let paths = [
            ("hotspot", 1_860_000, 236, silences, None),
            ("office", 2_400_000, 106, None, step),
        ];
        let works = [
            ("scroll", &scroll),
            ("typing", &typing),
            ("typing and whole screens", &screens),
        ];
        let mut least = SHOWN_A_SECOND.into_iter();
        for (name, rate, delay, holes, step_to) in paths {
            for (work, draws) in works {
                let mut path = Simulated::new(rate, Duration::from_millis(delay), start);
                path.holes = holes;
                let mut shown = session(&mut path, key, draws, step_to, &[], end);
                shown.retain(|f| f.took <= end);
                let mut steady = 0;
                for f in &shown {
                    let Some(fastest) = f.fastest() else {
                        continue;
                    };
                    let (lag, own) = (f.took - f.sent, path.time(f.bytes));
                    let grown = match step_to {
                        Some((_, Change::RoundTrip(to))) => to,
                        Some((_, Change::Rate(_))) | None => Duration::ZERO,
                    };
                    assert!(
                        lag <= fastest * 2 + RESPONSE + own + f.silent + grown,
                        "{name}, {work}: a frame {lag:?} on the way, the round trip {fastest:?}"
                    );
                    steady += usize::from(lag <= fastest + RESPONSE + own + f.silent);
                }
                let a_second = shown.len() as f64 / long;
                let mut on_the_way: Vec<Duration> = shown.iter().map(|f| f.took - f.sent).collect();
                let mut seen: Vec<Duration> = shown.iter().map(|f| f.took - f.drawn).collect();
                let lowest = shown
                    .iter()
                    .filter(|f| f.sent > start + secs(3.0))
                    .filter_map(|f| f.rate())
                    .min();
                println!(
                    "{name}, {work}: {a_second:.2} frames a second; sent to taken p50 {:?}, p90 \
                     {:?}, most {:?}; drawn to taken p50 {:?}, p90 {:?}, most {:?}; {steady} of \
                     {} within the steady bound; the least rate after 3 s {lowest:?}",
                    percentile(&mut on_the_way, 50),
                    percentile(&mut on_the_way, 90),
                    percentile(&mut on_the_way, 100),
                    percentile(&mut seen, 50),
                    percentile(&mut seen, 90),
                    percentile(&mut seen, 100),
                    shown.len(),
                );
                let floor = least.next().expect("a floor for every session");
                assert!(
                    a_second >= floor,
                    "{name}, {work}: {a_second:.1} frames a second"
                );
            }
        }
    }

    /// No frame is coded but one the gate let go, a stream's first as any other. Over
    /// generated sessions on generated paths that stall (silent both ways for seconds at
    /// a time), a scroll running throughout, with resizes, `v,0` then `v,1`, and `v,1`
    /// again, most of them while the path is stalled: whenever a frame is sent, what was
    /// sent before it and not yet told taken is no more than the link allowed on the way
    /// when its capture was let go (its window; nothing, while it knew nothing of the
    /// path). So what is on the way is never more than that and the one frame let go,
    /// however many streams start meanwhile. The sessions are held to have started streams
    /// that the gate held back, and streams that it did not.
    #[test]
    fn a_stream_starts_only_when_the_gate_lets_it() {
        let mut rng = Xorshift::new(0x3c6e_f372_fe94_f82b);
        let start = Instant::now();
        let secs = Duration::from_secs_f64;
        let long = 20.0;
        let (mut starts, mut held) = (0, 0);
        for case in 0..60 {
            let rate = 300_000 + rng.below(6_000_000);
            let mut path = Simulated::new(rate, Duration::from_millis(rng.range(5, 300)), start);
            // Silent for one to three seconds, every four to six.
            let (period, silence) = (rng.range(4_000, 6_000), rng.range(1_000, 3_000));
            path.holes = Some((
                Duration::from_millis(period),
                Duration::from_millis(silence),
            ));
            let whole = 100_000 + rng.below(400_000);
            let draws: Vec<(Instant, u64)> = (0..(long * 60.0) as u32)
                .map(|i| (start + secs(f64::from(i) / 60.0), 5_000 + rng.below(20_000)))
                .collect();
            // Three turns in four fall in a silence of the path.
            let mut turns: Vec<(Instant, Turn)> = Vec::new();
            for _ in 0..rng.range(10, 40) {
                let at = match rng.below(4) {
                    0 => rng.below((long * 1000.0) as u64),
                    1.. => period * rng.range(1, 4) + rng.below(silence),
                };
                let at = start + Duration::from_millis(at);
                match rng.below(8) {
                    0..4 => turns.push((at, Turn::Resize)),
                    4..7 => {
                        let back = at + Duration::from_millis(rng.range(1, 800));
                        turns.extend([(at, Turn::StopWatching), (back, Turn::Watch)]);
                    }
                    7.. => turns.push((at, Turn::Watch)),
                }
            }
            turns.sort_by_key(|(at, _)| *at);
            let end = start + secs(long);
            let shown = session(&mut path, (start, whole), &draws, None, &turns, end);
            for f in &shown {
                assert!(
                    f.ahead <= f.allowed,
                    "case {case}: a frame of {} went behind {} on the way, {} allowed",
                    f.bytes,
                    f.ahead,
                    f.allowed
                );
            }
            let keys = shown.iter().filter(|f| f.coding == Coding::Key);
            starts += keys.clone().count();
            held += keys.filter(|f| f.held).count();
        }
        assert!(
            held > 100 && starts - held > 100,
            "{starts} keyframes, {held} of them held by the gate"
        );
    }

    /// The most round trips the link's rate took to come within a tenth of a path's that
    /// fell to a third in the middle of a scroll, in `a_pause_a_silence_and_a_slower_path`:
    /// what the rate's memory was measured to take, which the test holds it to.
    ///
    /// @limitKind measured
    /// @owner this module's tests
    /// @unit round trips of the path, a frame's own time on it included
    /// @scope `a_pause_a_silence_and_a_slower_path`
    /// @evidence the test's own output (`--nocapture`): it prints the round trips, and the
    /// longest a frame was on the way meanwhile
    /// @observed 2026-11-19, the measuring machine's local day
    /// @environment the two measured paths as `sessions_over_the_measured_paths` simulates
    /// them, on a virtual clock
    /// @workload a keyframe of 400 KB, then a scroll (a frame of 20 to 25 KB wanted every
    /// sixtieth of a second) for 20 s; the path's rate falls to a third 8 s in
    /// @result 15.1 round trips on the silent path, 8.5 on the other: the ten of the rate's
    /// memory, and the time the frames already on the way took to show the fall. Meanwhile
    /// a frame was on the way for up to 0.96 s on the silent path (a silence of 0.43 s
    /// among it) and 0.39 s on the other, against 0.61 s and 0.34 s for a frame the window
    /// lets go at the path's true rate: what was sent into a window three times too wide
    /// @decision the measured most, rounded up to the next whole round trip
    /// @revalidate a change to the link's rate, its memory or what presses a path
    const FOLLOWS_WITHIN_TRIPS: f64 = 16.0;

    /// The fewest frames a second a scroll showed in its first round trip after a pause, in
    /// `a_pause_a_silence_and_a_slower_path`.
    ///
    /// @limitKind measured
    /// @owner this module's tests
    /// @unit frames a second, over the scroll's first round trip
    /// @scope `a_pause_a_silence_and_a_slower_path`
    /// @evidence the test's own output (`--nocapture`): it prints each
    /// @observed 2026-11-19, the measuring machine's local day
    /// @environment the two measured paths as `sessions_over_the_measured_paths` simulates
    /// them, on a virtual clock
    /// @workload a keyframe of 400 KB, a scroll for 4 s, a still screen or typing for 5 s,
    /// then the scroll again
    /// @result the silent path: 44 after a still screen, 44 after typing; the other path:
    /// 52 and 43; of the 47.6 a second that a frame coded in 15 to 27 ms leaves a scroll.
    /// The rate the link held through each pause was the rate the first scroll left
    /// (1,600 and 1,606 KB/s on the silent path, 1,467 KB/s on the other)
    /// @decision the measured least, rounded down to a whole frame a second
    /// @revalidate a change to the link's rate, its memory or what presses a path
    const AFTER_A_PAUSE_A_SECOND: f64 = 42.0;

    /// A pause does not forget the path, a silence is not a rate, and a path grown slower
    /// is still followed. Over the two measured paths, the capture loop's own decision
    /// driving each session:
    /// - a scroll, then five seconds of a still screen or of typing (which on the silent
    ///   path meets its silences), then a scroll again: the link's rate is never lower in
    ///   the pause or at the second scroll's start than the first scroll left it, and the
    ///   second scroll shows no fewer than `AFTER_A_PAUSE_A_SECOND` frames a second in its
    ///   first round trip, of the 47.6 its coding time leaves it;
    /// - a scroll under which the path's rate falls to a third: the link's rate is within
    ///   a tenth of the fallen rate within `FOLLOWS_WITHIN_TRIPS` round trips, and from
    ///   then on every frame is on the way no longer than a frame the window lets go can
    ///   be, with the silence it met.
    #[test]
    fn a_pause_a_silence_and_a_slower_path() {
        use crate::session::link::RESPONSE;
        let start = Instant::now();
        let secs = Duration::from_secs_f64;
        let mut rng = Xorshift::new(0x0fc1_9dc6_8b8c_d5b5);
        let key = (start, 400_000u64);
        let scroll = |from: f64, to: f64, rng: &mut Xorshift| -> Vec<(Instant, u64)> {
            let n = ((to - from) * 60.0) as u32;
            (0..n)
                .map(|i| {
                    (
                        start + secs(from + f64::from(i) / 60.0),
                        20_000 + rng.below(5_000),
                    )
                })
                .collect()
        };
        let silences = Some((secs(1.8), secs(0.43)));
        for (name, rate, delay, holes) in [
            ("the silent path", 1_860_000u64, 236, silences),
            ("the other", 2_400_000, 106, None),
        ] {
            let delay = Duration::from_millis(delay);
            let path = || {
                let mut path = Simulated::new(rate, delay, start);
                path.holes = holes;
                path
            };
            // A round trip, a frame's own time on the path included.
            let trip = delay + path().time(25_000);
            let a_second = |shown: &[Shown], from: Instant| {
                let sent = shown
                    .iter()
                    .filter(|f| f.sent >= from && f.sent < from + trip);
                sent.count() as f64 / trip.as_secs_f64()
            };
            for pause in ["a still screen", "typing"] {
                let (paused, again) = (start + secs(4.5), start + secs(9.5));
                let mut draws = scroll(0.6, 4.5, &mut rng);
                if pause == "typing" {
                    draws.extend((1..41).map(|i| (paused + secs(f64::from(i) * 0.12), 2_000)));
                }
                draws.extend(scroll(9.5, 14.5, &mut rng));
                let shown = session(&mut path(), key, &draws, None, &[], again + secs(5.0));
                let rates = |from: Instant, to: Instant| {
                    let sent = shown.iter().filter(move |f| f.sent >= from && f.sent < to);
                    sent.filter_map(|f| f.rate())
                };
                let left = rates(start, paused)
                    .next_back()
                    .expect("the first scroll showed a rate");
                let least = rates(paused, again + trip)
                    .min()
                    .expect("the second scroll began");
                assert!(
                    least >= left,
                    "{name}, {pause}: the rate fell from {left} to {least}"
                );
                let first = a_second(&shown, again);
                println!(
                    "{name}, a scroll after 5 s of {pause}: {first:.0} frames a second in its \
                     first round trip, the rate {left} B/s before and {least} at the least since"
                );
                assert!(
                    first >= AFTER_A_PAUSE_A_SECOND,
                    "{name}, {pause}: {first:.0} frames a second"
                );
            }
            let fell = start + secs(8.0);
            let draws = scroll(0.6, 20.0, &mut rng);
            let (mut path, third) = (path(), rate / 3);
            let change = Some((fell, Change::Rate(third)));
            let shown = session(&mut path, key, &draws, change, &[], start + secs(20.0));
            let followed = shown
                .iter()
                .find(|f| f.sent > fell && f.rate().is_some_and(|r| r <= third / 10 * 11))
                .map(|f| f.sent)
                .unwrap_or_else(|| panic!("{name}: the rate never followed the path's"));
            let trips = (followed - fell).as_secs_f64() / trip.as_secs_f64();
            let meanwhile = shown
                .iter()
                .filter(|f| f.sent > fell - trip && f.sent < followed);
            let longest = meanwhile.map(|f| f.took - f.sent).max();
            println!(
                "{name}, its rate falls to a third under a scroll: followed in {trips:.1} round \
                 trips; a frame on the way for up to {longest:?} meanwhile"
            );
            assert!(
                trips <= FOLLOWS_WITHIN_TRIPS,
                "{name}: {trips:.1} round trips"
            );
            for f in shown.iter().filter(|f| f.sent > followed + trip * 2) {
                let fastest = f.fastest().expect("the path is known");
                let (lag, own) = (f.took - f.sent, path.time(f.bytes));
                assert!(
                    lag <= fastest * 2 + RESPONSE + own + f.silent,
                    "{name}: a frame {lag:?} on the way once the rate followed"
                );
            }
        }
    }
}
