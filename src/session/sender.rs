//! A session's lanes as the media producers use them: frames in, each cut by the code into
//! pieces and repairs, dealt over the lanes and paced at the budget (`lanes::link`), and
//! the reports that come back for them.
//!
//! Everything here is synchronous, under the session's lock in the hub: a frame is planned,
//! coded and queued on its lanes in one step, so the room the capture loop reads next
//! already counts it. Each lane has a writer task that releases its queue at the pace the
//! plan set; with nothing queued it waits on nothing but the queue, so a still desktop
//! sends nothing and wakes nothing.

use crate::hub::Out;
use crate::lanes::code;
use crate::lanes::link::{Flight, Link};
use crate::lanes::model::Lanes;
use crate::lanes::wire::{
    self, Capability, Epoch, Fate, Generation, LaneIndex, Number, Outcome, Report, Seq, Stamp,
    Track, DATAGRAM,
};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// What a viewer's way out lets a frame take.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Room {
    /// This many bytes, what its lanes may still hold unreported.
    Open(usize),
    /// Nothing: its lanes hold all they may until reports come back.
    Full,
    /// No bound of its own: a recording, which takes each frame as it is coded.
    Unbounded,
}

/// What a keyframe may take of a viewer's way out.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum KeyRoom {
    Bytes(usize),
    /// No bound of its own: a recording.
    Unbounded,
}

/// A viewer's way out, as `voxl stats` says it.
#[derive(Debug, serde::Serialize)]
#[serde(tag = "way", rename_all = "snake_case")]
pub enum Way {
    /// A session no lane has attached to yet.
    Unattached,
    /// A session's lanes: what each lane holds unreported and the most it may, the blocks
    /// not yet reported, and the link's view (`lanes::link::Stats`): the long-run loss of
    /// its erasure runs, the bottleneck rate and the round trip it measured, the spread of
    /// arrivals, the time a miss takes to mend, the pacing rate and its share for the
    /// frames themselves, the share of time the picture is expected stale, and the lanes it
    /// wants.
    Lanes {
        held_bytes: Vec<usize>,
        lane_bound_bytes: usize,
        unreported_blocks: usize,
        loss: f64,
        btlbw_bytes_per_s: f64,
        rtprop_ms: Option<f64>,
        spread_ms: f64,
        recovery_ms: f64,
        pacing_bytes_per_s: f64,
        data_bytes_per_s: f64,
        stale: f64,
        wanted_lanes: usize,
    },
    Recording,
    /// A session whose control connection closed.
    Closed,
}

/// A viewer's way out: a session's lanes, or a recording in this process.
pub struct Sender {
    state: State,
}

enum State {
    /// Its control connection is open: the capability lanes attach with, and its lanes.
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
    /// Its control stream's writer: the lanes the link wants are told on it.
    control: tokio::sync::mpsc::UnboundedSender<Out>,
    /// The daemon's clock for this session's stamps.
    origin: Instant,
    encoder: code::Encoder,
    /// The capture's update going into its block, and the Opus packet waiting to go with
    /// the next capture.
    capture: Capture,
    lanes: Attached,
}

/// A capture's update as it goes into its block, and the Opus packet waiting for a capture:
/// it waits until the next packet, its own play time, and then goes alone.
#[derive(Default)]
struct Capture {
    members: Vec<(Track, bytes::Bytes)>,
    audio: Option<Vec<u8>>,
    /// The Opus packet whose play time passed with no capture, going alone.
    lone: Option<Vec<u8>>,
    /// Track lists of blocks reported, to be used again: a block's are kept in its flight.
    spare: Vec<Vec<Track>>,
}

