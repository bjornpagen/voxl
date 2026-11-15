//! Shared state between media producers (video/audio/cursor/clipboard) and websocket clients.

use bytes::Bytes;
use parking_lot::Mutex;
use std::collections::VecDeque;
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

/// The `hello` message: the stream's geometry and the server's settings.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Hello {
    pub w: u32,
    pub h: u32,
    /// Encoded stripes as (y, height).
    pub stripes: Vec<(u32, u32)>,
    /// What else a viewer is told (audio, files, ...), as `Hub::hello_extra` holds it.
    #[serde(flatten)]
    pub settings: serde_json::Map<String, serde_json::Value>,
}

pub struct Inflight {
    pub id: u32,
    pub sent: Instant,
    pub bytes: usize,
    /// Nothing sent before it was still unacknowledged when it went: whatever its
    /// delay, it did not queue behind this client's own frames.
    pub alone: bool,
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

/// The longest round trip a client's replies can show: 4 s. A client whose oldest
/// unacknowledged frame is that old is stalled, and the flow check resyncs it (video.rs),
/// so a longer round trip steers nothing; the bench's links have 80 to 140 ms. At half, a
/// path of 2 to 4 s would show no round trip and be judged by the 40 ms guess; at double,
/// a client's record would keep twice the pings for nothing.
const LONGEST_ROUND_TRIP: Duration = Duration::from_secs(4);

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
enum Viewer {
    /// It is sent no video.
    NotWatching,
    /// It waits for a keyframe, and none has been planned for it: the capture loop plans one.
    Waiting,
    /// A keyframe has been planned for it: it starts on the first whole keyframe sent. One
    /// still `Waiting` does not, as it may have joined after that frame was planned, even
    /// after the stream that coded it ended.
    Promised,
    /// It is sent every frame.
    Started,
}

pub struct Client {
    pub id: u64,
    pub tx: mpsc::UnboundedSender<Out>,
    viewer: Mutex<Viewer>,
    pub audio: AtomicBool,
    pub inflight: Mutex<VecDeque<Inflight>>,
    /// Smoothed min RTT (ms), from pings.
    pub min_rtt_ms: AtomicU32,
    /// The pings sent to it that it has not answered.
    pub pings: Mutex<Pings>,
    /// What the acks say about the path: its baseline delay, its capacity, its queue.
    pub path: Mutex<crate::path::PathEst>,
    /// How long the client keeps a frame between its arrival and the ack (decode, draw,
    /// frames ahead of it), as it reports it: smoothed ms, 0 for a client that doesn't.
    pub client_ms: Mutex<f64>,
}

impl Client {
    pub fn send(&self, m: Out) {
        let _ = self.tx.send(m);
    }

    /// Whether it watches: whether it is sent video once it has started.
    pub fn watching(&self) -> bool {
        match *self.viewer.lock() {
            Viewer::NotWatching => false,
            Viewer::Waiting | Viewer::Promised | Viewer::Started => true,
        }
    }

    /// Whether it waits for a keyframe that has not been planned.
    pub fn wants_keyframe(&self) -> bool {
        match *self.viewer.lock() {
            Viewer::Waiting => true,
            Viewer::NotWatching | Viewer::Promised | Viewer::Started => false,
        }
    }

    /// Whether it is sent every frame.
    pub fn started(&self) -> bool {
        match *self.viewer.lock() {
            Viewer::Started => true,
            Viewer::NotWatching | Viewer::Waiting | Viewer::Promised => false,
        }
    }

    /// It starts watching (`v,1`, or a recording begins): it waits for a keyframe, and acks
    /// for frames sent before no longer apply.
    pub fn watch(&self) {
        *self.viewer.lock() = Viewer::Waiting;
        self.inflight.lock().clear();
    }

    /// It stops watching (`v,0`).
    pub fn stop_watching(&self) {
        *self.viewer.lock() = Viewer::NotWatching;
    }

