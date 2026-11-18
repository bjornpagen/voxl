//! A session's lane as the media producers use it: frames in, and on the lane's socket only as
//! fast as the socket accepts them.
//!
//! The rate is the transport's: Hysteria2 under a datagram lane, TCP or QUIC under a stream
//! lane. A message leaves when the lane's socket takes it, waits in the lane's one queue until
//! then, and is never discarded. What the daemon reads of the rate is its own socket: the
//! drain, the bytes the socket accepted a second while it was full ([`Drain`]).
//! The capture is gated by a live lane's backlog (at most one update interval of the drain
//! waits), and the encoder aims at the drain times the frames' share of what the lane carries.
//! A lane whose socket breaks is lost: the relay is told ([`Out::Lost`]), its queue keeps what
//! it held for the lane that attaches next, and it gates nothing, since nothing waits on a
//! socket that will never take another message.
//!
//! - **Datagram lane**: frames go coded, in blocks ([`crate::lanes::code`]): one capture's
//!   update with the Opus packet waiting for it, or an Opus packet alone, with the repairs the
//!   link sizes ([`Link`]). The writer stamps each piece as the socket accepts it, and the
//!   block's deadline as it accepts its first piece. The relay reports every block, in order.
//! - **Stream lane**: frames go as they are, one framed message each, in order; the transport
//!   delivers every byte, so a frame sent is a frame delivered. No report, no deadline.
//!
//! Everything here is synchronous, under the session's lock in the hub; each lane has a writer
//! task (`lane.rs`) that empties the queue into the socket.

use crate::hub::Out;
use crate::lanes::code;
use crate::lanes::link::{Flight, Link};
use crate::lanes::path::RATE_MEMORY;
use crate::lanes::wire::{
    self, Capability, Fate, Generation, Lane, Number, Outcome, Report, Seq, Stamp, Track, DATAGRAM,
};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// What a viewer's way out lets a frame take.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Room {
    /// This many bytes, what may still wait for the socket within one update interval of its
    /// drain.
    Open(usize),
    /// Nothing: an update interval's drain already waits.
    Full,
    /// No bound: nothing waits for a live lane's socket (no lane is live, or its queue is
    /// empty), or a recording, which takes each frame as it is coded.
    Unbounded,
}

/// What reaches the page of what a viewer's way out is given.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Delivery {
    /// Everything sent arrives, in order: a stream lane, or a recording.
    Whole,
    /// Each frame is reported, decoded or missed: a datagram lane.
    Reported,
}

/// The rate the encoder aims at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Rate {
    /// Nothing waits for the socket: the lane carries all the encoder makes, so it codes at its
    /// quality target.
    Unlimited,
    /// The queue has backed up: the drain, bytes a second, times the frames' share of what the
    /// lane carries.
    Drain(f64),
}

/// A viewer's way out, as `voxl stats` says it.
#[derive(Debug, serde::Serialize)]
#[serde(tag = "way", rename_all = "snake_case")]
pub enum Way {
    /// A session no lane has attached to yet.
    Unattached,
    /// A datagram lane: its queue and socket, and the link's view (`lanes::link::Stats`).
    Datagram {
        backlog_bytes: usize,
        drain_bytes_per_s: Option<f64>,
        receive_buffer_bytes: u32,
        unreported_blocks: usize,
        missed: u64,
        expected_misses: f64,
        loss: f64,
        rtprop_ms: Option<f64>,
        spread_ms: f64,
        miss: f64,
        share: f64,
    },
    /// A stream lane: its queue and socket.
    Stream {
        backlog_bytes: usize,
        drain_bytes_per_s: Option<f64>,
    },
    Recording,
    /// A session whose control connection closed.
    Closed,
}

/// A viewer's way out: a session's lane, or a recording in this process.
pub struct Sender {
    state: State,
}

enum State {
    /// Its control connection is open: the capability its lane attaches with, and its lane.
    Open(Box<Open>),
    /// Its control connection closed: the capability is forgotten, and no lane is sent
    /// another byte.
    Closed,
    /// A recording (`voxl record`): each update goes to it whole, its header and its
    /// H.264, on its channel. It has no capability and no lane, and takes no report.
    Recording(tokio::sync::mpsc::UnboundedSender<Out>),
}

struct Open {
    capability: Capability,
    /// Its control stream's writer: the buffer the lane's socket was given is told on it.
    control: tokio::sync::mpsc::UnboundedSender<Out>,
    /// The daemon's clock for this session's stamps.
    origin: Instant,
    /// When each video update started, for the update interval the backlog is held to.
    updates: Updates,
    /// The next frame's number among a stream lane's frames.
    frame: Seq,
    lane: Attached,
    /// The receive buffer the relay was last told to give its lane socket.
    buffer: u32,
}

/// The lane attached, if any. It is already on the heap with the rest of `Open`.
#[allow(clippy::large_enum_variant)]
enum Attached {
    /// None yet: frames are sent nowhere, and the session is not looked at for them.
    None,
    Datagram(Datagram),
    Stream(Stream),
}