/// The lanes attached so far. It is already on the heap with the rest of `Open`: boxing
/// the set again would put every frame's step behind a second pointer.
#[allow(clippy::large_enum_variant)]
enum Attached {
    /// None yet: frames are sent nowhere, and the session is not looked at for them.
    None,
    /// A set: the link (from the first lane's attach on, across sets), the set's
    /// generation, its lanes by index, what each holds unreported, the frames sent and
    /// not yet reported, when the last block's pacing ends, and the lanes the relay was
    /// last told the link wants.
    Set {
        link: Link,
        generation: Generation,
        lanes: Vec<Lane>,
        /// How many `lanes` there are, as the link counts them.
        set: Lanes,
        flights: VecDeque<Sent>,
        paced_to: Instant,
        told: Lanes,
    },
}

/// A frame sent and not yet reported: what the link keeps of it, and the set it went on.
struct Sent {
    flight: Flight,
    generation: Generation,
}

/// A lane as the sender holds it: its index in the set, its queue, and the bytes it holds
/// unreported.
struct Lane {
    index: LaneIndex,
    queue: Arc<Queue>,
    held: usize,
}

/// One lane's messages waiting to leave, released by its writer at their times. Its writer
/// writes only under the queue's lock and only while it is open, so once the session closes
/// it (`close`), not one byte more leaves on the lane.
pub struct Queue {
    state: Mutex<Waiting>,
    wake: tokio::sync::Notify,
}

pub(super) struct Waiting {
    pub(super) phase: Phase,
    pub(super) messages: VecDeque<Message>,
    /// Buffers of messages that left, for the next ones: a steady stream allocates none.
    /// Boxed, as a `Message` holds one: a buffer moves between them as a pointer, not as
    /// its bytes.
    #[allow(clippy::vec_box)]
    spare: Vec<Box<Framed>>,
}

/// A lane message's buffer: the 2-byte length a stream lane writes before it, and room for
/// the longest.
pub(super) struct Framed {
    prefix: [u8; 2],
    body: [u8; DATAGRAM],
}

/// A message waiting: when it may leave, how long it is, and its buffer.
pub struct Message {
    pub(super) at: Instant,
    len: usize,
    buf: Box<Framed>,
}

impl Message {
    /// How many bytes a stream lane writes of it: its length, then it.
    pub(super) fn framed_len(&self) -> usize {
        2 + self.len
    }

    /// What a stream lane has still to write of it, `written` bytes in.
    pub(super) fn framed_from(&self, written: usize) -> &[u8] {
        match written.checked_sub(2) {
            None => &self.buf.prefix[written..],
            Some(into) => &self.buf.body[into..self.len],
        }
    }

    /// As a datagram lane sends it.
    pub(super) fn datagram(&self) -> &[u8] {
        &self.buf.body[..self.len]
    }
}

impl Queue {
    pub(crate) fn new() -> Arc<Queue> {
        Arc::new(Queue {
            state: Mutex::new(Waiting {
                phase: Phase::Open,
                messages: VecDeque::new(),
                spare: Vec::new(),
            }),
            wake: tokio::sync::Notify::new(),
        })
    }

    /// Its state, locked: its writer writes under this lock.
    pub(super) fn lock(&self) -> parking_lot::MutexGuard<'_, Waiting> {
        self.state.lock()
    }

    /// Wakes its writer: a message came, or it closed.
    pub(super) async fn woken(&self) {
        self.wake.notified().await
    }

    /// Closes it: nothing more leaves, and its writer ends.
    pub(super) fn close(&self) {
        let mut w = self.state.lock();
        w.phase = Phase::Closed;
        w.messages.clear();
        drop(w);
        self.wake.notify_one();
    }

    /// Queues the message `encode` writes, to leave at `at`; a closed lane (its session
    /// gone, or its socket broken) takes nothing.
    fn push(&self, at: Instant, encode: impl FnOnce(&mut [u8; DATAGRAM]) -> usize) {
        let mut w = self.state.lock();
        if w.phase == Phase::Closed {
            return;
        }
        let mut buf = w.spare.pop().unwrap_or_else(|| {
            Box::new(Framed {
                prefix: [0; 2],
                body: [0; DATAGRAM],
            })
        });
        let len = encode(&mut buf.body);
        buf.prefix = wire::lane_prefix(len);
        w.messages.push_back(Message { at, len, buf });
        drop(w);
        self.wake.notify_one();
    }
}

