//! Shared state between media producers (video, audio, cursor, clipboard) and the
//! sessions that watch them.

use crate::event::Switch;
use crate::session::control::Audio;
use crate::session::link::Gate;
use crate::session::sender::{Sender, Writer};
use crate::wire::{Capability, Sequence, Version};
use bytes::Bytes;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
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
    /// A lane attached.
    Attached,
    /// An Opus packet of desktop audio, to a recording alone (a session's audio goes on
    /// its lane): the packet, and the frames of silence the encoder left out since the
    /// one before.
    Audio {
        packet: Bytes,
        silent: u32,
    },
}

/// What a viewer is, as starting on a keyframe tells it: a page, which learns the stream
/// from the keyframe itself, or a recording, which is sent each start (`Start`) on a channel
/// of its own before the keyframe is put on its way: so when the keyframe reaches the
/// recording, its start is already there.
pub enum Watcher {
    Page,
    Recording(mpsc::UnboundedSender<Start>),
}

/// A recording starts on the keyframe named `version`, of the stream `configured` so (what
/// its `av1C` and sample entry say).
#[derive(Clone, Debug)]
pub struct Start {
    pub version: Version,
    pub configured: Arc<crate::av1::Configuration>,
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
            ServerMessage::Hello(Hello { codec, startup: s }) => {
                o.text("codec", codec);
                let audio = match s.audio {
                    Audio::Sent => "true",
                    Audio::Off => "false",
                };
                o.raw("audio", audio);
                let mut files = Object::empty();
                files.text("root", &s.files.root);
                files.text("dir", &s.files.dir);
                o.raw("files", &files.end());
                let mut resize = Object::empty();
                resize.raw("scale", &float(s.resize.scale));
                o.raw("resize", &resize.end());
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

/// The `hello` message: the stream's codec and what voxl started with, the fields `Hello` in
/// web/src/protocol.ts names, the same for the daemon's whole life. The desktop's size is
/// the keyframe's: the page lays out from the picture it decodes.
#[derive(Clone, Debug, PartialEq)]
pub struct Hello {
    /// The codec as WebCodecs names it (`av1::Stream::codec`), of the largest picture the
    /// desktop can be resized to: a page configures its decoder once, at the hello, and
    /// keeps it across every stream (a resize starts a new one), each of which declares
    /// its own level, no higher, in its sequence header.
    pub codec: String,
    pub startup: Startup,
}

/// What voxl started with, as a viewer needs it.
#[derive(Clone, Debug, PartialEq)]
pub struct Startup {
    pub audio: Audio,
    pub files: FilesConfig,
    /// Dynamic resolution, the one way a desktop is sized: its scale.
    pub resize: Resize,
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

/// Dynamic resolution: desktop pixels per CSS pixel of the viewer's window, finite and
/// over 0 (`--resize-scale`, parsed once where the daemon's flags are).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Resize {
    pub scale: f64,
}

/// What a viewer that is sent no video holds of the stream: the picture of a frame its lane
/// carried, if every frame of the stream up to it went on that lane, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Holds {
    /// Nothing it can go on from: it starts on a keyframe.
    Nothing,
    /// The stream's frames from a keyframe through the one named here: if no frame was
    /// coded since, it goes on from that one with the next delta.
    Through(Version),
}

/// What a client is to the stream. Each change is a method of `Client`, named for the step
/// that makes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Viewer {
    /// It is sent no video, and holds what it holds.
    NotWatching(Holds),
    /// It waits to start, and no keyframe has been planned for it: the capture loop plans
    /// one once it has a lane to be sent it on.
    Waiting,
    /// A keyframe has been planned for it: it starts on that keyframe. One still `Waiting` is
    /// not started by it, as it may have joined after the frame was planned, even after the
    /// stream that coded it ended.
    Promised,
    /// It is sent every frame from the keyframe it started on, through the one named here:
    /// its lane carries each in order, so every frame it is sent refers to one it was sent
    /// before.
    Started { through: Version },
}

/// What starting over makes of a viewer (the stream is rebuilt, or a new lane carries it):
/// one that watches waits to start on a keyframe, and one that does not holds nothing. A
/// keyframe planned for it, and what it held, went with the old stream or lane.
pub(crate) fn restarted(viewer: Viewer) -> Viewer {
    match viewer {
        Viewer::NotWatching(_) => Viewer::NotWatching(Holds::Nothing),
        Viewer::Waiting | Viewer::Promised | Viewer::Started { .. } => Viewer::Waiting,
    }
}