/// A datagram lane: its generation and queue, the code and the link, the capture gathering
/// into its block, and the blocks sent and not yet reported.
struct Datagram {
    generation: Generation,
    queue: Arc<Queue>,
    encoder: code::Encoder,
    link: Link,
    capture: Capture,
    flights: VecDeque<Flight>,
    /// The blocks missed, and the sum of the chances their plans gave them of a miss, over the
    /// session: the model checked against what happened.
    missed: u64,
    expected: f64,
}

/// A stream lane: its generation and queue.
struct Stream {
    generation: Generation,
    queue: Arc<Queue>,
}

/// A capture's update as it goes into its block, and the Opus packets waiting for a capture,
/// oldest first. A packet waits for the next capture until the next packet, its own play time,
/// and then goes, with every other waiting; while anything waits for the socket, packets wait
/// on for the next block, which takes them all, so that a backlog never fills with blocks of
/// one packet each (never more than a block's [`MEMBERS`] in all).
#[derive(Default)]
struct Capture {
    members: Vec<(Track, bytes::Bytes)>,
    audio: Vec<Vec<u8>>,
    /// Packets' buffers that left, for the next ones.
    spare_audio: Vec<Vec<u8>>,
    /// Track lists of blocks reported, to be used again: a block's are kept in its flight.
    spare: Vec<Vec<Track>>,
}

/// The most frames a block holds: its head counts them in a byte.
const MEMBERS: usize = u8::MAX as usize;

/// Which transport a lane is, as its socket says: the datagram socket or a stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Datagram,
    Stream,
}

/// What wakes the capture loop: called where a lane's backlog empties, or falls under the
/// mark a capture has room under ([`Waiting::sent`]), the one place it falls.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

/// One lane's messages waiting for its socket, written by its writer as the socket takes
/// them. Its writer writes only under the queue's lock and only while it is open, so once the
/// session closes it (`close`), not one byte more leaves on the lane.
pub struct Queue {
    kind: Kind,
    state: Mutex<Waiting>,
    wake: tokio::sync::Notify,
}

pub(super) struct Waiting {
    pub(super) phase: Phase,
    /// Who to tell, once, that the lane broke: its session's control stream and its
    /// generation, given when it attaches.
    tell: Option<Tell>,
    pub(super) messages: VecDeque<Message>,
    /// Bytes of the front message the socket has taken (a stream lane's, part by part).
    pub(super) written: usize,
    /// The bytes waiting.
    pub(super) backlog: usize,
    pub(super) drain: Drain,
    /// The session's clock, for the writer's stamps.
    pub(super) origin: Instant,
    /// What a datagram block's deadline waits past the socket's acceptance, in tenths
    /// ([`Link::after`]), and the last block whose deadline is stamped, with it: the next
    /// block's is never before it.
    pub(super) after: i64,
    pub(super) due: Option<(Seq, Stamp)>,
    /// The send buffer the socket is to have, and the one it was given.
    pub(super) buffer: Option<u32>,
    pub(super) given: Option<u32>,
    /// The backlog a capture has room under, as [`Sender::room`] last found it, and what to
    /// wake when the backlog falls under it or empties.
    room_at: usize,
    wake: Wake,
    /// Buffers of messages that left, for the next ones: a steady stream allocates none.
    #[allow(clippy::vec_box)]
    spare_datagrams: Vec<Box<[u8; DATAGRAM]>>,
    spare_frames: Vec<Vec<u8>>,
}

/// Where a lane's loss is told: its session's control stream, and the lane's generation.
struct Tell {
    control: tokio::sync::mpsc::UnboundedSender<Out>,
    generation: Generation,
}

/// A message waiting: when it was queued, and its bytes.
pub struct Message {
    pub(super) queued: Instant,
    pub(super) body: Body,
}

/// A message's bytes: a datagram lane's piece or repair of `block`, or a stream lane's framed
/// frame.
pub(super) enum Body {
    Datagram {
        block: Seq,
        len: usize,
        buf: Box<[u8; DATAGRAM]>,
    },
    Frame(Vec<u8>),
}

impl Message {
    pub(super) fn len(&self) -> usize {
        match &self.body {
            Body::Datagram { len, .. } => *len,
            Body::Frame(f) => f.len(),
        }
    }

    /// Its bytes from `written` on.
    pub(super) fn from(&self, written: usize) -> &[u8] {
        match &self.body {
            Body::Datagram { len, buf, .. } => &buf[written..*len],
            Body::Frame(f) => &f[written..],
        }
    }
}

/// The socket's drain: the bytes it accepted a second, over the last [`RATE_MEMORY`] messages
/// it refused before it took them, each over the time from the later of the acceptance before
/// it and its queueing. Only a refused message waited on the transport: one the socket took at
/// once found room in the transport's own queue (a burst after a quiet spell fills it at no
/// cost), which tells nothing of the rate it drains at, and is not counted.
pub(super) struct Drain {
    samples: VecDeque<(usize, Duration)>,
    bytes: usize,
    time: Duration,
    last: Option<Instant>,
    /// The socket refused the front message.
    refused: bool,
}

impl Drain {
    pub(super) fn new() -> Drain {
        Drain {
            samples: VecDeque::with_capacity(RATE_MEMORY + 1),
            bytes: 0,
            time: Duration::ZERO,
            last: None,
            refused: false,
        }
    }

