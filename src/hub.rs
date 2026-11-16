//! Shared state between media producers (video/audio/cursor/clipboard) and websocket clients.

use crate::timeline::FrameId;
use crate::window::{self, Way, Window};
use bytes::Bytes;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

pub enum Out {
    Bin(Bytes),
    Text(ServerMessage),
}

/// A text message to a viewer: JSON, named by its `t`. `ServerMessage` in
/// web/src/protocol.ts names the same five, and `json` is the one place one is written.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum ServerMessage {
    Hello(Hello),
    /// Names a ping; the viewer's reply echoes it.
    Ping {
        s: Token,
    },
    /// The desktop's clipboard, as text.
    Clip {
        d: String,
    },
    /// Whether the desktop's microphone is this viewer's.
    Mic {
        on: bool,
    },
    /// This viewer's subscription is over.
    Close,
}

impl ServerMessage {
    pub fn json(&self) -> String {
        serde_json::to_string(self).expect("a message to a viewer is JSON")
    }
}

/// The `hello` message: the stream's geometry and the server's settings, the fields
/// `Hello` in web/src/protocol.ts names.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Hello {
    pub w: u32,
    pub h: u32,
    /// Encoded stripes as (y, height).
    pub stripes: Vec<(u32, u32)>,
    #[serde(flatten)]
    pub settings: Settings,
}

/// What a hello tells a viewer besides the geometry. Each part has its writer: `run` says
/// what voxl started with, once, before it serves a viewer; on macOS each pipeline says
/// its displays; dynamic resolution says its scale, from `run` and on macOS from each
/// pipeline.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct Settings {
    /// None until `run` has started all it reports: only the first pipeline's hello,
    /// which goes to nobody, is without it.
    #[serde(flatten)]
    pub startup: Option<Startup>,
    #[cfg(target_os = "macos")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub displays: Option<Displays>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resize: Option<Resize>,
}

impl Settings {
    /// What `run` says, once it has started everything a viewer is told of.
    pub fn start(&mut self, startup: Startup, resize: Option<Resize>) {
        self.startup = Some(startup);
        if resize.is_some() {
            self.resize = resize;
        }
    }

    /// What a macOS pipeline says when it is built: its displays, and its scale if it
    /// resizes.
    #[cfg(target_os = "macos")]
    pub fn pipeline(&mut self, displays: Displays, resize: Option<Resize>) {
        self.displays = Some(displays);
        self.resize = resize;
    }
}

/// What voxl started with, as a viewer needs it.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Startup {
    /// Desktop audio is sent.
    pub audio: bool,
    pub files: FilesConfig,
    /// The browser's microphone can be the desktop's.
    pub mic: bool,
    /// The server's platform, as Rust names it (`std::env::consts::OS`).
    pub os: &'static str,
    /// Viewers only watch; said only when they do.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
}

/// What a viewer may do with files (`FilesConfig` in web/src/protocol.ts).
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct FilesConfig {
    /// The folder, relative to the shared root, the panel opens and uploads into.
    pub dir: String,
    pub download: bool,
    pub upload: bool,
}

/// Dynamic resolution: desktop pixels per CSS pixel of the viewer's window.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
pub struct Resize {
    pub scale: f64,
}

/// macOS: the displays a viewer may switch between, and the one it sees.
#[cfg(target_os = "macos")]
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Displays {
    pub available: Vec<Display>,
    pub selected: u32,
}

/// macOS: one display, by its CoreGraphics ID.
#[cfg(target_os = "macos")]
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Display {
    pub id: u32,
    pub name: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Inflight {
    pub id: FrameId,
    pub bytes: usize,
    /// When it left, and what its viewer's path was asked to carry with it, as its window
    /// took it in.
    pub left: window::Left,
}

/// How long a client says it kept a frame between its arrival and the ack (decode,
/// draw, frames ahead of it), as the ack's optional last field.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Hold {
    /// An older client, or the recorder: its delay samples include its own time.
    Unsaid,
    Said(Duration),
    /// The field isn't a time (not a number, negative, more than a Duration holds):
    /// the frame is acknowledged and says nothing about the path.
    Unreadable,
}

impl Hold {
    /// From the ack's hold field, in milliseconds. An ack without one is `Unsaid`.
    pub fn parse(field: &str) -> Hold {
        field
            .trim()
            .parse::<f64>()
            .ok()
            .and_then(|ms| Duration::try_from_secs_f64(ms / 1000.0).ok())
            .map_or(Hold::Unreadable, Hold::Said)
    }
}

/// How often a client is pinged: every 500 ms, as 93ad2d4 does. Its round trip is the
/// shortest of its last 20 replies, so a path that changes shows within 10 s.
pub const PING_EVERY: Duration = Duration::from_millis(500);

/// The longest round trip a client's replies can show: 4 s. A client whose oldest frame on
/// its way is that old has stalled, and the flow gate judges it by that alone (video.rs),
/// so a longer round trip steers nothing; the bench's links have 80 to 140 ms. At half, a
/// path of 2 to 4 s would give its window no ping's round trip, only its frames'; at
/// double, a client's record would keep twice the pings for nothing.
const LONGEST_ROUND_TRIP: Duration = crate::video::STALLED;

/// The pings a client's record keeps: one goes out every `PING_EVERY`, so this many are
/// out at once on the longest round trip, 8.
const PINGS_KEPT: usize = (LONGEST_ROUND_TRIP.as_millis() / PING_EVERY.as_millis()) as usize;

/// What names a ping, and what its reply echoes: 64 bits from the operating system's
/// random source, 16 hex digits on the wire. A ping used to be named by the time it went
/// out, which a viewer can tell before the ping reaches it (one goes every `PING_EVERY`,
/// late by the pinger's scheduling), and a burst of replies naming each microsecond
/// around it found it for a few kilobytes. A token it has not seen it can only guess, one
/// in 2^64 a reply.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Token(u64);

impl Token {
    pub fn random() -> Token {
        let mut bytes = [0; 8];
        getrandom::fill(&mut bytes).expect("random ping token");
        Token(u64::from_ne_bytes(bytes))
    }

    /// From a reply's field: 16 hex digits, as a ping carries it.
    pub fn parse(field: &str) -> Option<Token> {
        let field = field.trim();
        let digits = field.len() == 16 && field.bytes().all(|b| b.is_ascii_hexdigit());
        digits.then(|| u64::from_str_radix(field, 16).ok().map(Token))?
    }
}

impl std::fmt::Display for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

/// As a ping carries it: its 16 hex digits.
impl serde::Serialize for Token {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

/// A ping sent to a client: what names it, and when it went. The time never leaves the
/// server.
struct Ping {
    token: Token,
    sent: Instant,
}

/// The pings sent to a client and not yet answered, oldest first, at most `PINGS_KEPT`. A
/// reply is a round trip only if it echoes the token of one of them.
#[derive(Default)]
pub struct Pings(VecDeque<Ping>);

impl Pings {
    /// A ping named `token`, sent at `now`. The oldest unanswered one leaves once the
    /// record holds `PINGS_KEPT`.
    pub fn send(&mut self, now: Instant, token: Token) {
        if self.0.len() == PINGS_KEPT {
            self.0.pop_front();
        }
        self.0.push_back(Ping { token, sent: now });
    }

    /// At `now`, a reply echoing `token`: the round trip of the ping it names, if that ping
    /// is in the record, which it then leaves. A reply echoing anything else (a ping
    /// already answered, one older than the record keeps, a token never sent, or a time,
    /// as replies used to) is no round trip.
    pub fn answer(&mut self, now: Instant, token: Token) -> Option<Duration> {
        let i = self.0.iter().position(|p| p.token == token)?;
        let ping = self.0.remove(i)?;
        now.checked_duration_since(ping.sent)
    }
}

/// What a client is to the stream. Each change is a method of `Client`, named for the step
/// that makes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Viewer {
    /// It is sent no video.
    NotWatching,
    /// It waits for a keyframe, and none has been planned for it, and nothing is on its way
    /// to it: the capture loop plans one.
    Waiting,
    /// Its path stalled beside others' and it was left behind (`falls_behind`): it waits
    /// for a keyframe, but frames are still on their way to it. It is sent nothing, and
    /// wants its keyframe once all of them have come back, so that no keyframe goes into a
    /// path that is still clogged. A viewer that asks for a keyframe is not left behind: it
    /// may have dropped the frames on their way (a decoder that failed forgets what it has
    /// not acknowledged), and then nothing would ever bring them back.
    Behind,
    /// A keyframe has been planned for it: it starts on that keyframe if it comes out
    /// whole, and waits again if it does not. One still `Waiting` is not started by it, as
    /// it may have joined after the frame was planned, even after the stream that coded it
    /// ended.
    Promised,
    /// It is sent every frame.
    Started,
}