/// What stopping makes of a viewer (`v,0`): one started holds the frames it was sent; one
/// waiting for its keyframe holds nothing.
pub(crate) fn stopped(viewer: Viewer) -> Viewer {
    match viewer {
        Viewer::Started { through } => Viewer::NotWatching(Holds::Through(through)),
        Viewer::NotWatching(holds) => Viewer::NotWatching(holds),
        Viewer::Waiting | Viewer::Promised => Viewer::NotWatching(Holds::Nothing),
    }
}

/// What watching makes of a viewer (`v,1`), the stream's newest frame `head`: one that holds
/// that frame goes on from it with the next delta, and one that holds less, or nothing,
/// waits for a keyframe; one already watching watches as it was.
pub(crate) fn watching(viewer: Viewer, head: Option<Version>) -> Viewer {
    match (viewer, head) {
        (Viewer::NotWatching(Holds::Through(through)), Some(head)) if through == head => {
            Viewer::Started { through }
        }
        (Viewer::NotWatching(Holds::Through(_) | Holds::Nothing), Some(_) | None) => {
            Viewer::Waiting
        }
        (
            viewer @ (Viewer::Waiting | Viewer::Promised | Viewer::Started { .. }),
            Some(_) | None,
        ) => viewer,
    }
}

/// The newest frame of the stream: none before its first, and after a rebuild. A viewer
/// that holds it resumes on the next delta (`watching`).
#[derive(Default)]
pub struct Head(Mutex<Option<Version>>);

/// What a client is to the stream and its lane, under one lock, so that every change to
/// them is one step, and every read of them one state.
struct Viewing {
    viewer: Viewer,
    /// Whether it wants desktop audio: changed under the hub's `clients` too, which the
    /// audio thread waits on.
    audio: Switch,
    sender: Sender,
}

/// A session, as the media producers see it.
pub struct Client {
    pub id: u64,
    /// Its control stream's writer.
    control: mpsc::UnboundedSender<Out>,
    viewing: Mutex<Viewing>,
    /// The stream's newest frame, the hub's: read under `viewing`'s lock when it watches.
    head: Arc<Head>,
    watcher: Watcher,
}

/// What the capture loop sees of a watching client with a lane in one look at it: what it
/// is to the stream, and whether its link lets a capture start.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Seen {
    pub viewer: Viewer,
    pub gate: Gate,
}

/// The viewers a keyframe starts: at least one, each promised it (`Hub::promise`). A
/// keyframe is coded only for them: nothing else makes one.
pub struct Starts(Vec<Arc<Client>>);

