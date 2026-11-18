//! Shared state between media producers (video, audio, cursor, clipboard) and the
//! sessions that watch them.

use crate::lanes::wire::{Attach, Epoch, Generation, LaneIndex, Number};
use crate::session::sender::{KeyRoom, Queue, Room, Sender};
use bytes::Bytes;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// What a session's control stream carries to its relay: a message for the page, which
/// the relay hands on as it is (binary: a cursor's or the clipboard's image; text), or the
/// confirmation of a lane attached.
#[derive(Clone, Debug, PartialEq)]
pub enum Out {
    Bin(Bytes),
    Text(ServerMessage),
    Attached {
        lane: LaneIndex,
        generation: Generation,
    },
    /// The lanes the session's link wants the relay to deal its frames over.
    Lanes(crate::lanes::model::Lanes),
    /// An Opus packet of desktop audio, to a recording alone (a session's audio goes on
    /// its lanes): the packet, and the frames of silence the encoder left out since the
    /// one before.
    Audio {
        packet: Bytes,
        silent: u32,
    },
}

/// A text message to a viewer: JSON, named by its `t`. `ServerMessage` in
/// web/src/protocol.ts names the same three, and `json` is the one place one is written.
#[derive(Clone, Debug, PartialEq)]
pub enum ServerMessage {
    Hello(Hello),
    /// The desktop's clipboard, as text.
    Clip {
        d: String,
    },
    /// Whether the desktop's microphone is this viewer's.
    Mic {
        on: bool,
    },
}

impl ServerMessage {
    /// The message as a JSON object: its kind as `t`, first, then its fields, as `Hello`
    /// and the rest in web/src/protocol.ts name them. Every field is text, a whole number,
    /// a truth or a float, each written here, so nothing can fail to be written.
    pub fn json(&self) -> String {
        let mut o = Object::new(match self {
            ServerMessage::Hello(_) => "hello",
            ServerMessage::Clip { .. } => "clip",
            ServerMessage::Mic { .. } => "mic",
        });
        match self {
            ServerMessage::Hello(h) => {
                o.raw("w", &h.w.to_string());
                o.raw("h", &h.h.to_string());
                if let Some(s) = &h.settings.startup {
                    o.raw("audio", &s.audio.to_string());
                    let mut files = Object::empty();
                    files.text("root", &s.files.root);
                    files.text("dir", &s.files.dir);
                    o.raw("files", &files.end());
                }
                if let Some(r) = h.settings.resize {
                    let mut resize = Object::empty();
                    resize.raw("scale", &float(r.scale));
                    o.raw("resize", &resize.end());
                }
            }
            ServerMessage::Clip { d } => o.text("d", d),
            ServerMessage::Mic { on } => o.raw("on", &on.to_string()),
        }
        o.end()
    }
}

/// A JSON object being written.
struct Object(String);

impl Object {
    /// One whose first field is its kind, `t`.
    fn new(kind: &str) -> Object {
        let mut o = Object::empty();
        o.text("t", kind);
        o
    }
    fn empty() -> Object {
        Object(String::from("{"))
    }
    /// A field whose value is already JSON.
    fn raw(&mut self, name: &str, value: &str) {
        if self.0.len() > 1 {
            self.0.push(',');
        }
        quote(&mut self.0, name);
        self.0.push(':');
        self.0.push_str(value);
    }
    fn text(&mut self, name: &str, value: &str) {
        let mut v = String::with_capacity(value.len() + 2);
        quote(&mut v, value);
        self.raw(name, &v);
    }
    fn end(mut self) -> String {
        self.0.push('}');
        self.0
    }
}

/// `s` as a JSON string (RFC 8259 §7): a quote and a backslash escaped, and every control
/// character as `\u00XX`.
fn quote(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                let n = u32::from(c) as usize;
                out.push_str("\\u00");
                out.push(char::from(HEX[n >> 4]));
                out.push(char::from(HEX[n & 15]));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A float as JSON: Rust's shortest form that reads back as the same float, which JSON
/// takes; JSON has no NaN or infinity, which are `null`, as serde_json writes them.
fn float(f: f64) -> String {
    match f.is_finite() {
        true => format!("{f:?}"),
        false => "null".into(),
    }
}

/// The `hello` message: the stream's geometry and the server's settings, the fields
/// `Hello` in web/src/protocol.ts names.
#[derive(Clone, Debug, PartialEq)]
pub struct Hello {
    pub w: u32,
    pub h: u32,
    pub settings: Settings,
}

/// What a hello tells a viewer besides the geometry: what voxl started with, which `run`
/// says once, before it serves a viewer, and dynamic resolution's scale.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings {
    /// None until `run` has started all it reports: only the first pipeline's hello,
    /// which goes to nobody, is without it.
    pub startup: Option<Startup>,
    pub resize: Option<Resize>,
}