/// What a client is sent of a frame the encode loop has coded.
#[derive(Debug, PartialEq)]
pub enum Takes {
    /// Nothing: it has not started, or has been left behind.
    Nothing,
    /// The frame, which is in flight to it until acked.
    Frame,
    /// A close, once: it was too far behind, and it stops watching.
    Close,
}

/// What asking for a keyframe makes of a viewer: one that watches wants one, at once,
/// whatever is on its way.
fn asked(viewer: Viewer) -> Viewer {
    match viewer {
        Viewer::NotWatching => Viewer::NotWatching,
        Viewer::Waiting | Viewer::Behind | Viewer::Promised | Viewer::Started => Viewer::Waiting,
    }
}

/// What a client is to the stream, the frames sent to it and not yet acked, and what its
/// path carries, read from those frames: under one lock, so that every change to the three
/// is one step.
struct Viewing {
    viewer: Viewer,
    inflight: VecDeque<Inflight>,
    window: Window,
}

impl Viewing {
    /// What it has in flight is not coming back: acks for it no longer apply, and its
    /// window forgets it.
    fn forget(&mut self, now: Instant) {
        self.inflight.clear();
        self.window.forget(now);
    }
}

/// What a viewer's path is taken to carry in `window::LATENESS`, in bytes: what
/// `--min-kbps` and `--max-kbps` carry in it. Each viewer's window starts within them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Paths {
    pub least: usize,
    pub most: usize,
}

#[cfg(test)]
impl Paths {
    /// Any path: from a packet to as much as there is.
    pub const ANY: Paths = Paths {
        least: 0,
        most: usize::MAX,
    };
}

pub struct Client {
    pub id: u64,
    pub tx: mpsc::UnboundedSender<Out>,
    viewing: Mutex<Viewing>,
    pub audio: AtomicBool,
    /// Smoothed min RTT (ms), from pings.
    pub min_rtt_ms: AtomicU32,
    /// The pings sent to it that it has not answered.
    pub pings: Mutex<Pings>,
    /// How long the client keeps a frame between its arrival and the ack (decode, draw,
    /// frames ahead of it), as it reports it: smoothed ms, 0 for a client that doesn't.
    pub client_ms: Mutex<f64>,
    /// Its last frames as they were sent and acked (`/stats/trace`), kept a while once it
    /// has left.
    pub trace: Arc<Mutex<crate::timeline::Trace>>,
    /// The desktop size it last asked for (`sz`, as asked), if it has asked.
    asked: Mutex<Option<(u32, u32)>>,
}

impl Client {
    pub fn send(&self, m: Out) {
        let _ = self.tx.send(m);
    }

    /// It asks for the desktop to be `size`: whether that is a new ask. Asking again for the
    /// size it last asked for asks nothing, so a page that repeats its size on every hello
    /// (as the web client did before it asked only on connect and when its window changes)
    /// cannot move the screen back and forth by repeating it.
    pub fn asks_for(&self, size: (u32, u32)) -> bool {
        self.asked.lock().replace(size) != Some(size)
    }

    /// What its window says, as one read sees it.
    pub fn window<R>(&self, read: impl FnOnce(&Window) -> R) -> R {
        read(&self.viewing.lock().window)
    }

    /// What it is to the stream, as one read sees it.
    pub fn viewer(&self) -> Viewer {
        self.viewing.lock().viewer
    }

    /// It starts watching at `now` (`v,1`, or a recording begins): it waits for a keyframe,
    /// and acks for frames sent before no longer apply.
    pub fn watch(&self, now: Instant) {
        let mut v = self.viewing.lock();
        v.viewer = Viewer::Waiting;
        v.forget(now);
    }

    /// It stops watching (`v,0`).
    pub fn stop_watching(&self) {
        self.viewing.lock().viewer = Viewer::NotWatching;
    }

    /// It asks for a keyframe (`kf`, or a hello after a failed display switch): if it
    /// watches, it wants one at once, whatever is on its way.
    pub fn ask_keyframe(&self) {
        let mut v = self.viewing.lock();
        v.viewer = asked(v.viewer);
    }

    /// The stream starts over for it at `now` (it is rebuilt): if it watches, it waits for a
    /// keyframe; acks for frames sent before no longer apply.
    pub fn start_over(&self, now: Instant) {
        let mut v = self.viewing.lock();
        v.forget(now);
        v.viewer = asked(v.viewer);
    }

    /// At `now` a capture goes to the viewers that have room. One that has started and
    /// whose path has stalled (the oldest frame on its way has been on its way for
    /// `STALLED`) is left behind: it is sent nothing more, so it has missed frames, and it
    /// wants a keyframe once everything on its way has come back. Its window keeps what is
    /// on its way, which is how its path is known to be still clogged.
    pub fn falls_behind(&self, now: Instant) {
        let mut v = self.viewing.lock();
        let stall = crate::video::STALLED;
        if let (Viewer::Started, Way::Stalled) = (v.viewer, v.window.way(now, stall)) {
            v.viewer = Viewer::Behind;
        }
    }

    /// The capture loop planned a keyframe: one waiting for one is promised it. Whether it
    /// was, and so whether that keyframe starts it.
    pub fn keyframe_planned(&self) -> bool {
        let mut v = self.viewing.lock();
        match v.viewer {
            Viewer::Waiting => {
                v.viewer = Viewer::Promised;
                true
            }
            Viewer::NotWatching | Viewer::Behind | Viewer::Promised | Viewer::Started => false,
        }
    }

    /// At `now` the encode loop sent a keyframe this client was promised, `whole` when every
    /// stripe of it is a keyframe. A whole one starts it, and acks for frames sent before it
    /// no longer apply; one that is not leaves it waiting for another. A client that has
    /// since stopped watching, asked again or started over is what that made it.
    pub fn keyframe_sent(&self, whole: bool, now: Instant) {
        let mut v = self.viewing.lock();
        match (v.viewer, whole) {
            (Viewer::Promised, true) => {
                v.viewer = Viewer::Started;
                v.forget(now);
            }
            (Viewer::Promised, false) => v.viewer = Viewer::Waiting,
            (Viewer::NotWatching | Viewer::Waiting | Viewer::Behind | Viewer::Started, _) => {}
        }
    }

    /// The encode loop has frame `id`, of `bytes`, to send at `now`; `key`: every stripe of
    /// it is a keyframe. A client that has started takes it, and it is in flight until
    /// acked and in its trace, unless it already has more than `behind` frames in flight:
    /// then it stops watching, and is to be sent a close.
    pub fn takes(
        &self,
        id: FrameId,
        now: Instant,
        bytes: usize,
        key: bool,
        behind: usize,
    ) -> Takes {
        let mut v = self.viewing.lock();
        match v.viewer {
            Viewer::Started if v.inflight.len() > behind => {
                v.viewer = Viewer::NotWatching;
                Takes::Close
            }
            Viewer::Started => {
                let left = v.window.leave(now, bytes);
                let frame = Inflight { id, bytes, left };
                v.inflight.push_back(frame);
                self.trace.lock().sent(frame, key);
                Takes::Frame
            }
            Viewer::NotWatching | Viewer::Waiting | Viewer::Behind | Viewer::Promised => {
                Takes::Nothing
            }
        }
    }

    /// At `now`, the ack for frame `id` (and every one before it) arrived. Each frame it
    /// covers comes back to the window, oldest first. The one it names came back when the
    /// client had it: what the client says it held of the trip is its own time, not the
    /// path's. Any it covers without an ack of its own, and one whose hold cannot be true
    /// (unreadable, or longer than the trip it is part of), came back at `now`: later, so
    /// what the window reads of the path can only be lower. One left behind wants its
    /// keyframe once the last of what is on its way is back.
    pub fn on_ack(&self, now: Instant, id: u32, hold: Hold) {
        self.trace.lock().acked(now, id, hold);
        let mut v = self.viewing.lock();
        let Viewing {
            viewer,
            inflight,
            window,
        } = &mut *v;
        let mut said = None;
        while let Some(&f) = inflight.front().filter(|f| wrapping_le(f.id.get(), id)) {
            inflight.pop_front();
            let back = match hold {
                Hold::Said(held)
                    if f.id.get() == id && held <= now.saturating_duration_since(f.left.at) =>
                {
                    said = Some(held);
                    now - held
                }
                Hold::Unsaid | Hold::Said(_) | Hold::Unreadable => now,
            };
            window.came_back(f.left, back, f.bytes);
        }
        if let (Viewer::Behind, true) = (*viewer, inflight.is_empty()) {
            *viewer = Viewer::Waiting;
        }
        drop(v);
        if let Some(held) = said {
            let mut m = self.client_ms.lock();
            *m = *m * 0.9 + held.as_secs_f64() * 100.0;
        }
    }

