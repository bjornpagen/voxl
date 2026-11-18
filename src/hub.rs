//! Shared state between media producers (video, audio, cursor, clipboard) and the
//! sessions that watch them.

use crate::lanes::wire::{Attach, Generation, Number, Outcome};
use crate::session::sender::{Delivery, Queue, Rate, Room, Sender};
use bytes::Bytes;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;

/// What a session's control stream carries to its relay: a message for the page, which
/// the relay hands on as it is (binary: a cursor's or the clipboard's image; text), or the
/// confirmation of a lane attached.
#[derive(Clone, Debug, PartialEq)]
pub enum Out {
    Bin(Bytes),
    Text(ServerMessage),
    Attached {
        generation: Generation,
    },
    /// The send buffer the session's lane socket was given, for the relay's receive buffer.
    Buffer(u32),
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
                if let Some(codec) = &h.codec {
                    o.text("codec", codec);
                }
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

/// The `hello` message: the stream's geometry and codec and the server's settings, the
/// fields `Hello` in web/src/protocol.ts names.
#[derive(Clone, Debug, PartialEq)]
pub struct Hello {
    pub w: u32,
    pub h: u32,
    /// The stream's codec as WebCodecs names it: none only before the first pipeline is
    /// built, whose hello goes to nobody.
    pub codec: Option<String>,
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
    /// It waits to start, and no keyframe has been planned for it: the capture loop plans
    /// one once it has a lane to be sent it on.
    Waiting,
    /// A keyframe has been planned for it: it starts on that keyframe. One still `Waiting` is
    /// not started by it, as it may have joined after the frame was planned, even after the
    /// stream that coded it ended.
    Promised,
    /// It is sent every frame, from keyframe `key` on: on a datagram lane, a report of that
    /// keyframe missed starts it again.
    Started { key: Number },
}

/// What starting the stream over makes of a viewer: one that watches waits to start on the
/// new stream. A keyframe planned for it went with the old one.
fn restarted(viewer: Viewer) -> Viewer {
    match viewer {
        Viewer::NotWatching => Viewer::NotWatching,
        Viewer::Waiting | Viewer::Promised | Viewer::Started { .. } => Viewer::Waiting,
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

/// What the capture loop sees of a watching client with a lane in one look at it: what it
/// is to the stream, and what its lane lets a frame take.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Seen {
    pub viewer: Viewer,
    pub room: Room,
}

/// The viewers a keyframe starts: at least one, each promised it (`Hub::promise`). A
/// keyframe is coded only for them: nothing else makes one.
pub struct Starts(Vec<Arc<Client>>);

impl Starts {
    /// The keyframe, update `key`, is coded: each viewer it was promised to starts on it.
    pub fn started(&self, key: Number) {
        for c in &self.0 {
            c.started(key);
        }
    }
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
    /// on: one without a lane is sent nothing and holds nobody until it has one.
    pub fn look(&self) -> Option<Seen> {
        let v = self.viewing.lock();
        let watches = v.viewer != Viewer::NotWatching && v.sender.attached();
        watches.then(|| Seen {
            viewer: v.viewer,
            room: v.sender.room(),
        })
    }

    /// The rate its lane drains frames at, if it watches.
    pub fn rate(&self) -> Option<Rate> {
        let v = self.viewing.lock();
        (v.viewer != Viewer::NotWatching).then(|| v.sender.rate())
    }

    /// What `voxl stats` says of it, as one read of it sees it.
    pub fn stats(&self) -> serde_json::Value {
        let v = self.viewing.lock();
        let viewer = match v.viewer {
            Viewer::NotWatching => "not_watching",
            Viewer::Waiting => "waiting",
            Viewer::Promised => "promised",
            Viewer::Started { .. } => "started",
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

    /// The capture loop planned a keyframe: one waiting to start, with a lane to be sent it
    /// on, is promised it; so is one started on a datagram lane, which could lose it, and
    /// which is told `hello` first, so its page waits for that keyframe and decodes nothing
    /// that refers to it before it; one started on a lane that delivers everything takes it
    /// as any frame. Whether it was promised it.
    pub fn promise(&self, hello: &ServerMessage) -> bool {
        let mut v = self.viewing.lock();
        match (v.viewer, v.sender.delivery()) {
            (Viewer::Waiting, Some(_)) => {
                v.viewer = Viewer::Promised;
                true
            }
            (Viewer::Started { .. }, Some(Delivery::Reported)) => {
                self.send(Out::Text(hello.clone()));
                v.viewer = Viewer::Promised;
                true
            }
            (Viewer::Started { .. }, Some(Delivery::Whole))
            | (Viewer::Waiting | Viewer::Started { .. }, None)
            | (Viewer::NotWatching | Viewer::Promised, _) => false,
        }
    }

    /// The keyframe it was promised, update `key`, is coded: it starts on it. A client that
    /// has since stopped watching or started over is what that made it.
    fn started(&self, key: Number) {
        let mut v = self.viewing.lock();
        if v.viewer == Viewer::Promised {
            v.viewer = Viewer::Started { key };
        }
    }

    /// The encode loop has update `number` to send at `now`: a client that has started takes
    /// it onto its lane. How its lane delivers it, if it took it.
    pub fn takes(&self, number: Number, bytes: &Bytes, now: Instant) -> Option<Delivery> {
        let mut v = self.viewing.lock();
        let Viewing { viewer, sender } = &mut *v;
        match viewer {
            Viewer::Started { .. } => {
                sender.video(number, bytes, now);
                sender.delivery()
            }
            Viewer::NotWatching | Viewer::Waiting | Viewer::Promised => None,
        }
    }

    /// Its relay decided update `number`: a keyframe it started on that missed leaves it with
    /// nothing to decode from, and it waits to start again.
    pub fn decided(&self, number: Number, outcome: Outcome) {
        let mut v = self.viewing.lock();
        if let (Viewer::Started { key }, Outcome::Missed) = (v.viewer, outcome) {
            if key == number {
                v.viewer = Viewer::Waiting;
            }
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
    /// Still pictures coded again at the target quality.
    pub passes: AtomicU64,
    pub keyframes: AtomicU64,
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
    /// Wakes the capture thread (reports, joins, lanes attached, lanes drained).
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
    /// The updates' numbers, and what the sessions' relays decided of them, for the encoder.
    pub updates: Arc<crate::video::Updates>,
    /// How the stream is coded, once the first pipeline is built: its hello's codec and its
    /// recordings' `vpcC`.
    pub stream: Mutex<Option<crate::vp9::Stream>>,
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
            updates: Arc::default(),
            stream: Mutex::new(None),
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
        let generation = attach.generation;
        let taken = c.lanes(|s| s.attach(generation, queue, attach.sent, attach.round_trip, now));
        if taken {
            c.send(Out::Attached { generation });
            self.wake();
        }
        taken
    }

    /// Each watching client with a lane and what one look at it saw, into `into` in place
    /// of what it held, in the hub's order: one look a capture keeps one vector, and
    /// allocates nothing once it is big enough.
    pub fn look_into(&self, into: &mut Vec<(Arc<Client>, Seen)>) {
        into.clear();
        for c in self.clients.lock().iter() {
            into.extend(c.look().map(|seen| (c.clone(), seen)));
        }
    }

    /// A keyframe is planned: the viewers of `seen` it starts (`Client::promise`), if any.
    pub fn promise(&self, seen: &[(Arc<Client>, Seen)]) -> Option<Starts> {
        let hello = self.hello();
        let starts: Vec<Arc<Client>> = seen
            .iter()
            .filter(|(c, _)| c.promise(&hello))
            .map(|(c, _)| c.clone())
            .collect();
        (!starts.is_empty()).then_some(Starts(starts))
    }

    /// The rate the stream is sent at: the slowest drain of the watching clients' lanes;
    /// unlimited while none has a queue backed up.
    pub fn rate(&self) -> Rate {
        let clients = self.clients.lock();
        clients
            .iter()
            .filter_map(|c| c.rate())
            .fold(Rate::Unlimited, |least, r| match (least, r) {
                (Rate::Unlimited, r) | (r, Rate::Unlimited) => r,
                (Rate::Drain(a), Rate::Drain(b)) => Rate::Drain(a.min(b)),
            })
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
            "passes": n(&st.passes),
            "keyframes": n(&st.keyframes),
            "damage_wait_us_total": n(&st.damage_wait_us),
            "send_lat_us_total": n(&st.send_lat_us),
            "clients": clients,
        })
    }

    /// The `hello` message: current geometry and codec plus the server's settings.
    pub fn hello(&self) -> ServerMessage {
        let settings = self.settings.lock().clone();
        ServerMessage::Hello(Hello {
            w: self.width.load(Ordering::Relaxed),
            h: self.height.load(Ordering::Relaxed),
            codec: self.stream.lock().map(|s| s.codec()),
            settings,
        })
    }

    /// After the stream restarts (new size): every client needs the new geometry and codec,
    /// and to start again.
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
    use crate::lanes::wire::Capability;
    use crate::session::sender::Kind;
    use std::time::Duration;

    /// A step of the hub's, for one of two clients or for all.
    #[derive(Clone, Copy, Debug)]
    enum Step {
        /// `v,1`.
        Watch(usize),
        /// `v,0`.
        StopWatching(usize),
        /// The hello after a failed display switch: the stream starts over for this client
        /// alone, and what the encoder holds is still sent.
        StartOver(usize),
        /// A lane of this kind attaches, replacing any before.
        Attach(usize, Kind),
        /// `build` starts the stream over for every client (`restart_clients`), once the
        /// stream before has sent what it held.
        Restart,
        /// The capture loop reads the viewers and plans a keyframe if one waits to start, or
        /// a frame that is not one; the encoder holds at most two.
        Capture,
        /// The encode loop codes and sends the oldest frame it holds.
        Send,
        /// The client's relay reports the keyframe it started on missed.
        KeyMissed(usize),
    }

    const STEPS: [Step; 15] = [
        Step::Watch(0),
        Step::Watch(1),
        Step::StopWatching(0),
        Step::StopWatching(1),
        Step::StartOver(0),
        Step::StartOver(1),
        Step::Attach(0, Kind::Datagram),
        Step::Attach(1, Kind::Datagram),
        Step::Attach(0, Kind::Stream),
        Step::Attach(1, Kind::Stream),
        Step::Restart,
        Step::Capture,
        Step::Send,
        Step::KeyMissed(0),
        Step::KeyMissed(1),
    ];

    /// A frame the encoder holds: a keyframe, with the clients its plan promised it to, or a
    /// frame that is not one.
    type Held = Option<[bool; 2]>;

    /// What a client is to the stream, its keyframe kept as whether it has one: the numbers
    /// are the encode loop's.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    enum Is {
        NotWatching,
        Waiting,
        Promised,
        Started,
    }

    fn is(v: Viewer) -> Is {
        match v {
            Viewer::NotWatching => Is::NotWatching,
            Viewer::Waiting => Is::Waiting,
            Viewer::Promised => Is::Promised,
            Viewer::Started { .. } => Is::Started,
        }
    }

    /// A lane's kind, as the model keeps it.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    enum Lane {
        Datagram,
        Stream,
    }

    impl From<Kind> for Lane {
        fn from(k: Kind) -> Lane {
            match k {
                Kind::Datagram => Lane::Datagram,
                Kind::Stream => Lane::Stream,
            }
        }
    }

    impl From<Lane> for Kind {
        fn from(l: Lane) -> Kind {
            match l {
                Lane::Datagram => Kind::Datagram,
                Lane::Stream => Kind::Stream,
            }
        }
    }

    /// Two clients (what each is to the stream, and its lane's kind), and the frames the
    /// encoder holds, oldest first.
    #[derive(Clone, Debug, PartialEq, Eq, Hash)]
    struct Model {
        c: [(Is, Option<Lane>); 2],
        held: Vec<Held>,
    }

    /// The rule, step by step, as each value and method says it: what each step makes of
    /// the model, whether it planned a keyframe, whom a planned keyframe told `hello`, and
    /// who was sent the frame.
    fn rule(before: &Model, step: Step) -> (bool, Vec<usize>, Vec<usize>, Model) {
        use Is::{NotWatching, Promised, Started, Waiting};
        let mut m = before.clone();
        let c = &mut m.c;
        let (mut planned, mut helloed, mut sent) = (false, vec![], vec![]);
        match step {
            Step::Watch(i) => c[i].0 = Waiting,
            Step::StopWatching(i) => c[i].0 = NotWatching,
            Step::StartOver(i) => {
                if c[i].0 != NotWatching {
                    c[i].0 = Waiting;
                }
            }
            Step::Attach(i, kind) => c[i].1 = Some(kind.into()),
            Step::Restart => {
                for c in c.iter_mut() {
                    if c.0 != NotWatching {
                        c.0 = Waiting;
                    }
                }
                m.held.clear();
            }
            // A keyframe is planned when one waiting to start has a lane: it is promised to
            // those, and to those started on a datagram lane, who are told hello.
            Step::Capture => {
                planned = c.iter().any(|c| c.0 == Waiting && c.1.is_some());
                let mut starts = [false; 2];
                if planned {
                    for (i, c) in c.iter_mut().enumerate() {
                        match *c {
                            (Waiting, Some(_)) => starts[i] = true,
                            (Started, Some(Lane::Datagram)) => {
                                starts[i] = true;
                                helloed.push(i);
                            }
                            _ => {}
                        }
                        if starts[i] {
                            c.0 = Promised;
                        }
                    }
                }
                m.held.push(planned.then_some(starts));
            }
            Step::Send => {
                let starts = m.held.remove(0).unwrap_or([false; 2]);
                for (i, c) in c.iter_mut().enumerate() {
                    if starts[i] && c.0 == Promised {
                        c.0 = Started;
                    }
                    if c.0 == Started {
                        sent.push(i);
                    }
                }
            }
            Step::KeyMissed(i) => {
                if c[i].0 == Started {
                    c[i].0 = Waiting;
                }
            }
        }
        (planned, helloed, sent, m)
    }

    /// Whether the hub takes `step` in this state: the encoder holds two frames at most and
    /// sends only what it holds; only a datagram lane's relay reports; a lane is replaced
    /// only by one of its kind.
    fn applies(m: &Model, step: Step) -> bool {
        match step {
            Step::Capture => m.held.len() < 2,
            Step::Send => !m.held.is_empty(),
            Step::KeyMissed(i) => m.c[i].0 == Is::Started && m.c[i].1 == Some(Lane::Datagram),
            // A session's lane is replaced only by a newer one of its kind.
            Step::Attach(i, kind) => m.c[i].1.is_none_or(|l| l == Lane::from(kind)),
            Step::Watch(_) | Step::StopWatching(_) | Step::StartOver(_) | Step::Restart => true,
        }
    }

    /// A client of `hub`, with its control stream's receiver.
    fn client(hub: &Hub) -> (Arc<Client>, mpsc::UnboundedReceiver<Out>) {
        let now = Instant::now();
        let (tx, rx) = mpsc::unbounded_channel();
        let sender = Sender::new(Capability::mint().unwrap(), tx.clone(), now).unwrap();
        (hub.add_client(tx, sender), rx)
    }

    /// A lane of `kind` attaches to `c`, replacing any before.
    fn attach(c: &Client, kind: Kind, generation: Generation) {
        let now = Instant::now();
        let sent = crate::lanes::wire::Stamp::since(now, now);
        let queue = Queue::new(kind, now, Arc::new(|| {}));
        let rtt = Duration::from_millis(100);
        assert!(c.lanes(|s| s.attach(generation, queue, sent, rtt, now)));
    }

    /// Two real clients of `hub` set to `m`, each with its control stream's receiver.
    fn clients(hub: &Hub, m: &Model) -> Vec<(Arc<Client>, mpsc::UnboundedReceiver<Out>)> {
        hub.clients.lock().clear();
        m.c.iter()
            .map(|&(state, lane)| {
                let (c, rx) = client(hub);
                if let Some(lane) = lane {
                    attach(&c, lane.into(), Generation::first());
                }
                c.viewing.lock().viewer = match state {
                    Is::NotWatching => Viewer::NotWatching,
                    Is::Waiting => Viewer::Waiting,
                    Is::Promised => Viewer::Promised,
                    Is::Started => Viewer::Started {
                        key: Number::first(),
                    },
                };
                (c, rx)
            })
            .collect()
    }

    /// The same step, by the methods, on two real clients of `hub` set to `m`: what it
    /// planned, whom it told hello, whom it sent the frame, and the model after it as the
    /// clients are.
    fn new_step(hub: &Hub, m: &Model, step: Step) -> (bool, Vec<usize>, Vec<usize>, Model) {
        let mut c = clients(hub, m);
        let mut after = m.clone();
        let (mut planned, mut sent) = (false, vec![]);
        match step {
            Step::Watch(i) => c[i].0.watch(),
            Step::StopWatching(i) => c[i].0.stop_watching(),
            Step::StartOver(i) => c[i].0.start_over(hub.hello()),
            Step::Attach(i, kind) => attach(&c[i].0, kind, Generation::first().next()),
            Step::Restart => {
                hub.restart_clients();
                after.held.clear();
            }
            Step::Capture => {
                let mut seen = Vec::new();
                hub.look_into(&mut seen);
                planned = seen.iter().any(|(_, v)| v.viewer == Viewer::Waiting);
                let starts = match planned {
                    true => hub.promise(&seen),
                    false => None,
                };
                let promised = |id| {
                    starts
                        .as_ref()
                        .is_some_and(|s| s.0.iter().any(|c| c.id == id))
                };
                after
                    .held
                    .push(planned.then(|| [0, 1].map(|i| promised(c[i].0.id))));
            }
            Step::Send => {
                let key = Number::first().next();
                if let Some(starts) = after.held.remove(0) {
                    for (i, (c, _)) in c.iter().enumerate() {
                        if starts[i] {
                            c.started(key);
                        }
                    }
                }
                for (i, (c, _)) in c.iter().enumerate() {
                    let update = Bytes::from_static(&[0; 16]);
                    if c.takes(key, &update, Instant::now()).is_some() {
                        sent.push(i);
                    }
                }
            }
            Step::KeyMissed(i) => c[i].0.decided(Number::first(), Outcome::Missed),
        }
        let helloed = (0..2)
            .filter(|&i| {
                std::iter::from_fn(|| c[i].1.try_recv().ok())
                    .any(|o| matches!(o, Out::Text(ServerMessage::Hello(_))))
            })
            .filter(|_| step_hellos(step))
            .collect();
        after.c = [0, 1].map(|i| (is(c[i].0.viewer()), m.c[i].1.or(after.c[i].1)));
        if let Step::Attach(i, kind) = step {
            after.c[i].1 = Some(kind.into());
        }
        (planned, helloed, sent, after)
    }

    /// Whether a step's hellos are a planned keyframe's (start-overs and restarts send
    /// theirs by their own rule).
    fn step_hellos(step: Step) -> bool {
        matches!(step, Step::Capture)
    }

    /// A viewer starts only on a keyframe promised it, a keyframe is promised only to a
    /// viewer with a lane to be sent it on, a viewer is sent frames only once started, a
    /// viewer started on a datagram lane is told hello before any keyframe it could lose,
    /// and one whose keyframe missed waits to start again; every viewer watching with a lane
    /// is started in the end. Over every state two clients and the encoder reach in up to 8
    /// of the hub's steps, breadth first, every step from every state, run on a real hub's
    /// clients through the methods: each step does what the rule says, and from every state,
    /// once the encoder has sent what it holds, one capture and its keyframe start every
    /// viewer watching with a lane.
    #[test]
    fn a_viewer_starts_only_on_a_keyframe_promised_it() {
        let hub = Hub::new().unwrap();
        let start = Model {
            c: [(Is::NotWatching, None); 2],
            held: vec![],
        };
        let mut seen = std::collections::HashSet::from([start.clone()]);
        let mut frontier = vec![start];
        let (mut steps, mut helloed) = (0, 0);
        for _depth in 0..8 {
            let mut next = vec![];
            for before in frontier {
                for step in STEPS {
                    if !applies(&before, step) {
                        continue;
                    }
                    steps += 1;
                    let what = format!("{step:?} from {before:?}");
                    let got = new_step(&hub, &before, step);
                    assert_eq!(got, rule(&before, step), "{what}");
                    helloed += got.1.len();
                    if seen.insert(got.3.clone()) {
                        next.push(got.3);
                    }
                }
            }
            frontier = next;
        }
        for state in &seen {
            let mut m = state.clone();
            while !m.held.is_empty() {
                m = new_step(&hub, &m, Step::Send).3;
            }
            for step in [Step::Capture, Step::Send] {
                m = new_step(&hub, &m, step).3;
            }
            assert!(
                m.c.iter().all(|&(v, lane)| match lane {
                    Some(_) => matches!(v, Is::NotWatching | Is::Started),
                    None => matches!(v, Is::NotWatching | Is::Waiting),
                }),
                "left unstarted from {state:?}: {m:?}"
            );
        }
        let reached: std::collections::HashSet<Is> =
            seen.iter().flat_map(|m| m.c.map(|c| c.0)).collect();
        assert_eq!(reached.len(), 4, "{reached:?}");
        assert!(steps > 0 && helloed > 0);
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
                    codec: (self.next() & 1 == 1).then(|| self.text()),
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
    /// hub's hello, before a stream is built, is its geometry and its settings.
    #[test]
    fn a_message_is_its_fields_after_its_kind() {
        let mut rng = Rng(crate::xorshift::Xorshift::new(0x2545_f491_4f6c_dd1d));
        let hub = Hub::new().unwrap();
        for _ in 0..100_000 {
            let m = rng.message();
            say(&hub, &m);
            // A hub with no stream built has no codec to say.
            if let ServerMessage::Hello(Hello { codec: None, .. }) = m {
                // As JSON: a scale may be NaN, which equals nothing.
                assert_eq!(hub.hello().json(), m.json());
            }
            let (kind, fields) = match &m {
                // Its geometry and its settings, as `Hello` in web/src/protocol.ts names them.
                ServerMessage::Hello(h) => {
                    let mut f = serde_json::Map::new();
                    f.insert("w".into(), h.w.into());
                    f.insert("h".into(), h.h.into());
                    if let Some(codec) = &h.codec {
                        f.insert("codec".into(), codec.clone().into());
                    }
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