impl Settings {
    #[cfg(target_os = "linux")]
    /// What `run` says, once it has started everything a viewer is told of.
    pub fn start(&mut self, startup: Startup, resize: Option<Resize>) {
        self.startup = Some(startup);
        self.resize = resize;
    }
}

/// What voxl started with, as a viewer needs it.
#[derive(Clone, Debug, PartialEq)]
pub struct Startup {
    /// Desktop audio is sent.
    pub audio: bool,
    pub files: FilesConfig,
}

/// Where a viewer's files are (`FilesConfig` in web/src/protocol.ts), and what the relay
/// hands `voxl files` back for it: the root.
#[derive(Clone, Debug, PartialEq)]
pub struct FilesConfig {
    /// The shared folder (`$HOME`), canonical, as text: `voxl files --root`.
    pub root: String,
    /// The folder, relative to the shared root, the panel opens and uploads into.
    pub dir: String,
}

/// Dynamic resolution: desktop pixels per CSS pixel of the viewer's window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Resize {
    pub scale: f64,
}

/// What a client is to the stream. Each change is a method of `Client`, named for the step
/// that makes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Viewer {
    /// It is sent no video.
    NotWatching,
    /// It waits for a keyframe, and none has been planned for it: the capture loop plans
    /// one once it has a lane to be sent it on.
    Waiting,
    /// A keyframe has been planned for it: it starts on that keyframe if it comes out
    /// whole, and waits again if it does not. One still `Waiting` is not started by it, as
    /// it may have joined after the frame was planned, even after the stream that coded it
    /// ended.
    Promised,
    /// It is sent every frame.
    Started,
}

/// What asking for a keyframe makes of a viewer: one that watches wants one, at once,
/// unless its request is already granted, which asking again does not change. One promised
/// a keyframe keeps that promise: asking again would plan a second keyframe.
fn asked(viewer: Viewer) -> Viewer {
    match viewer {
        Viewer::NotWatching => Viewer::NotWatching,
        Viewer::Promised => Viewer::Promised,
        Viewer::Waiting | Viewer::Started => Viewer::Waiting,
    }
}

/// What starting the stream over makes of a viewer: one that watches waits for a keyframe
/// of the new stream. A keyframe planned for it went with the old one.
fn restarted(viewer: Viewer) -> Viewer {
    match viewer {
        Viewer::NotWatching => Viewer::NotWatching,
        Viewer::Waiting | Viewer::Promised | Viewer::Started => Viewer::Waiting,
    }
}

/// What a client is to the stream and its lanes, under one lock, so that every change to
/// them is one step, and every read of them one state.
struct Viewing {
    viewer: Viewer,
    sender: Sender,
}

/// A session, as the media producers see it.
pub struct Client {
    pub id: u64,
    /// Its control stream's writer.
    control: mpsc::UnboundedSender<Out>,
    viewing: Mutex<Viewing>,
    /// It wants desktop audio.
    pub audio: AtomicBool,
}

/// What a keyframe may take of a budget: what it sends in this long. Provisional: upstream
/// measured it on a 3024x1898 terminal page (CRF 24, a viewer behind an emulated 6 Mbit/s,
/// 80 ms link): a reload's first picture was 41-44 dB (246-284 KB) at 500 ms of the rate,
/// against 36 dB at 250 ms and 27.5-27.7 dB from one frame's share alone; the record of
/// those runs was not kept. What would measure it again is the bench's cold load over the
/// real tunnel at 250, 500 and 1000 ms: the first picture's PSNR and the time the page is
/// sharp.
pub const KEY_TIME: Duration = Duration::from_millis(500);

/// What the capture loop sees of a watching client with a lane in one look at it: what it
/// is to the stream, what its lanes let a frame take, what a keyframe may take, and what
/// its lanes hold unreported.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Seen {
    pub viewer: Viewer,
    pub room: Room,
    pub key: KeyRoom,
    /// What a sharpening pass may take: the room past what the next capture needs.
    pub spare: Room,
}

impl Client {
    /// A message on its control stream.
    pub fn send(&self, m: Out) {
        // A session whose control writer has ended is closing: it is sent nothing more.
        if self.control.send(m).is_err() {
            log::debug!("session {}: closing, a message dropped", self.id);
        }
    }

    /// What it is to the stream, as one read sees it.
    #[cfg(test)]
    pub fn viewer(&self) -> Viewer {
        self.viewing.lock().viewer
    }