    /// The socket refused the front message: it is full.
    pub(super) fn refused(&mut self) {
        self.refused = true;
    }

    /// The socket accepted the front message, of `len` bytes, queued at `queued`, at `now`.
    pub(super) fn accepted(&mut self, len: usize, queued: Instant, now: Instant) {
        if std::mem::take(&mut self.refused) {
            let from = self.last.map_or(queued, |last| last.max(queued));
            let gap = now.saturating_duration_since(from);
            self.samples.push_back((len, gap));
            self.bytes += len;
            self.time += gap;
            if self.samples.len() > RATE_MEMORY {
                if let Some((b, t)) = self.samples.pop_front() {
                    self.bytes -= b;
                    self.time -= t;
                }
            }
        }
        self.last = Some(now);
    }

    /// Bytes a second, once a message has waited.
    pub(super) fn rate(&self) -> Option<f64> {
        (!self.time.is_zero()).then_some(self.bytes as f64 / self.time.as_secs_f64())
    }
}

impl Queue {
    pub(crate) fn new(kind: Kind, origin: Instant, wake: Wake) -> Arc<Queue> {
        Arc::new(Queue {
            kind,
            state: Mutex::new(Waiting {
                phase: Phase::Open,
                tell: None,
                messages: VecDeque::new(),
                written: 0,
                backlog: 0,
                drain: Drain::new(),
                origin,
                after: 0,
                due: None,
                buffer: Some(SEND_BUFFER),
                given: None,
                room_at: 0,
                wake,
                spare_datagrams: Vec::new(),
                spare_frames: Vec::new(),
            }),
            wake: tokio::sync::Notify::new(),
        })
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// Its state, locked: its writer writes under this lock.
    pub(super) fn lock(&self) -> parking_lot::MutexGuard<'_, Waiting> {
        self.state.lock()
    }

    /// Wakes its writer: a message came, or it closed.
    pub(super) async fn woken(&self) {
        self.wake.notified().await
    }

    fn wake(&self) {
        self.wake.notify_one();
    }

    /// Closes it: nothing more leaves, and its writer ends. What waits stays, for a lane that
    /// replaces it ([`Queue::take_over`]).
    pub(super) fn close(&self) {
        self.state.lock().phase = Phase::Closed;
        self.wake();
    }

    /// Takes the messages `old` holds and has not begun to write (a stream lane's message half
    /// written goes again whole, on the new stream).
    fn take_over(&self, old: &Queue) {
        let mut from = old.state.lock();
        let mut to = self.state.lock();
        from.written = 0;
        to.backlog += from.backlog;
        from.backlog = 0;
        to.messages.append(&mut from.messages);
        // The last deadline stamped goes with the messages: the rest of a block half sent
        // keeps its head, and the next block's deadline is not before it.
        to.due = from.due.take();
        drop((from, to));
        self.wake();
    }

    /// Queues a datagram of `block` that `encode` writes; a closed lane takes nothing, a lost
    /// one keeps it for the next.
    fn push_datagram(
        &self,
        block: Seq,
        now: Instant,
        encode: impl FnOnce(&mut [u8; DATAGRAM]) -> usize,
    ) {
        let mut w = self.state.lock();
        if w.phase == Phase::Closed {
            return;
        }
        let mut buf = w
            .spare_datagrams
            .pop()
            .unwrap_or_else(|| Box::new([0; DATAGRAM]));
        let len = encode(&mut buf);
        w.backlog += len;
        w.messages.push_back(Message {
            queued: now,
            body: Body::Datagram { block, len, buf },
        });
        drop(w);
        self.wake();
    }

    /// Queues a stream lane's frame message; a closed lane takes nothing, a lost one keeps it
    /// for the next.
    fn push_frame(&self, frame: Seq, track: Track, bytes: &[u8], now: Instant) {
        let mut w = self.state.lock();
        if w.phase == Phase::Closed {
            return;
        }
        let mut buf = w.spare_frames.pop().unwrap_or_default();
        buf.clear();
        Lane::encode_frame(frame, track, bytes, &mut buf);
        w.backlog += buf.len();
        w.messages.push_back(Message {
            queued: now,
            body: Body::Frame(buf),
        });
        drop(w);
        self.wake();
    }

    /// What waits, and the drain, whether the lane is live or not.
    fn held(&self) -> (usize, Option<f64>) {
        let w = self.state.lock();
        (w.backlog, w.drain.rate())
    }

    /// What waits for a live lane's socket, and its drain: none for a lane lost or closed,
    /// whose socket takes nothing more, so that what it holds gates nothing.
    fn live(&self) -> Option<(usize, Option<f64>)> {
        let w = self.state.lock();
        (w.phase == Phase::Open).then(|| (w.backlog, w.drain.rate()))
    }
}

/// Whether a lane takes messages: open until its session closes or a newer lane replaces it
/// (closed), or its socket breaks (lost: it keeps what it holds, and what comes, for the lane
/// that attaches next).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Phase {
    Open,
    Lost,
    Closed,
}