/// Whether a lane takes messages: open until its session closes or its socket breaks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Phase {
    Open,
    Closed,
}

/// What a lane's writer finds in its queue.
pub(super) enum Due {
    /// The lane closed: the writer ends.
    Closed,
    /// Nothing waits.
    Idle,
    /// The front message leaves at this time.
    At(Instant),
    /// The front message, due now, taken out to be written.
    Now(Message),
}

impl Waiting {
    /// What leaves at `now`: the front message, taken out, if it is due.
    pub(super) fn due(&mut self, now: Instant) -> Due {
        match (self.phase, self.messages.pop_front()) {
            (Phase::Closed, _) => Due::Closed,
            (Phase::Open, None) => Due::Idle,
            (Phase::Open, Some(m)) if m.at > now => {
                let at = m.at;
                self.messages.push_front(m);
                Due::At(at)
            }
            (Phase::Open, Some(m)) => Due::Now(m),
        }
    }

    /// The lane's socket broke: it takes nothing more.
    pub(super) fn broken(&mut self) {
        self.phase = Phase::Closed;
        self.messages.clear();
    }

    /// A message that left: its buffer serves the next.
    pub(super) fn sent(&mut self, m: Message) {
        self.spare.push(m.buf);
    }
}

/// Why a report ends its session: the relay said what its decoder cannot have.
#[derive(Debug, PartialEq)]
pub enum Broken {
    /// No frame was sent that it could be of: none is unreported, or the next unreported
    /// is another.
    Unsent(Seq),
    /// The link refused it for its frame.
    Mismatch(crate::lanes::link::Mismatch),
}

/// A video update the relay missed: its number.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Missed {
    pub number: Number,
}

impl Sender {
    /// A session's, with the capability its lanes attach with, its control stream's
    /// writer, and its clock read from `origin`.
    pub fn new(
        capability: Capability,
        control: tokio::sync::mpsc::UnboundedSender<Out>,
        origin: Instant,
    ) -> Sender {
        Sender {
            state: State::Open(Box::new(Open {
                capability,
                control,
                origin,
                encoder: code::Encoder::new(),
                capture: Capture::default(),
                lanes: Attached::None,
            })),
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
            State::Open(o) => o.capability.matches(presented),
            State::Closed | State::Recording(_) => false,
        }
    }

    /// Its control connection closed: the capability is dropped (and zeroed), and every
    /// lane is closed before this returns.
    pub fn close(&mut self) {
        if let State::Open(o) = std::mem::replace(&mut self.state, State::Closed) {
            if let Attached::Set { lanes, .. } = &o.lanes {
                for l in lanes {
                    l.queue.close();
                }
            }
        }
    }

    /// Whether it has a way to be sent frames on: a lane, or a recording.
    pub fn attached(&self) -> bool {
        match &self.state {
            State::Open(o) => matches!(o.lanes, Attached::Set { .. }),
            State::Recording(_) => true,
            State::Closed => false,
        }
    }

