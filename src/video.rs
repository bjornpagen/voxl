//! Video pipeline: damage-driven capture -> I420 -> VP9 -> each session's lane, a capture
//! held while anything waits for a session's lane socket.
//!
//! One stream codes the screen for every viewer. It starts with a keyframe for the viewers
//! it starts, and codes every later frame from the frames before it, which every viewer's
//! lane carried in order; a keyframe is coded only for a viewer starting (`hub::Starts`).
//! Every frame is coded at one quality: a slow link lowers the frame rate, through the gate.

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
use crate::vp9;
use bytes::{BufMut, BytesMut};
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // read by the Linux-only capture loop
#[derive(Clone)]
pub struct VideoConfig {
    /// The X display.
    pub display: String,
    /// The cores the encoder may use: `vp9::Stream::new` derives its threads from them.
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

/// What a video frame says before its VP9, on the lane: the encode loop writes it, and
/// the recorder, the relay (which hands it to the page) and web/src/protocol.ts read it,
/// each checking its reader against web/src/video-header.json. What a frame is (video or
/// audio) the lane's `Track` says, as the page's type byte. Every update refers only to the
/// keyframe before it on its lane and frames after that keyframe, all of which the lane
/// carried before it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Header {
    /// The frame is a keyframe.
    pub key: bool,
    pub pipe: Pipe,
}

/// Capture to send, as a video frame carries it: in tenths of a millisecond, at most
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
    /// The VP9 starts this far into a frame.
    pub const LEN: usize = 5;
    /// The flags byte's one bit.
    const KEY: u8 = 1;

    /// Flags, then pipe, big-endian.
    pub fn bytes(&self) -> [u8; Header::LEN] {
        let mut b = [0; Header::LEN];
        b[0] = if self.key { Header::KEY } else { 0 };
        b[1..5].copy_from_slice(&self.pipe.0.to_be_bytes());
        b
    }

    /// What frame `m` is to a reader of video: the recorder, and the fixture's check of
    /// the writer.
    pub fn read(m: &[u8]) -> Result<Video<'_>, Short> {
        match m.split_first_chunk::<{ Header::LEN }>() {
            None => Err(Short),
            Some((b, [])) => Ok(Video::Empty(Header::of(b))),
            Some((b, picture)) => Ok(Video::Picture(Header::of(b), picture)),
        }
    }

    fn of(b: &[u8; Header::LEN]) -> Header {
        Header {
            key: b[0] & Header::KEY != 0,
            pipe: Pipe(u32::from_be_bytes([b[1], b[2], b[3], b[4]])),
        }
    }
}

/// A video frame, read.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Video<'a> {
    /// Its header, and its VP9 after it.
    Picture(Header, &'a [u8]),
    /// A header with nothing after it. The encode loop sends no frame that coded nothing.
    Empty(Header),
}

/// Bytes too few for a video frame's header.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Short;

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

#[cfg(target_os = "linux")]
fn union(a: Option<Band>, b: Band) -> Band {
    match a {
        None => b,
        Some(a) => Band {
            y0: a.y0.min(b.y0),
            y1: a.y1.max(b.y1),
        },
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // read by the Linux-only capture loop
/// What the viewers watching are sent: the encoder, and the buffer captures are converted
/// into for it. There is one only while somebody watches, and it starts with a keyframe.
///
/// One buffer, since a capture goes only to an idle encoder (`step`): a second, filled
/// while the encoder codes the first, only queued a frame behind it, and converted every
/// band twice, once into each.
struct Stream {
    /// Dropped first (fields drop in order), which ends the encode loop once it has sent
    /// what it was given.
    job_tx: mpsc::SyncSender<Job>,
    /// The buffer back from the encoder, once its frame is on its viewers' lanes.
    ret_rx: mpsc::Receiver<Box<I420>>,
    buf: Buffer<Box<I420>>,
    /// The rows that changed on screen since the buffer was last converted into.
    pending: Option<Band>,
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
        coded: vp9::Stream,
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
            buf: Buffer::Away,
            pending: None,
            _encode: encode,
        })
    }
}