impl Waiting {
    /// The front message left, all of it, at `now`: its buffer serves the next. The backlog
    /// falls here and nowhere else, so here the capture loop is woken when it empties or falls
    /// under the mark a capture has room under.
    pub(super) fn sent(&mut self, now: Instant) {
        if let Some(m) = self.messages.pop_front() {
            let len = m.len();
            let before = self.backlog;
            self.backlog -= len;
            if self.backlog == 0 || (before >= self.room_at && self.backlog < self.room_at) {
                (self.wake)();
            }
            self.written = 0;
            self.drain.accepted(len, m.queued, now);
            match m.body {
                Body::Datagram { buf, .. } => self.spare_datagrams.push(buf),
                Body::Frame(f) => self.spare_frames.push(f),
            }
        }
    }

    /// The socket refused the front message, or the rest of it: it is full.
    pub(super) fn refused(&mut self) {
        self.drain.refused();
    }

    /// Stamps the front message, a datagram, as the socket is about to take it at `now`: when
    /// it left, and its block's deadline, stamped as the socket takes the block's first piece:
    /// that moment, the drain of the block's pieces behind it (if any wait), and what a
    /// deadline waits past it ([`Link::after`]); and never before the deadline of the block
    /// before it. The relay decides a block nothing of which came by a later block's deadline
    /// passing, which says the earlier one's passed only while deadlines follow the blocks'
    /// order; a wait that shrinks between two blocks would break it.
    pub(super) fn stamp(&mut self, now: Instant) {
        let sent = Stamp::since(self.origin, now);
        let Some(Message {
            body: Body::Datagram { block, .. },
            ..
        }) = self.messages.front()
        else {
            return;
        };
        let block = *block;
        let due = match self.due {
            Some((b, due)) if b == block => due,
            _ => {
                let behind: usize = self
                    .messages
                    .iter()
                    .skip(1)
                    .take_while(|m| matches!(m.body, Body::Datagram { block: b, .. } if b == block))
                    .map(Message::len)
                    .sum();
                let drain = match (behind, self.drain.rate()) {
                    (0, _) | (_, None) => 0,
                    (behind, Some(rate)) => {
                        wire::tenths(Duration::from_secs_f64(behind as f64 / rate))
                    }
                };
                let due = sent.plus(drain + self.after);
                let due = match self.due {
                    Some((_, before)) if due.to(before) > 0 => before,
                    _ => due,
                };
                self.due = Some((block, due));
                due
            }
        };
        if let Some(Message {
            body: Body::Datagram { len, buf, .. },
            ..
        }) = self.messages.front_mut()
        {
            wire::stamp_datagram(&mut buf[..*len], due, sent);
        }
    }

    /// The lane's socket broke for good: it sends nothing more, the relay is told once, and
    /// the capture loop is woken, since the lane gates nothing now. What it holds stays for
    /// the lane that attaches next ([`Queue::take_over`]). A lane already closed is left so.
    pub(super) fn lost(&mut self) {
        if self.phase != Phase::Open {
            return;
        }
        self.phase = Phase::Lost;
        if let Some(Tell {
            control,
            generation,
        }) = self.tell.take()
        {
            // A session whose control writer has ended is closing: there is no one to tell.
            if control.send(Out::Lost { generation }).is_err() {
                log::debug!("a lost lane not told: the session is closing");
            }
        }
        (self.wake)();
    }
}

/// Why a report ends its session: the relay said what its decoder cannot have.
#[derive(Debug, PartialEq)]
pub enum Broken {
    /// No block was sent that it could be of: none is unreported, or the next unreported
    /// is another, or the lane is a stream lane, which has no reports.
    Unsent(Seq),
    /// The link refused it for its block.
    Mismatch(crate::lanes::link::Mismatch),
}

/// A video update the relay decided, and how.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Decided {
    pub number: Number,
    pub outcome: Outcome,
}

/// The video updates' interval, from the times they started: [`RATE_MEMORY`] gaps.
struct Updates {
    starts: VecDeque<Instant>,
}

impl Updates {
    fn new() -> Updates {
        Updates {
            starts: VecDeque::with_capacity(RATE_MEMORY + 1),
        }
    }

    fn push(&mut self, at: Instant) {
        if self.starts.len() == RATE_MEMORY + 1 {
            self.starts.pop_front();
        }
        self.starts.push_back(at);
    }

    /// The mean gap, once there is one.
    fn interval(&self) -> Option<Duration> {
        match (self.starts.front(), self.starts.back()) {
            (Some(&first), Some(&last)) if last > first => {
                Some((last - first) / (self.starts.len() - 1) as u32)
            }
            _ => None,
        }
    }
}