    /// Lane `index` of set `generation` attached, its queue `queue`, the attach stamped
    /// `sent` by the relay and arriving at `now`. A newer set replaces the lanes of the one
    /// before, which are closed; a lane of an older set is refused, as is one of a closed
    /// session. Whether it was taken.
    pub fn attach(
        &mut self,
        (index, generation): (LaneIndex, Generation),
        queue: Arc<Queue>,
        sent: Stamp,
        now: Instant,
    ) -> bool {
        let State::Open(o) = &mut self.state else {
            return false;
        };
        let received = Stamp::since(o.origin, now);
        let out = Lane {
            index,
            queue,
            held: 0,
        };
        match &mut o.lanes {
            Attached::None => {
                let link = Link::new(sent, received);
                let told = link.wanted();
                o.lanes = Attached::Set {
                    link,
                    generation,
                    lanes: vec![out],
                    set: Lanes::ONE,
                    flights: VecDeque::new(),
                    paced_to: now,
                    told,
                };
                true
            }
            Attached::Set {
                generation: g,
                lanes,
                set,
                ..
            } if *g == generation => {
                // The same place attached again replaces the lane there; a new place makes
                // the set one larger, which a set of lane indices always counts.
                let at = lanes.binary_search_by_key(&index.get(), |l| l.index.get());
                let count = lanes.len() + usize::from(at.is_err());
                let Some(larger) = u16::try_from(count).ok().and_then(Lanes::new) else {
                    return false;
                };
                match at {
                    Ok(i) => std::mem::replace(&mut lanes[i], out).queue.close(),
                    Err(i) => lanes.insert(i, out),
                }
                *set = larger;
                true
            }
            Attached::Set {
                generation: g,
                lanes,
                set,
                ..
            } if *g < generation => {
                for l in lanes.drain(..) {
                    l.queue.close();
                }
                lanes.push(out);
                *set = Lanes::ONE;
                *g = generation;
                true
            }
            Attached::Set { .. } => false,
        }
    }

    /// What a frame may take now.
    pub fn room(&self) -> Room {
        let o = match &self.state {
            State::Open(o) => o,
            State::Recording(_) => return Room::Unbounded,
            State::Closed => return Room::Full,
        };
        let Attached::Set {
            link, lanes, set, ..
        } = &o.lanes
        else {
            return Room::Full;
        };
        let bound = link.lane_bound(*set);
        match lanes.iter().map(|l| bound.saturating_sub(l.held)).sum() {
            0 => Room::Full,
            free => Room::Open(free),
        }
    }

    /// What a keyframe may take: what the pacing's share for the frames themselves sends
    /// in `time` (`Link::data`: repairs and headers take the rest).
    pub fn key(&self, time: Duration) -> KeyRoom {
        let o = match &self.state {
            State::Open(o) => o,
            State::Recording(_) => return KeyRoom::Unbounded,
            State::Closed => return KeyRoom::Bytes(0),
        };
        let Attached::Set { link, .. } = &o.lanes else {
            return KeyRoom::Bytes(0);
        };
        KeyRoom::Bytes((link.data() * time.as_secs_f64()) as usize)
    }

    /// What a sharpening pass may take: the room past what the next capture needs, which
    /// is what the pacing's share for the frames sends in `frame` (one frame interval). A
    /// pass so never takes room the next capture needs; with none past it, none.
    pub fn spare(&self, frame: Duration) -> Room {
        let o = match &self.state {
            State::Open(o) => o,
            State::Recording(_) => return Room::Unbounded,
            State::Closed => return Room::Full,
        };
        let Attached::Set { link, .. } = &o.lanes else {
            return Room::Full;
        };
        let next = (link.data() * frame.as_secs_f64()) as usize;
        match self.room() {
            Room::Open(free) if free > next => Room::Open(free - next),
            Room::Open(_) | Room::Full => Room::Full,
            Room::Unbounded => Room::Unbounded,
        }
    }

    /// What `voxl stats` says of it.
    pub fn stats(&self) -> Way {
        let o = match &self.state {
            State::Open(o) => o,
            State::Recording(_) => return Way::Recording,
            State::Closed => return Way::Closed,
        };
        let Attached::Set {
            link,
            lanes,
            set,
            flights,
            ..
        } = &o.lanes
        else {
            return Way::Unattached;
        };
        let st = link.stats();
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        Way::Lanes {
            held_bytes: lanes.iter().map(|l| l.held).collect(),
            lane_bound_bytes: link.lane_bound(*set),
            unreported_blocks: flights.len(),
            loss: st.loss,
            btlbw_bytes_per_s: st.btlbw,
            rtprop_ms: st.rtprop.map(ms),
            spread_ms: ms(st.spread),
            recovery_ms: ms(st.recovery),
            pacing_bytes_per_s: st.pacing,
            data_bytes_per_s: link.data(),
            stale: st.stale,
            wanted_lanes: link.wanted().get(),
        }
    }

