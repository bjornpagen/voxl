//! A session's way out, as the media producers use it: frames in, on its lane's socket as the
//! socket takes them, and each frame (a video update or an Opus packet) counted until the
//! relay says the page took it.
//!
//! The lane is one stream through the transport (a QUIC stream, or TCP, under the proxy).
//! Its socket takes whatever the daemon writes, far past what has arrived: the transport's
//! window is megabytes deep. So nothing here reads the socket for what the link carries.
//! What was delivered is told on the control stream: the lane keeps what it was sent and
//! has not been told taken (`link::Outstanding`), and its path as its transport states it
//! (`statement`, `link::Path`). From those comes the gate (`link.rs`), which every frame
//! passes: a video frame goes onto the lane only if its gate was open when the frame's
//! capture was planned, and an Opus packet only while it is open. The gate is this lane's
//! alone: when its path slows or stalls, the frames planned meanwhile go on without it, and
//! once it opens the screen as it then is goes next, on a keyframe (`hub::planned`); every
//! other session's lane is sent its frames as before. The sound is held too, but only the
//! newest packets the page's player holds ([`PLAYER_PACKETS`]): what is older would be
//! dropped by the page on arrival, behind it on the lane. Dropping beats lag, at the source.
//!
//! A session has a lane or it has none: a lane whose socket breaks is the session's lane
//! no longer, and what it held goes with it. The next lane the relay attaches starts its
//! viewer over on a keyframe (`hub::Client::attach`), since bytes the broken socket
//! accepted may never have reached the relay.
//!
//! Everything here is synchronous, under the session's lock in the hub: the lane's writer
//! task (`lane.rs`) takes that lock for each write, so what it writes is its session's lane
//! as it is at that moment, and a lane that has ended gets not one byte more.

use super::link::{Gate, Outstanding, Path, Stats};
use crate::hub::Out;
use crate::wire::{Capability, Frame, Sequence, Track, PLAYER_PACKETS};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

/// A viewer's way out, as `voxl stats` says it.
#[derive(Debug, serde::Serialize)]
#[serde(tag = "way", rename_all = "snake_case")]
pub enum Way {
    /// A session with no lane: none has attached, or the last one's socket broke.
    Unattached,
    /// Its lane: what is on the way, the path as it is known, and the gate.
    Lane(Stats),
    Recording,
    /// A session whose control connection closed.
    Closed,
}

/// A viewer's way out: a session's lane, or a recording in this process.
pub struct Sender {
    state: State,
}

/// A way out is exactly one of these. A lane that ends is no lane: the state becomes
/// `Unattached`, and what the lane held goes with it.
enum State {
    /// Its control connection is open, and no lane takes its frames: none has attached, or
    /// the last one's socket broke.
    Unattached(Session),
    /// Its control connection is open, and a lane takes its frames.
    Attached(Session, Lane),
    /// A recording (`voxl record`): each update goes to it whole, its header and its AV1, on
    /// its channel. It has no capability and no lane.
    Recording(tokio::sync::mpsc::UnboundedSender<Out>),
    /// Its control connection closed: the capability is forgotten, and no lane is sent
    /// another byte.
    Closed,
}

/// What an open session holds, with a lane or without.
struct Session {
    /// What its lane attaches with.
    capability: Capability,
    /// The name of the next frame its lanes are sent. It outlives a lane, so a took of a
    /// lane since replaced names nothing on the next.
    next: Sequence,
}

impl Session {
    fn new(capability: Capability) -> Session {
        Session {
            capability,
            next: Sequence(0),
        }
    }

    /// A frame of `track` and `bytes` onto `lane` at `now`, named the next of the session's
    /// names, and on the way until it is told taken.
    fn send(&mut self, lane: &mut Lane, track: Track, bytes: &[u8], now: Instant) {
        let sequence = self.next;
        self.next = sequence.next();
        lane.push(sequence, track, bytes);
        lane.sent.sent(sequence, bytes.len() as u64, now);
    }

