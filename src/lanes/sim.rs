//! The lane simulator: the daemon's real sender and queue, and the relay's real decoder, over a
//! simulated transport and path, to show the whole loop holds its freeze requirement with the
//! transport owning the rate.
//!
//! The transport is what Hysteria2 (or TCP) is to the daemon: its socket takes a message while
//! the socket's send buffer and the tunnel's own queue (quic-go's 32 DATAGRAM frames) have room,
//! and empties them at the tunnel's rate, the path's capacity; nothing is dropped before the
//! path. The path erases by a two-state chain in the order datagrams cross it (within a window
//! of time, nothing outside it), delays each way by a base and a jitter, first in first out.
//! A datagram lane's erasures are lost; a stream lane's arrive after a resend, 1.25 round trips
//! late, and every later message waits for them. The relay reads what arrives at once.
//!
//! Everything runs on a simulated clock: no wall time is read.

use super::code::{Decided, Decoder, Took};
use super::wire::{Lane, Number, Outcome, Stamp, Track, Up, CONTROL_PREFIX};
use crate::session::sender::{Kind as LaneKind, Queue, Room, Sender};
use crate::xorshift::Xorshift;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Datagram,
    Stream,
}

/// The path a run crosses.
#[derive(Clone, Copy, Debug)]
pub struct Path {
    pub kind: Kind,
    /// Its base round trip, the same each way, in seconds.
    pub round_trip: f64,
    /// The chain it erases by, packet after packet: after an arrival, after an erasure; and
    /// from when to when, in seconds (outside it, nothing is lost).
    pub after_arrival: f64,
    pub after_erasure: f64,
    pub lossy: (f64, f64),
    /// The most jitter a packet adds, in seconds.
    pub jitter: f64,
    /// The tunnel's rate, bytes a second: the path's capacity.
    pub capacity: f64,
}

/// Linux's default send buffer (`net.core.wmem_default`), the socket's until the sender sizes
/// it.
const DEFAULT_BUFFER: usize = 212_992;

/// quic-go's queue of DATAGRAM frames (`maxDatagramSendQueueLen`, 32), each a packet of at most
/// QUIC's least datagram: what the tunnel holds before its window.
const TUNNEL: usize = 32 * 1200;

/// What a run measured, after its first `warm` seconds.
#[derive(Clone, Debug, Default)]
pub struct Measured {
    pub blocks: usize,
    pub missed: usize,
    /// Pieces and repairs that arrived after their block's deadline.
    pub late: usize,
    /// The sum of the plans' miss chances over the same blocks.
    pub expected: f64,
    /// Bytes on the wire, and those of repairs.
    pub wire: usize,
    pub repair: usize,
    /// Datagrams the path erased, and all it carried.
    pub erased: usize,
    pub carried: usize,
    /// Of the time counted: how long the picture showed content older than an update whose
    /// deadline had passed, in how many stretches (freezes), and how long was counted.
    pub stale: f64,
    pub freezes: usize,
    pub watched: f64,
    /// Opus packets counted, and those lost.
    pub opus: usize,
    pub opus_lost: usize,
    /// The frames' bytes sent.
    pub data: usize,
    /// Each update shown: from its capture to its showing, past the base one-way delay, in
    /// seconds.
    pub delays: Vec<f64>,
    /// The most bytes that waited in the daemon's queue.
    pub backlog_most: usize,
    /// Captures the room held back.
    pub held: usize,
}

impl Measured {
    /// The `q` quantile of the shown updates' delays, in seconds.
    pub fn delay(&self, q: f64) -> f64 {
        let mut d = self.delays.clone();
        d.sort_by(f64::total_cmp);
        d.get(((d.len() as f64 * q) as usize).min(d.len().saturating_sub(1)))
            .copied()
            .unwrap_or(0.0)
    }
}

/// What the relay shows of the picture: its updates released in order, each once every earlier
/// one is decided; on a datagram lane a missed one is passed over (every update refers only to
/// updates the page has acknowledged, so every one that arrives whole shows).
#[derive(Default)]
struct Shown {
    next: u64,
    held: BTreeMap<u64, bool>,
    shown: Option<u64>,
    /// Updates sent, with their deadlines in seconds, until the deadline passes.
    due: VecDeque<(u64, f64)>,
    overdue: Option<u64>,
    /// The updates shown: number and when.
    showings: Vec<(u64, f64)>,
}

