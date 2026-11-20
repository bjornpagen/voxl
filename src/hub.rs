//! Shared state between media producers (video, audio, cursor, clipboard) and the
//! sessions that watch them.

use crate::event::Switch;
use crate::histogram::Histogram;
use crate::session::control::Audio;
use crate::session::link::{Gate, Path};
use crate::session::sender::{Sender, Writer};
pub use crate::wire::Holds;
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

/// What a client is to the stream. Each change is a method of `Client`, named for the step
/// that makes it, and each method's rule is one of the pure functions below.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Viewer {
    /// It is sent no video, and holds what it holds.
    NotWatching(Holds),
    /// It watches, and is not in the chain: it has just joined or come back, it starts over,
    /// or its gate was shut when a frame went on without it. What it holds says, at the
    /// first capture its gate is open for, whether it goes on with that frame or starts on a
    /// keyframe (`planned`).
    Watching(Holds),
    /// A keyframe has been planned for it: it starts on that keyframe. One `Watching` is not
    /// started by it, as it may have joined after the frame was planned, even after the
    /// stream that coded it ended.
    Promised,
    /// It is in the chain: it is sent every frame from the one it went on from, through the
    /// one named here, and its lane carries each in order, so every frame it is sent refers
    /// to one it was sent before.
    Started { through: Version },
}

/// What starting over makes of a viewer (the stream is rebuilt):
/// one that watches starts on a keyframe, and one that does not holds nothing. A keyframe
/// planned for it, and what it held, went with the old stream.
pub(crate) fn restarted(viewer: Viewer) -> Viewer {
    match viewer {
        Viewer::NotWatching(_) => Viewer::NotWatching(Holds::Nothing),
        Viewer::Watching(_) | Viewer::Promised | Viewer::Started { .. } => {
            Viewer::Watching(Holds::Nothing)
        }
    }
}

/// What a new lane makes of a viewer whose page holds `holds`, as the lane said when it
/// attached: it holds that, and nothing else it held or was promised, since only what the
/// page decoded counts, and bytes the lane before took may never have reached it. One that
/// watches is not in the chain until the first capture its gate is open for, which says
/// whether it goes on from what its page holds or starts on a keyframe (`planned`).
pub(crate) fn reattached(viewer: Viewer, holds: Holds) -> Viewer {
    match viewer {
        Viewer::NotWatching(_) => Viewer::NotWatching(holds),
        Viewer::Watching(_) | Viewer::Promised | Viewer::Started { .. } => Viewer::Watching(holds),
    }
}

/// What stopping makes of a viewer (`v,0`): one in the chain holds the frames it was sent;
/// one promised a keyframe holds nothing; one watching holds what it held.
pub(crate) fn stopped(viewer: Viewer) -> Viewer {
    match viewer {
        Viewer::Started { through } => Viewer::NotWatching(Holds::Through(through)),
        Viewer::NotWatching(holds) | Viewer::Watching(holds) => Viewer::NotWatching(holds),
        Viewer::Promised => Viewer::NotWatching(Holds::Nothing),
    }
}

/// What watching makes of a viewer (`v,1`): it watches, holding what it held, and the first
/// capture its gate is open for says whether it goes on from that (`planned`); one already
/// watching watches as it was.
pub(crate) fn watching(viewer: Viewer) -> Viewer {
    match viewer {
        Viewer::NotWatching(holds) => Viewer::Watching(holds),
        viewer @ (Viewer::Watching(_) | Viewer::Promised | Viewer::Started { .. }) => viewer,
    }
}

/// What a capture let go is to one viewer of the look.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    /// Its gate is open, and it holds the chain: it is sent the frame, a delta or a keyframe
    /// alike.
    Sent,
    /// Its gate is open, and it holds less than the stream's newest frame, or nothing: it is
    /// promised the frame, which is then a keyframe.
    Starts,
    /// Its gate is shut, or it no longer watches: the frame goes on without it. One that was
    /// in the chain is in it no longer, and holds what it held: once the frame is named, the
    /// stream's newest is one it was not sent, so it starts on a keyframe when its gate opens.
    Skipped,
}

/// What a capture let go makes of a viewer, its gate `gate` as one look saw it, on the chain
/// whose newest frame is `head`: its part in the frame, and what it is after. The one rule of
/// who is sent a frame, and it reads this viewer alone: no viewer's gate decides another's
/// part, and none that is shut is ever sent a frame.
///
/// A keyframe is a frame of the chain, which one encoder codes, so a viewer that holds the
/// chain is sent it as any other: passed by, it would leave the chain, and need a keyframe of
/// its own, which is the frame it was not sent.
pub(crate) fn planned(viewer: Viewer, gate: Gate, head: Option<Version>) -> (Part, Viewer) {
    match (viewer, gate) {
        (viewer @ (Viewer::Started { .. } | Viewer::Promised), Gate::Open) => (Part::Sent, viewer),
        (Viewer::Watching(Holds::Through(through)), Gate::Open) if Some(through) == head => {
            (Part::Sent, Viewer::Started { through })
        }
        (Viewer::Watching(Holds::Through(_) | Holds::Nothing), Gate::Open) => {
            (Part::Starts, Viewer::Promised)
        }
        (Viewer::Started { through }, Gate::Full | Gate::Late) => {
            (Part::Skipped, Viewer::Watching(Holds::Through(through)))
        }
        (Viewer::Promised, Gate::Full | Gate::Late) => {
            (Part::Skipped, Viewer::Watching(Holds::Nothing))
        }
        (viewer @ Viewer::Watching(_), Gate::Full | Gate::Late)
        | (viewer @ Viewer::NotWatching(_), Gate::Open | Gate::Full | Gate::Late) => {
            (Part::Skipped, viewer)
        }
    }
}