impl Sender {
    /// A session's, with the capability its lane attaches with, its control stream's writer,
    /// and its clock read from `origin`. Its first word on the control stream, after the
    /// capability, is the receive buffer the relay is to give its lane socket before it opens
    /// one: what the kernel gives a fresh lane socket here (`SO_SNDBUF`, `net.core.wmem_default`
    /// on Linux), until a drain and a round trip size it. Without a socket to read it from, the
    /// session can have no lane: that error.
    pub fn new(
        capability: Capability,
        control: tokio::sync::mpsc::UnboundedSender<Out>,
        origin: Instant,
    ) -> std::io::Result<Sender> {
        let fresh = std::os::unix::net::UnixDatagram::unbound()?;
        let given = nix::sys::socket::getsockopt(&fresh, nix::sys::socket::sockopt::SndBuf)
            .map_err(std::io::Error::from)?;
        let buffer = u32::try_from(given).unwrap_or(u32::MAX);
        // A session whose control writer has ended is closing, its lane with it.
        if control.send(Out::Buffer(buffer)).is_err() {
            log::debug!("the lane's receive buffer not told: the session is closing");
        }
        Ok(Sender {
            state: State::Open(Box::new(Open {
                capability,
                control,
                origin,
                updates: Updates::new(),
                frame: Seq::first(),
                lane: Attached::None,
                buffer,
            })),
        })
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
            State::Open(o) => o.capability.matches(presented),
            State::Closed | State::Recording(_) => false,
        }
    }

    /// Its control connection closed: the capability is dropped (and zeroed), and its lane is
    /// closed before this returns.
    pub fn close(&mut self) {
        if let State::Open(o) = std::mem::replace(&mut self.state, State::Closed) {
            if let Some(q) = o.lane.queue() {
                q.close();
            }
        }
    }

    /// Whether it has a way to be sent frames on: a live lane, or a recording.
    pub fn attached(&self) -> bool {
        match &self.state {
            State::Open(o) => o.lane.queue().and_then(|q| q.live()).is_some(),
            State::Recording(_) => true,
            State::Closed => false,
        }
    }

    /// A lane of `generation` attached, its queue `queue` (whose kind is the lane's), the
    /// attach stamped `sent` by the relay with its measured `round_trip`, arriving at `now`.
    /// A newer generation replaces the lane before, which is closed and whose waiting messages
    /// the new one sends; an older or the same, or a lane of a closed session, is refused.
    /// Whether it was taken.
    pub fn attach(
        &mut self,
        generation: Generation,
        queue: Arc<Queue>,
        sent: Stamp,
        round_trip: Duration,
        now: Instant,
    ) -> bool {
        let State::Open(o) = &mut self.state else {
            return false;
        };
        {
            let mut w = queue.state.lock();
            w.origin = o.origin;
            w.tell = Some(Tell {
                control: o.control.clone(),
                generation,
            });
        }
        if let Some((old, g)) = o.lane.queue().zip(o.lane.generation()) {
            if generation <= g || queue.kind() != old.kind() {
                return false;
            }
            old.close();
            queue.take_over(old);
        }
        match (&mut o.lane, queue.kind()) {
            (Attached::Datagram(d), Kind::Datagram) => {
                queue.state.lock().after = d.link.after();
                d.generation = generation;
                d.queue = queue;
            }
            (Attached::Stream(s), Kind::Stream) => {
                s.generation = generation;
                s.queue = queue;
            }
            (_, Kind::Datagram) => {
                let link = Link::new(sent, Stamp::since(o.origin, now), round_trip);
                queue.state.lock().after = link.after();
                o.lane = Attached::Datagram(Datagram {
                    generation,
                    queue,
                    encoder: code::Encoder::new(),
                    link,
                    capture: Capture::default(),
                    flights: VecDeque::new(),
                    missed: 0,
                    expected: 0.0,
                });
            }
            (_, Kind::Stream) => {
                o.lane = Attached::Stream(Stream { generation, queue });
            }
        }
        true
    }

    /// What a frame may take now: with no live lane, or nothing waiting for its socket, any;
    /// else what one update interval of the drain leaves past what waits. Only a live lane's
    /// backlog gates: a lost lane's waits on no socket.
    pub fn room(&self) -> Room {
        let o = match &self.state {
            State::Open(o) => o,
            State::Recording(_) => return Room::Unbounded,
            State::Closed => return Room::Full,
        };
        let Some((queue, live)) = o.lane.queue().and_then(|q| Some((q, q.live()?))) else {
            return Room::Unbounded;
        };
        match live {
            (0, _) => Room::Unbounded,
            (backlog, drain) => {
                let budget = drain
                    .zip(o.updates.interval())
                    .map_or(0, |(rate, gap)| (rate * gap.as_secs_f64()) as usize);
                queue.state.lock().room_at = budget;
                match budget.saturating_sub(backlog) {
                    0 => Room::Full,
                    free => Room::Open(free),
                }
            }
        }
    }

    /// What reaches the page of what it is given; `None` with no lane, or closed.
    pub fn delivery(&self) -> Option<Delivery> {
        match &self.state {
            State::Recording(_) => Some(Delivery::Whole),
            State::Closed => None,
            State::Open(o) => match o.lane {
                Attached::None => None,
                Attached::Stream(_) => Some(Delivery::Whole),
                Attached::Datagram(_) => Some(Delivery::Reported),
            },
        }
    }

    /// The rate the encoder aims at: unlimited while nothing waits for the socket; once the
    /// queue has backed up, the drain times the frames' share (a stream lane's frames are all
    /// it carries).
    pub fn rate(&self) -> Rate {
        let State::Open(o) = &self.state else {
            return Rate::Unlimited;
        };
        let Some(live) = o.lane.queue().and_then(|q| q.live()) else {
            return Rate::Unlimited;
        };
        let share = match &o.lane {
            Attached::Datagram(d) => d.link.share(),
            Attached::Stream(_) | Attached::None => 1.0,
        };
        match live {
            (0, _) | (_, None) => Rate::Unlimited,
            (_, Some(drain)) => Rate::Drain(drain * share),
        }
    }

    /// What `voxl stats` says of it.
    pub fn stats(&self) -> Way {
        let o = match &self.state {
            State::Open(o) => o,
            State::Recording(_) => return Way::Recording,
            State::Closed => return Way::Closed,
        };
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        match &o.lane {
            Attached::None => Way::Unattached,
            Attached::Datagram(d) => {
                let (backlog, drain) = d.queue.held();
                let st = d.link.stats();
                Way::Datagram {
                    backlog_bytes: backlog,
                    drain_bytes_per_s: drain,
                    receive_buffer_bytes: o.buffer,
                    unreported_blocks: d.flights.len(),
                    missed: d.missed,
                    expected_misses: d.expected,
                    loss: st.loss,
                    rtprop_ms: st.rtprop.map(ms),
                    spread_ms: ms(st.spread),
                    miss: st.miss,
                    share: st.share,
                }
            }
            Attached::Stream(s) => {
                let (backlog, drain) = s.queue.held();
                Way::Stream {
                    backlog_bytes: backlog,
                    drain_bytes_per_s: drain,
                }
            }
        }
    }

    /// Video update `number`, `bytes` long, at `now`: on a datagram lane in a block with the
    /// Opus packet waiting for it, on a stream lane as a frame of its own; a recording takes
    /// the bytes as they are. A recording whose recorder has gone takes nothing: its viewer is
    /// removed with it.
    pub fn video(&mut self, number: Number, bytes: &bytes::Bytes, now: Instant) {
        let o = match &mut self.state {
            State::Recording(to) => {
                if to.send(Out::Bin(bytes.clone())).is_err() {
                    log::debug!("a recording ended before its viewer left");
                }
                return;
            }
            State::Closed => return,
            State::Open(o) => o,
        };
        o.updates.push(now);
        let track = Track::Video { number };
        match &mut o.lane {
            Attached::None => {}
            Attached::Stream(s) => {
                s.queue.push_frame(o.frame, track, bytes, now);
                o.frame = o.frame.next();
            }
            Attached::Datagram(d) => {
                let mut members = std::mem::take(&mut d.capture.members);
                members.push((track, bytes.clone()));
                let mut audio = std::mem::take(&mut d.capture.audio);
                let all = members
                    .iter()
                    .map(|(t, b)| (*t, &b[..]))
                    .chain(audio.iter().map(|a| (Track::Audio, &a[..])));
                send(d, o.origin, all, now);
                members.clear();
                d.capture.members = members;
                d.capture.spare_audio.append(&mut audio);
                d.capture.audio = audio;
            }
        }
    }

    /// An Opus packet at `now`, `silent` frames of silence after the one before: on a datagram
    /// lane it waits for the next capture ([`Capture`]); on a stream lane it goes at once, a
    /// frame of its own; a recording takes it as it is, with the silence it follows.
    pub fn audio(&mut self, packet: &[u8], silent: u32, now: Instant) {
        let o = match &mut self.state {
            State::Recording(to) => {
                let packet = bytes::Bytes::copy_from_slice(packet);
                if to.send(Out::Audio { packet, silent }).is_err() {
                    log::debug!("a recording ended before its viewer left");
                }
                return;
            }
            State::Closed => return,
            State::Open(o) => o,
        };
        match &mut o.lane {
            Attached::None => {}
            Attached::Stream(s) => {
                s.queue.push_frame(o.frame, Track::Audio, packet, now);
                o.frame = o.frame.next();
            }
            Attached::Datagram(d) => {
                let mut buf = d.capture.spare_audio.pop().unwrap_or_default();
                buf.clear();
                buf.extend_from_slice(packet);
                d.capture.audio.push(buf);
                let waits = d.queue.held().0 > 0;
                let n = d.capture.audio.len();
                // The packets before this one, whose play time passed with no capture, go
                // together, unless the socket has a backlog and there is room in a block.
                if n > 1 && (!waits || n >= MEMBERS) {
                    let newest = d.capture.audio.pop();
                    let mut audio = std::mem::take(&mut d.capture.audio);
                    send(
                        d,
                        o.origin,
                        audio.iter().map(|a| (Track::Audio, &a[..])),
                        now,
                    );
                    d.capture.spare_audio.append(&mut audio);
                    d.capture.audio = audio;
                    d.capture.audio.extend(newest);
                }
            }
        }
    }

    /// A datagram lane's report that came up the control stream at `now`: the link learns from
    /// it, the deadline it gives the next blocks and the buffer its socket is given follow,
    /// and each video update of its block is returned, decided as the block was. Reports come
    /// in the order of their blocks; any other, or one for a stream lane, is broken.
    pub fn reported(&mut self, report: &Report<'_>, now: Instant) -> Result<Vec<Decided>, Broken> {
        let o = match &mut self.state {
            State::Open(o) => o,
            State::Closed => return Ok(Vec::new()),
            State::Recording(_) => return Err(Broken::Unsent(report.block)),
        };
        let Attached::Datagram(d) = &mut o.lane else {
            return Err(Broken::Unsent(report.block));
        };
        let flight = match d.flights.pop_front() {
            Some(f) if f.head.block == report.block => f,
            Some(f) => {
                d.flights.push_front(f);
                return Err(Broken::Unsent(report.block));
            }
            None => return Err(Broken::Unsent(report.block)),
        };
        d.link
            .reported(&flight, report, Stamp::since(o.origin, now))
            .map_err(Broken::Mismatch)?;
        d.queue.state.lock().after = d.link.after();
        o.buffer = received(&o.control, &d.queue, o.buffer, d.link.stats().rtprop);
        let outcome = match report.fate {
            Fate::Unseen => Outcome::Missed,
            Fate::Seen { outcome, .. } => outcome,
        };
        d.missed += u64::from(outcome == Outcome::Missed);
        d.expected += flight.miss;
        let decided = flight
            .tracks
            .iter()
            .filter_map(|t| match *t {
                Track::Video { number } => Some(Decided { number, outcome }),
                Track::Audio => None,
            })
            .collect();
        d.capture.spare.push(flight.tracks);
        Ok(decided)
    }
}