    /// Its lanes, for one step (a report, a desktop audio packet, its close).
    pub fn lanes<R>(&self, f: impl FnOnce(&mut Sender) -> R) -> R {
        f(&mut self.viewing.lock().sender)
    }

    /// What the capture loop sees of it, if it watches and has a lane to be sent frames
    /// on, captures coming `frame` apart: one without a lane is sent nothing and holds
    /// nobody until it has one.
    pub fn look(&self, frame: Duration) -> Option<Seen> {
        let v = self.viewing.lock();
        let watches = v.viewer != Viewer::NotWatching && v.sender.attached();
        watches.then(|| Seen {
            viewer: v.viewer,
            room: v.sender.room(),
            key: v.sender.key(KEY_TIME),
            spare: v.sender.spare(frame),
        })
    }

    /// What `voxl stats` says of it, as one read of it sees it.
    pub fn stats(&self) -> serde_json::Value {
        let v = self.viewing.lock();
        let viewer = match v.viewer {
            Viewer::NotWatching => "not_watching",
            Viewer::Waiting => "waiting",
            Viewer::Promised => "promised",
            Viewer::Started => "started",
        };
        serde_json::json!({
            "id": self.id,
            "viewer": viewer,
            "audio": self.audio.load(Ordering::Relaxed),
            "way": v.sender.stats(),
        })
    }

    /// It starts watching (`v,1`): it waits for a keyframe.
    pub fn watch(&self) {
        self.viewing.lock().viewer = Viewer::Waiting;
    }

    /// It stops watching (`v,0`).
    pub fn stop_watching(&self) {
        self.viewing.lock().viewer = Viewer::NotWatching;
    }

    /// It asks for a keyframe (`kf`: a decoder the browser broke): if it watches, it wants
    /// one at once, unless one is already promised it (`asked`).
    pub fn ask_keyframe(&self) {
        let mut v = self.viewing.lock();
        v.viewer = asked(v.viewer);
    }

    /// The stream starts over for it (it is rebuilt, or a display switch failed), and its
    /// page is told so by `hello`, which resets the page's decoders: if it watches, it waits
    /// for a keyframe of what follows. The hello is queued under the lock a keyframe is
    /// planned, sent and taken under, so a keyframe planned for it after this reaches the
    /// page behind the hello, and one planned before is not sent it.
    pub fn start_over(&self, hello: ServerMessage) {
        let mut v = self.viewing.lock();
        v.viewer = restarted(v.viewer);
        self.send(Out::Text(hello));
    }

    /// The capture loop planned a keyframe: one waiting for one, with a lane to be sent it
    /// on, is promised it. Whether it was, and so whether that keyframe starts it.
    pub fn keyframe_planned(&self) -> bool {
        let mut v = self.viewing.lock();
        match (v.viewer, v.sender.attached()) {
            (Viewer::Waiting, true) => {
                v.viewer = Viewer::Promised;
                true
            }
            (Viewer::Waiting, false)
            | (Viewer::NotWatching | Viewer::Promised | Viewer::Started, _) => false,
        }
    }

    /// The encode loop sent a keyframe this client was promised, `whole` when it came out
    /// a keyframe. A whole one starts it; one that is not leaves it waiting for
    /// another. A client that has since stopped watching, asked again or started over is
    /// what that made it.
    pub fn keyframe_sent(&self, whole: bool) {
        let mut v = self.viewing.lock();
        match (v.viewer, whole) {
            (Viewer::Promised, true) => v.viewer = Viewer::Started,
            (Viewer::Promised, false) => v.viewer = Viewer::Waiting,
            (Viewer::NotWatching | Viewer::Waiting | Viewer::Started, _) => {}
        }
    }

    /// The encode loop has an update (its number and epoch) to send at
    /// `now`: a client that has started takes it onto its lanes. Whether it did.
    pub fn takes(&self, update: (Number, Epoch), bytes: &Bytes, now: Instant) -> bool {
        let mut v = self.viewing.lock();
        let Viewing { viewer, sender } = &mut *v;
        match viewer {
            Viewer::Started => {
                sender.video(update, bytes, now);
                true
            }
            Viewer::NotWatching | Viewer::Waiting | Viewer::Promised => false,
        }
    }
}

/// A non-blocking, close-on-exec pipe for waking a thread that polls file descriptors
/// (`pipe2` with those flags, which macOS lacks).
pub fn wake_pipe() -> nix::Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    use nix::fcntl::{fcntl, FcntlArg, FdFlag, OFlag};
    let (r, w) = nix::unistd::pipe()?;
    for fd in [&r, &w] {
        fcntl(fd, FcntlArg::F_SETFL(OFlag::O_NONBLOCK))?;
        fcntl(fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
    }
    Ok((r, w))
}

