//! The lane simulator: the whole loop on a simulated path, to show the algebra holds its miss
//! rate on every kind of lane.
//!
//! A daemon (a [`Link`] and an [`Encoder`]) sends a screen's video updates and audio over
//! lanes to a relay (a [`Decoder`]), whose reports come back over a control stream. The path
//! erases by a two-state chain in the order its packets cross it, delays each way by a base
//! and a jitter, and serialises at a capacity. The lanes differ only as the transports do:
//!
//! - **datagram**: one lane; an erased message never arrives.
//! - **streams**: as many lanes as the link wants; an erased message arrives
//!   after its resend, 1.25 round trips late, and every later one on its lane waits for it.
//! - **connections**: six such lanes, opened whole.
//!
//! Everything runs on a simulated clock: no wall time is read.

use super::code::{Decided, Decoder, Encoder, Took};
use super::link::{Flight, Link};
use super::model::Lanes;
use super::wire::{
    Epoch, Fate, Lane, Number, Outcome, Report, Seq, Stamp, Track, Up, CONTROL_PREFIX, DATAGRAM,
    HEADER,
};
use crate::xorshift::Xorshift;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Datagram,
    Streams,
    Connections { count: u16 },
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
    /// Its capacity, bytes a second.
    pub capacity: f64,
}

/// What a run measured, after its first `warm` seconds.
#[derive(Clone, Debug, Default)]
pub struct Measured {
    pub blocks: usize,
    pub missed: usize,
    /// The sum of the plans' miss chances over the same blocks.
    pub expected: f64,
    /// Bytes on the wire, and those of repairs.
    pub wire: usize,
    pub repair: usize,
    /// Of the time counted: how long the picture showed content older than an update whose
    /// deadline had passed, in how many stretches (freezes), and how long was counted.
    pub stale: f64,
    pub freezes: usize,
    pub watched: f64,
    /// Opus packets counted, and those lost.
    pub opus: usize,
    pub opus_lost: usize,
    /// The blocks' bytes (their frames, and the tables naming them).
    pub data: usize,
    /// The pacing's mean over the time counted, bytes a second.
    pub budget_mean: f64,
    /// Each update shown: from its capture to its showing, past the base one-way delay, in
    /// seconds; and whether it waited on a stall (the piece that completed it, or the update
    /// it was held behind in order, came after a resend or behind one on its lane).
    pub delays: Vec<(f64, bool)>,
    /// Each block counted: when it was produced, its symbols, its bytes on the wire and its
    /// repairs' bytes.
    pub log: Vec<(f64, usize, usize, usize)>,
    /// The most lanes a block went over, and the sum over blocks, for the mean.
    pub lanes_most: usize,
    pub lanes_sum: usize,
}

impl Measured {
    /// The `q` quantile of the shown updates' delays, in seconds.
    pub fn delay(&self, q: f64) -> f64 {
        let mut d: Vec<f64> = self.delays.iter().map(|&(d, _)| d).collect();
        d.sort_by(f64::total_cmp);
        d.get(((d.len() as f64 * q) as usize).min(d.len().saturating_sub(1)))
            .copied()
            .unwrap_or(0.0)
    }

    /// The share of the shown updates that waited on a stall.
    pub fn waited(&self) -> f64 {
        let w = self.delays.iter().filter(|&&(_, w)| w).count();
        w as f64 / self.delays.len().max(1) as f64
    }
}

/// What the relay shows of the picture: its updates released in order, each dropped if its
/// epoch is older than the newest miss before it, as the relay's release does.
#[derive(Default)]
struct Shown {
    /// The next update to release, by number.
    next: u64,
    /// The newest miss released past.
    missed: Option<Epoch>,
    /// Decided or decoded updates not yet released, by number: `Some` decoded (its epoch,
    /// and whether the piece that completed it came after a stall), `None` missed.
    held: std::collections::BTreeMap<u64, Option<(Epoch, bool)>>,
    /// The updates shown: number, when, whether they waited on a stall.
    showings: Vec<(u64, f64, bool)>,
    /// The newest update shown.
    shown: Option<u64>,
    /// Updates sent, with their deadlines in seconds, until the deadline passes.
    due: std::collections::VecDeque<(u64, f64)>,
    /// The newest update whose deadline has passed.
    overdue: Option<u64>,
}