impl Attached {
    fn queue(&self) -> Option<&Arc<Queue>> {
        match self {
            Attached::None => None,
            Attached::Datagram(d) => Some(&d.queue),
            Attached::Stream(s) => Some(&s.queue),
        }
    }

    fn generation(&self) -> Option<Generation> {
        match self {
            Attached::None => None,
            Attached::Datagram(d) => Some(d.generation),
            Attached::Stream(s) => Some(s.generation),
        }
    }
}

/// The send buffer the daemon gives a lane's socket: the least, one lane datagram ([`DATAGRAM`],
/// the largest message a datagram lane sends; the kernel floors it at its own least). Not to be
/// tuned. The socket is the daemon's to the tunnel on this machine: what it holds past
/// acceptance waits where no deadline sees it, and is measured only as it widens the spread of
/// arrivals. A buffer sized from that spread (one deadline's worth at the drain) is a
/// measurement feeding on itself: its own wait widens the spread that sizes it, and in
/// simulation it ran away, doubling with each power-of-two step until the base round trip read
/// 30 s and every block came late. With the least, every backlog is in the one queue the daemon
/// sees ([`Queue`]), and what feeds the path is the tunnel's own queue (quic-go's 32 DATAGRAM
/// frames, a stream's window).
pub const SEND_BUFFER: u32 = DATAGRAM as u32;