    /// Video update `number` of `epoch`, `bytes` long, sent at `now` in a block with the
    /// Opus packet waiting for it; a recording takes the bytes as they are. A recording
    /// whose recorder has gone takes nothing: its viewer is removed with it.
    pub fn video(&mut self, (number, epoch): (Number, Epoch), bytes: &bytes::Bytes, now: Instant) {
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
        o.capture
            .members
            .push((Track::Video { number, epoch }, bytes.clone()));
        self.flush(now);
    }

    /// The capture gathered so far, with the Opus packet waiting for it, as one block.
    fn flush(&mut self, now: Instant) {
        let State::Open(o) = &mut self.state else {
            return;
        };
        let Open {
            capture,
            encoder,
            lanes,
            origin,
            control,
            ..
        } = &mut **o;
        let members = capture.members.iter().map(|(t, b)| (*t, &b[..]));
        let audio = capture.audio.iter().map(|a| (Track::Audio, &a[..]));
        send(
            encoder,
            lanes,
            *origin,
            control,
            &mut capture.spare,
            members.chain(audio),
            now,
        );
        capture.members.clear();
        capture.audio = None;
    }

    /// An Opus packet at `now`, `silent` frames of silence after the one before: on lanes
    /// it waits for the next capture, and the one before it, whose play time has passed
    /// with none, goes alone; a recording takes it as it is, with the silence it follows.
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
        let Open {
            capture,
            encoder,
            lanes,
            origin,
            control,
            ..
        } = &mut **o;
        capture.lone = capture.audio.replace(packet.to_vec());
        if capture.lone.is_some() {
            let lone = capture.lone.iter().map(|a| (Track::Audio, &a[..]));
            send(
                encoder,
                lanes,
                *origin,
                control,
                &mut capture.spare,
                lone,
                now,
            );
        }
    }

    /// A report that came up the control stream at `now`: its frame is no longer held on
    /// its lanes, the link learns from it, and a video update missed is returned, for the
    /// encoder to forget. Reports come in the order of their frames; any other is broken.
    pub fn reported(&mut self, report: &Report<'_>, now: Instant) -> Result<Vec<Missed>, Broken> {
        let o = match &mut self.state {
            State::Open(o) => o,
            State::Closed => return Ok(Vec::new()),
            State::Recording(_) => return Err(Broken::Unsent(report.block)),
        };
        let Attached::Set {
            link,
            generation,
            lanes,
            flights,
            ..
        } = &mut o.lanes
        else {
            return Err(Broken::Unsent(report.block));
        };
        let sent = match flights.pop_front() {
            Some(s) if s.flight.head.block == report.block => s,
            Some(s) => {
                flights.push_front(s);
                return Err(Broken::Unsent(report.block));
            }
            None => return Err(Broken::Unsent(report.block)),
        };
        link.reported(&sent.flight, report, Stamp::since(o.origin, now))
            .map_err(Broken::Mismatch)?;
        let head = sent.flight.head;
        if sent.generation == *generation {
            for i in 0..head.n() {
                if let Some(l) = lanes.get_mut(sent.flight.lane(i)) {
                    l.held = l.held.saturating_sub(head.message_len(i));
                }
            }
        }
        let missed = match report.fate {
            Fate::Unseen => true,
            Fate::Seen { outcome, .. } => outcome == Outcome::Missed,
        };
        let missed = match missed {
            true => sent
                .flight
                .tracks
                .iter()
                .filter_map(|t| match *t {
                    Track::Video { number, .. } => Some(Missed { number }),
                    Track::Audio => None,
                })
                .collect(),
            false => Vec::new(),
        };
        o.capture.spare.push(sent.flight.tracks);
        Ok(missed)
    }
}