/// Whom a capture now would be for, as one look at the watching viewers saw them: the most
/// any one of them asks for (`planned`). One whose gate is shut asks for nothing, and so
/// holds nobody back.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Audience {
    /// No watching viewer's gate is open: a capture now would go to nobody.
    Nobody,
    /// A viewer whose gate is open holds the chain, and none starts: a capture is a delta,
    /// if anything moved.
    Chain,
    /// A viewer whose gate is open starts: a capture is a keyframe, whether anything moved
    /// or not.
    Starting,
}

impl Audience {
    /// What the look `seen` asks of a capture, on the chain whose newest frame is `head`.
    pub fn of(seen: impl IntoIterator<Item = Seen>, head: Option<Version>) -> Audience {
        seen.into_iter()
            .map(|s| match planned(s.viewer, s.gate, head).0 {
                Part::Skipped => Audience::Nobody,
                Part::Sent => Audience::Chain,
                Part::Starts => Audience::Starting,
            })
            .fold(Audience::Nobody, Audience::max)
    }
}

/// The stream's frames as the capture loop plans them: the newest named, none before the
/// first. One a stream: a rebuilt stream starts a chain of its own, as its viewers start
/// over (`Hub::restart_clients`).
#[derive(Default)]
pub struct Chain {
    head: Option<Version>,
}

impl Chain {
    /// Whom a capture now would be for, from one look at the viewers (`Hub::look_into`).
    pub fn audience(&self, seen: &[(Arc<Client>, Seen)]) -> Audience {
        Audience::of(seen.iter().map(|(_, s)| *s), self.head)
    }

    /// A capture is let go: each viewer of the look takes its part in it (`Client::plan`),
    /// its gate as the look saw it. The viewers it promised the frame, if any: it is then a
    /// keyframe.
    pub fn parts(&self, seen: &[(Arc<Client>, Seen)]) -> Option<Starts> {
        let starts: Vec<Arc<Client>> = seen
            .iter()
            .filter(|(c, s)| c.plan(s.gate, self.head))
            .map(|(c, _)| c.clone())
            .collect();
        (!starts.is_empty()).then_some(Starts(starts))
    }

    /// The frame planned is named, the next name (`Hub::next_version`), and is the chain's
    /// newest: a viewer it went on without holds less from now on.
    pub fn name(&mut self, hub: &Hub) -> Version {
        let version = hub.next_version();
        self.head = Some(version);
        version
    }
}

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
    watcher: Watcher,
}

/// What the capture loop sees of a watching client with a lane in one look at it: what it
/// is to the stream, and whether its link takes a capture now. The gate is this viewer's
/// alone: it decides this viewer's part in a capture and no other's (`planned`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Seen {
    pub viewer: Viewer,
    pub gate: Gate,
}