/// The receive buffer the relay is told to give its datagram lane's socket: what a round trip
/// of the drain holds, the most the tunnel's window delivers to the relay at once, rounded up
/// to a power of two so that it changes only when it doubles or halves; told when it changes,
/// and kept as it `was` until there is a drain and a round trip to size it by.
fn received(
    control: &tokio::sync::mpsc::UnboundedSender<Out>,
    queue: &Queue,
    was: u32,
    round_trip: Option<Duration>,
) -> u32 {
    let (_, drain) = queue.held();
    let Some((rate, wait)) = drain.zip(round_trip) else {
        return was;
    };
    let bytes = ((rate * wait.as_secs_f64()) as usize)
        .max(DATAGRAM)
        .checked_next_power_of_two()
        .map_or(u32::MAX, |b| u32::try_from(b).unwrap_or(u32::MAX));
    if bytes != was && control.send(Out::Buffer(bytes)).is_err() {
        // A session whose control writer has ended is closing, its lane with it.
        log::debug!("the lane's receive buffer not told: the session is closing");
    }
    bytes
}

/// A block of `members` on a datagram lane: its repairs planned by the link, coded, and its
/// messages queued, to be stamped and sent as the socket takes them. Its track list is taken
/// from the spare ones.
fn send<'b>(
    d: &mut Datagram,
    origin: Instant,
    members: impl Iterator<Item = (Track, &'b [u8])> + Clone,
    now: Instant,
) {
    let mut tracks = d.capture.spare.pop().unwrap_or_default();
    tracks.clear();
    tracks.extend(members.clone().map(|(t, _)| t));
    let Ok(assembled) = d.encoder.assemble(members) else {
        // A block the code cannot take (empty, or over its window) is not sent; the
        // encoder's cap keeps frames far under it.
        log::warn!(
            "a block of {} frames is malformed for the lanes",
            tracks.len()
        );
        return;
    };
    let plan = d
        .link
        .plan(assembled.bytes(), &tracks, Stamp::since(origin, now));
    // The deadline and the time each message left are stamped as the socket takes it
    // (`Waiting::stamp`): coded with none yet.
    let Ok(coded) = assembled.code(plan.m, Stamp::since(origin, now)) else {
        log::warn!("a block is too large for the lanes with its repairs");
        return;
    };
    let head = *coded.head();
    for i in 0..head.n() {
        d.queue.push_datagram(head.block, now, |out| {
            coded.encode(i, Stamp::since(origin, now), out)
        });
    }
    d.flights.push_back(Flight {
        head,
        tracks,
        miss: plan.miss,
    });
}