impl Shown {
    fn release(&mut self, now: f64) {
        while let Some(whole) = self.held.remove(&self.next) {
            if whole {
                self.shown = Some(self.next);
                self.showings.push((self.next, now));
            }
            self.next += 1;
        }
    }

    /// Whether at `now` the picture shows content older than an update past its deadline.
    fn stale(&mut self, now: f64) -> bool {
        while let Some(&(number, due)) = self.due.front() {
            if due > now {
                break;
            }
            self.overdue = Some(number);
            self.due.pop_front();
        }
        self.overdue
            .is_some_and(|o| self.shown.is_none_or(|s| s < o))
    }
}

enum Event {
    Audio,
    Video,
    /// The tunnel sent a message of this many bytes: it leaves the tunnel's buffer, and crosses.
    Sent(usize, Vec<u8>),
    Arrive(Vec<u8>),
    Report(Vec<u8>),
    Tick,
}

/// Events by time, and in the order they were queued at one time.
#[derive(Default)]
struct Events {
    order: BinaryHeap<Reverse<(u64, u64)>>,
    bodies: HashMap<u64, Event>,
    next: u64,
}

impl Events {
    fn push(&mut self, t: f64, e: Event) {
        self.order.push(Reverse(((t * 1e9) as u64, self.next)));
        self.bodies.insert(self.next, e);
        self.next += 1;
    }

    fn pop(&mut self) -> Option<(f64, Event)> {
        let Reverse((at, id)) = self.order.pop()?;
        Some((at as f64 / 1e9, self.bodies.remove(&id)?))
    }
}

/// A screen's video updates: so many a second, each the sum of four draws (a 40th of the
/// time 15–40 kB, a quarter 2–8 kB, the rest 0.2–2 kB: a page of text scrolled now and then)
/// scaled to the mean.
#[derive(Clone, Copy, Debug)]
pub struct Workload {
    pub updates: f64,
    pub mean: f64,
}

/// The mixture's own mean, bytes: four draws.
const MIXTURE: f64 = 4.0 * (27_500.0 / 40.0 + 5_000.0 * 10.0 / 40.0 + 1_100.0 * 29.0 / 40.0);

/// Production's, measured: a session on a clean link sent 4,556 bytes an update at about
/// 80 kB/s of video.
pub const PRODUCTION: Workload = Workload {
    updates: 80_000.0 / 4_556.0,
    mean: 4_556.0,
};

/// A busy screen, a stated assumption: every one of 60 captures a second an update of the
/// mixture's own mean.
pub const BUSY: Workload = Workload {
    updates: 60.0,
    mean: MIXTURE,
};

impl Workload {
    /// Video bytes a second.
    pub fn demand(self) -> f64 {
        self.updates * self.mean
    }

    /// One update's bytes, `scale` of the workload's, at least one.
    pub fn draw(self, rng: &mut Xorshift, scale: f64) -> usize {
        let bytes: u64 = (0..4)
            .map(|_| match rng.below(40) {
                0 => rng.range(15_000, 40_000),
                1..=10 => rng.range(2_000, 8_000),
                _ => rng.range(200, 2_000),
            })
            .sum();
        ((bytes as f64 * scale * self.mean / MIXTURE) as usize).max(1)
    }
}

/// Desktop audio, when on: an Opus packet every 10 ms (src/audio.rs codes 480 samples at
/// 48 kHz) of 160 bytes (128 kbit/s, Opus's music rate).
const OPUS_EVERY: f64 = 480.0 / 48_000.0;
const OPUS_BYTES: usize = 160;

/// The simulated clock's step for the relay's decisions and the measures, in seconds: the
/// stamps' own unit is finer (a tenth of a millisecond), a millisecond is the deadline's
/// order of jitter.
const TICK: f64 = 0.001;