impl Shown {
    /// Releases what is in order at `now`: the update that let the rest go first, the rest
    /// held behind it, so waiting on a stall if it did.
    fn release(&mut self, now: f64) {
        let mut lead = None;
        while let Some(update) = self.held.remove(&self.next) {
            let stalled = *lead.get_or_insert(update.is_some_and(|(_, s)| s));
            match update {
                None => self.missed = Some(Epoch::After(Number(self.next))),
                Some((epoch, _)) if self.missed.is_some_and(|m| epoch < m) => {}
                Some((_, own)) => {
                    self.shown = Some(self.next);
                    self.showings.push((self.next, now, own || stalled));
                }
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
    /// An Opus packet is ready.
    Audio,
    /// The screen is captured: a video update is ready, if the pacer has sent what was
    /// captured before.
    Video,
    /// A message arrives; and whether it came after a resend, or behind one on its lane.
    Arrive(Vec<u8>, bool),
    Report(Vec<u8>),
    Tick,
}

/// Events by time, and in the order they were queued at one time.
#[derive(Default)]
struct Queue {
    order: BinaryHeap<Reverse<(u64, u64)>>,
    bodies: HashMap<u64, Event>,
    next: u64,
}

impl Queue {
    fn push(&mut self, t: f64, e: Event) {
        self.order.push(Reverse((ns(t), self.next)));
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

/// Seconds to nanoseconds, for ordering events.
fn ns(t: f64) -> u64 {
    (t * 1e9) as u64
}

fn stamp(t: f64, offset: u32) -> Stamp {
    Stamp(((t * 1e4) as u64 as u32).wrapping_add(offset))
}

/// Runs `seconds` of a screen in motion over `path`; each
/// capture's update goes in one block with the Opus packet waiting for it, if any (a packet
/// waits for the next capture until the next packet, its own play time, then goes alone).
pub fn run(
    path: Path,
    work: Workload,
    audio: bool,
    seed: u64,
    seconds: f64,
    warm: f64,
) -> Measured {
    let mut rng = Xorshift::new(seed);
    let offset = rng.next() as u32;
    let one_way = path.round_trip / 2.0;
    let mut events = Queue::default();
    // The attach: the daemon's first sample of the delay toward it.
    let mut link = Link::new(stamp(0.0, offset), stamp(one_way, 0));
    let mut frozen = false;
    let mut enc = Encoder::new();
    let mut dec = Decoder::new();
    let mut flights: HashMap<Seq, (Flight, f64, f64)> = HashMap::new();
    let mut lanes = match path.kind {
        Kind::Datagram | Kind::Streams => Lanes::ONE,
        Kind::Connections { count } => Lanes::new(count).unwrap(),
    };
    let mut lane_free = vec![0.0f64; 1 << 16];
    let (mut pacer, mut bottleneck) = (0.0f64, 0.0f64);
    let mut erased = false;
    // The bytes sent and not yet reported: a capture waits while they fill the in-flight cap,
    // as the daemon's capture loop waits for room.
    let mut held = 0usize;
    // When the path's last packet arrived.
    let mut fifo = 0.0f64;
    let mut number = Number::first();
    // The daemon's epoch, and what the relay shows.
    let mut epoch = Epoch::Start;
    let mut shown = Shown::default();
    // Each block's video update, by number: the relay learns a missed block's from the
    // updates after it; the simulation reads it from the daemon.
    let mut contents: HashMap<Seq, Option<u64>> = HashMap::new();
    let seconds_of = |due: Stamp| due.0.wrapping_sub(offset) as f64 / 1e4;
    let mut measured = Measured::default();
    // The Opus packet waiting for a capture.
    let mut waiting: Option<Vec<u8>> = None;
    // Frames: every 10 ms an Opus packet, every 1/60 s a video update.
    // Desktop audio only when it is on (the daemon sends it only with --audio-enabled).
    if audio {
        for i in 0..(seconds / OPUS_EVERY) as usize {
            events.push(i as f64 * OPUS_EVERY, Event::Audio);
        }
    }
    for i in 0..(seconds * work.updates) as usize {
        events.push(i as f64 / work.updates, Event::Video);
    }
    // When each update was captured, by number.
    let mut captured: HashMap<u64, f64> = HashMap::new();
    let mut t = 0.0;
    while t < seconds + 2.0 {
        events.push(t, Event::Tick);
        t += TICK;
    }
    let mut buf = [0u8; DATAGRAM];
    let mut up = Vec::new();
    while let Some((now, event)) = events.pop() {
        match event {
            e @ (Event::Audio | Event::Video) => {
                // The encoder holds the picture to the pacing's data: an update's bytes scale
                // down to the share of its demand that carries.
                let scale = (link.data() / work.demand()).min(1.0);
                let packet: Vec<u8> = (0..OPUS_BYTES).map(|i| i as u8).collect();
                // A block waits while the lanes hold their cap, as the daemon's sender does.
                // A capture waits while the lanes hold their cap, or while the pacer has more
                // than one capture's interval still to send: at most one capture waits behind
                // the one leaving.
                let full = held >= link.lane_bound(lanes) * lanes.get()
                    || (matches!(e, Event::Video) && pacer - now > 1.0 / work.updates);
                let blocks: Vec<Vec<(Track, Vec<u8>)>> = match e {
                    _ if full => Vec::new(),
                    Event::Audio => {
                        // The packet before goes alone: its play time has passed.
                        let alone = waiting.replace(packet);
                        alone.map(|p| vec![(Track::Audio, p)]).into_iter().collect()
                    }
                    _ => {
                        let bytes = work.draw(&mut rng, scale);
                        let track = Track::Video { number, epoch };
                        captured.insert(number.get(), now);
                        number = number.next();
                        let mut capture = vec![(track, (0..bytes).map(|i| i as u8).collect())];
                        capture.extend(waiting.take().map(|p| (Track::Audio, p)));
                        vec![capture]
                    }
                };
                for members in blocks {
                    if path.kind == Kind::Streams {
                        lanes = link.wanted();
                    }
                    let tracks: Vec<Track> = members.iter().map(|(t, _)| *t).collect();
                    let start = now.max(pacer);
                    let assembled = enc
                        .assemble(members.iter().map(|(t, b)| (*t, b.as_slice())))
                        .unwrap();
                    let plan = link.plan(assembled.bytes(), &tracks, lanes, stamp(start, 0));
                    if now >= warm {
                        measured.lanes_most = measured.lanes_most.max(lanes.get());
                        measured.lanes_sum += lanes.get();
                    }
                    let coded = assembled.code(plan.m, plan.due).unwrap();
                    let head = *coded.head();
                    let n = head.n();
                    let sending = plan.sending.as_secs_f64();
                    pacer = start + sending;
                    held += (0..n).map(|i| head.message_len(i)).sum::<usize>();
                    flights.insert(
                        head.block,
                        (plan.flight(&head, tracks.clone()), plan.miss, now),
                    );
                    let mut video = None;
                    for track in &tracks {
                        if let Track::Video { number, .. } = *track {
                            video = Some(number.get());
                            shown.due.push_back((number.get(), seconds_of(plan.due)));
                        }
                    }
                    contents.insert(head.block, video);
                    for i in 0..n {
                        let sent = start + sending * i as f64 / n as f64;
                        let len = coded.message(i, stamp(sent, 0)).encode(&mut buf);
                        let lane = plan.lane(i);
                        bottleneck = bottleneck.max(sent) + (len + HEADER) as f64 / path.capacity;
                        let pe = match (sent >= path.lossy.0 && sent < path.lossy.1, erased) {
                            (false, _) => 0.0,
                            (true, true) => path.after_erasure,
                            (true, false) => path.after_arrival,
                        };
                        erased = (rng.below(1 << 24) as f64) < pe * (1u64 << 24) as f64;
                        // The path is first in, first out: its jitter delays a packet and those
                        // queued behind it, never reorders them.
                        let mut arrive =
                            (bottleneck + one_way + path.jitter * rng.below(1000) as f64 / 1000.0)
                                .max(fifo);
                        fifo = arrive;
                        let stalled = match (path.kind, erased) {
                            (Kind::Datagram, true) => continue,
                            (Kind::Datagram, false) => false,
                            (_, e) => {
                                if e {
                                    arrive += 1.25 * path.round_trip;
                                }
                                let behind = lane_free[lane] > arrive;
                                arrive = arrive.max(lane_free[lane]);
                                lane_free[lane] = arrive;
                                e || behind
                            }
                        };
                        events.push(arrive, Event::Arrive(buf[..len].to_vec(), stalled));
                    }
                }
            }
            Event::Arrive(msg, stalled) => {
                let lane = Lane::decode(&msg).unwrap();
                let took = dec
                    .take(lane, stamp(now, offset))
                    .expect("a sender within its bounds is never refused");
                if let Took::Whole(head, bytes) = took {
                    for m in head.members(bytes).expect("the encoder's table") {
                        if let Track::Video { number, epoch } = m.track {
                            shown.held.insert(number.get(), Some((epoch, stalled)));
                            shown.release(now);
                        }
                    }
                }
            }
            Event::Tick => {
                if now >= warm && now < seconds {
                    measured.budget_mean += link.pacing() * TICK / (seconds - warm);
                    measured.watched += TICK;
                    let stale = shown.stale(now);
                    measured.stale += if stale { TICK } else { 0.0 };
                    measured.freezes += (stale && !frozen) as usize;
                    frozen = stale;
                }
                while let Some(Decided { report, .. }) = dec.decide(stamp(now, offset)) {
                    if let (true, Some(number)) = (missed(&report), contents[&report.block]) {
                        shown.held.insert(number, None);
                        shown.release(now);
                    }
                    up.clear();
                    Up::Report(report).encode(&mut up);
                    events.push(now + one_way, Event::Report(up[CONTROL_PREFIX..].to_vec()));
                }
            }
            Event::Report(body) => {
                let Ok(Up::Report(report)) = Up::decode(&body) else {
                    panic!("a report")
                };
                let (flight, miss, produced) = flights.remove(&report.block).unwrap();
                held -= (0..flight.head.n())
                    .map(|i| flight.head.message_len(i))
                    .sum::<usize>();
                link.reported(&flight, &report, stamp(now, 0)).unwrap();
                if produced >= warm {
                    count(&mut measured, &report, &flight, miss, produced);
                }
                if missed(&report) {
                    for track in &flight.tracks {
                        if let Track::Video { number, .. } = *track {
                            epoch = epoch.max(Epoch::After(number));
                        }
                    }
                }
            }
        }
    }
    measured.delays = shown
        .showings
        .iter()
        .filter(|&&(n, at, _)| captured[&n] >= warm && at < seconds)
        .map(|&(n, at, waited)| (at - captured[&n] - one_way, waited))
        .collect();
    measured
}

fn missed(report: &Report<'_>) -> bool {
    match report.fate {
        Fate::Unseen => true,
        Fate::Seen { outcome, .. } => outcome == Outcome::Missed,
    }
}

fn count(m: &mut Measured, report: &Report<'_>, flight: &Flight, miss: f64, produced: f64) {
    let head = &flight.head;
    let missed = missed(report);
    m.blocks += 1;
    m.data += head.bytes.get() as usize;
    m.expected += miss;
    m.missed += missed as usize;
    let wire = (0..head.n()).map(|i| head.message_len(i)).sum::<usize>();
    let repair = (head.k()..head.n())
        .map(|i| head.message_len(i))
        .sum::<usize>();
    m.wire += wire;
    m.repair += repair;
    m.log.push((produced, head.n(), wire, repair));
    for track in &flight.tracks {
        if let Track::Audio = track {
            m.opus += 1;
            m.opus_lost += missed as usize;
        }
    }
}

/// One row of the record: what a run measured, against the workload and the path.
pub fn row(kind: Kind, work: Workload, rtt: f64, audio: bool, m: &Measured) -> String {
    let counted = m.watched;
    let demand = work.demand()
        + match audio {
            true => OPUS_BYTES as f64 / OPUS_EVERY,
            false => 0.0,
        };
    format!(
        "{kind:?} {:.0} kB/s rtt {:.0} audio {}: {} freezes in {:.0} min, stale {:.4}; repair {:.3}; data {:.3} of \
         {:.3} MB/s; pacing {:.2} of 5 MB/s; delay p50 {:.0} p99 {:.0} ms, {:.3} waited \
         on a stall; lanes most {} mean {:.0}; {} missed ({:.1} expected)",
        work.demand() / 1e3,
        rtt * 1e3,
        match audio {
            true => "on",
            false => "off",
        },
        m.freezes,
        counted / 60.0,
        m.stale / counted,
        m.repair as f64 / m.wire.max(1) as f64,
        m.data as f64 / counted / 1e6,
        demand / 1e6,
        m.budget_mean / 1e6,
        m.delay(0.5) * 1e3,
        m.delay(0.99) * 1e3,
        m.waited(),
        m.lanes_most,
        m.lanes_sum as f64 / m.blocks.max(1) as f64,
        m.missed,
        m.expected,
    )
}

/// The path every property crosses at `rtt`: two-state loss of 9% in bursts of three on
/// average, 2 ms of jitter, and a capacity well over the screen's demand.
pub fn lossy(kind: Kind, rtt: f64) -> Path {
    let (b, p) = (2.0 / 3.0, 0.09);
    Path {
        kind,
        round_trip: rtt,
        after_arrival: p * (1.0 - b) / (1.0 - p),
        after_erasure: b,
        lossy: (0.0, f64::INFINITY),
        jitter: 0.002,
        capacity: 5e6,
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

    /// Runs the loop on `kind` for production's workload and a busy screen's, at round trips
    /// of 100 and 200 ms, over [`lossy`](super::lossy), for twice [`FREEZE_EVERY`] counted after
    /// 15 s (the pacing's climb from ten symbols a round trip), and asserts the freezes
    /// counted are within [`poisson_bound`] of one per [`FREEZE_EVERY`], with desktop audio on
    /// the Opus packets lost (the sound's dropouts) within it too, and the blocks missed within
    /// it of the model's expectation (the sum of the plans' miss chances). Printed with it, for
    /// the record, a [`row`](super::row), the misses against the model's expectation among it.
    fn freezes_at_most_once_per_busy_interval(kind: Kind, seed: u64) {
        let warm = 15.0;
        let seconds = warm + 2.0 * FREEZE_EVERY.as_secs_f64();
        for work in [PRODUCTION, BUSY] {
            for (j, rtt) in [0.1, 0.2].into_iter().enumerate() {
                // Production has desktop audio off; with it on, for the record.
                for audio in [false, true] {
                    let m = run(
                        lossy(kind, rtt),
                        work,
                        audio,
                        seed + j as u64,
                        seconds,
                        warm,
                    );
                    // Each of the picture's freezes and the sound's dropouts: one per
                    // FREEZE_EVERY.
                    let bound = poisson_bound(m.watched / FREEZE_EVERY.as_secs_f64());
                    eprintln!(
                        "{} ({} of {} Opus lost; bound {bound} each)",
                        row(kind, work, rtt, audio, &m),
                        m.opus_lost,
                        m.opus,
                    );
                    assert!(
                        m.freezes <= bound && m.opus_lost <= bound,
                        "{kind:?} {work:?} rtt {rtt} audio {audio}: {} freezes, {} dropouts, \
                         bound {bound}",
                        m.freezes,
                        m.opus_lost
                    );
                    // The blocks missed: the model's expectation's count.
                    let expected = poisson_bound(m.expected);
                    assert!(
                        m.missed <= expected,
                        "{kind:?} {work:?} rtt {rtt} audio {audio}: {} missed, {:.1} expected, \
                         bound {expected}",
                        m.missed,
                        m.expected
                    );
                }
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

    /// The path's loss steps from none to 9% in bursts of three for [`FREEZE_EVERY`], and back
    /// to none, as a link that changes route: production's workload on `kind` at 100 ms keeps
    /// its freezes within the bound across the steps and its misses within the bound of the
    /// model's expectation. After the step down, bursts start as rarely as the short fit has
    /// seen, and last as long as the long fit remembers: once the link has planned twice
    /// [`Erasures::SHORT`] symbols (a refit and a whole short window) its repair share is below
    /// the lossy stretch's, and once it has planned [`Erasures::MEMORY`] more it is within three
    /// standard errors of a run that never lost anything, over the same blocks' time.
    fn follows_a_step_in_loss(kind: Kind, seed: u64) {
        use crate::lanes::model::Erasures;
        let (warm, clean) = (15.0, 60.0);
        let up = warm + clean;
        let down = up + FREEZE_EVERY.as_secs_f64();
        let seconds = down + 4.0 * clean;
        let stepped = Path {
            lossy: (up, down),
            ..lossy(kind, 0.1)
        };
        let never = Path {
            lossy: (0.0, 0.0),
            ..lossy(kind, 0.1)
        };
        let m = run(stepped, PRODUCTION, false, seed, seconds, warm);
        let base = run(never, PRODUCTION, false, seed, seconds, warm);
        let bound = poisson_bound(m.watched / FREEZE_EVERY.as_secs_f64());
        let expected = poisson_bound(m.expected);
        // When the link has planned `symbols` after the step down.
        let after = |symbols: usize| {
            let mut planned = 0;
            m.log
                .iter()
                .filter(|&&(at, ..)| at >= down)
                .find(|&&(_, n, ..)| {
                    planned += n;
                    planned >= symbols
                })
                .map_or(seconds, |&(at, ..)| at)
        };
        let (short, long) = (
            after(2 * Erasures::SHORT),
            after(2 * Erasures::SHORT + Erasures::MEMORY),
        );
        let share = |m: &Measured, from: f64, to: f64| {
            let (wire, repair) = m
                .log
                .iter()
                .filter(|&&(at, ..)| at >= from && at < to)
                .fold((0, 0), |(w, r), &(_, _, bw, br)| (w + bw, r + br));
            repair as f64 / wire.max(1) as f64
        };
        // The never-lossy run's per-block repair shares over the same time: their spread.
        let blocks: Vec<f64> = base
            .log
            .iter()
            .filter(|&&(at, ..)| at >= long)
            .map(|&(_, _, w, r)| r as f64 / w as f64)
            .collect();
        let mean = blocks.iter().sum::<f64>() / blocks.len().max(1) as f64;
        let var =
            blocks.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / blocks.len().max(1) as f64;
        let se = (var / blocks.len().max(1) as f64).sqrt();
        let lossy = share(&m, up, down);
        let fell = share(&m, short, long);
        let back = share(&m, long, seconds);
        let never_lossy = share(&base, long, seconds);
        eprintln!(
            "{kind:?} step: {} freezes (bound {bound}), {} missed ({:.1} expected, bound \
             {expected}); repair {:.3} before, {lossy:.3} lossy, {fell:.3} from {:.1} s after \
             the step down, {back:.3} from {:.1} s, against {never_lossy:.3} never lossy \
             (3 SE {:.3})",
            m.freezes,
            m.missed,
            m.expected,
            share(&m, warm, up),
            short - down,
            long - down,
            3.0 * se
        );
        assert!(
            m.freezes <= bound,
            "{kind:?}: {} freezes, bound {bound}",
            m.freezes
        );
        assert!(
            m.missed <= expected,
            "{kind:?}: {} missed, {:.1} expected, bound {expected}",
            m.missed,
            m.expected
        );
        assert!(
            fell < lossy && back <= never_lossy + 3.0 * se && long < seconds,
            "{kind:?}: repair {lossy} lossy, {fell} after a short fit, {back} after a long \
             one, {never_lossy} never lossy"
        );
    }

    #[test]
    fn a_step_in_loss_is_followed_on_datagram_lanes() {
        follows_a_step_in_loss(Kind::Datagram, 0x5851_F42D_4C95_7F35);
    }

    #[test]
    fn a_step_in_loss_is_followed_on_stream_lanes() {
        follows_a_step_in_loss(Kind::Streams, 0x5851_F42D_4C95_7F37);
    }

    #[test]
    fn the_loop_freezes_at_most_once_per_busy_interval_on_datagram_lanes() {
        freezes_at_most_once_per_busy_interval(Kind::Datagram, 0x5851_F42D_4C95_7F2D);
    }

    /// Streams, as many as the link wants: the symbols that leave in one stall, up to about
    /// 900 for a busy screen at 200 ms and 300 for production's.
    #[test]
    fn the_loop_freezes_at_most_once_per_busy_interval_on_stream_lanes() {
        freezes_at_most_once_per_busy_interval(Kind::Streams, 0x5851_F42D_4C95_7F2F);
    }

    /// Stream lanes held to 64, about as many as production's demand wants: the deadline
    /// waits a stall out for some blocks and not for the next, the boundary where the
    /// deadlines must stay in block order and each wait keep its own erasure fit.
    #[test]
    fn the_loop_freezes_at_most_once_per_busy_interval_at_the_stall_boundary() {
        freezes_at_most_once_per_busy_interval(
            Kind::Connections { count: 64 },
            0x5851_F42D_4C95_7F33,
        );
    }

    /// Six connections, as a browser opens to one host: too few to keep a stall's pieces
    /// apart, so the deadline waits stalls out.
    #[test]
    fn the_loop_freezes_at_most_once_per_busy_interval_on_connection_lanes() {
        freezes_at_most_once_per_busy_interval(
            Kind::Connections { count: 6 },
            0x5851_F42D_4C95_7F31,
        );
    }
}