    /// At `now` a ping came back, `trip` after it went: a round trip that carried nothing.
    pub fn pinged(&self, now: Instant, trip: Duration) {
        self.viewing.lock().window.pinged(now, trip);
    }

    /// What it has in flight at `now`, if anything.
    pub fn backlog(&self, now: Instant) -> Option<Backlog> {
        let q = &self.viewing.lock().inflight;
        Some(Backlog {
            frames: NonZeroUsize::new(q.len())?,
            oldest: now.saturating_duration_since(q.front()?.left.at),
        })
    }
}

#[cfg(test)]
impl Client {
    /// Frame `id` of `bytes` leaves for it at `at`, whatever it is to the stream: in
    /// flight, and on its way in its window.
    pub fn fly(&self, id: FrameId, at: Instant, bytes: usize) {
        let mut v = self.viewing.lock();
        let left = v.window.leave(at, bytes);
        v.inflight.push_back(Inflight { id, bytes, left });
    }
}

/// The frames sent to a client that it has not acknowledged, while there are any.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Backlog {
    pub frames: NonZeroUsize,
    /// How long ago the oldest of them went.
    pub oldest: Duration,
}

/// A non-blocking, close-on-exec pipe for waking a thread that polls file descriptors
/// (`pipe2` with those flags, which macOS lacks).
#[cfg(unix)]
pub fn wake_pipe() -> nix::Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    use nix::fcntl::{fcntl, FcntlArg, FdFlag, OFlag};
    let (r, w) = nix::unistd::pipe()?;
    for fd in [&r, &w] {
        fcntl(fd, FcntlArg::F_SETFL(OFlag::O_NONBLOCK))?;
        fcntl(fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
    }
    Ok((r, w))
}

/// Something `wait_any` can wait for: a file descriptor on Unix, a handle on Windows.
#[cfg(unix)]
pub type Waitable<'a> = std::os::fd::BorrowedFd<'a>;
#[cfg(windows)]
pub type Waitable<'a> = std::os::windows::io::BorrowedHandle<'a>;

/// Wakes a thread blocked in `wait_any`: a pipe on Unix, an event on Windows.
pub struct Wake {
    #[cfg(unix)]
    r: std::os::fd::OwnedFd,
    #[cfg(unix)]
    w: std::os::fd::OwnedFd,
    #[cfg(windows)]
    event: std::os::windows::io::OwnedHandle,
}

impl Wake {
    #[cfg(unix)]
    pub fn new() -> std::io::Result<Self> {
        let (r, w) = wake_pipe()?;
        Ok(Wake { r, w })
    }

    #[cfg(windows)]
    pub fn new() -> std::io::Result<Self> {
        use std::os::windows::io::FromRawHandle;
        // Manual reset: signalled until drained, however many wakes came in between.
        let h = unsafe { windows::Win32::System::Threading::CreateEventW(None, true, false, None) }
            .map_err(std::io::Error::other)?;
        Ok(Wake {
            event: unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(h.0) },
        })
    }

    pub fn signal(&self) {
        #[cfg(unix)]
        let _ = nix::unistd::write(&self.w, &[1u8]);
        #[cfg(windows)]
        unsafe {
            use std::os::windows::io::AsRawHandle;
            let _ = windows::Win32::System::Threading::SetEvent(
                windows::Win32::Foundation::HANDLE(self.event.as_raw_handle()),
            );
        }
    }

    pub fn drain(&self) {
        #[cfg(unix)]
        {
            let mut buf = [0u8; 64];
            while matches!(nix::unistd::read(&self.r, &mut buf), Ok(n) if n > 0) {}
        }
        #[cfg(windows)]
        unsafe {
            use std::os::windows::io::AsRawHandle;
            let _ = windows::Win32::System::Threading::ResetEvent(
                windows::Win32::Foundation::HANDLE(self.event.as_raw_handle()),
            );
        }
    }

    pub fn waitable(&self) -> Waitable<'_> {
        #[cfg(unix)]
        return std::os::fd::AsFd::as_fd(&self.r);
        #[cfg(windows)]
        return std::os::windows::io::AsHandle::as_handle(&self.event);
    }
}

/// Blocks until one of `w` is ready or `timeout` passes, with no limit if it is None.
/// It allocates nothing. Linux and Android wait to the nanosecond (`ppoll`); other Unix
/// systems and Windows count whole milliseconds, rounded up so a wait never ends early.
pub fn wait_any<const N: usize>(w: [Waitable; N], timeout: Option<std::time::Duration>) {
    #[cfg(unix)]
    {
        use nix::poll::{PollFd, PollFlags};
        let mut p = w.map(|fd| PollFd::new(fd, PollFlags::POLLIN));
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let _ = nix::poll::ppoll(
            &mut p,
            timeout.map(nix::sys::time::TimeSpec::from_duration),
            None,
        );
        // Whole milliseconds, rounded up; one past what `poll` counts (24 days) is no limit.
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        let _ = nix::poll::poll(
            &mut p,
            timeout
                .and_then(|t| nix::poll::PollTimeout::try_from(t.as_micros().div_ceil(1000)).ok()),
        );
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        let hs = w.map(|h| HANDLE(h.as_raw_handle()));
        // Whole milliseconds, rounded up; one past what the call counts (49 days) is no limit.
        let ms = match timeout.and_then(|t| u32::try_from(t.as_micros().div_ceil(1000)).ok()) {
            Some(ms) => ms,
            None => windows::Win32::System::Threading::INFINITE,
        };
        unsafe { windows::Win32::System::Threading::WaitForMultipleObjects(&hs, false, ms) };
    }
}

fn wrapping_le(a: u32, b: u32) -> bool {
    b.wrapping_sub(a) < 0x8000_0000
}

#[derive(Default)]
pub struct Stats {
    pub captured: AtomicU64,
    pub encoded: AtomicU64,
    pub bytes: AtomicU64,
    pub encode_us: AtomicU64,
    pub grab_us: AtomicU64,
    pub convert_us: AtomicU64,
    pub flow_blocked: AtomicU64,
    pub paintovers: AtomicU64,
    pub keyframes: AtomicU64,
    pub refines: AtomicU64,
    pub damage_wait_us: AtomicU64,
    pub send_lat_us: AtomicU64,
}

pub struct Hub {
    #[cfg(target_os = "macos")]
    pub display_req: Mutex<Option<crate::mac::DisplayRequest>>,
    /// Connections older than this may no longer inject input after a display switch.
    #[cfg(target_os = "macos")]
    pub input_floor: AtomicU64,
    pub clients: Mutex<Vec<Arc<Client>>>,
    pub next_id: AtomicU64,
    pub stats: Stats,
    pub width: AtomicU32,
    pub height: AtomicU32,
    /// Encoded stripes as (y, height).
    pub stripes: Mutex<Vec<(u32, u32)>>,
    /// What each viewer's path is taken to carry, at least and at most.
    paths: Paths,
    /// Last cursor message, replayed to new clients.
    pub cursor: Mutex<Option<Bytes>>,
    /// Wakes the capture thread (acks, joins, keyframe requests).
    pub waker: Wake,
    pub input: Mutex<Option<std::sync::mpsc::Sender<crate::event::InputEvent>>>,
    pub last_clipboard: Mutex<Option<String>>,
    /// The newest new ask for a desktop size (dynamic resolution), until the capture loop
    /// takes it and gives it to the screen.
    pub resize_req: Mutex<Option<(u32, u32)>>,
    /// Largest desktop size (Some when dynamic resolution is on).
    pub resize_max: Mutex<Option<(u32, u32)>>,
    /// What every `hello` says besides the geometry.
    pub settings: Mutex<Settings>,
    /// Screenshot requests, answered by the capture thread with the current screen.
    pub snapshots: Mutex<Vec<tokio::sync::oneshot::Sender<Arc<crate::record::Snapshot>>>>,
    /// When each part of the screen last changed (`/wait`).
    pub activity: crate::wait::Activity,
    /// When each of the last frames passed each point of the pipeline.
    pub timeline: crate::timeline::Timeline,
    /// The traces of the last viewers to leave (`/stats/trace`).
    pub left: Mutex<crate::timeline::Leavers>,
}