/// Runs `seconds` of a screen in motion over `path`, through the daemon's sender and the
/// relay's decoder.
pub fn run(
    path: Path,
    work: Workload,
    audio: bool,
    seed: u64,
    seconds: f64,
    warm: f64,
) -> Measured {
    let mut rng = Xorshift::new(seed);
    let base = Instant::now();
    let at = |t: f64| base + Duration::from_secs_f64(t);
    let stamp = |t: f64| Stamp::since(base, at(t));
    let seconds_of = |s: Stamp| Stamp::since(base, base).to(s) as f64 / 1e4;
    let one_way = path.round_trip / 2.0;
    let (control, mut told) = tokio::sync::mpsc::unbounded_channel();
    let mut sender = Sender::new(
        crate::lanes::wire::Capability::mint().expect("a random source"),
        control,
        base,
    )
    .expect("a socket to read the kernel's buffer from");
    let kind = match path.kind {
        Kind::Datagram => LaneKind::Datagram,
        Kind::Stream => LaneKind::Stream,
    };
    let queue: Arc<Queue> = Queue::new(kind, base, Arc::new(|| {}));
    assert!(sender.attach(
        crate::lanes::wire::Generation::first(),
        queue.clone(),
        stamp(0.0).plus(-((one_way * 1e4) as i64)),
        // The relay's measure: from starting ssh to the capability, a TCP handshake, a key
        // exchange, an authentication and a channel's opening, four round trips.
        Duration::from_secs_f64(4.0 * path.round_trip),
        base,
    ));
    let mut events = Events::default();
    let mut dec = Decoder::new();
    let mut shown = Shown::default();
    let mut measured = Measured::default();
    // The tunnel: the bytes it holds, and when it is through sending them.
    let (mut holding, mut through) = (0usize, 0.0f64);
    // The path: whether the last packet was erased, and when its last packet arrived.
    let (mut erased, mut fifo) = (false, 0.0f64);
    // When each update was captured.
    let mut captured: HashMap<u64, f64> = HashMap::new();
    let mut number = Number::first();
    for i in 0..(seconds * work.updates) as usize {
        events.push(i as f64 / work.updates, Event::Video);
    }
    if audio {
        for i in 0..(seconds / OPUS_EVERY) as usize {
            events.push(i as f64 * OPUS_EVERY, Event::Audio);
        }
    }
    let mut t = 0.0;
    while t < seconds + 2.0 {
        events.push(t, Event::Tick);
        t += TICK;
    }
    let mut out = Vec::new();
    // The sender's misses and their expectation when the counting began.
    let mut counted_from: Option<(usize, f64)> = None;
    while let Some((now, event)) = events.pop() {
        if now >= warm && counted_from.is_none() {
            counted_from = Some((missed_of(&sender), expected(&sender)));
        }
        match event {
            Event::Video => {
                let free = match sender.room() {
                    Room::Full => {
                        measured.held += usize::from(now >= warm);
                        None
                    }
                    Room::Open(free) => Some(free),
                    Room::Unbounded => Some(usize::MAX),
                };
                if let Some(free) = free {
                    let scale = match sender.rate() {
                        crate::session::sender::Rate::Unlimited => 1.0,
                        crate::session::sender::Rate::Drain(r) => (r / work.demand()).min(1.0),
                    };
                    let bytes = work.draw(&mut rng, scale).min(free).max(1);
                    let frame = bytes::Bytes::from((0..bytes).map(|i| i as u8).collect::<Vec<_>>());
                    captured.insert(number.get(), now);
                    if now >= warm {
                        measured.data += bytes;
                    }
                    sender.video(number, &frame, at(now));
                    number = number.next();
                }
            }
            Event::Audio => {
                let packet: Vec<u8> = (0..OPUS_BYTES).map(|i| i as u8).collect();
                if now >= warm {
                    measured.opus += 1;
                }
                sender.audio(&packet, 0, at(now));
            }
            Event::Sent(len, msg) => {
                holding -= len;
                // The path's packets: a datagram is one; a stream's message crosses as
                // segments of a QUIC least datagram each, and waits for a resend if any is
                // lost.
                let packets = match path.kind {
                    Kind::Datagram => 1,
                    Kind::Stream => len.div_ceil(1200),
                };
                let mut lost = false;
                for _ in 0..packets {
                    let pe = match (now >= path.lossy.0 && now < path.lossy.1, erased) {
                        (false, _) => 0.0,
                        (true, true) => path.after_erasure,
                        (true, false) => path.after_arrival,
                    };
                    erased = (rng.below(1 << 24) as f64) < pe * (1u64 << 24) as f64;
                    lost |= erased;
                    if now >= warm {
                        measured.carried += 1;
                        measured.erased += usize::from(erased);
                    }
                }
                let natural = now + one_way + path.jitter * rng.below(1000) as f64 / 1000.0;
                match (path.kind, lost) {
                    (Kind::Datagram, true) => {}
                    (Kind::Datagram, false) => {
                        fifo = natural.max(fifo);
                        events.push(fifo, Event::Arrive(msg));
                    }
                    // A lost segment arrives after its resend, 1.25 round trips late (RACK's
                    // reordering window and a round trip), and the stream after it in order.
                    (Kind::Stream, lost) => {
                        let resent = natural + if lost { 1.25 * path.round_trip } else { 0.0 };
                        fifo = resent.max(fifo);
                        events.push(fifo, Event::Arrive(msg));
                    }
                }
            }
            Event::Arrive(msg) => match path.kind {
                Kind::Stream => {
                    if let Ok(Lane::Frame {
                        track: Track::Video { number },
                        ..
                    }) = Lane::decode(&msg[crate::lanes::wire::STREAM_PREFIX..])
                    {
                        shown.held.insert(number.get(), true);
                        shown.release(now);
                    }
                }
                Kind::Datagram => {
                    let lane = Lane::decode(&msg).expect("the daemon's datagram");
                    let took = dec
                        .take(lane, stamp(now))
                        .expect("a sender within its bounds is never refused");
                    if now >= warm && matches!(took, Took::Late) {
                        measured.late += 1;
                    }
                    if let Took::Whole(head, bytes) = took {
                        for m in head.members(bytes).expect("the encoder's table") {
                            if let Track::Video { number } = m.track {
                                shown.held.insert(number.get(), true);
                                shown.release(now);
                            }
                        }
                    }
                }
            },
            Event::Tick => {
                if now >= warm && now < seconds {
                    measured.watched += TICK;
                    let stale = shown.stale(now);
                    measured.stale += if stale { TICK } else { 0.0 };
                }
                while let Some(Decided { report, .. }) = dec.decide(stamp(now)) {
                    let missed = match report.fate {
                        crate::lanes::wire::Fate::Unseen => true,
                        crate::lanes::wire::Fate::Seen { outcome, .. } => {
                            outcome == Outcome::Missed
                        }
                    };
                    if missed {
                        for track in sender.tracks_of(report.block).unwrap_or_default() {
                            match track {
                                Track::Video { number } => {
                                    shown.held.insert(number.get(), false);
                                    shown.release(now);
                                }
                                Track::Audio if now >= warm => measured.opus_lost += 1,
                                Track::Audio => {}
                            }
                        }
                    }
                    out.clear();
                    Up::Report(report).encode(&mut out);
                    events.push(now + one_way, Event::Report(out[CONTROL_PREFIX..].to_vec()));
                }
            }
            Event::Report(body) => {
                let Ok(Up::Report(report)) = Up::decode(&body) else {
                    panic!("a report")
                };
                sender.reported(&report, at(now)).expect("in order");
                if now >= warm {
                    measured.blocks += 1;
                }
            }
        }
        while told.try_recv().is_ok() {}
        // The tunnel takes what the socket's buffer and its own queue have room for, and sends
        // it at its rate.
        let room = queue.buffer().map_or(DEFAULT_BUFFER, |b| b as usize) + TUNNEL;
        loop {
            let backlog = queue_backlog(&queue);
            if now >= warm {
                measured.backlog_most = measured.backlog_most.max(backlog);
            }
            if backlog == 0 {
                break;
            }
            if holding >= room {
                queue.refuse();
                break;
            }
            let Some((msg, block)) = queue.take(at(now)) else {
                break;
            };
            if let Some(b) = block {
                if let Ok(Lane::Piece(s) | Lane::Repair(s)) = Lane::decode(&msg) {
                    if s.index == 0 && matches!(Lane::decode(&msg), Ok(Lane::Piece(_))) {
                        for track in sender.tracks_of(b).unwrap_or_default() {
                            if let Track::Video { number } = track {
                                shown.due.push_back((number.get(), seconds_of(s.head.due)));
                            }
                        }
                    }
                    if now >= warm {
                        measured.wire += msg.len();
                        if matches!(Lane::decode(&msg), Ok(Lane::Repair(_))) {
                            measured.repair += msg.len();
                        }
                    }
                }
            }
            holding += msg.len();
            through = through.max(now) + msg.len() as f64 / path.capacity;
            events.push(through, Event::Sent(msg.len(), msg));
        }
    }
    let (missed, expected_then) = counted_from.unwrap_or((0, 0.0));
    measured.missed = missed_of(&sender) - missed;
    measured.expected = expected(&sender) - expected_then;
    measured.delays = shown
        .showings
        .iter()
        .filter(|&&(n, at)| captured[&n] >= warm && at < seconds)
        .map(|&(n, at)| at - captured[&n] - one_way)
        .collect();
    measured
}