    /// The Opus packets `lane` holds go onto it, oldest first, each while the gate is open
    /// at `now`.
    fn flow(&mut self, lane: &mut Lane, now: Instant) {
        loop {
            match lane.gate(now) {
                Gate::Open => {}
                Gate::Full | Gate::Late => return,
            }
            let Some(packet) = lane.held.pop_front() else {
                return;
            };
            self.send(lane, Track::Audio, &packet, now);
            lane.spare.push(packet);
        }
    }
}

/// A session's lane: the frames waiting for its socket, the Opus packets held while the gate
/// is not open, what it was sent and has not been told taken, and its path. Dropped, it is the
/// session's lane no longer, and its writer is woken to find that out.
struct Lane {
    writer: Writer,
    /// The frames waiting, each as the lane carries it.
    messages: VecDeque<Vec<u8>>,
    /// Bytes of the front message the socket has taken.
    written: usize,
    /// Buffers of messages that left, for the next ones: a steady stream allocates none.
    spare: Vec<Vec<u8>>,
    /// The newest Opus packets that came while the gate was not open, oldest first: at most
    /// [`PLAYER_PACKETS`], what the page's player holds, so a stall keeps no older sound.
    held: VecDeque<Vec<u8>>,
    sent: Outstanding,
    /// The path it crosses, as its transport states it: this lane's alone, since the next
    /// lane may cross another.
    path: Path,
}

/// A lane's writer's hold on its lane: what wakes it, and what its session knows the lane
/// by.
#[derive(Clone)]
pub struct Writer(Arc<tokio::sync::Notify>);

impl Writer {
    /// Woken: a frame came for its lane, or the lane is its session's no longer.
    pub(super) async fn woken(&self) {
        self.0.notified().await
    }

    fn is(&self, other: &Writer) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// What a lane's writer does next.
pub(super) enum Next {
    /// Write again: more waits, and the socket may take it.
    Write,
    /// Wait for the socket to take more bytes.
    Writable,
    /// Wait for a frame.
    Idle,
    /// The lane is its session's no longer: end.
    End,
}

impl Drop for Lane {
    fn drop(&mut self) {
        self.writer.0.notify_one();
    }
}

impl Lane {
    /// A lane whose path is `path` as it attaches: not stated, or not yet.
    fn new(writer: Writer, path: Path) -> Lane {
        Lane {
            writer,
            messages: VecDeque::new(),
            written: 0,
            spare: Vec::new(),
            held: VecDeque::with_capacity(PLAYER_PACKETS),
            sent: Outstanding::default(),
            path,
        }
    }

    /// Whether it takes a capture at `now`.
    fn gate(&self, now: Instant) -> Gate {
        self.sent.gate(self.path, now)
    }

    /// A buffer to fill: a spare one if one is left, else a new one.
    fn buffer(&mut self) -> Vec<u8> {
        let mut buf = self.spare.pop().unwrap_or_default();
        buf.clear();
        buf
    }

    /// Queues the frame named `sequence`, and wakes the writer.
    fn push(&mut self, sequence: Sequence, track: Track, bytes: &[u8]) {
        let mut buf = self.buffer();
        Frame::encode(sequence, track, bytes, &mut buf);
        self.messages.push_back(buf);
        self.writer.0.notify_one();
    }

    /// Holds an Opus packet until the gate opens: the newest [`PLAYER_PACKETS`] are kept, and
    /// the oldest held goes when one more comes.
    fn hold(&mut self, packet: &[u8]) {
        let mut buf = self.buffer();
        buf.extend_from_slice(packet);
        self.held.push_back(buf);
        let over = self.held.len().saturating_sub(PLAYER_PACKETS);
        self.spare.extend(self.held.drain(..over));
    }