impl Hub {
    pub fn new(paths: Paths) -> Arc<Self> {
        Arc::new(Hub {
            #[cfg(target_os = "macos")]
            display_req: Mutex::new(None),
            #[cfg(target_os = "macos")]
            input_floor: AtomicU64::new(0),
            clients: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            stats: Stats::default(),
            width: AtomicU32::new(0),
            height: AtomicU32::new(0),
            paths,
            stripes: Mutex::new(Vec::new()),
            cursor: Mutex::new(None),
            waker: Wake::new().expect("wake"),
            input: Mutex::new(None),
            last_clipboard: Mutex::new(None),
            resize_req: Mutex::new(None),
            resize_max: Mutex::new(None),
            settings: Mutex::new(Settings::default()),
            snapshots: Mutex::new(Vec::new()),
            activity: Default::default(),
            timeline: crate::timeline::Timeline::new(Instant::now()),
            left: Mutex::new(crate::timeline::Leavers::with_room()),
        })
    }

    pub fn wake(&self) {
        self.waker.signal();
    }

    pub fn drain_wake(&self) {
        self.waker.drain();
    }

    /// A client joins at `now`, with a window of its own from then.
    pub fn add_client(&self, tx: mpsc::UnboundedSender<Out>, now: Instant) -> Arc<Client> {
        let c = Arc::new(Client {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            tx,
            viewing: Mutex::new(Viewing {
                viewer: Viewer::NotWatching,
                inflight: VecDeque::new(),
                window: Window::new(now, self.paths.least, self.paths.most),
            }),
            audio: AtomicBool::new(false),
            min_rtt_ms: AtomicU32::new(0),
            pings: Mutex::new(Pings::default()),
            client_ms: Mutex::new(0.0),
            trace: Arc::new(Mutex::new(crate::timeline::Trace::with_room())),
            asked: Mutex::new(None),
        });
        self.clients.lock().push(c.clone());
        c
    }

    /// Each client's trace, by its id: the list is let go before a trace is copied, and a
    /// trace is held only while it is copied.
    pub fn traces(&self) -> Vec<(u64, crate::timeline::Trace)> {
        let clients = self.clients.lock().clone();
        clients
            .iter()
            .map(|c| (c.id, c.trace.lock().clone()))
            .collect()
    }

    /// The traces of the last viewers to leave, by id, with when each left, oldest first.
    pub fn left_traces(&self) -> Vec<(u64, Instant, crate::timeline::Trace)> {
        let left = self.left.lock();
        left.iter()
            .map(|l| (l.id, l.at, l.trace.lock().clone()))
            .collect()
    }

    /// Client `id` leaves at `now`: it is sent nothing more, and its trace is kept among the
    /// last to leave. One already gone (the display switch retires every client) leaves
    /// nothing more.
    pub fn remove_client(&self, id: u64, now: Instant) {
        let mut clients = self.clients.lock();
        let gone = clients
            .iter()
            .position(|c| c.id == id)
            .map(|i| clients.remove(i));
        drop(clients);
        if let Some(c) = gone {
            self.left.lock().left(crate::timeline::Leaver {
                id,
                at: now,
                trace: c.trace.clone(),
            });
        }
        self.wake();
    }

    /// Every client that watches, with what it is to the stream as one read of it saw it:
    /// whatever is decided from them is decided from that read.
    pub fn viewers(&self) -> Vec<(Arc<Client>, Viewer)> {
        let mut v = Vec::new();
        self.viewers_into(&mut v);
        v
    }

    /// As `viewers`, into `into` in place of what it held: one asking every frame keeps one
    /// vector, and allocates nothing once it is big enough.
    pub fn viewers_into(&self, into: &mut Vec<(Arc<Client>, Viewer)>) {
        into.clear();
        into.extend(
            self.clients
                .lock()
                .iter()
                .map(|c| (c.clone(), c.viewer()))
                .filter(|(_, viewer)| *viewer != Viewer::NotWatching),
        );
    }

    /// The `hello` message: current geometry plus the server's settings.
    pub fn hello(&self) -> ServerMessage {
        let settings = self.settings.lock().clone();
        let stripes = self.stripes.lock().clone();
        ServerMessage::Hello(Hello {
            w: self.width.load(Ordering::Relaxed),
            h: self.height.load(Ordering::Relaxed),
            stripes,
            settings,
        })
    }

    /// After the stream restarts (new size): every client needs the new geometry and
    /// a keyframe, and acks for the old stream's frames no longer apply.
    pub fn restart_clients(&self) {
        let hello = self.hello();
        let now = Instant::now();
        for c in self.clients.lock().iter() {
            c.start_over(now);
            c.send(Out::Text(hello.clone()));
        }
    }

    pub fn broadcast(&self, m: impl Fn() -> Out) {
        for c in self.clients.lock().iter() {
            c.send(m());
        }
    }