#[cfg(test)]
impl Queue {
    /// The front message as the lane's writer gives it to its socket at `now` (stamped, on a
    /// datagram lane), taken out: its bytes, and the block it is of, if a datagram.
    pub(crate) fn take(&self, now: Instant) -> Option<(Vec<u8>, Option<Seq>)> {
        let mut w = self.lock();
        w.stamp(now);
        let m = w.messages.front()?;
        let taken = (
            m.from(0).to_vec(),
            match m.body {
                Body::Datagram { block, .. } => Some(block),
                Body::Frame(_) => None,
            },
        );
        w.sent(now);
        Some(taken)
    }

    /// The socket refused the front message.
    pub(crate) fn refuse(&self) {
        self.lock().refused();
    }

    /// The send buffer the sender asks the socket to have.
    pub(crate) fn buffer(&self) -> Option<u32> {
        self.lock().buffer
    }

    /// The bytes waiting.
    pub(crate) fn backlog_bytes(&self) -> usize {
        self.lock().backlog
    }

    /// Its socket broke, as a writer finds it.
    pub(crate) fn lose(&self) {
        self.lock().lost();
    }
}

#[cfg(test)]
impl Sender {
    /// The tracks of `block`, a datagram lane's block not yet reported.
    pub(crate) fn tracks_of(&self, block: Seq) -> Option<Vec<Track>> {
        let State::Open(o) = &self.state else {
            return None;
        };
        let Attached::Datagram(d) = &o.lane else {
            return None;
        };
        d.flights
            .iter()
            .find(|f| f.head.block == block)
            .map(|f| f.tracks.clone())
    }
}

#[cfg(test)]
impl Sender {
    /// A session's sender with a datagram lane attached at `now`, the lane's queue (which no
    /// writer drains: a test reads it), and what its control stream is sent.
    pub fn with_lane(
        now: Instant,
    ) -> (
        Sender,
        Arc<Queue>,
        tokio::sync::mpsc::UnboundedReceiver<Out>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut s = Sender::new(Capability::mint().expect("a random source"), tx, now)
            .expect("a socket to read the kernel's buffer from");
        let queue = Queue::new(Kind::Datagram, now, Arc::new(|| {}));
        assert!(s.attach(
            Generation::first(),
            queue.clone(),
            Stamp::since(now, now),
            Duration::from_millis(100),
            now
        ));
        (s, queue, rx)
    }
}

/// A block as a test's relay has it from a lane: its head, its members (each frame's
/// track and bytes), and when its first piece left.
#[cfg(test)]
pub struct Received {
    pub head: wire::Head,
    pub members: Vec<(Track, Vec<u8>)>,
    pub sent: Stamp,
}

#[cfg(test)]
impl Queue {
    /// The blocks whose every source is queued, in the order they were numbered, each as
    /// its sources put it back together; the queue is emptied, as if the socket took all.
    pub fn frames(&self) -> Vec<Received> {
        let mut w = self.lock();
        type Sources = Vec<Option<Vec<u8>>>;
        let mut blocks: std::collections::BTreeMap<u64, (wire::Head, Stamp, Sources)> =
            Default::default();
        let now = Instant::now();
        while !w.messages.is_empty() {
            w.stamp(now);
            if let Some(m) = w.messages.front() {
                if let Ok(Lane::Piece(s)) = Lane::decode(m.from(0)) {
                    let (_, _, sources) = blocks
                        .entry(s.head.block.get())
                        .or_insert_with(|| (s.head, s.sent, vec![None; s.head.k()]));
                    sources[s.index as usize] = Some(s.data.to_vec());
                }
            }
            w.sent(now);
        }
        blocks
            .into_values()
            .filter_map(|(head, sent, sources)| {
                let bytes: Vec<u8> = sources.into_iter().collect::<Option<Vec<_>>>()?.concat();
                let members = head
                    .members(&bytes)
                    .expect("the encoder's table")
                    .map(|m| (m.track, m.bytes.to_vec()))
                    .collect();
                Some(Received {
                    head,
                    members,
                    sent,
                })
            })
            .collect()
    }
}

#[cfg(test)]
impl Received {
    /// What `with` makes of the report a relay makes of this block: `outcome` with every
    /// symbol arrived, or `None`, nothing of it seen.
    pub fn report<R>(&self, outcome: Option<Outcome>, with: impl FnOnce(&Report<'_>) -> R) -> R {
        let n = self.head.n();
        let bits: Vec<u8> = (0..n.div_ceil(8))
            .map(|b| match (b + 1) * 8 <= n {
                true => 0xff,
                false => (1u8 << (n % 8)) - 1,
            })
            .collect();
        let delays = vec![0u8; 4 * n];
        let fate = match outcome {
            Some(outcome) => Fate::Seen {
                outcome,
                arrivals: wire::Arrivals::new(n as u16, &bits, &delays).expect("arrivals"),
            },
            None => Fate::Unseen,
        };
        with(&Report {
            block: self.head.block,
            sent: self.sent,
            fate,
            late: wire::Late::new(&[], self.head.block).expect("no late"),
        })
    }
}