    /// Gives the front message's unwritten bytes to `socket`, which says how many it took:
    /// what the writer does next, or the error of a socket that broke.
    fn write(
        &mut self,
        socket: impl FnOnce(&[u8]) -> std::io::Result<usize>,
    ) -> std::io::Result<Next> {
        let Some(front) = self.messages.pop_front() else {
            return Ok(Next::Idle);
        };
        let rest = &front[self.written..];
        match socket(rest) {
            // All of it left: its buffer serves the next.
            Ok(n) if n == rest.len() => {
                self.written = 0;
                self.spare.push(front);
                Ok(Next::Write)
            }
            Ok(n) => {
                self.written += n;
                self.messages.push_front(front);
                Ok(Next::Write)
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                self.messages.push_front(front);
                Ok(Next::Writable)
            }
            Err(e) => Err(e),
        }
    }
}

impl Sender {
    /// A session's, with the capability its lane attaches with.
    pub fn new(capability: Capability) -> Sender {
        Sender {
            state: State::Unattached(Session::new(capability)),
        }
    }

    /// A recording's, its updates sent on `to`.
    pub fn recording(to: tokio::sync::mpsc::UnboundedSender<Out>) -> Sender {
        Sender {
            state: State::Recording(to),
        }
    }

    /// Whether `presented` is its capability: never once it has closed, and never a
    /// recording's, which has none.
    pub fn is(&self, presented: &Capability) -> bool {
        match &self.state {
            State::Unattached(s) | State::Attached(s, _) => s.capability.matches(presented),
            State::Recording(_) | State::Closed => false,
        }
    }

    /// Its control connection closed: the capability is dropped (and zeroed), and its lane
    /// is its lane no longer before this returns.
    pub fn close(&mut self) {
        self.state = State::Closed;
    }

    /// Whether its way out takes a capture at `now`: whether a capture planned now is sent
    /// it, and goes for it. None if it has no way to be sent frames on: it takes no part in
    /// a capture then. A recording is no link: it takes every frame at once.
    pub fn gate(&self, now: Instant) -> Option<Gate> {
        match &self.state {
            State::Attached(_, lane) => Some(lane.gate(now)),
            State::Recording(_) => Some(Gate::Open),
            State::Unattached(_) | State::Closed => None,
        }
    }

    /// A lane attaches, its path `path` (not stated, or not yet): it takes the place of the
    /// lane before, which is the session's no longer, and starts with nothing on the way.
    /// Its writer's hold on it; none for a closed session or a recording, which take no
    /// lane.
    pub fn attach(&mut self, path: Path) -> Option<Writer> {
        let writer = Writer(Arc::new(tokio::sync::Notify::new()));
        let lane = Lane::new(writer.clone(), path);
        // The lane before, if any, is dropped here, which wakes its writer to end.
        let (state, attached) = match std::mem::replace(&mut self.state, State::Closed) {
            State::Unattached(s) | State::Attached(s, _) => {
                (State::Attached(s, lane), Some(writer))
            }
            way @ (State::Recording(_) | State::Closed) => (way, None),
        };
        self.state = state;
        attached
    }

    /// The relay said at `now` that the page took the frame named `sequence`, which may open
    /// the gate to the sound the lane holds. A session with no lane has nothing on the way; a
    /// recording and a closed session are told nothing.
    pub fn took(&mut self, sequence: Sequence, now: Instant) {
        match &mut self.state {
            State::Attached(s, lane) => {
                lane.sent.took(sequence);
                s.flow(lane, now);
            }
            State::Unattached(_) | State::Recording(_) | State::Closed => {}
        }
    }

    /// The statement of the lane `writer` writes said at `now` that its path is `path`,
    /// which may open the gate to the sound the lane holds. A lane that is the session's no
    /// longer is told nothing: its statement is not the next lane's.
    pub(crate) fn stated(&mut self, writer: &Writer, path: Path, now: Instant) {
        match &mut self.state {
            State::Attached(s, lane) if lane.writer.is(writer) => {
                lane.path = path;
                s.flow(lane, now);
            }
            State::Attached(_, _) | State::Unattached(_) | State::Recording(_) | State::Closed => {}
        }
    }

    /// What `voxl stats` says of it at `now`.
    pub fn stats(&self, now: Instant) -> Way {
        match &self.state {
            State::Attached(_, lane) => Way::Lane(lane.sent.stats(lane.path, now)),
            State::Unattached(_) => Way::Unattached,
            State::Recording(_) => Way::Recording,
            State::Closed => Way::Closed,
        }
    }