    /// It asks for a keyframe (`kf`, or a hello after a failed display switch): if it
    /// watches, it waits for one.
    pub fn ask_keyframe(&self) {
        let mut v = self.viewer.lock();
        *v = match *v {
            Viewer::NotWatching => Viewer::NotWatching,
            Viewer::Waiting | Viewer::Promised | Viewer::Started => Viewer::Waiting,
        };
    }

    /// The stream starts over for it (it is rebuilt, or the client stalled): if it watches,
    /// it waits for a keyframe; acks for frames sent before no longer apply.
    pub fn start_over(&self) {
        self.ask_keyframe();
        self.inflight.lock().clear();
    }

    /// The capture loop planned a keyframe: one waiting for one has it on its way.
    pub fn keyframe_planned(&self) {
        let mut v = self.viewer.lock();
        *v = match *v {
            Viewer::Waiting => Viewer::Promised,
            same @ (Viewer::NotWatching | Viewer::Promised | Viewer::Started) => same,
        };
    }

    /// Whether it is sent a frame the encode loop has coded, `whole_key` when every stripe
    /// of it is a keyframe. One with a keyframe on its way starts on the first whole one,
    /// and acks for frames sent before it no longer apply.
    pub fn takes(&self, whole_key: bool) -> bool {
        let mut v = self.viewer.lock();
        match (*v, whole_key) {
            (Viewer::Started, _) => true,
            (Viewer::Promised, true) => {
                *v = Viewer::Started;
                self.inflight.lock().clear();
                true
            }
            (Viewer::Promised, false) | (Viewer::NotWatching | Viewer::Waiting, _) => false,
        }
    }

    /// At `now`, the ack for frame `id` (and every one before it) arrived. What the
    /// client held of the delay is its own time, not the path's, so the delay sample
    /// ends that much earlier.
    pub fn on_ack(&self, now: Instant, id: u32, hold: Hold) {
        let mut q = self.inflight.lock();
        let mut sample = None;
        while let Some(f) = q.front() {
            if wrapping_le(f.id, id) {
                if f.id == id {
                    sample = Some((f.sent, f.bytes, f.alone));
                }
                q.pop_front();
            } else {
                break;
            }
        }
        drop(q);
        let Some((sent, bytes, alone)) = sample else {
            return;
        };
        let arrived = match hold {
            Hold::Unsaid => now,
            // A hold is part of the delay it was measured in.
            Hold::Said(held) if held <= now.saturating_duration_since(sent) => {
                let mut m = self.client_ms.lock();
                *m = *m * 0.9 + held.as_secs_f64() * 100.0;
                now - held
            }
            Hold::Said(_) | Hold::Unreadable => return,
        };
        self.path.lock().on_ack(arrived, sent, bytes, alone);
    }

    /// Delay queued on the path to this client now (ms).
    pub fn queue_ms(&self) -> f64 {
        let idle = self.inflight.lock().is_empty();
        self.path.lock().queue_ms(Instant::now(), idle)
    }

    /// What the path to this client can carry, once its frames have told.
    pub fn capacity(&self) -> Option<crate::path::Kbps> {
        self.path.lock().capacity()
    }

    /// The smallest round trip its pings have shown, once one has been answered.
    pub fn rtt_ms(&self) -> Option<u32> {
        match self.min_rtt_ms.load(Ordering::Relaxed) {
            0 => None,
            ms => Some(ms),
        }
    }