/// A block of `members`: planned by the link, coded, and its messages queued on their lanes
/// at their times, paced after the block before. Its track list is taken from `spare`.
fn send<'b>(
    encoder: &mut code::Encoder,
    lanes: &mut Attached,
    origin: Instant,
    control: &tokio::sync::mpsc::UnboundedSender<Out>,
    spare: &mut Vec<Vec<Track>>,
    members: impl Iterator<Item = (Track, &'b [u8])> + Clone,
    now: Instant,
) {
    let Attached::Set {
        link,
        generation,
        lanes,
        set,
        flights,
        paced_to,
        told,
    } = lanes
    else {
        return;
    };
    let mut tracks = spare.pop().unwrap_or_default();
    tracks.clear();
    tracks.extend(members.clone().map(|(t, _)| t));
    let Ok(assembled) = encoder.assemble(members) else {
        // A block the code cannot take (empty, or over its window) is not sent; the
        // encoder's cap keeps frames far under it.
        log::warn!(
            "a block of {} frames is malformed for the lanes",
            tracks.len()
        );
        return;
    };
    let set = *set;
    let start = (*paced_to).max(now);
    let plan = link.plan(assembled.bytes(), &tracks, set, Stamp::since(origin, start));
    let wanted = link.wanted();
    if wanted != *told {
        *told = wanted;
        // A session whose control writer has ended is closing, its lanes with it.
        if control.send(Out::Lanes(wanted)).is_err() {
            log::debug!("the lanes wanted not told: the session is closing");
        }
    }
    let Ok(coded) = assembled.code(plan.m, plan.due) else {
        log::warn!("a block is too large for the lanes with its repairs");
        return;
    };
    let head = *coded.head();
    let n = head.n();
    for i in 0..n {
        // Message `i` leaves `i / n` of the way through the block's sending time.
        let at = start + plan.sending.mul_f64(i as f64 / n as f64);
        let sent = Stamp::since(origin, at);
        let lane = &mut lanes[plan.lane(i)];
        lane.held += head.message_len(i);
        lane.queue
            .push(at, |out| coded.message(i, sent).encode(out));
    }
    *paced_to = start + plan.sending;
    flights.push_back(Sent {
        flight: plan.flight(&head, tracks),
        generation: *generation,
    });
}

#[cfg(test)]
impl Sender {
    /// A session's sender with one lane attached at `now`, the lane's queue (which no
    /// writer drains: a test reads it), and what its control stream is sent.
    pub fn with_lane(
        now: Instant,
    ) -> (
        Sender,
        Arc<Queue>,
        tokio::sync::mpsc::UnboundedReceiver<Out>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut s = Sender::new(Capability::mint().expect("a random source"), tx, now);
        let queue = Queue::new();
        let place = (
            LaneIndex::all(Lanes::ONE).next().expect("one lane"),
            Generation::first(),
        );
        assert!(s.attach(place, queue.clone(), Stamp::since(now, now), now));
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
    /// its sources put it back together; the queue is emptied.
    pub fn frames(&self) -> Vec<Received> {
        let mut w = self.lock();
        type Sources = Vec<Option<Vec<u8>>>;
        let mut blocks: std::collections::BTreeMap<u64, (wire::Head, Stamp, Sources)> =
            Default::default();
        while let Some(m) = w.messages.pop_front() {
            if let Ok(wire::Lane::Piece(s)) = wire::Lane::decode(m.datagram()) {
                let (_, _, sources) = blocks
                    .entry(s.head.block.get())
                    .or_insert_with(|| (s.head, s.sent, vec![None; s.head.k()]));
                sources[s.index as usize] = Some(s.data.to_vec());
            }
            w.sent(m);
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
            late: wire::Late::new(&[]).expect("no late"),
        })
    }
}