    /// A video update, its header and its picture, at `now`, sent because the gate was open
    /// when its capture was planned: onto the lane, counted until it is told taken, or to a
    /// recording as it is. A recording whose recorder has gone takes nothing: its viewer is
    /// removed with it.
    pub fn video(&mut self, bytes: &bytes::Bytes, now: Instant) {
        match &mut self.state {
            State::Attached(s, lane) => s.send(lane, Track::Video, bytes, now),
            State::Recording(to) => {
                if to.send(Out::Bin(bytes.clone())).is_err() {
                    log::debug!("a recording ended before its viewer left");
                }
            }
            State::Unattached(_) | State::Closed => {}
        }
    }

    /// An Opus packet at `now`, `silent` frames of silence after the one before: onto the
    /// lane while its gate is open, held among the newest the page's player holds while it
    /// is not; or to a recording as it is, with the silence it follows.
    pub fn audio(&mut self, packet: &[u8], silent: u32, now: Instant) {
        match &mut self.state {
            State::Attached(s, lane) => {
                lane.hold(packet);
                s.flow(lane, now);
            }
            State::Recording(to) => {
                let packet = bytes::Bytes::copy_from_slice(packet);
                if to.send(Out::Audio { packet, silent }).is_err() {
                    log::debug!("a recording ended before its viewer left");
                }
            }
            State::Unattached(_) | State::Closed => {}
        }
    }

    /// The session's lane, if it is the one `writer` writes. A writer whose lane was
    /// replaced, or whose session closed, has none: two writers live at once only until
    /// the older finds that out here.
    fn lane_of(&mut self, writer: &Writer) -> Option<&mut Lane> {
        match &mut self.state {
            State::Attached(_, lane) => Some(lane).filter(|lane| lane.writer.is(writer)),
            State::Unattached(_) | State::Recording(_) | State::Closed => None,
        }
    }

    /// One step of `writer`: the front of its lane's queue is given to `socket`, which
    /// says how many bytes it took. Under the session's lock, so a lane that is the
    /// session's no longer is given nothing. A socket that broke ends the lane.
    pub(super) fn write(
        &mut self,
        writer: &Writer,
        socket: impl FnOnce(&[u8]) -> std::io::Result<usize>,
    ) -> Next {
        let wrote = match self.lane_of(writer) {
            Some(lane) => lane.write(socket),
            None => return Next::End,
        };
        match wrote {
            Ok(next) => next,
            Err(_broken) => {
                self.lost(writer);
                Next::End
            }
        }
    }

    /// The socket of the lane `writer` writes broke: if that lane is still the session's,
    /// the session has no lane now, and what the lane held is dropped with it.
    pub(crate) fn lost(&mut self, writer: &Writer) {
        self.state = match std::mem::replace(&mut self.state, State::Closed) {
            State::Attached(s, lane) => match lane.writer.is(writer) {
                true => State::Unattached(s),
                false => State::Attached(s, lane),
            },
            way @ (State::Unattached(_) | State::Recording(_) | State::Closed) => way,
        };
    }
}

#[cfg(test)]
impl Sender {
    /// The messages waiting for its lane's socket.
    pub fn queued(&self) -> usize {
        match &self.state {
            State::Attached(_, lane) => lane.messages.len(),
            State::Unattached(_) | State::Recording(_) | State::Closed => 0,
        }
    }

    /// The front message of its lane as the writer gives it to its socket, taken out whole.
    pub(crate) fn take(&mut self) -> Option<Vec<u8>> {
        let lane = match &mut self.state {
            State::Attached(_, lane) => lane,
            State::Unattached(_) | State::Recording(_) | State::Closed => return None,
        };
        let mut taken = None;
        lane.write(|bytes| {
            taken = Some(bytes.to_vec());
            Ok(bytes.len())
        })
        .expect("a socket that takes all");
        taken
    }