/// Something `wait_any` can wait for: a file descriptor.
pub type Waitable<'a> = std::os::fd::BorrowedFd<'a>;

/// Wakes a thread blocked in `wait_any`: a pipe.
pub struct Wake {
    r: std::os::fd::OwnedFd,
    w: std::os::fd::OwnedFd,
}

impl Wake {
    pub fn new() -> std::io::Result<Self> {
        let (r, w) = wake_pipe()?;
        Ok(Wake { r, w })
    }

    pub fn signal(&self) {
        // A full pipe already holds a wake; any other failure is the pipe's, said.
        match nix::unistd::write(&self.w, &[1u8]) {
            Ok(_) | Err(nix::errno::Errno::EAGAIN) => {}
            Err(e) => log::error!("a wake not given: {e}"),
        }
    }

    pub fn drain(&self) {
        let mut buf = [0u8; 64];
        while matches!(nix::unistd::read(&self.r, &mut buf), Ok(n) if n > 0) {}
    }

    pub fn waitable(&self) -> Waitable<'_> {
        std::os::fd::AsFd::as_fd(&self.r)
    }
}

/// Blocks until one of `w` is ready or `timeout` passes, with no limit if it is None, to
/// the nanosecond (`ppoll`). It allocates nothing. A signal ends it early, as a wake
/// does: its caller looks again. Any other failure is `ppoll`'s on fds it was handed,
/// which nothing here can mend.
#[cfg(target_os = "linux")]
pub fn wait_any<const N: usize>(
    w: [Waitable; N],
    timeout: Option<std::time::Duration>,
) -> nix::Result<()> {
    use nix::poll::{PollFd, PollFlags};
    let mut p = w.map(|fd| PollFd::new(fd, PollFlags::POLLIN));
    match nix::poll::ppoll(
        &mut p,
        timeout.map(nix::sys::time::TimeSpec::from_duration),
        None,
    ) {
        Ok(_) | Err(nix::errno::Errno::EINTR) => Ok(()),
        Err(e) => Err(e),
    }
}

/// What the pipeline has done since voxl started, for `voxl stats`: counts, and
/// microseconds spent in each step, all frames together.
#[derive(Default)]
pub struct Stats {
    pub captured: AtomicU64,
    pub encoded: AtomicU64,
    pub bytes: AtomicU64,
    pub encode_us: AtomicU64,
    pub grab_us: AtomicU64,
    pub convert_us: AtomicU64,
    pub paintovers: AtomicU64,
    pub keyframes: AtomicU64,
    pub refines: AtomicU64,
    pub damage_wait_us: AtomicU64,
    pub send_lat_us: AtomicU64,
}

pub struct Hub {
    pub clients: Mutex<Vec<Arc<Client>>>,
    /// Signalled, under `clients`, when a session asks for desktop audio: the audio
    /// thread waits on it while none wants any.
    audio_asked: parking_lot::Condvar,
    pub next_id: AtomicU64,
    pub width: AtomicU32,
    pub height: AtomicU32,
    /// Last cursor message, replayed to new clients.
    pub cursor: Mutex<Option<Bytes>>,
    /// Wakes the capture thread (reports, joins, lanes attached, keyframe requests).
    pub waker: Wake,
    pub input: Mutex<Option<std::sync::mpsc::Sender<crate::event::InputEvent>>>,
    #[cfg(target_os = "linux")]
    pub last_clipboard: Mutex<Option<String>>,
    /// The newest ask for a desktop size (dynamic resolution), until the capture loop takes
    /// it and gives it to the screen.
    pub resize_req: Mutex<Option<(u32, u32)>>,
    /// Largest desktop size (Some when dynamic resolution is on).
    pub resize_max: Mutex<Option<(u32, u32)>>,
    /// What every `hello` says besides the geometry.
    pub settings: Mutex<Settings>,
    /// The updates sessions reported missed, for the encoder.
    pub misses: Arc<crate::video::Misses>,
    pub stats: Stats,
    /// Screenshots asked for (`voxl screenshot`), answered by the capture thread with the
    /// screen as it is.
    pub snapshots: Mutex<Vec<tokio::sync::oneshot::Sender<Arc<crate::record::Snapshot>>>>,
    /// When the hub was made: `voxl stats` counts its times from it.
    pub start: Instant,
}