    /// Share the admission lock with input senders, so the barrier cannot overtake input.
    #[cfg(target_os = "macos")]
    pub fn pause_display_input(&self, id: u32, client: u64) -> Option<crate::mac::DisplayRequest> {
        let mut input = self.input.lock();
        if client < self.input_floor.load(Ordering::Relaxed) {
            return None;
        }
        let sender = input.take()?;
        let (done, ready) = std::sync::mpsc::channel();
        if sender
            .send(crate::event::InputEvent::DisplayBarrier(done))
            .is_err()
        {
            *input = Some(sender);
            return None;
        }
        self.input_floor
            .store(self.next_id.load(Ordering::Relaxed), Ordering::Relaxed);
        // Retire their subscriptions too: an old client ignoring close must not
        // stall flow control for the replacement connections.
        for client in self.clients.lock().drain(..) {
            client.send(Out::Text(ServerMessage::Close));
        }
        Some(crate::mac::DisplayRequest { id, sender, ready })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A step of the hub's, for one of two clients or for all.
    #[derive(Clone, Copy, Debug)]
    enum Step {
        /// `v,1` (server.rs).
        Watch(usize),
        /// `v,0` (server.rs).
        StopWatching(usize),
        /// `kf`, and the hello after a failed display switch, which runs the same lines
        /// (server.rs).
        AskKeyframe(usize),
        /// A recording begins (record.rs): only on a client nothing has been done to.
        Record(usize),
        /// A capture finds a client that has started, with frames in flight, stalled, and
        /// leaves it behind (video.rs).
        Stall(usize),
        /// `build` starts the stream over for every client (`restart_clients`), once the
        /// stream before has sent what it held.
        Restart,
        /// The capture loop reads the viewers and plans a keyframe if one wants it, or a
        /// frame that is not one; the encoder holds at most two.
        Capture,
        /// The encode loop sends the oldest frame it holds: a keyframe whole or not, any
        /// other frame not whole.
        Send { whole: bool },
        /// The client acks every frame it was sent.
        Ack(usize),
    }

    const STEPS: [Step; 16] = [
        Step::Watch(0),
        Step::Watch(1),
        Step::StopWatching(0),
        Step::StopWatching(1),
        Step::AskKeyframe(0),
        Step::AskKeyframe(1),
        Step::Record(0),
        Step::Record(1),
        Step::Stall(0),
        Step::Stall(1),
        Step::Restart,
        Step::Capture,
        Step::Send { whole: true },
        Step::Send { whole: false },
        Step::Ack(0),
        Step::Ack(1),
    ];

    /// The frames in flight past which the model's client is closed: small, so that the
    /// close is among the states reached.
    const BEHIND: usize = 2;

    /// A frame the encoder holds: a keyframe, with the clients its plan promised it to,
    /// or a frame that is not one.
    type Held = Option<[bool; 2]>;

    /// Two clients (what each is to the stream, and its frames in flight), whether
    /// nothing has been done to each, and the frames the encoder holds, oldest first.
    #[derive(Clone, Debug, PartialEq, Eq, Hash)]
    struct Model {
        c: [(Viewer, usize); 2],
        fresh: [bool; 2],
        held: Vec<Held>,
    }

    /// The rule, step by step, as each value and method says it: what each step makes of
    /// the model, whether it planned a keyframe, and who was sent the frame.
    fn rule(before: &Model, step: Step) -> (bool, Vec<usize>, Model) {
        use Viewer::{Behind, NotWatching, Promised, Started, Waiting};
        let mut m = before.clone();
        let c = &mut m.c;
        let (mut planned, mut sent) = (false, vec![]);
        // Asking for a keyframe: one that watches wants it at once.
        let asked = |(v, n): (Viewer, usize)| match v {
            NotWatching => (NotWatching, n),
            Waiting | Behind | Promised | Started => (Waiting, n),
        };
        match step {
            Step::Watch(i) | Step::Record(i) => c[i] = (Waiting, 0),
            Step::Stall(i) => c[i].0 = Behind,
            Step::StopWatching(i) => c[i].0 = NotWatching,
            Step::AskKeyframe(i) => c[i] = asked(c[i]),
            Step::Restart => {
                for c in c.iter_mut() {
                    if c.0 != NotWatching {
                        c.0 = Waiting;
                    }
                    c.1 = 0;
                }
            }
            Step::Capture => {
                planned = c.iter().any(|c| c.0 == Waiting);
                let starts = c.map(|c| c.0 == Waiting);
                for c in c.iter_mut() {
                    if c.0 == Waiting {
                        c.0 = Promised;
                    }
                }
                m.held.push(planned.then_some(starts));
            }
            Step::Send { whole } => {
                let starts = m.held.remove(0).unwrap_or([false; 2]);
                for (i, c) in c.iter_mut().enumerate() {
                    if starts[i] && c.0 == Promised {
                        *c = match whole {
                            true => (Started, 0),
                            false => asked(*c),
                        };
                    }
                    match c.0 {
                        Started if c.1 > BEHIND => c.0 = NotWatching,
                        Started => {
                            c.1 += 1;
                            sent.push(i);
                        }
                        NotWatching | Waiting | Behind | Promised => {}
                    }
                }
            }
            Step::Ack(i) => {
                c[i].1 = 0;
                if c[i].0 == Behind {
                    c[i].0 = Waiting;
                }
            }
        }
        if let Step::Watch(i)
        | Step::StopWatching(i)
        | Step::AskKeyframe(i)
        | Step::Record(i)
        | Step::Stall(i)
        | Step::Ack(i) = step
        {
            m.fresh[i] = false;
        }
        if let Step::Restart = step {
            m.fresh = [false; 2];
        }
        (planned, sent, m)
    }

    /// Whether the hub takes `step` in this state: a recording begins only on a new
    /// client; a capture leaves behind only a client that has started, with frames in
    /// flight; a
    /// restart comes once the encoder has sent what it held; the encoder holds two frames
    /// at most and sends only what it holds, and only a keyframe comes out whole.
    fn applies(m: &Model, step: Step) -> bool {
        match step {
            Step::Record(i) => m.fresh[i],
            Step::Stall(i) => m.c[i].0 == Viewer::Started && m.c[i].1 > 0,
            Step::Restart => m.held.is_empty(),
            Step::Capture => m.held.len() < 2,
            Step::Send { whole } => match m.held.first() {
                None => false,
                Some(held) => !whole || held.is_some(),
            },
            Step::Watch(_) | Step::StopWatching(_) | Step::AskKeyframe(_) | Step::Ack(_) => true,
        }
    }

    /// What the hub reads of a client: what it is to the stream, and its frames in flight.
    type Read = (Viewer, usize);

    /// What a step did: whether it planned a keyframe, which clients it sent a frame, the
    /// model after it, and what the hub reads of the two clients.
    type Stepped = (bool, Vec<usize>, Model, [Read; 2]);

    /// The same step, by the methods, on two real clients of `hub` set to `m`. Returns what
    /// it planned and sent, the model after it, and what the hub reads of the clients.
    fn new_step(hub: &Hub, m: &Model, step: Step) -> Stepped {
        let t0 = Instant::now();
        hub.clients.lock().clear();
        let c: Vec<Arc<Client>> =
            m.c.iter()
                .map(|&(viewer, inflight)| {
                    let c = hub.add_client(mpsc::unbounded_channel().0, t0);
                    c.viewing.lock().viewer = viewer;
                    for _ in 0..inflight {
                        c.fly(hub.timeline.number(), t0, 1000);
                    }
                    c
                })
                .collect();
        let mut after = m.clone();
        let (mut planned, mut sent) = (false, vec![]);
        match step {
            Step::Watch(i) | Step::Record(i) => c[i].watch(t0),
            Step::StopWatching(i) => c[i].stop_watching(),
            Step::AskKeyframe(i) => c[i].ask_keyframe(),
            Step::Stall(i) => {
                // Its frames went at `t0`: it has stalled `STALLED` later.
                c[i].falls_behind(t0 + crate::video::STALLED);
            }
            Step::Restart => hub.restart_clients(),
            Step::Capture => {
                let viewers = hub.viewers();
                planned = viewers.iter().any(|(_, v)| *v == Viewer::Waiting);
                let promised: Vec<u64> = match planned {
                    true => viewers
                        .iter()
                        .filter(|(c, v)| *v == Viewer::Waiting && c.keyframe_planned())
                        .map(|(c, _)| c.id)
                        .collect(),
                    false => vec![],
                };
                let starts = [0, 1].map(|i| promised.contains(&c[i].id));
                after.held.push(planned.then_some(starts));
            }
            Step::Send { whole } => {
                if let Some(starts) = after.held.remove(0) {
                    for (i, c) in c.iter().enumerate() {
                        if starts[i] {
                            c.keyframe_sent(whole, t0);
                        }
                    }
                }
                let id = hub.timeline.number();
                for (v, _) in hub.viewers() {
                    if v.takes(id, t0, 1000, whole, BEHIND) == Takes::Frame {
                        sent.push(c.iter().position(|c| c.id == v.id).unwrap());
                    }
                }
            }
            Step::Ack(i) => {
                let last = c[i].viewing.lock().inflight.back().map(|f| f.id.get());
                if let Some(id) = last {
                    c[i].on_ack(t0 + Duration::from_millis(50), id, Hold::Unsaid);
                }
            }
        }
        after.c = [0, 1].map(|i| {
            let v = c[i].viewing.lock();
            // The window has on its way what is in flight, whatever the step did.
            let bytes: usize = v.inflight.iter().map(|f| f.bytes).sum();
            assert_eq!(v.window.flying(), bytes, "client {i}'s window, {step:?}");
            (v.viewer, v.inflight.len())
        });
        after.fresh = rule(m, step).2.fresh;
        let read = [0, 1].map(|i| {
            let c = &c[i];
            let inflight = c.backlog(t0).map_or(0, |b| b.frames.get());
            (c.viewer(), inflight)
        });
        let listed = hub.viewers();
        for (i, c) in c.iter().enumerate() {
            assert_eq!(
                listed.iter().find(|(v, _)| v.id == c.id).map(|(_, v)| *v),
                (after.c[i].0 != Viewer::NotWatching).then_some(after.c[i].0),
                "the hub's viewers"
            );
        }
        (planned, sent, after, read)
    }

    /// A viewer starts only on a whole keyframe promised it, is sent frames only once
    /// started, and is started in the end. Over every state two clients and the encoder
    /// reach in up to 12 of the hub's steps (`v,1`, `v,0`, `kf`, a recording on a new
    /// client, a stall, `restart_clients`, a capture, a frame sent, an ack), breadth first,
    /// every step from every state, run on a real hub's clients through the methods: each
    /// step does what the rule says, the hub reads each client as its value is, a client
    /// that starts was promised a keyframe and was sent a whole one, and from every state,
    /// once the encoder has sent what it holds, one capture and its keyframe sent whole
    /// start every viewer watching.
    #[test]
    fn a_viewer_starts_only_on_a_whole_keyframe_promised_it() {
        let hub = Hub::new(Paths::ANY);
        let start = Model {
            c: [(Viewer::NotWatching, 0); 2],
            fresh: [true; 2],
            held: vec![],
        };
        let mut seen = std::collections::HashSet::from([start.clone()]);
        let mut frontier = vec![start];
        let mut steps = 0;
        // Viewers that asked for a keyframe with frames on their way.
        let mut asked_with_frames = 0;
        for _depth in 0..12 {
            let mut next = vec![];
            for before in frontier {
                for step in STEPS {
                    if !applies(&before, step) {
                        continue;
                    }
                    steps += 1;
                    let what = format!("{step:?} from {before:?}");
                    let (planned, sent, after, read) = new_step(&hub, &before, step);
                    assert_eq!(
                        (planned, sent.clone(), after.clone()),
                        rule(&before, step),
                        "{what}"
                    );
                    for (i, &(viewer, inflight)) in after.c.iter().enumerate() {
                        assert_eq!(read[i], (viewer, inflight), "client {i}: {what}");
                        if viewer == Viewer::Started && before.c[i].0 != Viewer::Started {
                            assert!(
                                matches!(step, Step::Send { whole: true })
                                    && before.c[i].0 == Viewer::Promised
                                    && before.held[0].is_some_and(|starts| starts[i]),
                                "client {i} started: {what}"
                            );
                        }
                    }
                    assert!(
                        sent.iter().all(|&i| after.c[i].0 == Viewer::Started),
                        "{what}"
                    );
                    // A viewer that asks for a keyframe is promised one by the next capture,
                    // whatever is on its way.
                    if let (Step::AskKeyframe(i), true) = (step, applies(&after, Step::Capture)) {
                        let (planned, _, then, _) = new_step(&hub, &after, Step::Capture);
                        if after.c[i].0 != Viewer::NotWatching {
                            assert!(planned && then.c[i].0 == Viewer::Promised, "{what}");
                            asked_with_frames += usize::from(after.c[i].1 > 0);
                        }
                    }
                    if seen.insert(after.clone()) {
                        next.push(after);
                    }
                }
            }
            frontier = next;
        }
        // Liveness: from every state, the encoder sends what it holds (each keyframe whole),
        // every client acks what it was sent, then one capture, and its keyframe sent whole:
        // nobody watching is left unstarted, one left behind included.
        for state in &seen {
            let mut m = state.clone();
            let mut path = vec![];
            while let Some(held) = m.held.first() {
                let send = Step::Send {
                    whole: held.is_some(),
                };
                path.push(send);
                m = new_step(&hub, &m, send).2;
            }
            for step in [
                Step::Ack(0),
                Step::Ack(1),
                Step::Capture,
                Step::Send { whole: true },
            ] {
                if applies(&m, step) {
                    path.push(step);
                    m = new_step(&hub, &m, step).2;
                }
            }
            assert!(
                m.c.iter()
                    .all(|c| matches!(c.0, Viewer::NotWatching | Viewer::Started)),
                "left unstarted from {state:?} by {path:?}: {m:?}"
            );
        }
        let reached: std::collections::HashSet<Viewer> =
            seen.iter().flat_map(|m| m.c.map(|c| c.0)).collect();
        assert_eq!(reached.len(), 5, "{reached:?}");
        assert!(steps > 0 && asked_with_frames > 0, "{asked_with_frames}");
    }

    /// What a client's window says at `at`: what its path carries, what is on its way,
    /// the fastest round trip, and the room it leaves a frame.
    type Said = (usize, usize, Option<Duration>, Option<usize>);

    fn said(c: &Client, at: Instant) -> Said {
        c.window(|w| (w.carries(), w.flying(), w.fastest(), w.room(at)))
    }

    /// A frame of a generated run: when it leaves, its number and bytes, when it arrives
    /// at the viewer and when the viewer acks it, and whether it is acked itself or
    /// covered by the ack of a later one.
    #[derive(Clone, Copy)]
    struct Sent {
        at: Instant,
        id: FrameId,
        bytes: usize,
        arrives: Instant,
        acks: Instant,
        acked: bool,
    }

    impl Sent {
        /// How long the viewer held it between its arrival and its ack.
        fn held(&self) -> Duration {
            self.acks - self.arrives
        }
    }

    /// A run of 200 frames a viewer of `hub` is sent, numbered by the hub's one count and
    /// generated from `seed`: each leaves 1 to 40 ms after the one before, of 200 B to
    /// 60 KB, arrives 0 to 120 ms later, and is acked 0 to 50 ms after that, each no sooner
    /// than the one before (TCP keeps them in order); with `covering`, about one in four is
    /// covered by a later frame's ack. The last is acked.
    fn run(hub: &Hub, seed: u64, covering: bool) -> Vec<Sent> {
        let mut rng = Rng(crate::xorshift::Xorshift::new(seed));
        let t0 = Instant::now();
        let (mut at, mut arrives, mut acks) = (t0, t0, t0);
        let mut frames: Vec<Sent> = (0..200)
            .map(|_| {
                at += Duration::from_millis(1 + rng.below(40));
                arrives = arrives.max(at + Duration::from_millis(rng.below(121)));
                acks = acks.max(arrives + Duration::from_millis(rng.below(51)));
                Sent {
                    at,
                    id: hub.timeline.number(),
                    bytes: 200 + rng.below(60_000) as usize,
                    arrives,
                    acks,
                    acked: !covering || rng.below(4) > 0,
                }
            })
            .collect();
        frames.last_mut().unwrap().acked = true;
        frames
    }

    /// `frames` on a new client of `hub` that has started: each is sent when it leaves,
    /// and each acked is acked at what `when` says with what `hold` says, in the order the
    /// acks arrive.
    fn acked(
        hub: &Hub,
        frames: &[Sent],
        when: impl Fn(&Sent) -> Instant,
        hold: impl Fn(&Sent) -> Hold,
    ) -> Arc<Client> {
        // It joins as the run starts: before the first frame, by the run's own clock.
        let c = hub.add_client(mpsc::unbounded_channel().0, frames[0].at);
        c.viewing.lock().viewer = Viewer::Started;
        let mut acks = frames.iter().filter(|f| f.acked).peekable();
        for f in frames {
            // The acks that arrive before it leaves, of frames already sent.
            while let Some(a) = acks.next_if(|a| a.id.get() < f.id.get() && when(a) <= f.at) {
                c.on_ack(when(a), a.id.get(), hold(a));
            }
            let took = c.takes(f.id, f.at, f.bytes, false, usize::MAX);
            assert_eq!(took, Takes::Frame);
        }
        for a in acks {
            c.on_ack(when(a), a.id.get(), hold(a));
        }
        c
    }

    /// The time a client keeps a frame (decoding, drawing) is its own, not the path's: a
    /// client that says how long it held each frame reads, in its window, as a twin that
    /// acked each the moment it arrived and said nothing. One that does not say reads
    /// otherwise, in nearly every run. Over 50 generated runs, every frame acked.
    #[test]
    fn the_clients_own_time_is_not_the_paths() {
        let mut otherwise = 0;
        for seed in 1..=50u64 {
            let hub = Hub::new(Paths::ANY);
            let frames = run(&hub, seed.wrapping_mul(0x9E37_79B9_7F4A_7C15), false);
            let end = frames.last().unwrap().acks + Duration::from_secs(1);
            let says = acked(&hub, &frames, |f| f.acks, |f| Hold::Said(f.held()));
            let twin = acked(&hub, &frames, |f| f.arrives, |_| Hold::Unsaid);
            let silent = acked(&hub, &frames, |f| f.acks, |_| Hold::Unsaid);
            assert_eq!(said(&says, end), said(&twin, end), "seed {seed}");
            otherwise += u32::from(said(&silent, end) != said(&twin, end));
        }
        assert!(otherwise > 40, "{otherwise} of 50");
    }

    /// A frame an ack covers without one of its own comes back to the window when the ack
    /// arrives, before the frame it names: as for a twin that acked it then itself, saying
    /// no hold. Over 50 generated runs, about one frame in four covered.
    #[test]
    fn a_frame_an_ack_covers_comes_back_when_the_ack_arrives() {
        for seed in 1..=50u64 {
            let hub = Hub::new(Paths::ANY);
            let frames = run(&hub, seed.wrapping_mul(0x2545_F491_4F6C_DD1D), true);
            let end = frames.last().unwrap().acks + Duration::from_secs(1);
            // Each frame acked, a covered one when the ack that covers it arrives.
            let mut each = frames.clone();
            for i in (0..each.len()).rev() {
                if !each[i].acked {
                    each[i].acks = each[i + 1].acks;
                }
            }
            let says = |f: &Sent| match f.acked {
                true => Hold::Said(f.held()),
                false => Hold::Unsaid,
            };
            let c = acked(&hub, &frames, |f| f.acks, says);
            for f in &mut each {
                f.acked = true;
            }
            let covered: std::collections::HashSet<u32> = frames
                .iter()
                .filter(|f| !f.acked)
                .map(|f| f.id.get())
                .collect();
            let twin = acked(
                &hub,
                &each,
                |f| f.acks,
                |f| match covered.contains(&f.id.get()) {
                    true => Hold::Unsaid,
                    false => Hold::Said(f.held()),
                },
            );
            assert!(!covered.is_empty());
            assert_eq!(said(&c, end), said(&twin, end), "seed {seed}");
        }
    }

    /// The hold is whatever a client sends. A field that isn't a time parses to
    /// `Unreadable`, whatever it is; and an ack whose hold is unreadable, or longer than
    /// the trip it is part of, is read as one that says none: the window reads it as a
    /// twin's that says nothing, and the client's hold is the twin's. Over 50 generated
    /// runs, each lie in place of every other frame's hold.
    #[test]
    fn a_hold_that_cannot_be_true_says_nothing() {
        let unreadable = [
            "", "x", "NaN", "inf", "-inf", "-1", "-0.001", "1e30", "1e400",
        ];
        for field in unreadable {
            assert_eq!(Hold::parse(field), Hold::Unreadable, "{field:?}");
        }
        assert_eq!(Hold::parse(" 3.5"), Hold::Said(Duration::from_micros(3500)));

        let lies: [fn(&Sent) -> Hold; 2] = [
            |_| Hold::Unreadable,
            |f| Hold::Said(f.acks - f.at + Duration::from_nanos(1)),
        ];
        for seed in 1..=50u64 {
            for lie in lies {
                let hub = Hub::new(Paths::ANY);
                let frames = run(&hub, seed.wrapping_mul(0xD1B5_4A32_D192_ED03), true);
                let end = frames.last().unwrap().acks + Duration::from_secs(1);
                let truth = |f: &Sent| Hold::Said(f.held());
                let c = acked(
                    &hub,
                    &frames,
                    |f| f.acks,
                    |f| match f.id.get() % 2 {
                        0 => lie(f),
                        _ => truth(f),
                    },
                );
                let twin = acked(
                    &hub,
                    &frames,
                    |f| f.acks,
                    |f| match f.id.get() % 2 {
                        0 => Hold::Unsaid,
                        _ => truth(f),
                    },
                );
                assert_eq!(said(&c, end), said(&twin, end), "seed {seed}");
                assert_eq!(*c.client_ms.lock(), *twin.client_ms.lock(), "seed {seed}");
            }
        }
    }

    /// A reply is a round trip only if it echoes the token of a ping that was sent and not
    /// yet answered: the latest, or an older one still out. One already answered, one never
    /// sent, and one older than the record keeps are none. A token goes out and comes back
    /// as 16 hex digits, and a reply in any other shape, a time as replies used to be among
    /// them, is no token.
    #[test]
    fn a_reply_is_a_round_trip_only_to_a_ping_still_out() {
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        let mut pings = Pings::default();
        for (at, token) in [(0, 10), (500, 11), (1000, 12)] {
            pings.send(t0 + ms(at), Token(token));
        }
        for (now, echoes, round_trip) in [
            // The latest ping.
            (1040, 12, Some(ms(40))),
            // An older one, still out.
            (1060, 11, Some(ms(560))),
            // One already answered.
            (1080, 12, None),
            // One never sent.
            (1100, 13, None),
        ] {
            let got = pings.answer(t0 + ms(now), Token(echoes));
            assert_eq!(got, round_trip, "token {echoes} at {now} ms");
        }
        // One older than the record keeps: the first of PINGS_KEPT + 1, a ping apart.
        let mut pings = Pings::default();
        for i in 0..=PINGS_KEPT as u32 {
            pings.send(t0 + PING_EVERY * i, Token(i.into()));
        }
        let now = t0 + PING_EVERY * (PINGS_KEPT as u32 + 1);
        assert_eq!(pings.answer(now, Token(0)), None);
        assert_eq!(
            pings.answer(now, Token(1)),
            Some(PING_EVERY * PINGS_KEPT as u32)
        );
        assert_eq!(PINGS_KEPT, 8);

        let token = Token(0x0123_4567_89ab_cdef);
        assert_eq!(token.to_string(), "0123456789abcdef");
        assert_eq!(Token::parse(&token.to_string()), Some(token));
        for field in [
            "",
            "1",
            "9999000",
            "+123456789abcdef",
            "0123456789abcdeg",
            "0123456789abcdef0",
        ] {
            assert_eq!(Token::parse(field), None, "{field:?}");
        }
    }

    /// A reply cannot answer a ping before it arrives (wip/red-ping-guess 8808024): what a
    /// viewer could know of the next ping before having it is when it is due, one
    /// `PING_EVERY` after the last, and the tokens of the pings it has had. A burst of
    /// replies naming each microsecond of the millisecond the ping went out in, written as
    /// replies wrote times and as tokens, and each token within 500 of one it has had, is
    /// no round trip.
    #[test]
    fn a_reply_cannot_answer_a_ping_before_it_arrives() {
        let us = Duration::from_micros;
        let real_round_trip = Duration::from_millis(100);
        let t0 = Instant::now();
        let mut pings = Pings::default();
        // Pings as the pinger sends them: due every 500 ms, each run a little late.
        let due = |k: u32| t0 + PING_EVERY * k;
        let late = [us(173), us(412), us(58), us(731)];
        let mut had = vec![];
        for (k, &l) in late.iter().enumerate().take(3) {
            let token = Token::random();
            pings.send(due(k as u32) + l, token);
            assert!(pings
                .answer(due(k as u32) + l + real_round_trip, token)
                .is_some());
            had.push(token);
        }
        // The fourth goes out; the viewer has not had it yet. 1 ms after it is due, a
        // burst arrives naming everything it could have guessed.
        pings.send(due(3) + late[3], Token::random());
        let now = due(3) + Duration::from_millis(1);
        // Each microsecond it could have gone out at, as replies wrote it and as a token.
        let times = (0..1000u64).flat_map(|j| {
            let us = (PING_EVERY * 3 + us(j)).as_micros();
            [us.to_string(), format!("{us:016x}")]
        });
        let neighbours = had.iter().flat_map(|t| {
            (0..1000u64).map(move |j| Token(t.0.wrapping_add(j).wrapping_sub(500)).to_string())
        });
        let shortest = times
            .chain(neighbours)
            .filter_map(|guess| pings.answer(now, Token::parse(&guess)?))
            .min();
        assert!(
            shortest.is_none_or(|rt| rt >= real_round_trip),
            "a {shortest:?} round trip on a {real_round_trip:?} path"
        );
    }

    /// The tests' generator, drawing the hub's values.
    struct Rng(crate::xorshift::Xorshift);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0.next()
        }
        fn below(&mut self, n: u64) -> u64 {
            self.0.below(n)
        }
        /// Text that JSON escapes and text it doesn't, and the words the recorder looks for.
        fn text(&mut self) -> String {
            const PIECES: [&str; 14] = [
                "hello",
                "close",
                "\"",
                "\\",
                "/",
                "a",
                " ",
                "\u{7f}",
                "é",
                "€",
                "😀",
                "\u{2028}",
                "\"t\":\"close\"",
                "{}",
            ];
            (0..self.below(8))
                .map(|_| match self.below(3) {
                    0 => char::from(self.below(0x20) as u8).to_string(),
                    _ => PIECES[self.below(PIECES.len() as u64) as usize].to_string(),
                })
                .collect()
        }
        /// Any f64, NaN and the infinities among them.
        fn float(&mut self) -> f64 {
            f64::from_bits(self.next())
        }
        fn startup(&mut self) -> Startup {
            Startup {
                audio: self.next() & 1 == 1,
                files: FilesConfig {
                    dir: self.text(),
                    download: self.next() & 1 == 1,
                    upload: self.next() & 1 == 1,
                },
                mic: self.next() & 1 == 1,
                os: ["linux", "macos", "windows"][self.below(3) as usize],
                read_only: self.next() & 1 == 1,
            }
        }
        fn resize(&mut self) -> Option<Resize> {
            (self.next() & 1 == 1).then(|| Resize {
                scale: self.float(),
            })
        }
        #[cfg(target_os = "macos")]
        fn displays(&mut self) -> Displays {
            Displays {
                available: (0..self.below(4))
                    .map(|_| Display {
                        id: self.next() as u32,
                        name: self.text(),
                    })
                    .collect(),
                selected: self.next() as u32,
            }
        }
        fn settings(&mut self) -> Settings {
            Settings {
                startup: (self.next() & 1 == 1).then(|| self.startup()),
                #[cfg(target_os = "macos")]
                displays: (self.next() & 1 == 1).then(|| self.displays()),
                resize: self.resize(),
            }
        }
        fn message(&mut self) -> ServerMessage {
            match self.below(5) {
                0 => ServerMessage::Hello(Hello {
                    w: self.next() as u32,
                    h: self.next() as u32,
                    stripes: (0..self.below(17))
                        .map(|_| (self.next() as u32, self.next() as u32))
                        .collect(),
                    settings: self.settings(),
                }),
                1 => ServerMessage::Ping {
                    s: Token(self.next()),
                },
                2 => ServerMessage::Clip { d: self.text() },
                3 => ServerMessage::Mic {
                    on: self.next() & 1 == 1,
                },
                _ => ServerMessage::Close,
            }
        }
    }

    /// Gives `hub` the geometry and settings of `m`, if it is a hello.
    fn say(hub: &Hub, m: &ServerMessage) {
        if let ServerMessage::Hello(h) = m {
            hub.width.store(h.w, Ordering::Relaxed);
            hub.height.store(h.h, Ordering::Relaxed);
            *hub.stripes.lock() = h.stripes.clone();
            *hub.settings.lock() = h.settings.clone();
        }
    }

    /// The model: a message is a JSON object of its fields, and its kind as `t`, first (a
    /// viewer may know a ping by its first bytes, as bench/nclient.py does). A hub's hello
    /// is its geometry and its settings.
    #[test]
    fn a_message_is_its_fields_after_its_kind() {
        let mut rng = Rng(crate::xorshift::Xorshift::new(0x2545_f491_4f6c_dd1d));
        let hub = Hub::new(Paths::ANY);
        for _ in 0..100_000 {
            let m = rng.message();
            say(&hub, &m);
            if let ServerMessage::Hello(_) = m {
                // As JSON: a scale may be NaN, which equals nothing.
                assert_eq!(hub.hello().json(), m.json());
            }
            let (kind, fields) = match &m {
                // Its geometry and its settings, as `Hello` in web/src/protocol.ts names them.
                ServerMessage::Hello(h) => {
                    let mut f = serde_json::Map::new();
                    f.insert("w".into(), h.w.into());
                    f.insert("h".into(), h.h.into());
                    f.insert("stripes".into(), serde_json::json!(h.stripes));
                    if let Some(s) = &h.settings.startup {
                        f.insert("audio".into(), s.audio.into());
                        let files = &s.files;
                        f.insert(
                            "files".into(),
                            serde_json::json!({
                                "dir": files.dir, "download": files.download, "upload": files.upload
                            }),
                        );
                        f.insert("mic".into(), s.mic.into());
                        f.insert("os".into(), s.os.into());
                        if s.read_only {
                            f.insert("readOnly".into(), true.into());
                        }
                    }
                    #[cfg(target_os = "macos")]
                    if let Some(d) = &h.settings.displays {
                        let available: Vec<_> = (d.available.iter())
                            .map(|x| serde_json::json!({ "id": x.id, "name": x.name }))
                            .collect();
                        f.insert(
                            "displays".into(),
                            serde_json::json!({ "available": available, "selected": d.selected }),
                        );
                    }
                    if let Some(r) = h.settings.resize {
                        f.insert("resize".into(), serde_json::json!({ "scale": r.scale }));
                    }
                    ("hello", serde_json::Value::Object(f))
                }
                ServerMessage::Ping { s } => ("ping", serde_json::json!({ "s": s.to_string() })),
                ServerMessage::Clip { d } => ("clip", serde_json::json!({ "d": d })),
                ServerMessage::Mic { on } => ("mic", serde_json::json!({ "on": on })),
                ServerMessage::Close => ("close", serde_json::json!({})),
            };
            let json = m.json();
            assert!(json.starts_with(&format!("{{\"t\":\"{kind}\"")), "{json}");
            let mut object: serde_json::Map<String, serde_json::Value> =
                serde_json::from_str(&json).unwrap();
            assert_eq!(object.remove("t"), Some(kind.into()), "{json}");
            // Read back as the message is: serde_json reads some floats one unit off.
            let fields: serde_json::Value = serde_json::from_str(&fields.to_string()).unwrap();
            assert_eq!(serde_json::Value::Object(object), fields, "{json}");
        }
    }

    /// `run` says what voxl started with, and its scale if it has one; without one, the
    /// scale a pipeline said stays. A macOS pipeline says its displays and its scale, or
    /// that it has none.
    #[test]
    fn each_writer_says_its_part_of_the_settings() {
        let scale = |r: Option<Resize>| r.map(|r| r.scale.to_bits());
        let mut rng = Rng(crate::xorshift::Xorshift::new(0x3c6e_f372_fe94_f82b));
        for _ in 0..20_000 {
            let mut s = rng.settings();
            let before = s.clone();
            let (startup, resize) = (rng.startup(), rng.resize());
            s.start(startup.clone(), resize);
            assert_eq!(s.startup, Some(startup));
            assert_eq!(scale(s.resize), scale(resize.or(before.resize)));
            #[cfg(target_os = "macos")]
            {
                assert_eq!(s.displays, before.displays);
                let (displays, resize) = (rng.displays(), rng.resize());
                s.pipeline(displays.clone(), resize);
                assert_eq!(s.displays, Some(displays));
                assert_eq!(scale(s.resize), scale(resize));
            }
        }
    }

    /// An ack pops nothing it did not name, whatever came between: over any interleaving of
    /// viewers that watch, stop and ask for keyframes, streams rebuilt, frames sent, and acks
    /// of any frame numbered before (one in flight, one acked already, one from before a
    /// watch or a rebuild, one sent to the other viewer), every frame an ack takes out of
    /// flight, and every frame it covers in the trace, left no later than the frame it
    /// names. Frames are numbered as the encode loop numbers them, by the hub's timeline;
    /// the test knows each frame by the order it numbered it in (its bytes), not by its
    /// number. Numbering that started again at a rebuild fails it.
    #[test]
    fn an_ack_pops_nothing_it_did_not_name() {
        let mut rng = Rng(crate::xorshift::Xorshift::new(0x9E37_79B9_7F4A_7C15));
        // Acks that took frames out of flight; that named a frame from before the viewer
        // last watched, started over or started, and took nothing.
        let (mut popped, mut stale) = (0, 0);
        for _ in 0..40 {
            let t0 = Instant::now();
            let hub = Hub::new(Paths::ANY);
            let c = [0, 1].map(|_| hub.add_client(mpsc::unbounded_channel().0, t0));
            // Every frame numbered, in the order numbered: its number.
            let mut numbered: Vec<u32> = vec![];
            let mut now = t0;
            for _ in 0..500 {
                now += Duration::from_millis(rng.below(20));
                let i = rng.below(2) as usize;
                match rng.below(12) {
                    0 => c[i].watch(now),
                    1 => c[i].stop_watching(),
                    2 => c[i].ask_keyframe(),
                    3 => hub.restart_clients(),
                    4 => {
                        // A keyframe planned and sent: it starts who it was promised to.
                        let promised = c.each_ref().map(|c| c.keyframe_planned());
                        for (c, _) in c.iter().zip(promised).filter(|(_, p)| *p) {
                            c.keyframe_sent(true, now);
                        }
                    }
                    5..=7 => {
                        let id = hub.timeline.number();
                        let k = numbered.len();
                        numbered.push(id.get());
                        for c in &c {
                            c.takes(id, now, k, false, usize::MAX);
                        }
                    }
                    _ if numbered.is_empty() => {}
                    _ => {
                        // Any frame numbered so far, most often a recent one.
                        let back = match rng.below(2) {
                            0 => rng.below(4),
                            _ => rng.below(numbered.len() as u64),
                        };
                        let named = numbered.len() - 1 - (back as usize).min(numbered.len() - 1);
                        let flying = |c: &Client| -> Vec<usize> {
                            c.viewing.lock().inflight.iter().map(|f| f.bytes).collect()
                        };
                        let covered = |c: &Client| -> Vec<usize> {
                            let t = c.trace.lock();
                            t.frames()
                                .filter(|f| f.ack.is_some())
                                .map(|f| f.frame.bytes)
                                .collect()
                        };
                        let (before, covered_before) = (flying(&c[i]), covered(&c[i]));
                        c[i].on_ack(now, numbered[named], Hold::Unsaid);
                        let (after, covered_after) = (flying(&c[i]), covered(&c[i]));
                        let gone = &before[..before.len() - after.len()];
                        assert_eq!(after, before[gone.len()..], "only from the front");
                        assert!(
                            gone.iter().all(|&k| k <= named),
                            "the ack of frame {named} took {gone:?} out of flight"
                        );
                        let newly = covered_after.iter().filter(|k| !covered_before.contains(k));
                        assert!(newly.clone().all(|&k| k <= named), "covered {newly:?}");
                        popped += usize::from(!gone.is_empty());
                        stale += usize::from(
                            gone.is_empty() && before.first().is_some_and(|&k| k > named),
                        );
                    }
                }
            }
        }
        assert!(
            popped > 100 && stale > 100,
            "popped {popped}, stale {stale}"
        );
    }
}