    /// Every frame its lane holds, in order, each its name, its track and its bytes; the
    /// queue is emptied, as if the socket took all.
    pub(crate) fn frames(&mut self) -> Vec<(Sequence, Track, Vec<u8>)> {
        std::iter::from_fn(|| self.take())
            .map(|m| match Frame::decode(&m[crate::wire::PREFIX..]) {
                Ok(f) => (f.sequence, f.track, f.bytes.to_vec()),
                Err(e) => panic!("a lane held what is no frame: {e}"),
            })
            .collect()
    }

    /// The video updates its lane holds, in order, each its bytes; the queue is emptied,
    /// as if the socket took all.
    pub(crate) fn updates(&mut self) -> Vec<Vec<u8>> {
        video(self.frames())
    }

    /// Every frame its lane holds, taken off as its socket would and each told taken at
    /// `now`, as the page and its relay say it: the video updates among them, in order.
    pub(crate) fn delivered(&mut self, now: Instant) -> Vec<Vec<u8>> {
        let frames = self.frames();
        for (sequence, _, _) in &frames {
            self.took(*sequence, now);
        }
        video(frames)
    }

    /// The Opus packets its lane holds while the gate is not open.
    pub(crate) fn held(&self) -> usize {
        match &self.state {
            State::Attached(_, lane) => lane.held.len(),
            State::Unattached(_) | State::Recording(_) | State::Closed => 0,
        }
    }

    /// The bytes on its lane's way: sent and not told taken.
    pub(crate) fn outstanding(&self) -> u64 {
        match &self.state {
            State::Attached(_, lane) => lane.sent.bytes(),
            State::Unattached(_) | State::Recording(_) | State::Closed => 0,
        }
    }