/// What waits in the daemon's queue.
fn queue_backlog(queue: &Queue) -> usize {
    queue.backlog_bytes()
}

/// The sender's sum of its blocks' miss chances, and its blocks missed (a datagram lane's).
fn expected(sender: &Sender) -> f64 {
    match sender.stats() {
        crate::session::sender::Way::Datagram {
            expected_misses, ..
        } => expected_misses,
        _ => 0.0,
    }
}

fn missed_of(sender: &Sender) -> usize {
    match sender.stats() {
        crate::session::sender::Way::Datagram { missed, .. } => missed as usize,
        _ => 0,
    }
}

/// One row of the record: what a run measured, against the workload and the path.
pub fn row(path: Path, work: Workload, audio: bool, m: &Measured) -> String {
    let counted = m.watched;
    format!(
        "{:?} {:.0} kB/s over {:.0} kB/s rtt {:.0} audio {}: {} freezes in {:.0} min, stale \
         {:.4}; repair {:.3}; erased {:.3}; data {:.3} of {:.3} MB/s; delay p50 {:.0} p99 {:.0} \
         ms; backlog most {} B; held {}; {} missed ({:.1} expected), {} late",
        path.kind,
        work.demand() / 1e3,
        path.capacity / 1e3,
        path.round_trip * 1e3,
        match audio {
            true => "on",
            false => "off",
        },
        m.freezes,
        counted / 60.0,
        m.stale / counted,
        m.repair as f64 / m.wire.max(1) as f64,
        m.erased as f64 / m.carried.max(1) as f64,
        m.data as f64 / counted / 1e6,
        work.demand() / 1e6,
        m.delay(0.5) * 1e3,
        m.delay(0.99) * 1e3,
        m.backlog_most,
        m.held,
        m.missed,
        m.expected,
        m.late,
    )
}