impl Hub {
    /// A hub with no session; its waker's pipe is what can fail.
    pub fn new() -> std::io::Result<Arc<Self>> {
        Ok(Arc::new(Hub {
            clients: Mutex::new(Vec::new()),
            audio_asked: parking_lot::Condvar::new(),
            next_id: AtomicU64::new(1),
            width: AtomicU32::new(0),
            height: AtomicU32::new(0),
            cursor: Mutex::new(None),
            waker: Wake::new()?,
            input: Mutex::new(None),
            #[cfg(target_os = "linux")]
            last_clipboard: Mutex::new(None),
            resize_req: Mutex::new(None),
            resize_max: Mutex::new(None),
            settings: Mutex::new(Settings::default()),
            misses: Arc::default(),
            stats: Stats::default(),
            snapshots: Mutex::new(Vec::new()),
            start: Instant::now(),
        }))
    }

    pub fn wake(&self) {
        self.waker.signal();
    }

    pub fn drain_wake(&self) {
        self.waker.drain();
    }

    /// A session joins, its control stream written through `control`, its lanes `sender`.
    pub fn add_client(&self, control: mpsc::UnboundedSender<Out>, sender: Sender) -> Arc<Client> {
        let c = Arc::new(Client {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            control,
            viewing: Mutex::new(Viewing {
                viewer: Viewer::NotWatching,
                sender,
            }),
            audio: AtomicBool::new(false),
        });
        self.clients.lock().push(c.clone());
        c
    }

    /// A recording joins: it takes every frame, as a page's video messages on `to`, and
    /// starts on a keyframe as any viewer does; it wants the desktop's audio, which a
    /// desktop without audio never sends.
    pub fn add_recorder(&self, to: mpsc::UnboundedSender<Out>) -> Arc<Client> {
        let c = self.add_client(to.clone(), Sender::recording(to));
        self.set_audio(&c, true);
        c.watch();
        self.wake();
        c
    }

    /// Client `id` leaves: it is sent nothing more.
    pub fn remove_client(&self, id: u64) {
        self.clients.lock().retain(|c| c.id != id);
        self.wake();
    }

    /// A lane's attach, arriving at `now` with its writer's queue: it joins the session
    /// whose capability it presents, which is told so on its control stream. Every open
    /// session's capability is compared, each in constant time. Whether one took it.
    pub fn attach(&self, attach: Attach, queue: Arc<Queue>, now: Instant) -> bool {
        let clients = self.clients.lock().clone();
        let whose = clients.iter().fold(None, |found, c| {
            let is = c.lanes(|s| s.is(&attach.capability));
            found.or(is.then_some(c))
        });
        let Some(c) = whose else {
            return false;
        };
        let (lane, generation) = (attach.lane, attach.generation);
        let taken = c.lanes(|s| s.attach((lane, generation), queue, attach.sent, now));
        if taken {
            c.send(Out::Attached { lane, generation });
            self.wake();
        }
        taken
    }

    /// Each watching client with a lane and what one look at it saw, into `into` in place
    /// of what it held, in the hub's order: one look a capture keeps one vector, and
    /// allocates nothing once it is big enough.
    pub fn look_into(&self, frame: Duration, into: &mut Vec<(Arc<Client>, Seen)>) {
        into.clear();
        for c in self.clients.lock().iter() {
            into.extend(c.look(frame).map(|seen| (c.clone(), seen)));
        }
    }

    /// Every client, into `into` in place of what it held: the encode loop's, which keeps
    /// one vector.
    pub fn clients_into(&self, into: &mut Vec<Arc<Client>>) {
        into.clear();
        into.extend(self.clients.lock().iter().cloned());
    }

    /// What `voxl stats` says: the pipeline's counts and times since the hub was made, and
    /// each client as one read of it sees it.
    pub fn stats(&self) -> serde_json::Value {
        let st = &self.stats;
        let n = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let clients: Vec<_> = self.clients.lock().iter().map(|c| c.stats()).collect();
        serde_json::json!({
            "uptime_us": self.start.elapsed().as_micros() as u64,
            "captured": n(&st.captured),
            "encoded": n(&st.encoded),
            "bytes": n(&st.bytes),
            "encode_us_total": n(&st.encode_us),
            "grab_us_total": n(&st.grab_us),
            "convert_us_total": n(&st.convert_us),
            "paintovers": n(&st.paintovers),
            "keyframes": n(&st.keyframes),
            "refines": n(&st.refines),
            "damage_wait_us_total": n(&st.damage_wait_us),
            "send_lat_us_total": n(&st.send_lat_us),
            "clients": clients,
        })
    }

    /// The `hello` message: current geometry plus the server's settings.
    pub fn hello(&self) -> ServerMessage {
        let settings = self.settings.lock().clone();
        ServerMessage::Hello(Hello {
            w: self.width.load(Ordering::Relaxed),
            h: self.height.load(Ordering::Relaxed),
            settings,
        })
    }