    /// A session's sender with a lane attached, its path not stated, which no writer
    /// drains: a test reads it.
    pub fn with_lane() -> (Sender, Writer) {
        let mut s = Sender::new(Capability::mint().expect("a random source"));
        let writer = s
            .attach(Path::Unstated)
            .expect("an open session takes a lane");
        (s, writer)
    }
}

/// The video updates among `frames`, in order, each its bytes.
#[cfg(test)]
fn video(frames: Vec<(Sequence, Track, Vec<u8>)>) -> Vec<Vec<u8>> {
    frames
        .into_iter()
        .filter_map(|(_, track, bytes)| match track {
            Track::Video => Some(bytes),
            Track::Audio => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::link::Path;
    use crate::xorshift::Xorshift;
    use std::collections::HashSet;
    use std::time::Duration;

    /// One step of a generated session.
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Step {
        /// A capture, if the gate lets one start, and its update onto the lane.
        Capture,
        /// An Opus packet comes.
        Packet,
        /// The relay says a frame of the lane taken, and so every frame before it.
        Took,
        /// The relay says taken a frame of a lane since replaced, or one never sent.
        Stale,
        /// A stall: time passes and no took comes.
        Stall,
        /// A little time passes.
        Tick,
        /// A new lane takes the place of the one before.
        Replace,
        /// The lane's statement says its path, or ends.
        State,
        /// The statement of a lane since replaced says a path.
        StaleState,
    }

    /// A lane's path as it attaches: not stated, or not yet.
    fn new_path(rng: &mut Xorshift) -> Path {
        [Path::Unstated, Path::Unknown][rng.below(2) as usize]
    }

    /// A path a statement says: any rate from 20 KB/s to 40 MB/s and any round trip from 1
    /// to 400 ms; or, one time in five, its end.
    fn said(rng: &mut Xorshift) -> Path {
        match rng.below(5) {
            0 => Path::Unknown,
            1.. => Path::Known(crate::session::link::Known {
                round_trip: Duration::from_micros(rng.range(1_000, 400_000)),
                rate: crate::session::link::BytesPerSecond(
                    (20_000u64 << rng.below(11)) + rng.below(20_000),
                ),
            }),
        }
    }

    /// The steps a session is drawn from, each as often as it is listed: sound comes more
    /// often than anything else, as it does (100 packets a second), and tooks less, so the
    /// gate is shut for long runs of it.
    const STEPS: [Step; 15] = [
        Step::Capture,
        Step::Capture,
        Step::Packet,
        Step::Packet,
        Step::Packet,
        Step::Packet,
        Step::Took,
        Step::Stale,
        Step::Stall,
        Step::Tick,
        Step::Tick,
        Step::Replace,
        Step::State,
        Step::State,
        Step::StaleState,
    ];

    /// Every frame of a lane is told taken by its name, and a shut gate keeps only the newest
    /// sound. Over generated sessions of captures (each only while the gate is open, as the
    /// capture loop starts them), Opus packets, tooks of frames on the lane, tooks of frames
    /// of lanes since replaced and of names never sent, stalls, and lanes replaced; the gate
    /// is the session's own, read as the capture loop reads it, on a path its lane's
    /// statement says and says again (and the statement of a lane since replaced says to
    /// nobody). The model keeps, apart from
    /// the sender, the packets not yet sent (the newest of them, at most the player's) and
    /// the frames on the lane not yet named. After every step:
    /// - every frame that went onto the lane has a name no frame of the session had before;
    /// - a packet goes onto the lane only from the front of the model's held packets, in
    ///   order, and none goes in a step that began with the gate shut, unless a took or a
    ///   path stated opened it: while the gate is shut, no sound goes and at most the player's packets wait,
    ///   the newest;
    /// - with the gate open, no packet waits;
    /// - a took takes exactly the frames up to the one it names on the lane, and a took of
    ///   any other name takes nothing: what is on the way is the model's frames, video and
    ///   audio alike, byte for byte;
    /// - the lane's path is exactly what its own statement said last, or what it attached
    ///   with;
    /// - the gate's bound covers the sound's bytes: with the path unknown, frames on the way
    ///   (sound alone too) hold the gate, and with it known, more on the way than the window
    ///   holds it.
    #[test]
    fn every_frame_is_told_taken_and_a_shut_gate_keeps_only_the_newest_sound() {
        let mut rng = Xorshift::new(0x5BE0_CD19_137E_2179);
        let (mut shut, mut dropped, mut released, mut stale) = (0, 0, 0, 0);
        let (mut sound_alone, mut over_window, mut cumulative) = (0, 0, 0);
        let (mut stated, mut stale_stated) = (0, 0);
        for case in 0..400 {
            let (mut s, mut writer) = Sender::with_lane();
            let (mut path, mut old) = (Path::Unstated, None);
            let mut now = Instant::now();
            let mut held: VecDeque<u32> = VecDeque::new();
            let mut on: Vec<(Sequence, Track, u64)> = Vec::new();
            let mut names: HashSet<Sequence> = HashSet::new();
            let mut gone: Vec<Sequence> = Vec::new();
            let mut packets = 0u32;
            for step in 0..rng.range(20, 300) {
                let before = s.gate(now);
                let does = STEPS[rng.below(STEPS.len() as u64) as usize];
                match does {
                    Step::Capture => match before {
                        Some(Gate::Open) => {
                            let len = match rng.below(4) {
                                0 => rng.range(50_000, 400_000),
                                1.. => rng.range(100, 5_000),
                            };
                            s.video(&bytes::Bytes::from(vec![7; len as usize]), now)
                        }
                        Some(Gate::Full | Gate::Late) | None => {}
                    },
                    Step::Packet => {
                        let mut packet = packets.to_be_bytes().to_vec();
                        packet.resize(rng.range(4, 400) as usize, 1);
                        s.audio(&packet, 0, now);
                        held.push_back(packets);
                        packets += 1;
                        if held.len() > PLAYER_PACKETS {
                            held.pop_front();
                            dropped += 1;
                        }
                        match before {
                            Some(Gate::Open) => {}
                            Some(Gate::Full | Gate::Late) | None => shut += 1,
                        }
                    }
                    Step::Took => match on.len() {
                        0 => {}
                        n @ 1.. => {
                            let upto = rng.below(n as u64) as usize;
                            s.took(on[upto].0, now);
                            cumulative += upto;
                            gone.extend(on.drain(..=upto).map(|(name, _, _)| name));
                        }
                    },
                    Step::Stale => {
                        let name = match (rng.below(2), gone.len()) {
                            (0, n @ 1..) => gone[rng.below(n as u64) as usize],
                            (0, 0) | (1.., _) => loop {
                                let name = Sequence(rng.next() as u32);
                                if !names.contains(&name) {
                                    break name;
                                }
                            },
                        };
                        s.took(name, now);
                        stale += 1;
                    }
                    Step::Stall => now += Duration::from_millis(rng.range(500, 30_000)),
                    Step::Tick => now += Duration::from_millis(rng.range(1, 50)),
                    Step::Replace => {
                        path = new_path(&mut rng);
                        let new = s.attach(path).expect("an open session takes a lane");
                        old = Some(std::mem::replace(&mut writer, new));
                        gone.extend(on.drain(..).map(|(name, _, _)| name));
                        held.clear();
                    }
                    Step::State => {
                        path = said(&mut rng);
                        s.stated(&writer, path, now);
                        stated += 1;
                    }
                    Step::StaleState => {
                        if let Some(old) = &old {
                            s.stated(old, said(&mut rng), now);
                            stale_stated += 1;
                        }
                    }
                }
                let at = format!("case {case} step {step} ({does:?})");
                for (sequence, track, bytes) in s.frames() {
                    assert!(names.insert(sequence), "{at}: a name given twice");
                    match track {
                        Track::Video => assert_eq!(does, Step::Capture, "{at}"),
                        Track::Audio => {
                            match (before, does) {
                                (Some(Gate::Open), _) | (_, Step::Took | Step::State) => {}
                                (Some(Gate::Full | Gate::Late) | None, _) => {
                                    panic!("{at}: sound went while the gate was shut")
                                }
                            }
                            let id = u32::from_be_bytes(bytes[..4].try_into().unwrap());
                            assert_eq!(held.pop_front(), Some(id), "{at}: not the next held");
                            released += 1;
                        }
                    }
                    on.push((sequence, track, bytes.len() as u64));
                }
                assert!(s.held() <= PLAYER_PACKETS, "{at}");
                assert_eq!(s.held(), held.len(), "{at}");
                let on_bytes: u64 = on.iter().map(|(_, _, b)| b).sum();
                assert_eq!(s.outstanding(), on_bytes, "{at}");
                let Way::Lane(stats) = s.stats(now) else {
                    panic!("{at}: a session with a lane says its lane");
                };
                assert_eq!(stats.outstanding_frames, on.len(), "{at}");
                assert_eq!(stats.path, path, "{at}");
                match (stats.gate, held.len()) {
                    (Gate::Open, 0) | (Gate::Full | Gate::Late, _) => {}
                    (Gate::Open, 1..) => panic!("{at}: sound waits at an open gate"),
                }
                let sound_only = on.iter().all(|(_, t, _)| *t == Track::Audio);
                match (stats.path, on.len()) {
                    (Path::Unstated | Path::Unknown, 0) => {}
                    (Path::Unstated | Path::Unknown, 1..) => {
                        assert_ne!(stats.gate, Gate::Open, "{at}: the first flight let more");
                        sound_alone += usize::from(sound_only);
                    }
                    (Path::Known(known), _) => {
                        if on_bytes > known.window() {
                            assert_ne!(stats.gate, Gate::Open, "{at}: over the window");
                            over_window += 1;
                        }
                    }
                }
            }
        }
        assert!(
            shut > 4_000 && dropped > 2_000 && released > 8_000 && stale > 3_000,
            "{shut} {dropped} {released} {stale}"
        );
        assert!(
            sound_alone > 1_500 && over_window > 1_200 && cumulative > 2_000,
            "{sound_alone} {over_window} {cumulative}"
        );
        assert!(
            stated > 4_000 && stale_stated > 1_000,
            "{stated} {stale_stated}"
        );
    }
}