    /// Frames in flight and age of the oldest unacknowledged one.
    pub fn backlog(&self) -> (usize, u128) {
        let q = self.inflight.lock();
        (
            q.len(),
            q.front().map(|f| f.sent.elapsed().as_millis()).unwrap_or(0),
        )
    }
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

/// Blocks until one of `w` is ready.
pub fn wait_event(w: &[Waitable]) {
    wait_any(w, -1)
}

/// Blocks until one of `w` is ready or `timeout_ms` passes (negative: no timeout).
pub fn wait_any(w: &[Waitable], timeout_ms: i32) {
    #[cfg(unix)]
    {
        use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
        let mut p: Vec<PollFd> = w
            .iter()
            .map(|&fd| PollFd::new(fd, PollFlags::POLLIN))
            .collect();
        let _ = poll(
            &mut p,
            PollTimeout::try_from(timeout_ms).unwrap_or(PollTimeout::NONE),
        );
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        let hs: Vec<HANDLE> = w.iter().map(|h| HANDLE(h.as_raw_handle())).collect();
        let ms = if timeout_ms < 0 {
            windows::Win32::System::Threading::INFINITE
        } else {
            timeout_ms as u32
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
    /// The bitrate the stream is steered to (kbit/s; 0 until a pipeline has set one).
    /// Kept across pipelines; read through `target`.
    target_kbps: AtomicU32,
    /// Last cursor message, replayed to new clients.
    pub cursor: Mutex<Option<Bytes>>,
    /// Wakes the capture thread (acks, joins, keyframe requests).
    pub waker: Wake,
    pub input: Mutex<Option<std::sync::mpsc::Sender<crate::event::InputEvent>>>,
    pub last_clipboard: Mutex<Option<String>>,
    /// Desktop size a client asked for (dynamic resolution), applied by the video thread.
    pub resize_req: Mutex<Option<(u32, u32)>>,
    /// Largest desktop size (Some when dynamic resolution is on).
    pub resize_max: Mutex<Option<(u32, u32)>>,
    /// Server settings sent in every `hello` besides the geometry (audio, files, ...).
    pub hello_extra: Mutex<serde_json::Map<String, serde_json::Value>>,
    /// Screenshot requests, answered by the capture thread with the current screen.
    pub snapshots: Mutex<Vec<tokio::sync::oneshot::Sender<Arc<crate::record::Snapshot>>>>,
    /// When each part of the screen last changed (`/wait`).
    pub activity: crate::wait::Activity,
}

impl Hub {
    pub fn new() -> Arc<Self> {
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
            target_kbps: AtomicU32::new(0),
            stripes: Mutex::new(Vec::new()),
            cursor: Mutex::new(None),
            waker: Wake::new().expect("wake"),
            input: Mutex::new(None),
            last_clipboard: Mutex::new(None),
            resize_req: Mutex::new(None),
            resize_max: Mutex::new(None),
            hello_extra: Mutex::new(serde_json::Map::new()),
            snapshots: Mutex::new(Vec::new()),
            activity: Default::default(),
        })
    }

    pub fn wake(&self) {
        self.waker.signal();
    }

    pub fn drain_wake(&self) {
        self.waker.drain();
    }

    pub fn add_client(&self, tx: mpsc::UnboundedSender<Out>) -> Arc<Client> {
        let c = Arc::new(Client {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            tx,
            viewer: Mutex::new(Viewer::NotWatching),
            audio: AtomicBool::new(false),
            inflight: Mutex::new(VecDeque::new()),
            min_rtt_ms: AtomicU32::new(0),
            pings: Mutex::new(Pings::default()),
            path: Mutex::new(crate::path::PathEst::new(Instant::now())),
            client_ms: Mutex::new(0.0),
        });
        self.clients.lock().push(c.clone());
        c
    }

    pub fn remove_client(&self, id: u64) {
        self.clients.lock().retain(|c| c.id != id);
        self.wake();
    }

    pub fn video_clients(&self) -> Vec<Arc<Client>> {
        self.clients
            .lock()
            .iter()
            .filter(|c| c.watching())
            .cloned()
            .collect()
    }

    /// The bitrate the stream is steered to, once a pipeline has set one.
    pub fn target(&self) -> Option<crate::path::Kbps> {
        match self.target_kbps.load(Ordering::Relaxed) {
            0 => None,
            kbps => Some(crate::path::Kbps::new(kbps as f64)),
        }
    }

    pub fn set_target(&self, target: crate::path::Kbps) {
        self.target_kbps
            .store(target.get().max(1.0) as u32, Ordering::Relaxed);
    }

    /// The `hello` message: current geometry plus the server's settings.
    pub fn hello(&self) -> ServerMessage {
        let settings = self.hello_extra.lock().clone();
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
        for c in self.clients.lock().iter() {
            c.start_over();
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
        /// The flow check finds a watching client with frames in flight stalled (video.rs).
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
        use Viewer::{NotWatching, Promised, Started, Waiting};
        let mut m = before.clone();
        let c = &mut m.c;
        let (mut planned, mut sent) = (false, vec![]);
        match step {
            Step::Watch(i) | Step::Record(i) | Step::Stall(i) => c[i] = (Waiting, 0),
            Step::StopWatching(i) => c[i].0 = NotWatching,
            Step::AskKeyframe(i) => {
                if c[i].0 != NotWatching {
                    c[i].0 = Waiting;
                }
            }
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
                m.held.remove(0);
                for (i, c) in c.iter_mut().enumerate() {
                    if whole && c.0 == Promised {
                        *c = (Started, 0);
                    }
                    if c.0 == Started {
                        c.1 += 1;
                        sent.push(i);
                    }
                }
            }
            Step::Ack(i) => c[i].1 = 0,
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
    /// client; the flow check stalls only a watching client with frames in flight; a
    /// restart comes once the encoder has sent what it held; the encoder holds two frames
    /// at most and sends only what it holds, and only a keyframe comes out whole.
    fn applies(m: &Model, step: Step) -> bool {
        match step {
            Step::Record(i) => m.fresh[i],
            Step::Stall(i) => m.c[i].0 != Viewer::NotWatching && m.c[i].1 > 0,
            Step::Restart => m.held.is_empty(),
            Step::Capture => m.held.len() < 2,
            Step::Send { whole } => match m.held.first() {
                None => false,
                Some(held) => !whole || held.is_some(),
            },
            Step::Watch(_) | Step::StopWatching(_) | Step::AskKeyframe(_) | Step::Ack(_) => true,
        }
    }

    /// What the hub reads of a client: whether it watches, wants a keyframe and has
    /// started, and its frames in flight.
    type Read = (bool, bool, bool, usize);

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
                    let c = hub.add_client(mpsc::unbounded_channel().0);
                    *c.viewer.lock() = viewer;
                    for id in 1..=inflight as u32 {
                        c.inflight.lock().push_back(Inflight {
                            id,
                            sent: t0,
                            bytes: 1000,
                            alone: true,
                        });
                    }
                    c
                })
                .collect();
        let mut after = m.clone();
        let (mut planned, mut sent) = (false, vec![]);
        match step {
            Step::Watch(i) | Step::Record(i) => c[i].watch(),
            Step::StopWatching(i) => c[i].stop_watching(),
            Step::AskKeyframe(i) => c[i].ask_keyframe(),
            Step::Stall(i) => {
                assert!(
                    c[i].watching() && c[i].backlog().0 > 0,
                    "stalls only a watcher"
                );
                c[i].start_over();
            }
            Step::Restart => hub.restart_clients(),
            Step::Capture => {
                let clients = hub.video_clients();
                planned = clients.iter().any(|c| c.wants_keyframe());
                let starts = [0, 1].map(|i| c[i].wants_keyframe());
                if planned {
                    for c in &clients {
                        c.keyframe_planned();
                    }
                }
                after.held.push(planned.then_some(starts));
            }
            Step::Send { whole } => {
                after.held.remove(0);
                for v in hub.video_clients() {
                    if v.takes(whole) {
                        let id = v.inflight.lock().back().map_or(1, |f| f.id + 1);
                        v.inflight.lock().push_back(Inflight {
                            id,
                            sent: t0,
                            bytes: 1000,
                            alone: true,
                        });
                        sent.push(c.iter().position(|c| c.id == v.id).unwrap());
                    }
                }
            }
            Step::Ack(i) => {
                let last = c[i].inflight.lock().back().map(|f| f.id);
                if let Some(id) = last {
                    c[i].on_ack(t0 + Duration::from_millis(50), id, Hold::Unsaid);
                }
            }
        }
        after.c = [0, 1].map(|i| (*c[i].viewer.lock(), c[i].inflight.lock().len()));
        after.fresh = rule(m, step).2.fresh;
        let read = [0, 1].map(|i| {
            let c = &c[i];
            let inflight = c.backlog().0;
            (c.watching(), c.wants_keyframe(), c.started(), inflight)
        });
        let listed = hub.video_clients();
        for (i, c) in c.iter().enumerate() {
            assert_eq!(
                listed.iter().any(|v| v.id == c.id),
                after.c[i].0 != Viewer::NotWatching,
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
    #[ignore = "fails until a keyframe starts the viewers it was planned for, or sends them back to wait"]
    fn a_viewer_starts_only_on_a_whole_keyframe_promised_it() {
        let hub = Hub::new();
        let start = Model {
            c: [(Viewer::NotWatching, 0); 2],
            fresh: [true; 2],
            held: vec![],
        };
        let mut seen = std::collections::HashSet::from([start.clone()]);
        let mut frontier = vec![start];
        let mut steps = 0;
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
                        let as_it_is = (
                            viewer != Viewer::NotWatching,
                            viewer == Viewer::Waiting,
                            viewer == Viewer::Started,
                            inflight,
                        );
                        assert_eq!(read[i], as_it_is, "client {i}: {what}");
                        if viewer == Viewer::Started && before.c[i].0 != Viewer::Started {
                            assert!(
                                matches!(step, Step::Send { whole: true })
                                    && before.c[i].0 == Viewer::Promised,
                                "client {i} started: {what}"
                            );
                        }
                    }
                    assert!(
                        sent.iter().all(|&i| after.c[i].0 == Viewer::Started),
                        "{what}"
                    );
                    if seen.insert(after.clone()) {
                        next.push(after);
                    }
                }
            }
            frontier = next;
        }
        // Liveness: from every state, the encoder sends what it holds (each keyframe whole),
        // then one capture, and its keyframe sent whole: nobody watching is left unstarted.
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
            for step in [Step::Capture, Step::Send { whole: true }] {
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
        assert_eq!(reached.len(), 4, "{reached:?}");
        assert!(steps > 0);
    }