    /// After the stream restarts (new size): every client needs the new geometry and a
    /// keyframe.
    pub fn restart_clients(&self) {
        let hello = self.hello();
        for c in self.clients.lock().iter() {
            c.start_over(hello.clone());
        }
    }

    /// Session `c` wants desktop audio, or no longer does.
    pub fn set_audio(&self, c: &Client, on: bool) {
        let _clients = self.clients.lock();
        c.audio.store(on, Ordering::Relaxed);
        self.audio_asked.notify_all();
    }

    #[cfg(target_os = "linux")]
    /// Whether any session wants desktop audio.
    pub fn audio_wanted(&self) -> bool {
        let clients = self.clients.lock();
        clients.iter().any(|c| c.audio.load(Ordering::Relaxed))
    }

    #[cfg(target_os = "linux")]
    /// Blocks until a session wants desktop audio.
    pub fn wait_for_audio(&self) {
        let mut clients = self.clients.lock();
        while !clients.iter().any(|c| c.audio.load(Ordering::Relaxed)) {
            self.audio_asked.wait(&mut clients);
        }
    }

    #[cfg(target_os = "linux")]
    /// An Opus packet of desktop audio, `silent` frames of silence after the one before:
    /// onto the lanes of every session that wants it.
    pub fn audio(&self, packet: &[u8], silent: u32) {
        let now = Instant::now();
        for c in self.clients.lock().iter() {
            if c.audio.load(Ordering::Relaxed) {
                c.lanes(|s| s.audio(packet, silent, now));
            }
        }
    }