#[cfg(target_os = "linux")]
/// Runs the pipeline. With a `Screen` (dynamic resolution), the capture loop gives a
/// client's size request to the screen; only if the screen's size really changes does
/// the loop end: the encoder drains, the pipeline is rebuilt at the new size, and clients
/// get a keyframe of it.
pub fn spawn(
    cfg: VideoConfig,
    hub: Arc<Hub>,
    mut screen: Option<crate::screen::Screen>,
) -> Result<(), String> {
    if let Some(s) = &screen {
        *hub.resize_max.lock() = Some(s.max());
    }
    let (mut cap, mut coded) = build(&cfg, &hub)?;
    std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || loop {
            let looped = capture_loop(&mut cap, coded, cfg.clone(), hub.clone(), screen.as_mut());
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
fn build(cfg: &VideoConfig, hub: &Arc<Hub>) -> Result<(Capturer, vp9::Stream), String> {
    let cap = Capturer::new(&cfg.display, cfg.debug, cfg.convert_threads)?;
    let (w, h) = (cap.width, cap.height);
    let shape = vp9::Shape {
        width: w,
        height: h,
        fps: FRAMERATE,
    };
    let coded =
        vp9::Stream::new(shape, cfg.cores).map_err(|e| format!("no VP9 level holds {:?}", e.0))?;
    *hub.stream.lock() = Some(coded);
    // A desktop that cannot be resized declares its one size's codec, at its first build.
    hub.codec.get_or_init(|| coded.codec());
    log::info!(
        "capturing {w}x{h} @ {FRAMERATE} fps as {}, {} threads",
        coded.codec(),
        coded.threads
    );
    // Clients already connected start again on a keyframe of the new size; no-op at startup.
    hub.restart_clients();
    Ok((cap, coded))
}

/// Whether the sessions' lanes let a capture go: open while nothing waits for any viewer's
/// lane socket.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Gated {
    Open,
    /// Held by a viewer whose lane socket has not yet taken what waits for it: only the
    /// socket taking it, the lane ending, or a viewer coming or going can open the gate, and
    /// each of those wakes the hub.
    Held,
}

impl Gated {
    #[cfg(target_os = "linux")]
    /// What the look at the viewers saw of their lanes: held by any whose bytes wait. A
    /// recording bounds nothing.
    fn of(seen: &[(Arc<crate::hub::Client>, Seen)]) -> Gated {
        match seen.iter().any(|(_, v)| v.waits) {
            true => Gated::Held,
            false => Gated::Open,
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
enum Step<B> {
    /// Wait, the buffer where it was.
    Wait(Wait, Buffer<B>),
    /// Capture into the buffer that was home.
    Capture(B),
}

/// What the capture loop waits for before it looks again. Damage wakes it through the
/// capturer; the hub's waker, through a viewer's command, a lane emptying or ending, a viewer
/// leaving, a screenshot or recording asked for, or a buffer coming home from the encoder.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Wait {
    /// Damage or a wake: nothing is due.
    Change,
    /// The rest of the least interval since the last capture, or a wake: damage that comes
    /// meanwhile goes with the capture it holds back.
    Interval { rest: Duration },
    /// The lanes: a socket taking what waits, a lane ending, a viewer going (a wake), or
    /// damage to take in. No time: a capture held by the lanes waits for what they answer,
    /// and with nothing answered nothing goes.
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

/// What the capture loop knows when it looks, while a stream runs.
struct Look<B> {
    key: Key,
    damage: Damage,
    now: Instant,
    /// When the last capture went.
    captured: Instant,
    /// The least time between two captures.
    min_interval: Duration,
    gate: Gated,
    buffer: Buffer<B>,
}

/// What the loop does next, from what it knows. A capture takes the buffer, so one is
/// made only when it is home; a wait leaves it where it was.
fn step<B>(look: Look<B>) -> Step<B> {
    let Look {
        key,
        damage,
        now,
        captured,
        min_interval,
        gate,
        buffer,
    } = look;
    // Nothing moved and nobody waits to start: nothing to capture.
    if let (Damage::None, Key::NotWanted) = (damage, key) {
        return Step::Wait(Wait::Change, buffer);
    }
    let since = now.saturating_duration_since(captured);
    if since < min_interval {
        let rest = min_interval - since;
        return Step::Wait(Wait::Interval { rest }, buffer);
    }
    // A capture goes once every lane is empty.
    if gate == Gated::Held {
        return Step::Wait(Wait::Gate, buffer);
    }
    // A capture goes only to an idle encoder. One made while it codes the frame before
    // would wait for it, a whole encode older by the time it is coded; made once it is
    // done, it holds what changed meanwhile, and goes at once.
    match buffer {
        Buffer::Home(buf) => Step::Capture(buf),
        Buffer::Away => Step::Wait(Wait::Buffer, Buffer::Away),
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
        (Some(s), _) => Planned::Send(Plan::Key(s)),
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
    fn take_damage(&mut self) -> Result<Option<Band>, String>;
    /// Every row.
    fn full(&self) -> Band;
    /// The whole screen as BGRX, for a screenshot.
    fn snapshot(&mut self) -> Result<Vec<u8>, String>;
    /// Reads `band` of the screen into the frame `convert` reads.
    fn grab(&mut self, band: Band) -> Result<(), String>;
    /// Converts rows [y0, y1) of the grabbed frame into `dst`, on the capturer's threads.
    fn convert(&self, dst: &mut I420, y0: usize, y1: usize) -> Result<(), String>;
    /// The grabbed frame as BGRX and its stride (the debug marker's probe).
    fn bgrx(&self) -> Option<(&[u8], usize)>;
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
    fn take_damage(&mut self) -> Result<Option<Band>, String> {
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
    fn bgrx(&self) -> Option<(&[u8], usize)> {
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
/// Only a screen whose size really changed rebuilds it and says hello to the viewers; an
/// ask the screen refuses, or one for the size it has, changes nothing: a rebuild for
/// either would restart every viewer's stream and say hello for nothing.
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
    coded: vp9::Stream,
    cfg: VideoConfig,
    hub: Arc<Hub>,
    mut screen: Option<&mut crate::screen::Screen>,
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
    // The encoder, while somebody watches. Every way out of the loop drops it, and with it
    // waits for the encoder to send what it holds.
    let mut stream: Option<Stream> = None;
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
        if let (Some(s), Some(want)) = (screen.as_deref_mut(), asked) {
            if rebuilds_for(s, want, cap.size()) {
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
        if let Some(s) = stream.as_mut() {
            loop {
                match s.ret_rx.try_recv() {
                    Ok(buf) => s.buf = Buffer::Home(buf),
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
            // Nobody watches: a stream is dropped here, once its encoder has sent what it
            // holds.
            stream = None;
            Wait::Change.wait(cap, &hub)?;
            continue;
        }
        let key = Key::of(&seen);
        let Some(s) = stream.as_mut() else {
            // No stream: one starts with a keyframe of the whole screen, for the viewers
            // waiting to start, and with nobody waiting there is nothing to send.
            let starts = match key {
                Key::Wanted => hub.promise(&seen),
                Key::NotWanted => None,
            };
            let Some(starts) = starts else {
                Wait::Change.wait(cap, &hub)?;
                continue;
            };
            let (cw, ch) = cap.size();
            cap.grab(cap.full())?;
            // The whole screen is grabbed: it holds what was drawn while nobody watched, so
            // that damage is taken with it.
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
            stream = Some(Stream::start(&hub, coded, first, cfg.debug)?);
            continue;
        };
        let look = Look {
            key,
            damage: *cap.damage(),
            now,
            captured,
            min_interval,
            gate: Gated::of(&seen),
            buffer: std::mem::replace(&mut s.buf, Buffer::Away),
        };
        let mut buf = match step(look) {
            Step::Wait(wait, buffer) => {
                s.buf = buffer;
                wait.wait(cap, &hub)?;
                continue;
            }
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
        let band = cap.take_damage()?;
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
        let starts = match key {
            Key::Wanted => hub.promise(&seen),
            Key::NotWanted => None,
        };
        let frame = match plan(starts, band) {
            Planned::Send(frame) => frame,
            Planned::Empty => {
                s.buf = Buffer::Home(buf);
                continue;
            }
        };
        let todo = match (s.pending.take(), band) {
            (p, Some(b)) => Some(union(p, b)),
            (p, None) => p,
        };
        if let Some(t) = todo {
            let pic: &mut I420 = &mut buf;
            cap.convert(pic, t.y0 as usize, t.y1 as usize)?;
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

/// Codes each frame: the encoder, and what it puts out.
struct Coder {
    enc: vp9::Encoder,
    /// What libvpx put out for the last frame: kept, so a frame allocates nothing.
    out: Vec<u8>,
    arenas: Arenas,
    debug: bool,
}

impl Coder {
    /// The stream `coded`'s coder, and its first frame, `first`, a keyframe: a stream starts
    /// with the viewers it starts.
    fn start(
        coded: vp9::Stream,
        first: &I420,
        ms: i64,
        debug: bool,
    ) -> Result<(Coder, BytesMut), vp9::Failed> {
        let mut coder = Coder {
            enc: vp9::Encoder::new(&coded)?,
            out: Vec::new(),
            arenas: Arenas::new(),
            debug,
        };
        let coded = coder.code(first, ms, vp9::Coding::Key)?;
        Ok((coder, coded))
    }

    /// Codes `buf` at `ms` as `coding`: a header's room, then the VP9.
    fn code(&mut self, buf: &I420, ms: i64, coding: vp9::Coding) -> Result<BytesMut, vp9::Failed> {
        let planes = vp9::Planes {
            y: &buf.y,
            u: &buf.u,
            v: &buf.v,
            stride_y: buf.width,
            stride_uv: buf.stride_uv(),
        };
        self.out.clear();
        let te = Instant::now();
        self.enc.encode(&planes, ms, coding, &mut self.out)?;
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
        Ok(self.arenas.message(&self.out))
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
    coded: vp9::Stream,
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
/// until the capture loop is gone; a frame libvpx fails is the error, which ends the stream.
fn encode_frames(
    coded: vp9::Stream,
    first: Job,
    rx: mpsc::Receiver<Job>,
    ret: mpsc::SyncSender<Box<I420>>,
    hub: &Hub,
    debug: bool,
) -> Result<(), vp9::Failed> {
    let epoch = Instant::now();
    let ms = |job: &Job| job.damage.saturating_duration_since(epoch).as_millis() as i64;
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
    let (mut coder, out) = Coder::start(coded, &first.buf, ms(&first), debug)?;
    let mut sent = |job: Job, mut bytes: BytesMut, t0: Instant| {
        let enc_us = t0.elapsed().as_micros() as u64;
        let header = Header {
            key: matches!(job.frame, Plan::Key(_)),
            pipe: Pipe::of(job.damage.elapsed()),
        };
        bytes[..Header::LEN].copy_from_slice(&header.bytes());
        let bytes = bytes.freeze();
        hub.stats.encode_us.fetch_add(enc_us, Ordering::Relaxed);
        hub.stats.encoded.fetch_add(1, Ordering::Relaxed);
        hub.stats
            .bytes
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        // The viewers a keyframe starts start on it, and are sent it.
        if let Plan::Key(starts) = &job.frame {
            hub.stats.keyframes.fetch_add(1, Ordering::Relaxed);
            starts.started();
        }
        hub.clients_into(&mut viewers);
        for c in &viewers {
            c.takes(&bytes);
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
            Plan::Key(_) => vp9::Coding::Key,
            Plan::Motion => vp9::Coding::Delta,
        };
        let out = coder.code(&job.buf, ms(&job), coding)?;
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
    use crate::session::sender::{Queue, Sender};
    use crate::xorshift::Xorshift;

    /// A session of `hub` with a lane, which no writer drains, and the lane's queue.
    fn viewer(hub: &Hub) -> (Arc<crate::hub::Client>, Arc<Queue>) {
        let (sender, queue) = Sender::with_lane();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        (hub.add_client(tx, sender), queue)
    }

    /// The stream a test codes: `w` x `h` on one thread.
    fn coded(w: u32, h: u32) -> vp9::Stream {
        let shape = vp9::Shape {
            width: w,
            height: h,
            fps: FRAMERATE,
        };
        vp9::Stream::new(shape, std::num::NonZeroUsize::MIN).unwrap()
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
            _ => inside(body, rest),
        }
    }

    /// A recording is an MP4 whose boxes parse and hold the frames recorded, and which
    /// ffmpeg decodes: `ftyp`, then `moov`, then `mdat`, tiling the file; one track as large
    /// as the screen, a `vp09` sample entry whose `vpcC` names the stream's level, a sample
    /// table counting every frame the recorder took, one chunk inside `mdat`; and ffmpeg,
    /// every error fatal, decodes exactly that many pictures of that size. Through the real
    /// encode loop with the recorder as a viewer, over runs of 1 to 20 frames after the
    /// keyframe that starts it.
    #[tokio::test]
    async fn a_recording_is_an_mp4_of_its_frames() {
        let (w, h) = (64u32, 64u32);
        let mut rng = Xorshift::new(0x1f83_d9ab_fb41_bd6b);
        for run in 0..6 {
            let hub = Hub::new().unwrap();
            *hub.stream.lock() = Some(coded(w, h));
            let (stop, stopped) = tokio::sync::oneshot::channel();
            let recording = tokio::spawn({
                let hub = hub.clone();
                async move {
                    let until = crate::record::Until::Stopped(stopped);
                    crate::record::record(&hub, until, Duration::from_secs(3600)).await
                }
            });
            while hub.clients.lock().is_empty() {
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
            let stsd = inside(traks[0], &[b"mdia", b"minf", b"stbl", b"stsd"]).unwrap();
            let entry = boxes(&stsd[8..]).unwrap();
            assert_eq!(entry[0].0, *b"vp09");
            // The visual sample entry's 78 bytes, then its boxes: `vpcC`, version 1, then
            // profile 0, the level, and 8 bits.
            let vpcc = inside(&entry[0].1[78..], &[b"vpcC"]).unwrap();
            assert_eq!(&vpcc[..4], &[1, 0, 0, 0]);
            assert_eq!(
                (vpcc[4], vpcc[5], vpcc[6] >> 4),
                (0, coded(w, h).level(), 8)
            );
            let path: [&[u8; 4]; 4] = [b"mdia", b"minf", b"stbl", b"stsz"];
            let stsz = inside(traks[0], &path).expect("a sample table");
            assert_eq!(u32_at(stsz, 8) as usize, n + 1, "samples against frames");
            let stco = inside(traks[0], &[b"mdia", b"minf", b"stbl", b"stco"]).unwrap();
            let at = u32_at(stco, 8) as usize;
            assert!(at >= mdat_at && at < mp4.len(), "a chunk outside mdat");
            let file = std::env::temp_dir()
                .join(format!("voxl-recording-{}-{run}.mp4", std::process::id()));
            std::fs::write(&file, &mp4).unwrap();
            let pictures = vp9::tests::decode(&file);
            std::fs::remove_file(&file).unwrap();
            let frame = (w * h * 3 / 2) as usize;
            assert_eq!(pictures.unwrap().len(), (n + 1) * frame, "run {run}");
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

    /// Every viewer's lane carries a stream that decodes from the keyframe it starts on,
    /// through the real encode loop, a replaced lane included: over generated runs, a viewer
    /// watches from the start, a second joins partway, and a third's lane is replaced partway
    /// (the relay reopened it) and again later; a keyframe is coded whenever one waits to
    /// start, as the capture loop codes one. The first viewer's lane is the whole stream.
    /// Each lane's updates begin with a keyframe, and decode, by ffmpeg with every error
    /// fatal, to exactly what the whole stream decodes them to.
    #[test]
    fn every_lane_decodes_from_its_keyframe() {
        let (w, h) = (64u32, 64u32);
        let mut rng = Xorshift::new(0x510e_527f_ade6_82d1);
        let dir = std::env::temp_dir().join(format!("voxl-video-gate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut lanes_checked = 0;
        for case in 0..8 {
            let hub = Hub::new().unwrap();
            let (whole, whole_lane) = viewer(&hub);
            whole.watch();
            let (moved, first_lane) = viewer(&hub);
            moved.watch();
            let first = job(w, h, Plan::Key(promised(&hub)), &mut rng);
            let s = Stream::start(&hub, coded(w, h), first, Debugging::read(|_| None).unwrap())
                .unwrap();
            s.ret_rx.recv().unwrap();
            let (join, reopen) = (rng.range(5, 50), rng.range(5, 40));
            let again = reopen + rng.range(1, 20);
            // Each lane's updates, as its socket took them; the lane `moved` has now.
            let mut lanes: Vec<Vec<Vec<u8>>> = Vec::new();
            let mut moving = first_lane;
            let mut joined = None;
            for f in 0..60 {
                if f == join {
                    let (b, lane) = viewer(&hub);
                    b.watch();
                    joined = Some((b, lane));
                }
                if f == reopen || f == again {
                    // The socket took what the lane held before it broke.
                    lanes.push(moving.updates());
                    moving = Queue::new(Arc::new(|| {}));
                    assert!(moved.attach(moving.clone()));
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
            lanes.push(moving.updates());
            lanes.extend(joined.map(|(_, lane)| lane.updates()));
            let decode = |name: &str, frames: &[Vec<u8>]| {
                let mut file = vp9::tests::ivf_header(w as usize, h as usize);
                for (n, b) in frames.iter().enumerate() {
                    vp9::tests::ivf_frame(&mut file, n as u64, &b[Header::LEN..]);
                }
                let path = dir.join(format!("{name}{case}.ivf"));
                std::fs::write(&path, file).unwrap();
                vp9::tests::decode(&path).unwrap_or_else(|e| panic!("case {case} {name}: {e}"))
            };
            let frame = (w * h * 3 / 2) as usize;
            let all = whole_lane.updates();
            let truth = decode("whole", &all);
            assert_eq!(truth.len(), all.len() * frame, "case {case}");
            for (l, got) in lanes.iter().enumerate() {
                assert!(!got.is_empty(), "case {case}: lane {l} carried nothing");
                assert_eq!(got[0][0] & 1, 1, "case {case}: lane {l} starts on a delta");
                // Where the lane's first update is in the whole stream: the same bytes.
                let at = all.iter().position(|u| *u == got[0]).unwrap();
                assert_eq!(got[..], all[at..at + got.len()], "case {case}: lane {l}");
                let pictures = decode(&format!("lane{l}-"), got);
                assert!(
                    pictures[..] == truth[at * frame..(at + got.len()) * frame],
                    "case {case}: lane {l}"
                );
                lanes_checked += 1;
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(lanes_checked, 8 * 4);
    }

    /// A viewer that joins while the stream is rebuilt (a resize, a display switch)
    /// starts on the rebuilt stream: no frame of the old stream reaches its lanes, whose
    /// decoder would take it as the start of the stream its hello then resets. Steps, the
    /// loops' own: a page joins and the capture loop plans its keyframe; a resize ends the
    /// capture loop, and the real encode loop sends that keyframe while a second page has
    /// just joined; then `build`'s `restart_clients` says hello. The first page's lane has
    /// the keyframe, so the test is of the stream, not of a loop that sent nothing.
    #[test]
    fn a_viewer_that_joins_while_the_stream_is_rebuilt_starts_on_the_rebuilt_stream() {
        let hub = Hub::new().unwrap();
        let (w, h) = (64, 64);
        let (page, page_lane) = viewer(&hub);
        page.watch();
        let mut rng = Xorshift::new(0x6a09_e667_f3bc_c908);
        let first = job(w, h, Plan::Key(promised(&hub)), &mut rng);
        // A resize ends the capture loop, and a second page joins, before the encode loop
        // has coded that keyframe.
        let (second, second_lane) = viewer(&hub);
        second.watch();
        let s =
            Stream::start(&hub, coded(w, h), first, Debugging::read(|_| None).unwrap()).unwrap();
        drop(s);
        // build(): the rebuilt stream's hello.
        hub.restart_clients();
        assert_eq!(
            page_lane.updates().len(),
            1,
            "the first page had no keyframe"
        );
        assert!(
            second_lane.updates().is_empty(),
            "the second page's lane had a frame of the old stream before its hello"
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
        fn take_damage(&mut self) -> Result<Option<Band>, String> {
            Ok(self.damage.take(self.canvas.h))
        }
        fn full(&self) -> Band {
            Band {
                y0: 0,
                y1: self.canvas.h,
            }
        }
        fn grab(&mut self, _band: Band) -> Result<(), String> {
            Ok(())
        }
        fn convert(&self, dst: &mut I420, y0: usize, y1: usize) -> Result<(), String> {
            let d = self.canvas.drawn.lock();
            let stride = self.canvas.w as usize * 4;
            self.rows.convert(&d.bgrx, stride, dst, y0, y1)
        }
        fn snapshot(&mut self) -> Result<Vec<u8>, String> {
            Ok(self.canvas.drawn.lock().bgrx.clone())
        }
        fn bgrx(&self) -> Option<(&[u8], usize)> {
            None
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
    /// through rows of any height, and lets the screen go still until its pass goes when
    /// one is due; and plays the viewer's socket, taking each frame off its lane. First each
    /// kind of step goes through 24 times, which brings every buffer the
    /// stream keeps to its size; then 60 generated steps are counted. The test's own thread
    /// is the viewer, so it is not the stream's.
    ///
    /// What it cannot see: libvpx and libyuv are C, and allocate with C's malloc, which the
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
        let hub = Hub::new().unwrap();
        let (client, lane) = viewer(&hub);
        client.watch();
        drop(client);
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
        };
        let capture = {
            let (cfg, hub) = (cfg.clone(), hub.clone());
            let coded = vp9::Stream::new(
                vp9::Shape {
                    width: w,
                    height: h,
                    fps: FRAMERATE,
                },
                cfg.cores,
            )
            .unwrap();
            std::thread::Builder::new()
                .name("capture".into())
                .spawn(move || capture_loop(&mut drawing, coded, cfg, hub, None))
                .unwrap()
        };
        // The viewer: it takes frames off its lane until there is one, as its socket does.
        struct Viewer {
            lane: Arc<Queue>,
            frames: usize,
        }
        impl Viewer {
            fn frame(&mut self) {
                loop {
                    let updates = self.lane.updates().len();
                    if updates == 0 {
                        // Nothing yet: the stream's threads are coding it.
                        std::thread::yield_now();
                        continue;
                    }
                    self.frames += updates;
                    return;
                }
            }
        }
        let mut viewer = Viewer { lane, frames: 0 };
        let mut rng = Xorshift::new(0x2545F4914F6CDD1D);
        let mut next = move |n: u64| rng.below(n);
        // Text scrolls: a line, a block of lines, the whole screen.
        let mut step = |n: usize, viewer: &mut Viewer| {
            let y0 = next(h as u64) as u32;
            let y1 = match next(3) {
                0 => (y0 + 48).min(h),
                1 => y0 + 1 + next((h - y0) as u64) as u32,
                _ => h,
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
    /// waits to start, the last capture, the pacing, the gate, whether the buffer is home, and
    /// the area the damage comes to when taken.
    #[derive(Clone, Copy)]
    struct StepCase {
        damage: Damage,
        want_key: bool,
        now: Instant,
        captured: Instant,
        min_interval: Duration,
        gate: Gated,
        home: bool,
        band: Option<Band>,
    }

    impl StepCase {
        fn buffer(&self) -> Buffer<()> {
            match self.home {
                true => Buffer::Home(()),
                false => Buffer::Away,
            }
        }
        fn look(&self) -> Look<()> {
            Look {
                key: match self.want_key {
                    true => Key::Wanted,
                    false => Key::NotWanted,
                },
                damage: self.damage,
                now: self.now,
                captured: self.captured,
                min_interval: self.min_interval,
                gate: self.gate,
                buffer: self.buffer(),
            }
        }
    }

    /// How many looks `each_case` makes: four frame rates, three damages, seven times since
    /// the last capture, the gate open or held, and 8 ways for the rest.
    const CASES: usize = 4 * 3 * 7 * 2 * 8;

    /// Every look over a discretised domain: each state the capturer's damage takes; four
    /// frame rates, at seven times since the last capture, either side of the least interval
    /// and its rounding to whole milliseconds; a viewer waiting to start or not; the lanes
    /// empty (the gate open) or not; the buffer home or out; damage that came to an area or
    /// to none.
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
                for (gate, case) in [Gated::Open, Gated::Held]
                    .into_iter()
                    .flat_map(|g| (0..8).map(move |c| (g, c)))
                {
                    let bit = |i: u32| case >> i & 1 == 1;
                    check(StepCase {
                        damage,
                        want_key: bit(0),
                        now,
                        captured: now - since,
                        min_interval,
                        gate,
                        home: bit(1),
                        band: bit(2).then_some(Band { y0: 16, y1: 48 }),
                    });
                    cases += 1;
                }
            }
        }
        cases
    }

    /// A look waits for the first of these that holds it, and only for that, else it
    /// captures if the encoder is idle, or waits for the buffer to come home: a still screen
    /// (nothing damaged, nobody waiting to start), for a change; the rest of the least
    /// interval since the last capture; bytes waiting for a lane's socket, with no time.
    /// Checked over every look of `each_case`.
    #[test]
    fn a_capture_waits_only_for_what_holds_it() {
        let cases = each_case(|c| {
            let got = step(c.look());
            let still = c.damage == Damage::None && !c.want_key;
            let since = c.now - c.captured;
            let early = (since < c.min_interval).then(|| c.min_interval - since);
            // A wait leaves the buffer where it was; a capture takes it.
            let wait = |w| Step::Wait(w, c.buffer());
            let want = match (still, early, c.gate, c.home) {
                (true, _, _, _) => wait(Wait::Change),
                (false, Some(rest), _, _) => wait(Wait::Interval { rest }),
                (false, None, Gated::Held, _) => wait(Wait::Gate),
                (false, None, Gated::Open, false) => wait(Wait::Buffer),
                (false, None, Gated::Open, true) => Step::Capture(()),
            };
            assert_eq!(got, want, "still {still}, early {early:?}, home {}", c.home);
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
            let Step::Wait(wait, _) = step(c.look()) else {
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
                let starts = (c.want_key && promised).then_some(());
                let shown = format!("key {}, band {:?}, {promised}", c.want_key, c.band);
                match plan(starts, c.band) {
                    Planned::Send(Plan::Key(())) => assert!(c.want_key && promised, "{shown}"),
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
    /// the capturer is new or something was drawn since the last take; it has rows exactly when rows were drawn, stamped with the first of them,
    /// spanning them all; and a take returns those rows widened to even rows
    /// within the picture, and leaves nothing.
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
                    _ => {
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

    /// The tests' generator, drawing the header's values.
    struct Rng(Xorshift);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0.next()
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
            pipe: Pipe(rng.any() as u32),
        }
    }

    #[test]
    fn a_header_reads_back_as_it_was_written() {
        let mut rng = Rng(Xorshift::new(0x9e37_79b9_7f4a_7c15));
        for _ in 0..100_000 {
            let header = random_header(&mut rng);
            let picture: Vec<u8> = (0..1 + rng.next() % 64).map(|_| rng.next() as u8).collect();
            let message = [&header.bytes()[..], &picture].concat();
            assert_eq!(Header::read(&message), Ok(Video::Picture(header, &picture)));
        }
    }

    #[test]
    fn a_header_alone_is_empty_and_less_is_short() {
        let mut rng = Rng(Xorshift::new(0x2545_f491_4f6c_dd1d));
        for _ in 0..100_000 {
            let header = random_header(&mut rng);
            let bytes = header.bytes();
            assert_eq!(Header::read(&bytes), Ok(Video::Empty(header)));
            let cut = (rng.next() % Header::LEN as u64) as usize;
            assert_eq!(Header::read(&bytes[..cut]), Err(Short));
        }
    }

    #[test]
    fn a_pipe_is_whole_tenths_of_a_millisecond() {
        let mut rng = Rng(Xorshift::new(0x6a09_e667_f3bc_c908));
        let tenth = Duration::from_micros(100);
        for _ in 0..100_000 {
            let d = Duration::from_nanos(rng.any());
            let Pipe(p) = Pipe::of(d);
            // As many whole tenths as fit, or as many as a header holds.
            assert!(tenth * p <= d, "{d:?} as {p}");
            if p < u32::MAX {
                assert!(d < tenth * (p + 1), "{d:?} as {p}");
            }
        }
    }

    /// One case of web/src/video-header.json.
    #[derive(serde::Deserialize)]
    struct Case {
        header: String,
        picture: String,
        key: bool,
        pipe: u32,
    }

    fn hex(s: &str) -> Vec<u8> {
        let digits: String = s.split_whitespace().collect();
        (0..digits.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&digits[i..i + 2], 16).unwrap())
            .collect()
    }

    /// The page messages web/src/protocol.ts checks its reader against carry what the
    /// daemon writes and reads: each is the relay's one page-type byte, then the header.
    #[test]
    fn the_header_is_what_the_fixture_says() {
        let cases: Vec<Case> =
            serde_json::from_str(include_str!("../web/src/video-header.json")).unwrap();
        for c in cases {
            let header = Header {
                key: c.key,
                pipe: Pipe(c.pipe),
            };
            let page = hex(&c.header);
            let bytes = &page[1..];
            assert_eq!(header.bytes().to_vec(), bytes, "{}", c.header);
            let picture = hex(&c.picture);
            let message = [bytes, &picture].concat();
            assert_eq!(
                Header::read(&message),
                Ok(Video::Picture(header, &picture)),
                "{}",
                c.header
            );
        }
    }

    /// The hot path at production's shape, as a benchmark (`--ignored`, by name; wall
    /// clock, not a test): one 3024x1898 page of text, coded and sent onto a session's lane
    /// as the encode loop does, a capture a frame interval apart; the lane's socket takes
    /// each frame once it is on the lane. Typing-sized updates (a glyph changed) and
    /// scroll-sized ones (the page moved up 8 rows), 600 captures each, at each thread
    /// count from one, doubling, to the count `vp9::Stream::new` derives for this machine's
    /// cores and that count itself. Prints, per workload and thread count, the CPU each
    /// second of screen takes, the bytes a frame, and percentiles of each capture's latency
    /// from its job to its picture home, its frame on the lane: 60 frames a second needs the
    /// p99 at or under 16.7 ms.
    #[test]
    #[ignore = "a benchmark: run by name with --ignored --nocapture"]
    fn bench_hot_path() {
        const W: usize = 3024;
        const H: usize = 1898;
        const CAPTURES: usize = 600;
        /// Captures before allocations are counted: the stream's buffers grow to size.
        const WARM: usize = 120;
        // The process's CPU time.
        let cpu = || {
            // SAFETY: getrusage writes the struct it is given.
            let mut u: libc::rusage = unsafe { std::mem::zeroed() };
            unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
            let t = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
            t(u.ru_utime) + t(u.ru_stime)
        };
        if !in_a_process_of_its_own("video::tests::bench_hot_path") {
            return;
        }
        let cores = std::thread::available_parallelism().unwrap();
        let derived = vp9::Stream::new(
            vp9::Shape {
                width: W as u32,
                height: H as u32,
                fps: FRAMERATE,
            },
            cores,
        )
        .unwrap();
        let mut counts: Vec<u32> = std::iter::successors(Some(1u32), |t| Some(t * 2))
            .take_while(|&t| t < derived.threads)
            .chain([derived.threads])
            .collect();
        counts.dedup();
        println!(
            "{} cores; {} derives {} threads",
            cores,
            derived.codec(),
            derived.threads
        );
        let mut rng = Xorshift::new(0x0123_4567_89ab_cdef);
        let page = vp9::tests::page(W, H + 8 * CAPTURES, &mut rng);
        let workloads = counts
            .into_iter()
            .flat_map(|t| [("typing", false, t), ("scroll", true, t)]);
        for (name, scroll, threads) in workloads {
            let hub = Hub::new().unwrap();
            let (client, lane) = viewer(&hub);
            client.watch();
            drop(client);
            let mut buf = Box::new(I420::new(W, H));
            buf.y.copy_from_slice(&page[..W * H]);
            let first = Job {
                buf,
                frame: Plan::Key(promised(&hub)),
                damage: Instant::now(),
                marker_ms: None,
            };
            let s = Stream::start(
                &hub,
                derived.on(threads),
                first,
                Debugging::read(|_| None).unwrap(),
            )
            .unwrap();
            let mut buf = s.ret_rx.recv().unwrap();
            let mut latencies = Vec::with_capacity(CAPTURES);
            let mut bytes = 0;
            // The test's socket reading the lane back: not the daemon's work.
            let mut relay_us = 0;
            let (wall, cpu0) = (Instant::now(), cpu());
            for i in 1..=CAPTURES {
                // Allocations counted past the warm-up: libvpx's own (C's malloc) are not.
                if i == WARM {
                    counting::arm();
                }
                match scroll {
                    true => buf.y.copy_from_slice(&page[8 * i * W..(8 * i + H) * W]),
                    false => {
                        let (gx, gy) = (8 + (i * 9) % (W - 16), 900 + (i * 9 / W) * 20 % 600);
                        for y in gy..gy + 12 {
                            for x in gx..gx + 7 {
                                buf.y[y * W + x] = buf.y[y * W + x].wrapping_add(97);
                            }
                        }
                    }
                }
                let t0 = Instant::now();
                s.job_tx
                    .send(Job {
                        buf,
                        frame: Plan::Motion,
                        damage: t0,
                        marker_ms: None,
                    })
                    .unwrap();
                buf = s.ret_rx.recv().unwrap();
                latencies.push(t0.elapsed());
                let relay0 = thread_cpu_us().unwrap();
                bytes += lane.updates().iter().map(Vec::len).sum::<usize>();
                relay_us += thread_cpu_us().unwrap() - relay0;
                let next = t0 + Duration::from_micros(16_667);
                std::thread::sleep(next.saturating_duration_since(Instant::now()));
            }
            let (counted, _) = counting::disarm();
            let encoding = counted
                .iter()
                .filter(|t| t.name.starts_with(b"encode"))
                .map(|t| t.allocations)
                .sum::<usize>();
            let (wall, used) = (wall.elapsed(), cpu() - cpu0);
            let used = used - Duration::from_micros(relay_us as u64);
            drop(s);
            latencies.sort();
            let at = |p: usize| latencies[(latencies.len() - 1) * p / 100];
            println!(
                "{name}, {threads} threads: {:.3} CPU s per screen s; {} B a frame; latency \
                 p50 {:?} p90 {:?} p99 {:?} max {:?}; {:.2} allocations a capture on the \
                 encode thread",
                used.as_secs_f64() / wall.as_secs_f64(),
                bytes / CAPTURES,
                at(50),
                at(90),
                at(99),
                at(100),
                encoding as f64 / (CAPTURES - WARM) as f64,
            );
        }
    }
}