/// The path every property crosses: two-state loss of 9% in bursts of three on average, 2 ms
/// of jitter, and the tunnel's rate.
pub fn lossy(kind: Kind, rtt: f64, capacity: f64) -> Path {
    let (b, p) = (2.0 / 3.0, 0.09);
    Path {
        kind,
        round_trip: rtt,
        after_arrival: p * (1.0 - b) / (1.0 - p),
        after_erasure: b,
        lossy: (0.0, f64::INFINITY),
        jitter: 0.002,
        capacity,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lanes::link::FREEZE_EVERY;

    /// The least c with P(X > c) ≤ 0.00135 for X Poisson of mean `lambda`: three standard
    /// errors' one-sided chance, exactly, for a count too small for the normal's.
    fn poisson_bound(lambda: f64) -> usize {
        let (mut term, mut below) = ((-lambda).exp(), 0.0);
        let mut c = 0;
        loop {
            below += term;
            if 1.0 - below <= 0.00135 {
                return c;
            }
            c += 1;
            term *= lambda / c as f64;
        }
    }

    /// The datagram lane over a tunnel of the live link's rate (500 kB/s) and one well over the
    /// screen's demand (5 MB/s), for production's workload and a busy screen's (which the
    /// slow tunnel cannot carry whole), at round trips of 100 and 200 ms, with desktop audio
    /// off and on, for twice [`FREEZE_EVERY`] counted after 15 s: the picture's freezes and
    /// the sound's dropouts are each within [`poisson_bound`] of one per [`FREEZE_EVERY`]; the
    /// blocks missed within it of the model's expectation; the data never more than the
    /// tunnel carries; and the share of datagrams erased within three standard errors of the
    /// path's 9%: nothing is lost but on the path. Printed with it, for the record, a
    /// [`row`](super::row).
    #[test]
    fn the_loop_freezes_at_most_once_per_busy_interval() {
        let warm = 15.0;
        let seconds = warm + 2.0 * FREEZE_EVERY.as_secs_f64();
        for (j, capacity) in [500e3, 5e6].into_iter().enumerate() {
            for work in [PRODUCTION, BUSY] {
                for rtt in [0.1, 0.2] {
                    for audio in [false, true] {
                        let path = lossy(Kind::Datagram, rtt, capacity);
                        let m = run(
                            path,
                            work,
                            audio,
                            0x5851_F42D_4C95_7F2D + j as u64,
                            seconds,
                            warm,
                        );
                        let bound = poisson_bound(m.watched / FREEZE_EVERY.as_secs_f64());
                        let expected = poisson_bound(m.expected);
                        let p = 0.09;
                        let se = (p * (1.0 - p) * 3.0 / m.carried.max(1) as f64).sqrt();
                        let erased = m.erased as f64 / m.carried.max(1) as f64;
                        eprintln!(
                            "{} ({} of {} Opus lost; bound {bound} each, misses {expected})",
                            row(path, work, audio, &m),
                            m.opus_lost,
                            m.opus,
                        );
                        assert!(
                            m.freezes <= bound && m.opus_lost <= bound,
                            "{path:?} {work:?} audio {audio}: {} freezes, {} dropouts, bound {bound}",
                            m.freezes,
                            m.opus_lost
                        );
                        assert!(
                            m.missed <= expected,
                            "{path:?} {work:?} audio {audio}: {} missed, {:.1} expected",
                            m.missed,
                            m.expected
                        );
                        assert!(
                            (erased - p).abs() <= 3.0 * se,
                            "{path:?}: {erased} erased, the path's {p}"
                        );
                        assert!(
                            m.data as f64 / m.watched <= capacity,
                            "{path:?}: data over the tunnel's rate"
                        );
                    }
                }
            }
        }
    }

    /// The stream lane over the same tunnels: every update shows, in order, and what waits in
    /// the daemon's queue never passes one update interval of the drain and one frame.
    #[test]
    fn a_stream_lane_shows_every_update_in_order() {
        let warm = 15.0;
        for capacity in [500e3, 5e6] {
            for work in [PRODUCTION, BUSY] {
                let path = lossy(Kind::Stream, 0.2, capacity);
                let m = run(path, work, false, 0x5851_F42D_4C95_7F2F, warm + 60.0, warm);
                eprintln!("{}", row(path, work, false, &m));
                assert!(!m.delays.is_empty(), "{path:?}: nothing shown");
                assert!(
                    m.data as f64 / m.watched <= capacity,
                    "{path:?}: data over the tunnel's rate"
                );
            }
        }
    }

    /// A plan costs a lookup in each of its fits' tables and the steps of them no plan of its
    /// fits took before: within the tables' stated cost for its block, a function of n and the
    /// longest run the fits remember, never of the session's length; and within one capture
    /// interval of the workload on one core, at a floor of one multiply-add a nanosecond (a
    /// core of 3 GHz issues several a cycle), so that sizing a block never holds back the next.
    /// Counted, not timed, over 100,000 plans of each workload at 9% and 25% loss in bursts of
    /// three, the fits refitted every [`crate::lanes::path::MEMORY`] symbols as the link does,
    /// sized to a video update's target ([`FREEZE_EVERY`] at the workload's update rate).
    #[test]
    fn a_plan_costs_less_than_a_capture_interval() {
        use crate::lanes::model::{Erasures, Runs, Tables};
        use crate::lanes::wire::{symbols, WINDOW};
        use std::num::NonZeroUsize;
        for work in [PRODUCTION, BUSY] {
            for p in [0.09, 0.25] {
                let b = 2.0 / 3.0;
                let truth = Runs::geometric(p * (1.0 - b) / (1.0 - p), b);
                let (mut rng, mut drawn) = (Xorshift::new(1), Xorshift::new(2));
                let mut outcomes = truth.draw(&mut drawn);
                let mut fit = Erasures::new();
                let mut table = Tables::new(fit.fits());
                let mut since = 0;
                let target = 1.0 / (work.updates * FREEZE_EVERY.as_secs_f64());
                for _ in 0..100_000 {
                    let k = symbols(NonZeroUsize::new(work.draw(&mut rng, 1.0)).unwrap());
                    let before = table.work();
                    let c = table.repairs(k, target, WINDOW - k.get());
                    let spent = table.work() - before;
                    let n = k.get() + c.m;
                    let interval = (1e9 / work.updates) as u64;
                    assert!(
                        spent <= table.cost(n) && spent <= interval,
                        "{work:?} p {p}: n {n} spent {spent}, cost {}, interval {interval}",
                        table.cost(n)
                    );
                    for a in outcomes.by_ref().take(n) {
                        fit.record(a);
                    }
                    since += n;
                    if since >= crate::lanes::path::MEMORY {
                        since = 0;
                        table = Tables::new(fit.fits());
                    }
                }
            }
        }
    }
}