    #[cfg(target_os = "linux")]
    pub fn broadcast(&self, m: impl Fn() -> Out) {
        for c in self.clients.lock().iter() {
            c.send(m());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lanes::model::Lanes;
    use crate::lanes::wire::Capability;

    /// A step of the hub's, for one of two clients or for all.
    #[derive(Clone, Copy, Debug)]
    enum Step {
        /// `v,1`.
        Watch(usize),
        /// `v,0`.
        StopWatching(usize),
        /// `kf`.
        AskKeyframe(usize),
        /// The hello after a failed display switch: the stream starts over for this client
        /// alone, and what the encoder holds is still sent.
        StartOver(usize),
        /// Its first lane attaches.
        Attach(usize),
        /// `build` starts the stream over for every client (`restart_clients`), once the
        /// stream before has sent what it held.
        Restart,
        /// The capture loop reads the viewers and plans a keyframe if one wants it, or a
        /// frame that is not one; the encoder holds at most two.
        Capture,
        /// The encode loop sends the oldest frame it holds: a keyframe whole or not, any
        /// other frame not whole.
        Send { whole: bool },
    }

    const STEPS: [Step; 13] = [
        Step::Watch(0),
        Step::Watch(1),
        Step::StopWatching(0),
        Step::StopWatching(1),
        Step::AskKeyframe(0),
        Step::AskKeyframe(1),
        Step::StartOver(0),
        Step::StartOver(1),
        Step::Attach(0),
        Step::Attach(1),
        Step::Restart,
        Step::Capture,
        Step::Send { whole: true },
    ];

    /// A frame the encoder holds: a keyframe, with the clients its plan promised it to,
    /// or a frame that is not one.
    type Held = Option<[bool; 2]>;

    /// Two clients (what each is to the stream, and whether it has a lane), and the frames
    /// the encoder holds, oldest first.
    #[derive(Clone, Debug, PartialEq, Eq, Hash)]
    struct Model {
        c: [(Viewer, bool); 2],
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
            Step::Watch(i) => c[i].0 = Waiting,
            Step::StopWatching(i) => c[i].0 = NotWatching,
            // Asking: one that watches wants a keyframe at once, unless one is promised it.
            Step::AskKeyframe(i) => {
                if let Waiting | Started = c[i].0 {
                    c[i].0 = Waiting;
                }
            }
            Step::StartOver(i) => {
                if c[i].0 != NotWatching {
                    c[i].0 = Waiting;
                }
            }
            Step::Attach(i) => c[i].1 = true,
            Step::Restart => {
                for c in c.iter_mut() {
                    if c.0 != NotWatching {
                        c.0 = Waiting;
                    }
                }
                m.held.clear();
            }
            // A keyframe is planned for those waiting with a lane, and only they are
            // promised it.
            Step::Capture => {
                let starts = c.map(|c| c == (Waiting, true));
                planned = starts.iter().any(|&s| s);
                for c in c.iter_mut() {
                    if *c == (Waiting, true) {
                        c.0 = Promised;
                    }
                }
                m.held.push(planned.then_some(starts));
            }
            Step::Send { whole } => {
                let starts = m.held.remove(0).unwrap_or([false; 2]);
                for (i, c) in c.iter_mut().enumerate() {
                    if starts[i] && c.0 == Promised {
                        c.0 = match whole {
                            true => Started,
                            false => Waiting,
                        };
                    }
                    if c.0 == Started {
                        sent.push(i);
                    }
                }
            }
        }
        (planned, sent, m)
    }

    /// Whether the hub takes `step` in this state: the encoder holds two frames at most
    /// and sends only what it holds, and only a keyframe comes out whole.
    fn applies(m: &Model, step: Step) -> bool {
        match step {
            Step::Capture => m.held.len() < 2,
            Step::Send { whole } => match m.held.first() {
                None => false,
                Some(held) => !whole || held.is_some(),
            },
            Step::Watch(_)
            | Step::StopWatching(_)
            | Step::AskKeyframe(_)
            | Step::StartOver(_)
            | Step::Attach(_)
            | Step::Restart => true,
        }
    }

    /// A client of `hub` that is `viewer` to the stream, with a lane or not.
    fn client(hub: &Hub, (viewer, lane): (Viewer, bool)) -> Arc<Client> {
        let now = Instant::now();
        let (tx, _rx) = mpsc::unbounded_channel();
        let sender = Sender::new(Capability::mint().unwrap(), tx.clone(), now);
        let c = hub.add_client(tx, sender);
        c.viewing.lock().viewer = viewer;
        if lane {
            attach(&c);
        }
        c
    }

    fn attach(c: &Client) {
        let now = Instant::now();
        let place = (
            LaneIndex::all(Lanes::ONE).next().unwrap(),
            Generation::first(),
        );
        let sent = crate::lanes::wire::Stamp::since(now, now);
        assert!(c.lanes(|s| s.attach(place, Queue::new(), sent, now)));
    }

    /// The same step, by the methods, on two real clients of `hub` set to `m`: what it
    /// planned and sent, and the model after it as the clients are.
    fn new_step(hub: &Hub, m: &Model, step: Step) -> (bool, Vec<usize>, Model) {
        hub.clients.lock().clear();
        let c: Vec<Arc<Client>> = m.c.iter().map(|&s| client(hub, s)).collect();
        let mut after = m.clone();
        let (mut planned, mut sent) = (false, vec![]);
        match step {
            Step::Watch(i) => c[i].watch(),
            Step::StopWatching(i) => c[i].stop_watching(),
            Step::AskKeyframe(i) => c[i].ask_keyframe(),
            Step::StartOver(i) => c[i].start_over(hub.hello()),
            Step::Attach(i) => {
                // A second lane of the same set replaces the first: it is attached still.
                attach(&c[i]);
            }
            Step::Restart => {
                hub.restart_clients();
                after.held.clear();
            }
            Step::Capture => {
                let mut seen = Vec::new();
                hub.look_into(Duration::from_millis(16), &mut seen);
                planned = seen.iter().any(|(_, v)| v.viewer == Viewer::Waiting);
                let promised: Vec<u64> = match planned {
                    true => seen
                        .iter()
                        .filter(|(c, v)| v.viewer == Viewer::Waiting && c.keyframe_planned())
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
                            c.keyframe_sent(whole);
                        }
                    }
                }
                let epoch = Epoch::Start;
                for (i, c) in c.iter().enumerate() {
                    let update = Bytes::from_static(&[0; 16]);
                    if c.takes((Number::first(), epoch), &update, Instant::now()) {
                        sent.push(i);
                    }
                }
            }
        }
        after.c = [0, 1].map(|i| (c[i].viewer(), c[i].lanes(|s| s.attached())));
        (planned, sent, after)
    }

    /// A viewer starts only on a whole keyframe promised it, a keyframe is promised only
    /// to a viewer with a lane to be sent it on, a viewer is sent frames only once
    /// started, and every viewer watching with a lane is started in the end. Over every
    /// state two clients and the encoder reach in up to 10 of the hub's steps, breadth
    /// first, every step from every state, run on a real hub's clients through the
    /// methods: each step does what the rule says, a client that starts was promised a
    /// keyframe and was sent a whole one, and from every state, once the encoder has sent
    /// what it holds, one capture and its keyframe sent whole start every viewer watching
    /// with a lane.
    #[test]
    fn a_viewer_starts_only_on_a_whole_keyframe_promised_it() {
        let hub = Hub::new().unwrap();
        let start = Model {
            c: [(Viewer::NotWatching, false); 2],
            held: vec![],
        };
        let mut seen = std::collections::HashSet::from([start.clone()]);
        let mut frontier = vec![start];
        let (mut steps, mut waited_for_a_lane) = (0, 0);
        for _depth in 0..10 {
            let mut next = vec![];
            for before in frontier {
                for step in STEPS.into_iter().chain([Step::Send { whole: false }]) {
                    if !applies(&before, step) {
                        continue;
                    }
                    steps += 1;
                    let what = format!("{step:?} from {before:?}");
                    let (planned, sent, after) = new_step(&hub, &before, step);
                    assert_eq!(
                        (planned, sent.clone(), after.clone()),
                        rule(&before, step),
                        "{what}"
                    );
                    for (i, &(viewer, _)) in after.c.iter().enumerate() {
                        if viewer == Viewer::Started && before.c[i].0 != Viewer::Started {
                            assert!(
                                matches!(step, Step::Send { whole: true })
                                    && before.held[0].is_some_and(|starts| starts[i]),
                                "client {i} started: {what}"
                            );
                        }
                        if let (Step::Capture, (Viewer::Waiting, false)) = (step, before.c[i]) {
                            assert_eq!(after.c[i].0, Viewer::Waiting, "{what}");
                            waited_for_a_lane += 1;
                        }
                    }
                    if seen.insert(after.clone()) {
                        next.push(after);
                    }
                }
            }
            frontier = next;
        }
        for state in &seen {
            let mut m = state.clone();
            while let Some(held) = m.held.first() {
                let send = Step::Send {
                    whole: held.is_some(),
                };
                m = new_step(&hub, &m, send).2;
            }
            for step in [Step::Capture, Step::Send { whole: true }] {
                m = new_step(&hub, &m, step).2;
            }
            assert!(
                m.c.iter().all(|&(v, lane)| match lane {
                    true => matches!(v, Viewer::NotWatching | Viewer::Started),
                    false => matches!(v, Viewer::NotWatching | Viewer::Waiting),
                }),
                "left unstarted from {state:?}: {m:?}"
            );
        }
        let reached: std::collections::HashSet<Viewer> =
            seen.iter().flat_map(|m| m.c.map(|c| c.0)).collect();
        assert_eq!(reached.len(), 4, "{reached:?}");
        assert!(steps > 0 && waited_for_a_lane > 0);
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
                    root: self.text(),
                    dir: self.text(),
                },
            }
        }
        fn resize(&mut self) -> Option<Resize> {
            (self.next() & 1 == 1).then(|| Resize {
                scale: self.float(),
            })
        }
        fn settings(&mut self) -> Settings {
            Settings {
                startup: (self.next() & 1 == 1).then(|| self.startup()),
                resize: self.resize(),
            }
        }
        fn message(&mut self) -> ServerMessage {
            match self.below(3) {
                0 => ServerMessage::Hello(Hello {
                    w: self.next() as u32,
                    h: self.next() as u32,
                    settings: self.settings(),
                }),
                1 => ServerMessage::Clip { d: self.text() },
                _ => ServerMessage::Mic {
                    on: self.next() & 1 == 1,
                },
            }
        }
    }

    /// Gives `hub` the geometry and settings of `m`, if it is a hello.
    fn say(hub: &Hub, m: &ServerMessage) {
        if let ServerMessage::Hello(h) = m {
            hub.width.store(h.w, Ordering::Relaxed);
            hub.height.store(h.h, Ordering::Relaxed);
            *hub.settings.lock() = h.settings.clone();
        }
    }

    /// The model: a message is a JSON object of its fields, and its kind as `t`, first. A
    /// hub's hello is its geometry and its settings.
    #[test]
    fn a_message_is_its_fields_after_its_kind() {
        let mut rng = Rng(crate::xorshift::Xorshift::new(0x2545_f491_4f6c_dd1d));
        let hub = Hub::new().unwrap();
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
                    if let Some(s) = &h.settings.startup {
                        f.insert("audio".into(), s.audio.into());
                        let files = &s.files;
                        f.insert(
                            "files".into(),
                            serde_json::json!({ "root": files.root, "dir": files.dir }),
                        );
                    }
                    if let Some(r) = h.settings.resize {
                        f.insert("resize".into(), serde_json::json!({ "scale": r.scale }));
                    }
                    ("hello", serde_json::Value::Object(f))
                }
                ServerMessage::Clip { d } => ("clip", serde_json::json!({ "d": d })),
                ServerMessage::Mic { on } => ("mic", serde_json::json!({ "on": on })),
            };
            let json = m.json();
            assert!(json.starts_with(&format!("{{\"t\":\"{kind}\"")), "{json}");
            let mut object: serde_json::Map<String, serde_json::Value> =
                serde_json::from_str(&json).unwrap();
            assert_eq!(object.remove("t"), Some(kind.into()), "{json}");
            assert_eq!(serde_json::Value::Object(object), fields, "{json}");
        }
    }
}