impl Starts {
    /// The keyframe named `version`, of the stream `configured`, is coded: each viewer it
    /// was promised to starts on it.
    pub fn started(&self, version: Version, configured: &Arc<crate::av1::Configuration>) {
        for c in &self.0 {
            c.started(version, configured);
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

    /// Its lane, for one step (a desktop audio packet, its close).
    pub fn lane<R>(&self, f: impl FnOnce(&mut Sender) -> R) -> R {
        f(&mut self.viewing.lock().sender)
    }

    /// What the capture loop sees of it now, if it watches and has a lane to be sent
    /// frames on: one without a lane is sent nothing and holds nobody until it has one.
    ///
    /// Here and wherever its link is given a time, the clock is read under the session's
    /// lock, which is what keeps a link's events in time order (`session::link`).
    pub fn look(&self) -> Option<Seen> {
        let v = self.viewing.lock();
        let gate = v.sender.gate(Instant::now())?;
        match v.viewer {
            Viewer::NotWatching(_) => None,
            viewer @ (Viewer::Waiting | Viewer::Promised | Viewer::Started { .. }) => {
                Some(Seen { viewer, gate })
            }
        }
    }

    /// The relay said that this session's page took the lane message named `sequence`.
    pub fn took(&self, sequence: Sequence) {
        let mut v = self.viewing.lock();
        v.sender.took(sequence, Instant::now());
    }

    /// What `voxl stats` says of it now, as one read of it sees it.
    pub fn stats(&self) -> serde_json::Value {
        let v = self.viewing.lock();
        let viewer = match v.viewer {
            Viewer::NotWatching(Holds::Nothing) => "not_watching",
            Viewer::NotWatching(Holds::Through(_)) => "not_watching_holding",
            Viewer::Waiting => "waiting",
            Viewer::Promised => "promised",
            Viewer::Started { .. } => "started",
        };
        serde_json::json!({
            "id": self.id,
            "viewer": viewer,
            "audio": v.audio == Switch::On,
            "way": v.sender.stats(Instant::now()),
        })
    }

    /// Whether it wants desktop audio.
    #[cfg(target_os = "linux")]
    fn wants_audio(&self) -> bool {
        self.viewing.lock().audio == Switch::On
    }

    /// It starts watching (`v,1`): it goes on with the next delta if it holds the stream's
    /// newest frame, else it waits for a keyframe (`watching`). The head is read under its
    /// lock, which the encode loop takes to send it a frame after it moved the head on: so
    /// a frame coded after this read is sent it, and one coded before it is in the head.
    pub fn watch(&self) {
        let mut v = self.viewing.lock();
        let head = *self.head.0.lock();
        v.viewer = watching(v.viewer, head);
    }

    /// It stops watching (`v,0`), holding what it was sent (`stopped`).
    pub fn stop_watching(&self) {
        let mut v = self.viewing.lock();
        v.viewer = stopped(v.viewer);
    }

    /// The stream starts over for it (it is rebuilt, or a display switch failed): if it
    /// watches, it waits for a keyframe of what follows, which carries the new size. Under
    /// the lock a keyframe is planned, sent and taken under, so one planned before is not
    /// sent it.
    pub fn start_over(&self) {
        let mut v = self.viewing.lock();
        v.viewer = restarted(v.viewer);
    }

    /// The capture loop planned a keyframe: one waiting to start, with a way to be sent it
    /// on, is promised it; one started takes it as any frame. Whether it was promised it.
    pub fn promise(&self) -> bool {
        let mut v = self.viewing.lock();
        match (v.viewer, v.sender.sends()) {
            (Viewer::Waiting, true) => {
                v.viewer = Viewer::Promised;
                true
            }
            (Viewer::Waiting, false)
            | (Viewer::NotWatching(_) | Viewer::Promised | Viewer::Started { .. }, true | false) => {
                false
            }
        }
    }

    /// The keyframe it was promised, named `version`, of the stream `configured`, is coded:
    /// it starts on it, and a recording is told the stream first (`Watcher`). A client that
    /// has since stopped watching or started over is what that made it.
    fn started(&self, version: Version, configured: &Arc<crate::av1::Configuration>) {
        let mut v = self.viewing.lock();
        if v.viewer == Viewer::Promised {
            v.viewer = Viewer::Started { through: version };
            match &self.watcher {
                Watcher::Page => {}
                Watcher::Recording(starts) => {
                    let configured = configured.clone();
                    if starts
                        .send(Start {
                            version,
                            configured,
                        })
                        .is_err()
                    {
                        log::debug!("a recording ended before its viewer left");
                    }
                }
            }
        }
    }

    /// The encode loop has an update named `version` to send: a client that has started
    /// takes it onto its lane, and holds it. Whether it took it.
    pub fn takes(&self, bytes: &Bytes, version: Version) -> bool {
        let mut v = self.viewing.lock();
        let Viewing { viewer, sender, .. } = &mut *v;
        match viewer {
            Viewer::Started { through } => {
                *through = version;
                sender.video(bytes, Instant::now());
                true
            }
            Viewer::NotWatching(_) | Viewer::Waiting | Viewer::Promised => false,
        }
    }

    /// A lane attached with its capability, in place of any before: the viewer starts over
    /// on a keyframe sent on it, since bytes a lane before took may never have reached the
    /// page. The lane's writer's hold on it, if the session took it.
    pub fn attach(&self) -> Option<Writer> {
        let mut v = self.viewing.lock();
        let writer = v.sender.attach()?;
        v.viewer = restarted(v.viewer);
        Some(writer)
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
    /// The frames coded since the daemon started: the next one's name.
    versions: AtomicU32,
    /// Last cursor message, replayed to new clients.
    pub cursor: Mutex<Option<Bytes>>,
    /// Wakes the capture thread (joins, lanes attached or ended, updates told taken).
    pub waker: Wake,
    pub input: Mutex<Option<std::sync::mpsc::Sender<crate::event::InputEvent>>>,
    #[cfg(target_os = "linux")]
    pub last_clipboard: Mutex<Option<String>>,
    /// The newest ask for a desktop size, until the capture loop takes it and gives it to
    /// the screen: none while no ask waits.
    pub resize_req: Mutex<Option<(u32, u32)>>,
    /// What every session is told first.
    hello: Hello,
    /// The stream's newest frame, which every client reads when it watches.
    head: Arc<Head>,
    pub stats: Stats,
    /// Screenshots asked for (`voxl screenshot`), answered by the capture thread with the
    /// screen as it is.
    pub snapshots: Mutex<Vec<tokio::sync::oneshot::Sender<Arc<crate::record::Snapshot>>>>,
    /// When the hub was made: `voxl stats` counts its times from it.
    pub start: Instant,
}

impl Hub {
    /// A hub with no session, whose sessions are told `hello`; its waker's pipe is what can
    /// fail.
    pub fn new(hello: Hello) -> std::io::Result<Arc<Self>> {
        Ok(Arc::new(Hub {
            clients: Mutex::new(Vec::new()),
            audio_asked: parking_lot::Condvar::new(),
            next_id: AtomicU64::new(1),
            versions: AtomicU32::new(0),
            cursor: Mutex::new(None),
            waker: Wake::new()?,
            input: Mutex::new(None),
            #[cfg(target_os = "linux")]
            last_clipboard: Mutex::new(None),
            resize_req: Mutex::new(None),
            hello,
            head: Arc::default(),
            stats: Stats::default(),
            snapshots: Mutex::new(Vec::new()),
            start: Instant::now(),
        }))
    }

    pub fn wake(&self) {
        self.waker.signal();
    }

    /// The name of the frame coded next: the capture's number, counted for the daemon, not
    /// for a stream, so that a name is one frame's for as long as a viewer may hold it (a
    /// stream rebuilt for a new size starts no count again). It wraps after 2^32 frames,
    /// 828 days of them at 60 a second.
    pub fn next_version(&self) -> Version {
        Version(self.versions.fetch_add(1, Ordering::Relaxed))
    }

    /// The frame named `version` is coded, the stream's newest: set before any viewer is
    /// sent it, so that one that watches after this is either sent it or waits for a
    /// keyframe.
    pub fn coded(&self, version: Version) {
        *self.head.0.lock() = Some(version);
    }

    pub fn drain_wake(&self) {
        self.waker.drain();
    }

    /// A session joins, its control stream written through `control`, its lane `sender`.
    pub fn add_client(&self, control: mpsc::UnboundedSender<Out>, sender: Sender) -> Arc<Client> {
        self.join(control, sender, Watcher::Page)
    }

    /// A viewer joins, `watcher`, its control messages sent on `control`, its frames through
    /// `sender`.
    fn join(
        &self,
        control: mpsc::UnboundedSender<Out>,
        sender: Sender,
        watcher: Watcher,
    ) -> Arc<Client> {
        let c = Arc::new(Client {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            control,
            viewing: Mutex::new(Viewing {
                viewer: Viewer::NotWatching(Holds::Nothing),
                audio: Switch::Off,
                sender,
            }),
            head: self.head.clone(),
            watcher,
        });
        self.clients.lock().push(c.clone());
        c
    }

    /// A recording joins: it takes every frame, as a page's video messages on `to`, and
    /// starts on a keyframe as any viewer does, told each start on `starts`; it wants the
    /// desktop's audio, which a desktop without audio never sends.
    pub fn add_recorder(
        &self,
        to: mpsc::UnboundedSender<Out>,
        starts: mpsc::UnboundedSender<Start>,
    ) -> Arc<Client> {
        let c = self.join(
            to.clone(),
            Sender::recording(to),
            Watcher::Recording(starts),
        );
        self.set_audio(&c, Switch::On);
        c.watch();
        self.wake();
        c
    }

    /// Client `id` leaves: it is sent nothing more.
    pub fn remove_client(&self, id: u64) {
        self.clients.lock().retain(|c| c.id != id);
        self.wake();
    }

    /// A lane presenting a capability: it joins the session whose capability it is, which
    /// is told so on its control stream. Every open session's capability is compared, each
    /// in constant time. The session that took it and the lane's writer's hold on it, if
    /// one did.
    pub fn attach(&self, presented: &Capability) -> Option<(Arc<Client>, Writer)> {
        let clients = self.clients.lock().clone();
        let whose = clients.into_iter().fold(None, |found, c| {
            let is = c.lane(|s| s.is(presented));
            found.or(is.then_some(c))
        })?;
        let writer = whose.attach()?;
        whose.send(Out::Attached);
        self.wake();
        Some((whose, writer))
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
        let starts: Vec<Arc<Client>> = seen
            .iter()
            .filter(|(c, _)| c.promise())
            .map(|(c, _)| c.clone())
            .collect();
        (!starts.is_empty()).then_some(Starts(starts))
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
            "keyframes": n(&st.keyframes),
            "damage_wait_us_total": n(&st.damage_wait_us),
            "send_lat_us_total": n(&st.send_lat_us),
            "clients": clients,
        })
    }

    /// The `hello` message: the codec and what voxl started with.
    pub fn hello(&self) -> ServerMessage {
        ServerMessage::Hello(self.hello.clone())
    }

    /// After the stream restarts (new size): it has no frame yet, and every client starts
    /// again on a keyframe of the new stream, which carries its size.
    pub fn restart_clients(&self) {
        *self.head.0.lock() = None;
        for c in self.clients.lock().iter() {
            c.start_over();
        }
    }

    /// Session `c` wants desktop audio, or no longer does.
    pub fn set_audio(&self, c: &Client, audio: Switch) {
        let _clients = self.clients.lock();
        c.viewing.lock().audio = audio;
        self.audio_asked.notify_all();
    }

    #[cfg(target_os = "linux")]
    /// Whether any session wants desktop audio.
    pub fn audio_wanted(&self) -> bool {
        let clients = self.clients.lock();
        clients.iter().any(|c| c.wants_audio())
    }

    #[cfg(target_os = "linux")]
    /// Blocks until a session wants desktop audio.
    pub fn wait_for_audio(&self) {
        let mut clients = self.clients.lock();
        while !clients.iter().any(|c| c.wants_audio()) {
            self.audio_asked.wait(&mut clients);
        }
    }

    #[cfg(target_os = "linux")]
    /// An Opus packet of desktop audio, `silent` frames of silence after the one before:
    /// onto the lane of every session that wants it.
    pub fn audio(&self, packet: &[u8], silent: u32) {
        for c in self.clients.lock().iter() {
            let mut v = c.viewing.lock();
            match v.audio {
                Switch::On => v.sender.audio(packet, silent, Instant::now()),
                Switch::Off => {}
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
pub(crate) mod tests {
    use super::*;
    use crate::session::sender::Way;
    use crate::wire::Capability;

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
        /// A lane attaches, replacing any before (the relay reopened it).
        Attach(usize),
        /// The lane's socket breaks.
        Lose(usize),
        /// `build` starts the stream over for every client (`restart_clients`), once the
        /// stream before has sent what it held.
        Restart,
        /// The capture loop reads the viewers and plans a keyframe if one waits to start, or
        /// a frame that is not one; the encoder holds at most two.
        Capture,
        /// The encode loop codes and sends the oldest frame it holds.
        Send,
    }

    const STEPS: [Step; 13] = [
        Step::Watch(0),
        Step::Watch(1),
        Step::StopWatching(0),
        Step::StopWatching(1),
        Step::StartOver(0),
        Step::StartOver(1),
        Step::Attach(0),
        Step::Attach(1),
        Step::Lose(0),
        Step::Lose(1),
        Step::Restart,
        Step::Capture,
        Step::Send,
    ];

    /// A frame the encoder holds: a keyframe, with the clients its plan promised it to, or a
    /// frame that is not one.
    type Held = Option<[bool; 2]>;

    /// What a client's lane is, as the model keeps it.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    enum Lane {
        None,
        Live,
        /// Its socket broke; none has attached since.
        Lost,
    }

    /// What a client's page decodes from: the frame its decoder showed last, of the stream
    /// as it is, if its live lane brought every frame up to it. The oracle the hub is held
    /// to: a page is never sent a delta that does not follow the frame it showed.
    type Page = Option<Version>;

    /// Two clients (what each is to the stream, its lane, its page), the frames the encoder
    /// holds, oldest first, the stream's newest frame, and the next frame's name.
    #[derive(Clone, Debug, PartialEq, Eq, Hash)]
    struct Model {
        c: [(Viewer, Lane, Page); 2],
        held: Vec<Held>,
        head: Option<Version>,
        next: u32,
    }

    /// The rule, step by step, as each value and method says it: what each step makes of
    /// the model, whether it planned a keyframe, and who was sent the frame.
    fn rule(before: &Model, step: Step) -> (bool, Vec<usize>, Model) {
        use Viewer::{Promised, Started, Waiting};
        let mut m = before.clone();
        let head = m.head;
        let c = &mut m.c;
        let (mut planned, mut sent) = (false, vec![]);
        match step {
            Step::Watch(i) => c[i].0 = watching(c[i].0, head),
            Step::StopWatching(i) => c[i].0 = stopped(c[i].0),
            // The page starts over: its decoder too.
            Step::StartOver(i) => c[i] = (restarted(c[i].0), c[i].1, None),
            // A new lane starts its viewer over: what the lane before took may be lost.
            Step::Attach(i) => c[i] = (restarted(c[i].0), Lane::Live, None),
            Step::Lose(i) => c[i] = (c[i].0, Lane::Lost, None),
            Step::Restart => {
                for c in c.iter_mut() {
                    *c = (restarted(c.0), c.1, None);
                }
                m.held.clear();
                m.head = None;
            }
            // A keyframe is planned when one waiting to start has a live lane: it is promised
            // to those.
            Step::Capture => {
                let starts = c.map(|c| (c.0, c.1) == (Waiting, Lane::Live));
                planned = starts.contains(&true);
                for (i, c) in c.iter_mut().enumerate() {
                    if starts[i] {
                        c.0 = Promised;
                    }
                }
                m.held.push(planned.then_some(starts));
            }
            Step::Send => {
                let version = Version(m.next);
                m.next += 1;
                m.head = Some(version);
                let starts = m.held.remove(0);
                for (i, c) in c.iter_mut().enumerate() {
                    if starts.is_some_and(|s| s[i]) && c.0 == Promised {
                        c.0 = Started { through: version };
                    }
                    if let Started { through } = &mut c.0 {
                        *through = version;
                        sent.push(i);
                        if c.1 == Lane::Live {
                            c.2 = Some(version);
                        }
                    }
                }
            }
        }
        (planned, sent, m)
    }

    /// Whether the hub takes `step` in this state: the encoder holds two frames at most and
    /// sends only what it holds; only a live lane breaks.
    fn applies(m: &Model, step: Step) -> bool {
        match step {
            Step::Capture => m.held.len() < 2,
            Step::Send => !m.held.is_empty(),
            Step::Lose(i) => m.c[i].1 == Lane::Live,
            Step::Watch(_)
            | Step::StopWatching(_)
            | Step::StartOver(_)
            | Step::Attach(_)
            | Step::Restart => true,
        }
    }

    /// A lane attaches to `c`: its writer's hold on it.
    fn attach(c: &Client) -> Writer {
        c.attach().expect("an open session takes a lane")
    }

    /// Two real clients of `hub` set to `m`, each with the writer of the lane it was last
    /// given, if any; the hub's head is `m`'s.
    fn clients(hub: &Hub, m: &Model) -> Vec<(Arc<Client>, Option<Writer>)> {
        hub.clients.lock().clear();
        *hub.head.0.lock() = m.head;
        m.c.iter()
            .map(|&(viewer, lane, _)| {
                let (tx, _rx) = mpsc::unbounded_channel();
                let c = hub.add_client(tx, Sender::new(Capability::mint().unwrap()));
                let writer = match lane {
                    Lane::None => None,
                    Lane::Live | Lane::Lost => Some(attach(&c)),
                };
                if let (Lane::Lost, Some(w)) = (lane, &writer) {
                    c.lane(|s| s.lost(w));
                }
                c.viewing.lock().viewer = viewer;
                (c, writer)
            })
            .collect()
    }

    /// A stream's configuration, as a keyframe's viewers are told it.
    fn configured() -> Arc<crate::av1::Configuration> {
        let mut rng = crate::xorshift::Xorshift::new(0x1f83_d9ab_fb41_bd6b);
        Arc::new(crate::av1::tests::configuration(&mut rng))
    }

    /// The same step, by the methods, on two real clients of `hub` set to `m`, the encode
    /// loop's part as it does it (`video::encode_frames`): what it planned, whom it sent the
    /// frame, and the model after it as the clients are. Each page is the model's: what the
    /// clients were sent moves it, and a delta sent a live lane must follow what it showed.
    fn new_step(
        hub: &Hub,
        stream: &Arc<crate::av1::Configuration>,
        m: &Model,
        step: Step,
    ) -> (bool, Vec<usize>, Model) {
        let mut c = clients(hub, m);
        let mut after = m.clone();
        let (mut planned, mut sent) = (false, vec![]);
        match step {
            Step::Watch(i) => c[i].0.watch(),
            Step::StopWatching(i) => c[i].0.stop_watching(),
            Step::StartOver(i) => {
                c[i].0.start_over();
                after.c[i].2 = None;
            }
            Step::Attach(i) => {
                c[i].1 = Some(attach(&c[i].0));
                after.c[i].2 = None;
            }
            Step::Lose(i) => {
                if let Some(w) = &c[i].1 {
                    c[i].0.lane(|s| s.lost(w));
                }
                after.c[i].2 = None;
            }
            Step::Restart => {
                hub.restart_clients();
                after.held.clear();
                after.c.iter_mut().for_each(|c| c.2 = None);
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
                let promised = [0, 1].map(|i| promised(c[i].0.id));
                // A keyframe goes only to a viewer that missed frames or has none.
                for i in 0..2 {
                    assert!(
                        !promised[i] || m.head.is_none() || m.c[i].2 != m.head,
                        "{step:?} from {m:?}: a keyframe for a page that shows the head"
                    );
                }
                after.held.push(planned.then_some(promised));
            }
            Step::Send => {
                let version = Version(after.next);
                after.next += 1;
                let key = after.held.remove(0);
                hub.coded(version);
                if let Some(starts) = key {
                    for (i, (c, _)) in c.iter().enumerate() {
                        if starts[i] {
                            c.started(version, stream);
                        }
                    }
                }
                for (i, (c, _)) in c.iter().enumerate() {
                    if c.takes(&Bytes::from_static(&[0; 16]), version) {
                        sent.push(i);
                        if m.c[i].1 == Lane::Live {
                            let page = &mut after.c[i].2;
                            let follows = key.is_some() || *page == m.head;
                            assert!(follows, "{step:?} from {m:?}: a delta {i} cannot decode");
                            *page = Some(version);
                        }
                    }
                }
                after.head = Some(version);
            }
        }
        // What `voxl stats` says of each lane is what the capture loop was shown of it.
        for i in 0..2 {
            let way = c[i].0.lane(|s| s.stats(Instant::now()));
            let lane = match (&c[i].1, way) {
                (None, Way::Unattached) => Lane::None,
                (Some(_), Way::Lane(_)) => Lane::Live,
                (Some(_), Way::Unattached) => Lane::Lost,
                (None, way @ Way::Lane(_))
                | (None | Some(_), way @ (Way::Recording | Way::Closed)) => {
                    panic!("{step:?}: a session's way is {way:?}")
                }
            };
            after.c[i].0 = c[i].0.viewer();
            after.c[i].1 = lane;
        }
        after.head = *hub.head.0.lock();
        (planned, sent, after)
    }

    /// A viewer starts only on a keyframe promised it, a keyframe is promised only to a
    /// viewer with a live lane to be sent it on whose page does not show the stream's newest
    /// frame, a viewer is sent frames only once started, a page is never sent a delta that
    /// does not follow the frame it showed, and a viewer that stopped watching and comes
    /// back showing the newest frame goes on with the next delta, no keyframe. Over every
    /// state two clients and the encoder reach in up to 9 of the hub's steps, breadth first,
    /// every step from every state, run on a real hub's clients through the methods: each
    /// step does what the rule says, and from every state, once the encoder has sent what it
    /// holds, one capture and its frame leave every viewer watching with a live lane started.
    #[test]
    fn a_viewer_starts_only_on_a_keyframe_promised_it() {
        let hub = Hub::new(hello()).unwrap();
        let stream = configured();
        let start = Model {
            c: [(Viewer::NotWatching(Holds::Nothing), Lane::None, None); 2],
            held: vec![],
            head: None,
            next: 0,
        };
        let mut seen = std::collections::HashSet::from([start.clone()]);
        let mut frontier = vec![start];
        let (mut steps, mut resumed) = (0, 0);
        for _depth in 0..9 {
            let mut next = vec![];
            for before in frontier {
                for step in STEPS {
                    if !applies(&before, step) {
                        continue;
                    }
                    steps += 1;
                    let what = format!("{step:?} from {before:?}");
                    let got = new_step(&hub, &stream, &before, step);
                    assert_eq!(got, rule(&before, step), "{what}");
                    // One that comes back showing the newest frame goes on from it.
                    if let Step::Watch(i) = step {
                        let (v, lane, page) = before.c[i];
                        if let (Viewer::NotWatching(_), Lane::Live, Some(_)) = (v, lane, page) {
                            let shows = page == before.head;
                            let goes_on = matches!(got.2.c[i].0, Viewer::Started { .. });
                            assert_eq!(goes_on, shows, "{what}");
                            resumed += usize::from(goes_on);
                        }
                    }
                    if seen.insert(got.2.clone()) {
                        next.push(got.2);
                    }
                }
            }
            frontier = next;
        }
        for state in &seen {
            let mut m = state.clone();
            while !m.held.is_empty() {
                m = new_step(&hub, &stream, &m, Step::Send).2;
            }
            for step in [Step::Capture, Step::Send] {
                m = new_step(&hub, &stream, &m, step).2;
            }
            assert!(
                m.c.iter().all(|&(v, lane, _)| match lane {
                    Lane::Live => matches!(v, Viewer::NotWatching(_) | Viewer::Started { .. }),
                    Lane::None | Lane::Lost => v != Viewer::Promised,
                }),
                "left unstarted from {state:?}: {m:?}"
            );
        }
        let reached: std::collections::HashSet<std::mem::Discriminant<Viewer>> = seen
            .iter()
            .flat_map(|m| m.c.map(|c| std::mem::discriminant(&c.0)))
            .collect();
        assert_eq!(reached.len(), 4, "{reached:?}");
        assert!(steps > 0 && resumed > 0, "{resumed} resumed on a delta");
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
                    1.. => PIECES[self.below(PIECES.len() as u64) as usize].to_string(),
                })
                .collect()
        }
        /// Any f64, NaN and the infinities among them.
        fn float(&mut self) -> f64 {
            f64::from_bits(self.next())
        }
        fn startup(&mut self) -> Startup {
            Startup {
                audio: [Audio::Sent, Audio::Off][self.below(2) as usize],
                files: FilesConfig {
                    root: self.text(),
                    dir: self.text(),
                },
                resize: Resize {
                    scale: self.float(),
                },
            }
        }
        fn message(&mut self) -> ServerMessage {
            match self.below(3) {
                0 => ServerMessage::Hello(Hello {
                    codec: self.text(),
                    startup: self.startup(),
                }),
                1 => ServerMessage::Clip { d: self.text() },
                2.. => ServerMessage::Mic {
                    on: self.next() & 1 == 1,
                },
            }
        }
    }