/// The viewers a keyframe starts: at least one, each promised it (`Chain::parts`). A
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
    /// frames on: one without a lane is not looked at, and takes no part in a capture,
    /// until it has one.
    ///
    /// Here and wherever its link is given a time, the clock is read under the session's
    /// lock, which is what keeps a link's events in time order (`session::link`).
    pub fn look(&self) -> Option<Seen> {
        let v = self.viewing.lock();
        let gate = v.sender.gate(Instant::now())?;
        match v.viewer {
            Viewer::NotWatching(_) => None,
            viewer @ (Viewer::Watching(_) | Viewer::Promised | Viewer::Started { .. }) => {
                Some(Seen { viewer, gate })
            }
        }
    }

    /// The relay said that this session's page took the lane message named `sequence`.
    pub fn took(&self, sequence: Sequence) {
        let mut v = self.viewing.lock();
        v.sender.took(sequence, Instant::now());
    }

    /// The statement of the lane `writer` writes said its path is `path`.
    pub fn stated(&self, writer: &Writer, path: Path) {
        let mut v = self.viewing.lock();
        v.sender.stated(writer, path, Instant::now());
    }

    /// What `voxl stats` says of it now, as one read of it sees it.
    pub fn stats(&self) -> serde_json::Value {
        let v = self.viewing.lock();
        let viewer = match v.viewer {
            Viewer::NotWatching(Holds::Nothing) => "not_watching",
            Viewer::NotWatching(Holds::Through(_)) => "not_watching_holding",
            Viewer::Watching(Holds::Nothing) => "watching",
            Viewer::Watching(Holds::Through(_)) => "watching_holding",
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

    /// It starts watching (`v,1`), holding what it held (`watching`): the first capture its
    /// gate is open for says whether it goes on from that or starts on a keyframe.
    pub fn watch(&self) {
        let mut v = self.viewing.lock();
        v.viewer = watching(v.viewer);
    }

    /// It stops watching (`v,0`), holding what it was sent (`stopped`).
    pub fn stop_watching(&self) {
        let mut v = self.viewing.lock();
        v.viewer = stopped(v.viewer);
    }

    /// The stream starts over for it, as for every client (it is rebuilt:
    /// `Hub::restart_clients`): if it watches, it starts on a keyframe of what follows, which
    /// carries the new size. Under the lock a keyframe is planned, sent and taken under, so
    /// one planned before is not sent it.
    fn start_over(&self) {
        let mut v = self.viewing.lock();
        v.viewer = restarted(v.viewer);
    }

    /// A capture is let go on the chain whose newest frame is `head`, its gate `gate` as the
    /// look saw it: it takes its part in the frame (`planned`), from what it is now, which
    /// may have changed since the look. Whether it was promised the frame, a keyframe.
    fn plan(&self, gate: Gate, head: Option<Version>) -> bool {
        let mut v = self.viewing.lock();
        let (part, after) = planned(v.viewer, gate, head);
        v.viewer = after;
        part == Part::Starts
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

    /// The encode loop has an update named `version` to send: a client in the chain takes it
    /// onto its lane, and holds it. Whether it took it.
    pub fn takes(&self, bytes: &Bytes, version: Version) -> bool {
        let mut v = self.viewing.lock();
        let Viewing { viewer, sender, .. } = &mut *v;
        match viewer {
            Viewer::Started { through } => {
                *through = version;
                sender.video(bytes, Instant::now());
                true
            }
            Viewer::NotWatching(_) | Viewer::Watching(_) | Viewer::Promised => false,
        }
    }

    /// A lane attached with its capability, in place of any before, its path `path` (not
    /// stated, or not yet), its page holding `holds`: the viewer holds that (`reattached`),
    /// so a page that decoded the stream's newest frame goes on with the next on this lane,
    /// and any other starts on a keyframe sent on it. The lane's writer's hold on it, if the
    /// session took it.
    pub fn attach(&self, path: Path, holds: Holds) -> Option<Writer> {
        let mut v = self.viewing.lock();
        let writer = v.sender.attach(path)?;
        v.viewer = reattached(v.viewer, holds);
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

/// What the pipeline has done since voxl started, for `voxl stats`: counts, and how long
/// each step took, as a distribution of its times (`Histogram`).
#[derive(Default)]
pub struct Stats {
    pub captured: AtomicU64,
    pub encoded: AtomicU64,
    pub bytes: AtomicU64,
    pub keyframes: AtomicU64,
    /// How long what a capture shows waited while no capture was let go: nobody watched, or
    /// no watching viewer's gate was open (`video::Hold`). It is the viewers' links' time,
    /// and no part of any time below.
    pub gate_wait: Histogram,
    /// From when a capture's change could first be captured to its capture: the rest of the
    /// least interval, and the encoder busy with the frame before.
    pub capture_wait: Histogram,
    pub grab: Histogram,
    pub convert: Histogram,
    pub encode: Histogram,
    /// From when a frame's change could first be captured to the frame on its viewers'
    /// lanes: the pipeline's whole time, which the frame's header carries (`wire::Pipe`).
    pub send: Histogram,
}

pub struct Hub {
    pub clients: Mutex<Vec<Arc<Client>>>,
    /// Signalled, under `clients`, when a session asks for desktop audio: the audio
    /// thread waits on it while none wants any.
    audio_asked: parking_lot::Condvar,
    pub next_id: AtomicU64,
    /// The frames named since the daemon started: the next one's name.
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
            stats: Stats::default(),
            snapshots: Mutex::new(Vec::new()),
            start: Instant::now(),
        }))
    }

    pub fn wake(&self) {
        self.waker.signal();
    }

    /// The name of the frame planned next (`Chain::name`): the capture's number, counted
    /// for the daemon, not for a stream, so that a name is one frame's for as long as a
    /// viewer may hold it (a stream rebuilt for a new size starts no count again). It wraps
    /// after 2^32 frames, 828 days of them at 60 a second.
    fn next_version(&self) -> Version {
        Version(self.versions.fetch_add(1, Ordering::Relaxed))
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
    /// in constant time. The lane's path is `path` as it attaches, and its page holds
    /// `holds`. The session that took it and the lane's writer's hold on it, if one did.
    pub fn attach(
        &self,
        presented: &Capability,
        path: Path,
        holds: Holds,
    ) -> Option<(Arc<Client>, Writer)> {
        let clients = self.clients.lock().clone();
        let whose = clients.into_iter().fold(None, |found, c| {
            let is = c.lane(|s| s.is(presented));
            found.or(is.then_some(c))
        })?;
        let writer = whose.attach(path, holds)?;
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

    /// Every client, into `into` in place of what it held: the encode loop's, which keeps
    /// one vector.
    pub fn clients_into(&self, into: &mut Vec<Arc<Client>>) {
        into.clear();
        into.extend(self.clients.lock().iter().cloned());
    }

    /// What `voxl stats` says: the pipeline's counts and each step's times since the hub was
    /// made, and each client as one read of it sees it.
    pub fn stats(&self) -> serde_json::Value {
        let st = &self.stats;
        let n = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let clients: Vec<_> = self.clients.lock().iter().map(|c| c.stats()).collect();
        serde_json::json!({
            "uptime_us": self.start.elapsed().as_micros() as u64,
            "captured": n(&st.captured),
            "encoded": n(&st.encoded),
            "bytes": n(&st.bytes),
            "keyframes": n(&st.keyframes),
            "gate_wait": st.gate_wait.stats(),
            "capture_wait": st.capture_wait.stats(),
            "grab": st.grab.stats(),
            "convert": st.convert.stats(),
            "encode": st.encode.stats(),
            "send": st.send.stats(),
            "clients": clients,
        })
    }

    /// The `hello` message: the codec and what voxl started with.
    pub fn hello(&self) -> ServerMessage {
        ServerMessage::Hello(self.hello.clone())
    }

    /// After the stream restarts (new size): its chain has no frame yet (a new `Chain`), and
    /// every client starts again on a keyframe of the new stream, which carries its size.
    pub fn restart_clients(&self) {
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

    /// A step of the hub's, for one client or for all.
    #[derive(Clone, Copy, Debug)]
    enum Step {
        /// `v,1`.
        Watch(usize),
        /// `v,0`.
        StopWatching(usize),
        /// A lane attaches, replacing any before (the relay reopened it), and says what its
        /// page last decoded.
        Attach(usize),
        /// The lane's socket breaks.
        Lose(usize),
        /// The session ends, and a new one joins in its place.
        Leave(usize),
        /// What the page's link says of its lane now. Each page's is its own, and any may stay
        /// shut for good: a page gone, a stalled lane, a background tab.
        Link(usize, Gate),
        /// `build` starts the stream over for every client (`restart_clients`), once the
        /// stream before has sent what it held, and the capture loop starts a new chain.
        Restart,
        /// The capture loop looks at the viewers and lets a capture go if one's gate is open,
        /// with something moved: each viewer takes its part, and the frame is named. The
        /// encoder holds at most two.
        Capture,
        /// The encode loop codes and sends the oldest frame it holds.
        Send,
    }

    /// What a client of the model is: a page's session, or a recording, which watches from
    /// its start and takes every frame at once.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Kind {
        Page,
        Recording,
    }

    /// The steps a model of clients `kinds` takes: each page's own, and the stream's.
    fn steps(kinds: &[Kind]) -> Vec<Step> {
        let mut steps = Vec::new();
        for (i, kind) in kinds.iter().enumerate() {
            match kind {
                Kind::Page => steps.extend([
                    Step::Watch(i),
                    Step::StopWatching(i),
                    Step::Attach(i),
                    Step::Lose(i),
                    Step::Leave(i),
                    Step::Link(i, Gate::Open),
                    Step::Link(i, Gate::Full),
                    Step::Link(i, Gate::Late),
                ]),
                Kind::Recording => {}
            }
        }
        steps.extend([Step::Restart, Step::Capture, Step::Send]);
        steps
    }

    /// What a client's way out is, as the model keeps it.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    enum Lane {
        None,
        Live,
        /// Its socket broke; none has attached since.
        Lost,
        Recording,
    }

    /// What a client's page decodes from (a recording's file, for a recording): the frame its
    /// decoder showed last, of the stream as it is, if its way out brought every frame up to
    /// it. The oracle the hub is held to: a page is never sent a delta that does not follow
    /// the frame it showed.
    type Page = Option<Version>;

    /// A client as the model keeps it: what it is to the stream, its way out, its page, the
    /// update its page last decoded on any lane and of any stream (what its relay says when
    /// a lane attaches: the page keeps it through a lost lane, and its relay knows of no
    /// stream rebuilt), and what its link says (a recording's is always open).
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    struct Seat {
        viewer: Viewer,
        lane: Lane,
        page: Page,
        decoded: Option<Version>,
        gate: Gate,
    }

    impl Seat {
        /// A client as it joins.
        fn joined(kind: Kind) -> Seat {
            let (viewer, lane) = match kind {
                Kind::Page => (Viewer::NotWatching(Holds::Nothing), Lane::None),
                Kind::Recording => (Viewer::Watching(Holds::Nothing), Lane::Recording),
            };
            Seat {
                viewer,
                lane,
                page: None,
                decoded: None,
                gate: Gate::Open,
            }
        }

        /// Whether the capture loop's look sees it: it watches, with a way to be sent on.
        fn seen(&self) -> bool {
            let watches = !matches!(self.viewer, Viewer::NotWatching(_));
            watches && matches!(self.lane, Lane::Live | Lane::Recording)
        }
    }

    /// A frame the encoder holds: its name, the clients its plan promised it to if it is a
    /// keyframe, and the clients seen with their gates open when it was planned.
    #[derive(Clone, Debug, PartialEq, Eq, Hash)]
    struct Held {
        version: Version,
        key: Option<Vec<bool>>,
        open: Vec<bool>,
    }

    /// The clients, the frames the encoder holds, oldest first, the chain's newest frame as
    /// planned, the stream's newest frame coded, and the next frame's name.
    #[derive(Clone, Debug, PartialEq, Eq, Hash)]
    struct Model {
        c: Vec<Seat>,
        held: Vec<Held>,
        head: Option<Version>,
        coded: Option<Version>,
        next: u32,
    }

    impl Model {
        fn new(kinds: &[Kind]) -> Model {
            Model {
                c: kinds.iter().map(|&k| Seat::joined(k)).collect(),
                held: Vec::new(),
                head: None,
                coded: None,
                next: 0,
            }
        }
    }

    /// The rule, step by step, written from what each step means and not from the hub's
    /// functions (but for `v,1`, `v,0` and starting over, whose rules are the hub's pure
    /// ones): what each step makes of the model, and whom the frame was sent.
    fn rule(before: &Model, step: Step) -> (Vec<usize>, Model) {
        let mut m = before.clone();
        let mut sent = Vec::new();
        match step {
            Step::Watch(i) => m.c[i].viewer = watching(m.c[i].viewer),
            Step::StopWatching(i) => m.c[i].viewer = stopped(m.c[i].viewer),
            // A new lane says what the page last decoded, and the viewer holds that and
            // nothing else: what the lane before took beyond it is lost. The page shows what
            // it showed.
            Step::Attach(i) => {
                let c = &mut m.c[i];
                let holds = c.decoded.map_or(Holds::Nothing, Holds::Through);
                c.viewer = match c.viewer {
                    Viewer::NotWatching(_) => Viewer::NotWatching(holds),
                    Viewer::Watching(_) | Viewer::Promised | Viewer::Started { .. } => {
                        Viewer::Watching(holds)
                    }
                };
                c.lane = Lane::Live;
            }
            // The page shows what it showed: frames go on without it.
            Step::Lose(i) => m.c[i].lane = Lane::Lost,
            Step::Leave(i) => m.c[i] = Seat::joined(Kind::Page),
            Step::Link(i, gate) => m.c[i].gate = gate,
            Step::Restart => {
                for c in m.c.iter_mut() {
                    (c.viewer, c.page) = (restarted(c.viewer), None);
                }
                m.held.clear();
                (m.head, m.coded) = (None, None);
            }
            // A capture goes when the gate of a viewer seen is open, whatever the others'. Each
            // one seen whose gate is open is in the frame: one in the chain, or that holds the
            // chain's newest, is sent it; any other is promised it, a keyframe. Each one seen
            // whose gate is shut is passed by: it leaves the chain, holding what it held.
            Step::Capture => {
                let open: Vec<bool> =
                    m.c.iter()
                        .map(|c| c.seen() && c.gate == Gate::Open)
                        .collect();
                if open.contains(&true) {
                    let mut key = vec![false; m.c.len()];
                    for (i, c) in m.c.iter_mut().enumerate() {
                        if !c.seen() {
                            continue;
                        }
                        c.viewer = match (c.viewer, open[i]) {
                            (Viewer::Watching(Holds::Through(t)), true) if Some(t) == m.head => {
                                Viewer::Started { through: t }
                            }
                            (Viewer::Watching(_), true) => {
                                key[i] = true;
                                Viewer::Promised
                            }
                            (Viewer::Started { through }, false) => {
                                Viewer::Watching(Holds::Through(through))
                            }
                            (Viewer::Promised, false) => Viewer::Watching(Holds::Nothing),
                            (v @ (Viewer::Started { .. } | Viewer::Promised), true)
                            | (v @ (Viewer::Watching(_) | Viewer::NotWatching(_)), false)
                            | (v @ Viewer::NotWatching(_), true) => v,
                        };
                    }
                    let version = Version(m.next);
                    m.next += 1;
                    m.head = Some(version);
                    let key = key.contains(&true).then_some(key);
                    m.held.push(Held { version, key, open });
                }
            }
            Step::Send => {
                let Held { version, key, .. } = m.held.remove(0);
                for (i, c) in m.c.iter_mut().enumerate() {
                    if key.as_ref().is_some_and(|k| k[i]) && c.viewer == Viewer::Promised {
                        c.viewer = Viewer::Started { through: version };
                    }
                    if let Viewer::Started { through } = &mut c.viewer {
                        *through = version;
                        sent.push(i);
                        if matches!(c.lane, Lane::Live | Lane::Recording) {
                            (c.page, c.decoded) = (Some(version), Some(version));
                        }
                    }
                }
                m.coded = Some(version);
            }
        }
        (sent, m)
    }

    /// Whether the hub takes `step` in this state: the encoder holds two frames at most and
    /// sends only what it holds; only a live lane breaks; a link says something new.
    fn applies(m: &Model, step: Step) -> bool {
        match step {
            Step::Capture => m.held.len() < 2,
            Step::Send => !m.held.is_empty(),
            Step::Lose(i) => m.c[i].lane == Lane::Live,
            Step::Link(i, gate) => m.c[i].gate != gate,
            Step::Watch(_)
            | Step::StopWatching(_)
            | Step::Attach(_)
            | Step::Leave(_)
            | Step::Restart => true,
        }
    }

    /// A lane attaches to `c`, its page holding `holds`: its writer's hold on it.
    fn attach(c: &Client, holds: Holds) -> Writer {
        c.attach(Path::Unstated, holds)
            .expect("an open session takes a lane")
    }

    /// A real client of `hub` as `seat` says: a page with the writer of the lane it was last
    /// given, if any, or a recording.
    fn client(hub: &Hub, seat: &Seat) -> (Arc<Client>, Option<Writer>) {
        let (c, writer) = match seat.lane {
            Lane::Recording => {
                let (to, _) = mpsc::unbounded_channel();
                let (starts, _) = mpsc::unbounded_channel();
                (hub.add_recorder(to, starts), None)
            }
            lane @ (Lane::None | Lane::Live | Lane::Lost) => {
                let (tx, _rx) = mpsc::unbounded_channel();
                let c = hub.add_client(tx, Sender::new(Capability::mint().unwrap()));
                let writer = match lane {
                    Lane::None => None,
                    Lane::Live | Lane::Lost | Lane::Recording => Some(attach(&c, Holds::Nothing)),
                };
                if let (Lane::Lost, Some(w)) = (lane, &writer) {
                    c.lane(|s| s.lost(w));
                }
                (c, writer)
            }
        };
        c.viewing.lock().viewer = seat.viewer;
        (c, writer)
    }

    /// The real clients of `hub` set to `m`; the hub names the next frame as `m` does.
    fn clients(hub: &Hub, m: &Model) -> Vec<(Arc<Client>, Option<Writer>)> {
        hub.clients.lock().clear();
        hub.versions.store(m.next, Ordering::Relaxed);
        m.c.iter().map(|seat| client(hub, seat)).collect()
    }

    /// A stream's configuration, as a keyframe's viewers are told it.
    fn configured() -> Arc<crate::av1::Configuration> {
        let mut rng = crate::xorshift::Xorshift::new(0x1f83_d9ab_fb41_bd6b);
        Arc::new(crate::av1::tests::configuration(&mut rng))
    }

    /// What the oracles counted over a run.
    #[derive(Default)]
    struct Count {
        steps: usize,
        /// Viewers that came back holding the chain's newest frame and went on with it.
        resumed: usize,
        /// Viewers in the chain a frame went on without.
        passed: usize,
        /// Frames planned while a viewer seen had its gate shut.
        despite: usize,
        /// Lanes attached whose page held the chain's newest frame.
        holding: usize,
    }

    /// The same step, by the methods, on real clients of `hub` set to `m`, the capture loop's
    /// part through a `Chain` as it does it, the encode loop's as it does it
    /// (`video::encode_frames`): whom it sent the frame, and the model after it as the clients
    /// are. The oracles, held to every step:
    /// - a capture goes exactly when a viewer seen has its gate open, whatever the others'
    ///   gates: no viewer holds another back;
    /// - after it, every viewer seen whose gate is open is in the frame, and none whose gate
    ///   is shut is;
    /// - a keyframe goes only to a viewer whose page does not show the chain's newest frame;
    /// - a frame is sent only to a viewer whose gate was open when it was planned;
    /// - a page is never sent a delta that does not follow the frame it showed, on the lane
    ///   it had or on one attached since, which said what it last decoded;
    /// - a recording is sent every frame.
    fn new_step(
        hub: &Hub,
        stream: &Arc<crate::av1::Configuration>,
        m: &Model,
        step: Step,
        count: &mut Count,
    ) -> (Vec<usize>, Model) {
        let mut c = clients(hub, m);
        let mut after = m.clone();
        let mut sent = Vec::new();
        let n = m.c.len();
        let what = format!("{step:?} from {m:?}");
        match step {
            Step::Watch(i) => c[i].0.watch(),
            Step::StopWatching(i) => c[i].0.stop_watching(),
            Step::Attach(i) => {
                let holds = m.c[i].decoded.map_or(Holds::Nothing, Holds::Through);
                c[i].1 = Some(attach(&c[i].0, holds));
                count.holding += usize::from(m.head.is_some() && m.c[i].decoded == m.head);
            }
            Step::Lose(i) => {
                if let Some(w) = &c[i].1 {
                    c[i].0.lane(|s| s.lost(w));
                }
            }
            Step::Leave(i) => {
                hub.remove_client(c[i].0.id);
                assert!(
                    hub.clients.lock().iter().all(|x| x.id != c[i].0.id),
                    "{what}"
                );
                c[i] = client(hub, &Seat::joined(Kind::Page));
                // A new session's link has nothing on the way.
                (after.c[i].page, after.c[i].gate) = (None, Gate::Open);
                after.c[i].decoded = None;
            }
            Step::Link(i, gate) => after.c[i].gate = gate,
            // The capture loop of the rebuilt stream starts a chain of its own.
            Step::Restart => {
                hub.restart_clients();
                after.held.clear();
                after.c.iter_mut().for_each(|c| c.page = None);
                (after.head, after.coded) = (Chain::default().head, None);
            }
            Step::Capture => {
                let mut seen = Vec::new();
                hub.look_into(&mut seen);
                let at = |id: u64| c.iter().position(|(x, _)| x.id == id).unwrap();
                // Each page's gate is what the model's link says; a recording's is its own.
                for (x, s) in seen.iter_mut() {
                    match m.c[at(x.id)].lane {
                        Lane::Recording => assert_eq!(s.gate, Gate::Open, "{what}"),
                        Lane::None | Lane::Live | Lane::Lost => s.gate = m.c[at(x.id)].gate,
                    }
                }
                let mut open = vec![false; n];
                let mut shut = vec![false; n];
                for (x, s) in &seen {
                    match s.gate {
                        Gate::Open => open[at(x.id)] = true,
                        Gate::Full | Gate::Late => shut[at(x.id)] = true,
                    }
                }
                let mut chain = Chain { head: m.head };
                let audience = chain.audience(&seen);
                assert_eq!(audience != Audience::Nobody, open.contains(&true), "{what}");
                if audience != Audience::Nobody {
                    count.despite += usize::from(shut.contains(&true));
                    let starts = chain.parts(&seen);
                    let key = starts.map(|s| {
                        (0..n)
                            .map(|i| s.0.iter().any(|x| x.id == c[i].0.id))
                            .collect::<Vec<bool>>()
                    });
                    assert_eq!(key.is_some(), audience == Audience::Starting, "{what}");
                    for i in 0..n {
                        let viewer = c[i].0.viewer();
                        let in_frame = matches!(viewer, Viewer::Started { .. } | Viewer::Promised);
                        assert!(
                            !open[i] || in_frame,
                            "{what}: {i}, open, is not in the frame"
                        );
                        assert!(!shut[i] || !in_frame, "{what}: {i}, shut, is in the frame");
                        let promised = key.as_ref().is_some_and(|k| k[i]);
                        let shows = m.head.is_some() && m.c[i].page == m.head;
                        assert!(
                            !(promised && shows),
                            "{what}: a keyframe for a page that shows the head"
                        );
                        let was = m.c[i].viewer;
                        if let (Viewer::Watching(_), Viewer::Started { .. }) = (was, viewer) {
                            count.resumed += 1;
                        }
                        if let (Viewer::Started { .. }, Viewer::Watching(_)) = (was, viewer) {
                            count.passed += 1;
                        }
                    }
                    let version = chain.name(hub);
                    after.held.push(Held { version, key, open });
                }
                after.head = chain.head;
            }
            Step::Send => {
                let Held { version, key, open } = after.held.remove(0);
                if let Some(key) = &key {
                    for (i, (c, _)) in c.iter().enumerate() {
                        if key[i] {
                            c.started(version, stream);
                        }
                    }
                }
                for (i, (c, _)) in c.iter().enumerate() {
                    if c.takes(&Bytes::from_static(&[0; 16]), version) {
                        sent.push(i);
                        // A way out it has (one without a lane takes frames into nothing,
                        // until a lane attaches and starts it over).
                        if matches!(m.c[i].lane, Lane::Live | Lane::Recording) {
                            assert!(open[i], "{what}: {i} is sent a frame its shut gate let by");
                            let page = &mut after.c[i].page;
                            let follows = key.is_some() || *page == m.coded;
                            assert!(follows, "{what}: a delta {i} cannot decode");
                            *page = Some(version);
                            after.c[i].decoded = Some(version);
                        }
                    }
                }
                for (i, seat) in m.c.iter().enumerate() {
                    if seat.lane == Lane::Recording {
                        assert!(sent.contains(&i), "{what}: the recording missed a frame");
                    }
                }
                after.coded = Some(version);
            }
        }
        // What `voxl stats` says of each way out is what the capture loop was shown of it.
        for (i, (x, writer)) in c.iter().enumerate() {
            let way = x.lane(|s| s.stats(Instant::now()));
            let lane = match (writer, way) {
                (None, Way::Unattached) => Lane::None,
                (None, Way::Recording) => Lane::Recording,
                (Some(_), Way::Lane(_)) => Lane::Live,
                (Some(_), Way::Unattached) => Lane::Lost,
                (None, way @ Way::Lane(_))
                | (Some(_), way @ Way::Recording)
                | (None | Some(_), way @ Way::Closed) => {
                    panic!("{what}: a session's way is {way:?}")
                }
            };
            after.c[i].viewer = x.viewer();
            after.c[i].lane = lane;
        }
        after.next = hub.versions.load(Ordering::Relaxed);
        (sent, after)
    }

    /// One step through the methods, held to the rule: whom it sent the frame, and the model
    /// after it.
    fn checked(
        hub: &Hub,
        stream: &Arc<crate::av1::Configuration>,
        m: &Model,
        step: Step,
        count: &mut Count,
    ) -> (Vec<usize>, Model) {
        count.steps += 1;
        let got = new_step(hub, stream, m, step, count);
        assert_eq!(got, rule(m, step), "{step:?} from {m:?}");
        got
    }

    /// From `state`, once the encoder has sent what it holds, the pages of `stuck` have their
    /// gates shut for good (late) and every other page's opens: one capture and its frame
    /// leave every viewer seen whose gate is open in the chain, sent the frame, and those
    /// stuck sent nothing. So a viewer that missed frames is back on a keyframe, and no
    /// viewer whose gate stays shut holds anyone back.
    fn unstuck(
        hub: &Hub,
        stream: &Arc<crate::av1::Configuration>,
        state: &Model,
        stuck: &[bool],
        count: &mut Count,
    ) {
        let mut m = state.clone();
        while !m.held.is_empty() {
            m = checked(hub, stream, &m, Step::Send, count).1;
        }
        for (i, &stuck) in stuck.iter().enumerate() {
            if m.c[i].lane != Lane::Recording {
                let gate = [Gate::Open, Gate::Late][usize::from(stuck)];
                m = checked(hub, stream, &m, Step::Link(i, gate), count).1;
            }
        }
        m = checked(hub, stream, &m, Step::Capture, count).1;
        let (sent, m) = match m.held.is_empty() {
            true => (Vec::new(), m),
            false => checked(hub, stream, &m, Step::Send, count),
        };
        for (i, seat) in m.c.iter().enumerate() {
            match (seat.seen(), stuck[i]) {
                (true, false) => {
                    let on = matches!(seat.viewer, Viewer::Started { .. }) && sent.contains(&i);
                    assert!(on, "{i} left unstarted from {state:?}: {m:?}");
                }
                (true, true) => {
                    assert!(
                        !sent.contains(&i),
                        "{i}, stuck, sent a frame from {state:?}"
                    )
                }
                (false, false | true) => {}
            }
        }
    }

    /// Every combination of stuck pages among `kinds`.
    fn stuck_sets(kinds: &[Kind]) -> Vec<Vec<bool>> {
        (0..1u32 << kinds.len())
            .map(|set| {
                let stuck = |i: usize| set >> i & 1 == 1 && kinds[i] == Kind::Page;
                (0..kinds.len()).map(stuck).collect()
            })
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// A viewer starts only on a keyframe promised it, or goes on from the chain's newest
    /// frame it holds; a keyframe is promised only to a viewer with a live lane, its gate open,
    /// whose page does not show the chain's newest; a viewer is sent frames only in the chain,
    /// and only those planned while its gate was open; a page is never sent a delta that does
    /// not follow the frame it showed; a recording is sent every frame; and a capture goes
    /// whenever any viewer's gate is open, every viewer seen with its gate open in it,
    /// whatever the others' gates. Over every state two pages and a recording, each page's
    /// gate its own, and the encoder reach in up to 8 of the hub's steps, breadth first, every
    /// step from every state, run on a real hub's clients through the methods: each step does
    /// what the rule says, and from every state, with any of the pages stuck for good, once
    /// the encoder has sent what it holds, one capture and its frame leave every other viewer
    /// in the chain.
    #[test]
    fn a_viewer_starts_only_on_a_keyframe_promised_it() {
        let hub = Hub::new(hello()).unwrap();
        let stream = configured();
        let kinds = [Kind::Page, Kind::Page, Kind::Recording];
        let steps = steps(&kinds);
        let start = Model::new(&kinds);
        let mut seen = std::collections::HashSet::from([start.clone()]);
        let mut frontier = vec![start];
        let mut count = Count::default();
        for _depth in 0..8 {
            let mut next = vec![];
            for before in frontier {
                for &step in &steps {
                    if !applies(&before, step) {
                        continue;
                    }
                    let got = checked(&hub, &stream, &before, step, &mut count).1;
                    if seen.insert(got.clone()) {
                        next.push(got);
                    }
                }
            }
            frontier = next;
        }
        let stuck = stuck_sets(&kinds);
        for state in &seen {
            for stuck in &stuck {
                unstuck(&hub, &stream, state, stuck, &mut Count::default());
            }
        }
        let reached: std::collections::HashSet<std::mem::Discriminant<Viewer>> = seen
            .iter()
            .flat_map(|m| m.c.iter().map(|c| std::mem::discriminant(&c.viewer)))
            .collect();
        assert_eq!(reached.len(), 4, "{reached:?}");
        let Count {
            steps,
            resumed,
            passed,
            despite,
            holding,
        } = count;
        println!(
            "{} states, {steps} steps, {resumed} resumed, {passed} passed by, {holding} lanes \
             attached holding the newest",
            seen.len()
        );
        assert!(steps > 0 && resumed > 0 && passed > 0 && despite > 0 && holding > 0);
    }

    /// No viewer holds another back: the oracles of each step and the rule, over generated
    /// sequences of four pages and a recording, each page's gate its own (open, full, late,
    /// and some never open again), with joins, leaves, `v,0` and `v,1`, lanes replaced and
    /// lost, restarts, captures and sends; and at each sequence's end, with a generated set
    /// of pages stuck for good, one capture and its frame leave every other viewer in the
    /// chain.
    #[test]
    fn no_viewer_holds_another_back() {
        let hub = Hub::new(hello()).unwrap();
        let stream = configured();
        let kinds = [
            Kind::Page,
            Kind::Page,
            Kind::Page,
            Kind::Page,
            Kind::Recording,
        ];
        // Drawn so that sessions get going: a capture, a frame sent, a lane attached and a
        // viewer watching come more often than what ends them.
        let steps: Vec<Step> = steps(&kinds)
            .into_iter()
            .flat_map(|step| {
                let weight = match step {
                    Step::Capture | Step::Send => 24,
                    Step::Watch(_) | Step::Attach(_) => 4,
                    Step::Link(_, _) => 2,
                    Step::StopWatching(_) | Step::Lose(_) | Step::Leave(_) | Step::Restart => 1,
                };
                std::iter::repeat_n(step, weight)
            })
            .collect();
        let stuck = stuck_sets(&kinds);
        let mut rng = crate::xorshift::Xorshift::new(0x9b05_688c_2b3e_6c1f);
        let mut count = Count::default();
        for _ in 0..400 {
            let mut m = Model::new(&kinds);
            // Pages whose gates, once shut, never open again in this sequence.
            let gone: Vec<bool> = (0..kinds.len()).map(|_| rng.below(4) == 0).collect();
            for _ in 0..60 {
                let step = steps[rng.below(steps.len() as u64) as usize];
                let reopens = match step {
                    Step::Link(i, Gate::Open) => !gone[i] || m.c[i].gate == Gate::Open,
                    Step::Link(_, Gate::Full | Gate::Late)
                    | Step::Watch(_)
                    | Step::StopWatching(_)
                    | Step::Attach(_)
                    | Step::Lose(_)
                    | Step::Leave(_)
                    | Step::Restart
                    | Step::Capture
                    | Step::Send => true,
                };
                if applies(&m, step) && reopens {
                    m = checked(&hub, &stream, &m, step, &mut count).1;
                }
            }
            let stuck = &stuck[rng.below(stuck.len() as u64) as usize];
            unstuck(&hub, &stream, &m, stuck, &mut count);
        }
        let Count {
            steps,
            resumed,
            passed,
            despite,
            holding,
        } = count;
        println!(
            "{steps} steps, {resumed} resumed, {passed} passed by, {despite} despite, {holding} \
             lanes attached holding the newest"
        );
        assert!(passed > 100 && despite > 1000 && holding > 20);
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