    /// Frame `id` of `bytes`, sent `id` x 100 ms after `t0`, which spent `path_ms` on
    /// the path and `held_ms` in the client and is acknowledged with `hold`.
    fn ack(
        c: &Client,
        t0: Instant,
        id: u32,
        bytes: usize,
        (path_ms, held_ms): (u64, u64),
        hold: Hold,
    ) {
        let sent = t0 + Duration::from_millis(id as u64 * 100);
        c.inflight.lock().push_back(Inflight {
            id,
            sent,
            bytes,
            alone: true,
        });
        c.on_ack(sent + Duration::from_millis(path_ms + held_ms), id, hold);
    }

    /// The time a client keeps a frame (decoding, drawing) is its own, not the path's:
    /// frames it was slow to show don't read as a queue once it says how long it took.
    #[test]
    fn the_clients_own_time_is_not_queueing() {
        let hub = Hub::new();
        let t0 = Instant::now();
        for says in [true, false] {
            let c = hub.add_client(mpsc::unbounded_channel().0);
            for (id, held) in (1..=20)
                .map(|id| (id, 2))
                .chain((21..=40).map(|id| (id, 40)))
            {
                let hold = if says {
                    Hold::Said(Duration::from_millis(held))
                } else {
                    Hold::Unsaid
                };
                ack(&c, t0, id, 2000, (50, held), hold);
            }
            let q = c.path.lock().queue_ms(t0, false);
            if says {
                assert!(q < 5.0, "{q:.1} ms of queue from a client that said");
            } else {
                assert!(
                    q > 15.0,
                    "{q:.1} ms: the old ack should still read the queue"
                );
            }
        }
    }