    /// A hello as the daemon's: the codec of a stream as `av1::Stream` names it, and what
    /// voxl started with.
    pub(crate) fn hello() -> Hello {
        let shape = crate::av1::Shape {
            width: 3024,
            height: 1898,
        };
        let stream = crate::av1::Stream::new(shape, std::num::NonZeroUsize::MIN).unwrap();
        Hello {
            codec: stream.codec(),
            startup: Startup {
                audio: Audio::Sent,
                files: FilesConfig {
                    root: "/home/v".into(),
                    dir: "Downloads".into(),
                },
                resize: Resize { scale: 1.5 },
            },
        }
    }

    /// The model: a message is a JSON object of its fields, and its kind as `t`, first; a
    /// hub's hello is the hello it was made with.
    #[test]
    fn a_message_is_its_fields_after_its_kind() {
        let mut rng = Rng(crate::xorshift::Xorshift::new(0x2545_f491_4f6c_dd1d));
        for _ in 0..100_000 {
            let m = rng.message();
            if let ServerMessage::Hello(h) = &m {
                // As JSON: a scale may be NaN, which equals nothing.
                assert_eq!(Hub::new(h.clone()).unwrap().hello().json(), m.json());
            }
            let (kind, fields) = match &m {
                // Its codec and what voxl started with, as `Hello` in web/src/protocol.ts
                // names them.
                ServerMessage::Hello(Hello { codec, startup: s }) => {
                    let mut f = serde_json::Map::new();
                    f.insert("codec".into(), codec.clone().into());
                    f.insert("audio".into(), (s.audio == Audio::Sent).into());
                    let files = &s.files;
                    f.insert(
                        "files".into(),
                        serde_json::json!({ "root": files.root, "dir": files.dir }),
                    );
                    let scale = s.resize.scale;
                    f.insert("resize".into(), serde_json::json!({ "scale": scale }));
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