    /// The hold is whatever a client sends. A field that isn't a time parses to
    /// `Unreadable`, whatever it is; and an ack whose hold is unreadable, or longer
    /// than the delay it is part of, leaves the path estimate as it is for a twin that
    /// was never sent that frame.
    #[test]
    fn a_hold_that_cannot_be_true_says_nothing() {
        let unreadable = [
            "", "x", "NaN", "inf", "-inf", "-1", "-0.001", "1e30", "1e400",
        ];
        for field in unreadable {
            assert_eq!(Hold::parse(field), Hold::Unreadable, "{field:?}");
        }
        assert_eq!(Hold::parse(" 3.5"), Hold::Said(Duration::from_micros(3500)));

        let hub = Hub::new();
        let (c, twin) = (
            hub.add_client(mpsc::unbounded_channel().0),
            hub.add_client(mpsc::unbounded_channel().0),
        );
        let t0 = Instant::now();
        let good = Hold::Said(Duration::from_millis(1));
        for (i, lie) in [Hold::Unreadable, Hold::Said(Duration::from_secs(3600))]
            .into_iter()
            .cycle()
            .take(20)
            .enumerate()
        {
            let id = i as u32 * 2 + 1;
            ack(&c, t0, id, 90_000, (50, 1), lie);
            ack(&c, t0, id + 1, 2000 + i * 3000, (50 + i as u64, 1), good);
            ack(&twin, t0, id + 1, 2000 + i * 3000, (50 + i as u64, 1), good);
        }
        assert!(twin.capacity().is_some());
        assert_eq!(c.capacity(), twin.capacity());
        assert_eq!(
            c.path.lock().queue_ms(t0, false),
            twin.path.lock().queue_ms(t0, false)
        );
        assert_eq!(*c.client_ms.lock(), *twin.client_ms.lock());
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

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
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
        fn value(&mut self, depth: u32) -> serde_json::Value {
            use serde_json::Value;
            match self.below(if depth == 0 { 5 } else { 7 }) {
                0 => Value::Null,
                1 => Value::Bool(self.next() & 1 == 1),
                2 => Value::from(self.next() as i64 >> self.below(64)),
                // Not finite: null, as serde_json writes it.
                3 => Value::from(f64::from_bits(self.next())),
                4 => Value::String(self.text()),
                5 => Value::Array((0..self.below(4)).map(|_| self.value(depth - 1)).collect()),
                _ => Value::Object(
                    (0..self.below(4))
                        .map(|_| (self.text(), self.value(depth - 1)))
                        .collect(),
                ),
            }
        }
        fn message(&mut self) -> ServerMessage {
            match self.below(5) {
                0 => {
                    // What lib.rs and the macOS pipeline put there, with anything in them.
                    let mut settings = serde_json::Map::new();
                    for k in [
                        "os", "audio", "mic", "readOnly", "files", "resize", "displays",
                    ] {
                        if self.next() & 1 == 1 {
                            settings.insert(k.into(), self.value(2));
                        }
                    }
                    ServerMessage::Hello(Hello {
                        w: self.next() as u32,
                        h: self.next() as u32,
                        stripes: (0..self.below(17))
                            .map(|_| (self.next() as u32, self.next() as u32))
                            .collect(),
                        settings,
                    })
                }
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
            *hub.hello_extra.lock() = h.settings.clone();
        }
    }

    /// The model: a message is a JSON object of its fields, and its kind as `t`, first (a
    /// viewer may know a ping by its first bytes, as bench/nclient.py does). A hub's hello
    /// is its geometry and its settings.
    #[test]
    fn a_message_is_its_fields_after_its_kind() {
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        let hub = Hub::new();
        for _ in 0..100_000 {
            let m = rng.message();
            say(&hub, &m);
            if let ServerMessage::Hello(_) = m {
                assert_eq!(hub.hello(), m);
            }
            let (kind, fields) = match &m {
                ServerMessage::Hello(h) => {
                    let mut f = h.settings.clone();
                    f.insert("w".into(), h.w.into());
                    f.insert("h".into(), h.h.into());
                    f.insert("stripes".into(), serde_json::json!(h.stripes));
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
}
