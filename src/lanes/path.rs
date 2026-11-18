//! The path, as one model: BBR's state (Cardwell et al., "BBR: Congestion-Based Congestion
//! Control", ACM Queue 14(5), 2016; and draft-cardwell-iccrg-bbr-congestion-control-02)
//! and the erasure channel, fed by reports. Every control is a function of it.
//!
//! - **BtlBw**, the bottleneck's delivery rate: the most any arrival's rate sample showed over
//!   the last [`BTLBW_ROUNDS`] round trips ([`Path::delivered`]).
//! - **RTprop**, the base round trip: the least delay each way over the last
//!   [`RTPROP_WINDOW`] (each holds the two clocks' offset, which the sum cancels).
//! - **The erasure channel**: the runs of erasures and of arrivals in send order
//!   ([`Erasures`]), one for each [`Wait`] a deadline can have, since what arrives by a
//!   deadline depends on how long it waits.
//! - **The spread** of arrivals that waited on no resend, over the base delay down.
//! - **Each track's frame rate**: video updates and Opus packets a second.
//!
//! From them: pacing = gain × BtlBw / (1 − p), the gain BBR's ProbeBW cycle ([`CYCLE`]) at
//! the phase a frame starts in, each phase one RTprop; the in-flight cap =
//! [`CWND_GAIN`] × BtlBw × RTprop; a block's deadline = when its last byte is
//! through the bottleneck at BtlBw + the base delay + the spread; and how long a miss leaves the picture stale, from RTprop. Before the cycle, STARTUP: the
//! gain [`STARTUP_GAIN`] until BtlBw has not grown by [`STARTUP_GROWTH`] in
//! [`STARTUP_ROUNDS`] rounds, then the cycle from its draining phase.
//!
//! One departure from BBR: there is no app-limited marking. A block's delivery sample is
//! taken over its own crossing alone (from its first message leaving to its last arriving),
//! so a pause between blocks never dilutes it; what leaves is the screen's data and its
//! repairs, the pacing a ceiling, never a target to fill; a probe phase adds the gain's share
//! of a block's symbols as more repairs of it, useful to the relay, so the data rate does not
//! move.
//!
//! The spread: a resend reaches the relay no sooner than a base round trip after its piece
//! first left, so an arrival later than that past the base delay may have waited on one and
//! one earlier cannot have; arrivals after a block's deadline are reported too (as late), or
//! the spread could only ever be read below the deadline it sets. Pieces queued on a stream
//! behind a resent one are not told from jitter by this rule and widen the spread on stream
//! lanes.

use super::model::{Arrival, Erasures, Runs};
use super::wire::{from_tenths, tenths, OneWay, Stamp, Track, DATAGRAM, TENTH, WINDOW};
use std::collections::VecDeque;
use std::time::Duration;

/// ProbeBW's pacing gains, one phase each, a phase lasting one RTprop: BBR's published cycle
/// (Cardwell et al. 2016: "a pacing_gain ... cycle of eight phases: 5/4, 3/4, 1, 1, 1, 1, 1,
/// 1"). The first probes, the second drains what the probe queued.
pub const CYCLE: [f64; 8] = [1.25, 0.75, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0];

/// The cycle's draining phase, where it starts after STARTUP: the index of [`CYCLE`]'s least
/// gain.
const DRAIN: usize = {
    let mut at = 0;
    let mut i = 1;
    while i < CYCLE.len() {
        if CYCLE[i] < CYCLE[at] {
            at = i;
        }
        i += 1;
    }
    at
};

/// STARTUP's gain, 2/ln 2, the least that doubles the delivery rate a round trip (BBR draft
/// §4.3.2: "BBRHighGain = 2/ln(2)").
pub const STARTUP_GAIN: f64 = 2.0 / std::f64::consts::LN_2;

/// STARTUP ends once BtlBw has not grown by this factor (BBR draft §4.3.2.2: "grows by at
/// least 25%") ...
pub const STARTUP_GROWTH: f64 = 1.25;

/// ... in this many rounds running (BBR draft §4.3.2.2: "for three round trips").
pub const STARTUP_ROUNDS: u32 = 3;

/// The in-flight cap's gain over the bandwidth-delay product (Cardwell et al. 2016: "a
/// cwnd_gain of 2").
pub const CWND_GAIN: f64 = 2.0;

/// The round trips BtlBw's max filter remembers (BBR draft §4.5.2: "BBRBtlBwFilterLen = 10
/// packet-timed round trips").
pub const BTLBW_ROUNDS: u64 = 10;

/// What may be sent a round trip before any delivery is measured: ten symbols, the initial
/// window of RFC 6928, for the same reason (nothing is known of the path yet). BtlBw never
/// reads less than this a base round trip.
pub const START: usize = 10;

/// What a round trip is taken to be before one is measured: RFC 6298's initial
/// retransmission timeout, the time TCP gives a first segment when nothing at all is known of
/// the path (§2.1: "the sender SHOULD set RTO <- 1 second"). It is also the spread before any
/// arrival is reported.
pub const FIRST_ROUND_TRIP: Duration = Duration::from_secs(1);

/// The samples the spread and the stall remember, and the reported symbols between refits of
/// the erasure fit ([`super::link::Link`]). Stated as a precision: the standard error of a share
/// read from this many is at most one percentage point (at a share of one half, the worst):
/// `sqrt(0.25 / MEMORY) = 0.01`.
pub const MEMORY: usize = 2500;

/// How long RTprop's min filter remembers each delay (BBR draft §4.5.3: "BBR.RTpropFilterLen
/// ... 10 secs"). A time, not a count: a queue the path holds for a while must not become
/// its base, and a count of samples covers less time the faster the path delivers.
pub const RTPROP_WINDOW: Duration = Duration::from_secs(10);

/// The least a resend's stall lasts, in RTprop: a round trip and RACK's reordering window of a
/// quarter of one ([`Path::lane_stall`]).
pub const RACK_STALL: f64 = 1.25;

/// The base round trips a miss takes to mend, past the block's own sending and the spread:
/// three one-way crossings (the block's way down, the report's way up, the next block's way
/// down), each half a round trip with the two directions taken as equal (RFC 5905 §8).
const RECOVERY_ROUND_TRIPS: f64 = 1.5;

/// The gaps between a track's frames its rate is read from. Stated as a precision: read from
/// this many gaps a rate has a relative standard error of a tenth (`1 / sqrt(100)`, the gaps
/// taken as a Poisson process's); the miss chance a block may have is inversely proportional
/// to the rate, and the chance falls geometrically with each repair, so a tenth of it moves m
/// by less than one repair.
pub const RATE_MEMORY: usize = 100;

/// Where the gain stands.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    /// STARTUP: the best BtlBw so far and the rounds since it last grew by a quarter.
    Startup { best: f64, flat: u32 },
    /// ProbeBW, at this phase of [`CYCLE`] since this stamp.
    Cycle { phase: usize, since: Stamp },
}

/// One message of a block that arrived by its deadline: when it left, past the block's start,
/// and when it arrived, past the block's start and the base delay down
/// ([`Path::past_base`]), both in tenths; and its bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Piece {
    pub left: i64,
    pub arrived: i64,
    pub bytes: usize,
}

/// The delays toward the relay: the way, and the most of the last [`MEMORY`] that waited on
/// no resend (the spread) and that did (the stall), each as its delay.
struct Delays {
    way: Way,
    spread: Extreme<true>,
    stall: Extreme<true>,
}

/// What a block's deadline waits for past its crossing and the base delay: the spread alone,
/// or a stall too, when the lanes are too few to keep a stalled lane's later pieces out of it
/// ([`super::link::Link::plan`]). The same path erases differently under each, so each has its
/// own erasure channel: one fit of both would follow neither when the lanes the link wants
/// rise and fall about the lanes it has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wait {
    Spread,
    Stall,
}

/// The path.
pub struct Path {
    erasures: [Erasures; 2],
    up: Way,
    /// The delays toward the relay, once one is reported.
    down: Option<Delays>,
    /// BtlBw's samples: the most of each round, newest last.
    btlbw: VecDeque<(u64, f64)>,
    /// The round now, and when it began (on the daemon's clock): it ends at the report of a
    /// block that started to leave after that.
    round: u64,
    round_began: Stamp,
    mode: Mode,
    /// Video updates' rate.
    /// Each track's frame rate: video's, then audio's.
    rates: [Rate; 2],
}

impl Path {
    /// A session's path, from its first lane's attach (stamped `sent` by the relay and
    /// `received` by the daemon).
    pub fn new(sent: Stamp, received: Stamp) -> Path {
        Path {
            erasures: [Erasures::new(), Erasures::new()],
            up: Way::new(received, OneWay::of(sent, received)),
            down: None,
            btlbw: VecDeque::with_capacity(BTLBW_ROUNDS as usize + 1),
            round: 0,
            round_began: received,
            mode: Mode::Startup { best: 0.0, flat: 0 },
            rates: [Rate::new(), Rate::new()],
        }
    }

    /// The two fits ([`Erasures::fits`]) of the blocks whose deadline waits `wait`.
    pub fn fits(&self, wait: Wait) -> [Runs; 2] {
        self.erasures[wait as usize].fits()
    }

    /// RTprop, once a delay each way has been seen.
    pub fn rtprop(&self) -> Option<Duration> {
        self.down
            .as_ref()
            .map(|down| from_tenths(down.way.base() + self.up.base()))
    }

    fn rtprop_or_first(&self) -> Duration {
        self.rtprop().unwrap_or(FIRST_ROUND_TRIP)
    }

    /// BtlBw, in bytes a second: never under [`START`] symbols a base round trip.
    pub fn btlbw(&self) -> f64 {
        let floor = (START * DATAGRAM) as f64 / self.rtprop_or_first().as_secs_f64();
        self.btlbw.iter().map(|&(_, r)| r).fold(floor, f64::max)
    }

    /// The gain a block starting now is sent at.
    fn gain(&self) -> f64 {
        match self.mode {
            Mode::Startup { .. } => STARTUP_GAIN,
            Mode::Cycle { phase, .. } => CYCLE[phase],
        }
    }

    /// The pacing rate, in bytes a second: gain × BtlBw, BBR's own. No share for loss: BtlBw
    /// is the most any stretch of arrivals delivered, and a stretch that lost nothing shows
    /// the bottleneck whole, so a loss correction on top of the max paced past the path (in
    /// simulation by 5%, a queue that grew whenever the pacer was busy).
    pub fn pacing(&self) -> f64 {
        self.gain() * self.btlbw()
    }

    /// The most the lanes may hold unreported, in bytes: [`CWND_GAIN`] × BtlBw × RTprop, as
    /// BBR's congestion window, and BtlBw × the time a report waits past an ack's round trip
    /// (the spread, and `wait`, the stall a deadline waits out): an ack leaves on arrival, a
    /// report at its block's deadline, and what has arrived and waits on it holds no room
    /// on the path. Never more than [`WINDOW`] symbols, which the relay holds at most.
    pub fn in_flight(&self, wait: Duration) -> usize {
        let held =
            CWND_GAIN * self.rtprop_or_first().as_secs_f64() + (self.spread() + wait).as_secs_f64();
        let cap = self.btlbw() * held;
        cap.min((WINDOW * DATAGRAM) as f64) as usize
    }

    /// The spread of arrivals that waited on no resend; before any arrival is reported, a
    /// whole first round trip.
    pub fn spread(&self) -> Duration {
        match &self.down {
            None => FIRST_ROUND_TRIP,
            Some(down) => match down.spread.get() {
                Some(most) => from_tenths(most - down.way.least.get()),
                // Every arrival so far waited on a resend: none spread past the base.
                None => Duration::ZERO,
            },
        }
    }

    /// The stall: the most any arrival that waited on a resend came past the base delay
    /// down. A stream lane that loses a piece holds every later one until the resend
    /// arrives, so this is also how long a lane stalls; zero while none has waited on one.
    pub fn stall(&self) -> Duration {
        match &self.down {
            Some(down) => match down.stall.get() {
                Some(most) => from_tenths(most - down.way.least.get()),
                None => Duration::ZERO,
            },
            None => Duration::ZERO,
        }
    }

    /// How long a stream lane may stall, for keeping a lane's pieces apart: the stall, but
    /// never less than a resend can take. A stream's sender declares a piece lost no sooner
    /// than a reordering window of a quarter of the least round trip after a later one is
    /// acknowledged (RACK, RFC 8985 §6.2: "RACK.reo_wnd ... min_RTT / 4"), and the resend then
    /// crosses: [`RACK_STALL`] RTprop past the base delay, before any resend is seen.
    pub fn lane_stall(&self) -> Duration {
        self.stall().max(self.rtprop_or_first().mul_f64(RACK_STALL))
    }

    /// The deadline, on the relay's clock, of a block whose last byte is through the
    /// bottleneck at `through` (on the daemon's): then the base delay and the spread. Before
    /// any delay toward the relay is known, the base one-way delay is taken as the offset the
    /// delays toward the daemon show, as if those took no time.
    pub fn due(&self, through: Stamp) -> Stamp {
        let base = match &self.down {
            Some(down) => down.way.base(),
            None => -self.up.base(),
        };
        through.plus(base).plus(tenths(self.spread()))
    }

    /// How long after a block starts to leave its miss is mended by the next: its deadline's
    /// wait (the base delay down, its sending time, the spread), the report's way up and the
    /// next block's way down, 1.5 RTprop with the two directions taken as equal (as NTP takes
    /// them, RFC 5905 §8, since only their sum is measured).
    pub fn recovery(&self, sending: Duration) -> Duration {
        self.rtprop_or_first().mul_f64(RECOVERY_ROUND_TRIPS) + sending + self.spread()
    }

    /// The repairs a block of `n` symbols is padded with at a gain over 1: the gain's share of
    /// its symbols, so that it leaves at the gain with its data unchanged.
    pub fn pad(&self, n: usize) -> usize {
        match self.gain() {
            g if g > 1.0 => ((g - 1.0) * n as f64).ceil() as usize,
            _ => 0,
        }
    }

    /// A frame of `track` starts to leave at `start`: its track's rate, and the cycle's phase
    /// (one RTprop each).
    pub fn frame(&mut self, track: Track, start: Stamp) {
        self.rates[Kind::of(track) as usize].push(start);
        let rtprop = tenths(self.rtprop_or_first());
        if let Mode::Cycle { phase, since } = self.mode {
            if since.to(start) >= rtprop {
                self.mode = Mode::Cycle {
                    phase: (phase + 1) % CYCLE.len(),
                    since: start,
                };
            }
        }
    }

    /// Frames of a track a second: video updates, or Opus packets.
    pub fn rate(&self, kind: Kind) -> f64 {
        self.rates[kind as usize].get()
    }

    /// How far `d`, a delay toward the relay, is past the base delay down, in tenths.
    pub fn past_base(&self, d: OneWay) -> i64 {
        match &self.down {
            Some(down) => d.over(down.way.first) - down.way.least.get(),
            // No delay toward the relay is known before this one: it is the base.
            None => 0,
        }
    }

    /// A report's (or an attach's) delay toward the daemon.
    pub fn up(&mut self, sent: Stamp, received: Stamp) {
        self.up.push(received, OneWay::of(sent, received));
    }

    /// A piece's or repair's delay toward the relay, reported (on time or late) at `at`.
    pub fn down(&mut self, d: OneWay, at: Stamp) {
        let rtt = self.up.base();
        let down = self.down.get_or_insert_with(|| Delays {
            way: Way::new(at, d),
            spread: Extreme::new(),
            stall: Extreme::new(),
        });
        let v = down.way.push(at, d);
        let excess = v - down.way.least.get();
        match excess < down.way.base() + rtt {
            true => down.spread.push(v),
            false => down.stall.push(v),
        }
    }

    /// What became of one symbol of a block whose deadline waited `wait`, in send order.
    pub fn erasure(&mut self, wait: Wait, a: Arrival) {
        self.erasures[wait as usize].record(a);
    }

    /// A block that started to leave at `start`, its report received at `received`, its
    /// `pieces` that arrived in the order they left: BtlBw's samples, and the round.
    ///
    /// Each arrival is BBR's rate sample (the draft's §4.1.2): the bytes delivered since the
    /// block's first piece to leave, over the longer of the time they took to leave and the
    /// time from it leaving to this one arriving, past the base delay. Delivered means through
    /// the bottleneck: the path is first in, first out, so every piece that left before this
    /// one and arrived at all went through before it, even one a stalled stream lane then held
    /// at the relay's end. No sample reads faster than the pieces left, and a stalled piece
    /// lowers only its own sample.
    pub fn delivered(&mut self, start: Stamp, pieces: &[Piece], received: Stamp) {
        if self.round_began.to(start) >= 0 {
            self.round += 1;
            self.round_began = received;
            self.next_round();
        }
        let Some((first, rest)) = pieces.split_first() else {
            return;
        };
        let mut bytes = 0;
        let mut best: Option<f64> = None;
        for p in rest {
            bytes += p.bytes;
            // Stamped to a tenth: up to a tenth longer than it reads.
            let over = (p.arrived - first.left + tenths(TENTH)).max(p.left - first.left);
            let sample = bytes as f64 / from_tenths(over).as_secs_f64();
            best = Some(best.map_or(sample, |b| b.max(sample)));
        }
        let Some(sample) = best else {
            return;
        };
        match self.btlbw.back_mut() {
            Some((r, most)) if *r == self.round => *most = most.max(sample),
            _ => self.btlbw.push_back((self.round, sample)),
        }
        while self
            .btlbw
            .front()
            .is_some_and(|&(r, _)| r + BTLBW_ROUNDS <= self.round)
        {
            self.btlbw.pop_front();
        }
    }

    /// A round ended: STARTUP looks at whether BtlBw still grows.
    fn next_round(&mut self) {
        if let Mode::Startup { best, flat } = self.mode {
            let now = self.btlbw();
            self.mode = match (now >= best * STARTUP_GROWTH, flat + 1 >= STARTUP_ROUNDS) {
                (true, _) => Mode::Startup { best: now, flat: 0 },
                (false, false) => Mode::Startup {
                    best,
                    flat: flat + 1,
                },
                (false, true) => Mode::Cycle {
                    phase: DRAIN,
                    since: self.round_began,
                },
            };
        }
    }
}

/// The two tracks, as their rates are kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Video,
    Audio,
}

impl Kind {
    pub fn of(track: Track) -> Kind {
        match track {
            Track::Video { .. } => Kind::Video,
            Track::Audio => Kind::Audio,
        }
    }
}

/// The last [`RATE_MEMORY`] frames of a track, when each started to leave.
struct Rate {
    starts: VecDeque<Stamp>,
}

impl Rate {
    fn new() -> Rate {
        Rate {
            starts: VecDeque::with_capacity(RATE_MEMORY + 1),
        }
    }

    fn push(&mut self, start: Stamp) {
        if self.starts.len() == RATE_MEMORY + 1 {
            self.starts.pop_front();
        }
        self.starts.push_back(start);
    }

    /// Frames a second over the remembered gaps; none until there is a gap.
    fn get(&self) -> f64 {
        match (self.starts.front(), self.starts.back()) {
            (Some(&first), Some(&last)) if first.to(last) > 0 => {
                (self.starts.len() - 1) as f64 / (first.to(last) as f64 / 1e4)
            }
            _ => 0.0,
        }
    }
}

/// The least (or, with `MOST`, the most) of the last [`MEMORY`] samples, by a deque of the
/// samples that can still be it.
struct Extreme<const MOST: bool> {
    count: u64,
    keep: VecDeque<(u64, i64)>,
}

impl<const MOST: bool> Extreme<MOST> {
    fn new() -> Self {
        Extreme {
            count: 0,
            keep: VecDeque::with_capacity(MEMORY),
        }
    }

    fn push(&mut self, v: i64) {
        while let Some(&(_, back)) = self.keep.back() {
            if (MOST && back <= v) || (!MOST && back >= v) {
                self.keep.pop_back();
            } else {
                break;
            }
        }
        self.keep.push_back((self.count, v));
        self.count += 1;
        while let Some(&(at, _)) = self.keep.front() {
            if at + (MEMORY as u64) < self.count {
                self.keep.pop_front();
            } else {
                break;
            }
        }
    }

    fn get(&self) -> Option<i64> {
        self.keep.front().map(|&(_, v)| v)
    }
}

/// The least of the samples of the last [`RTPROP_WINDOW`], of which there is always one (the
/// newest is always kept).
struct Least {
    keep: VecDeque<(Stamp, i64)>,
    least: i64,
}

impl Least {
    fn new(at: Stamp, first: i64) -> Least {
        Least {
            keep: VecDeque::from([(at, first)]),
            least: first,
        }
    }

    fn push(&mut self, at: Stamp, v: i64) {
        while self.keep.back().is_some_and(|&(_, back)| back >= v) {
            self.keep.pop_back();
        }
        self.keep.push_back((at, v));
        let window = tenths(RTPROP_WINDOW);
        while self.keep.len() > 1 && self.keep.front().is_some_and(|&(t, _)| t.to(at) > window) {
            self.keep.pop_front();
        }
        if let Some(&(_, least)) = self.keep.front() {
            self.least = least;
        }
    }

    fn get(&self) -> i64 {
        self.least
    }
}

/// Delays of one direction, each as tenths of a millisecond over the first.
struct Way {
    first: OneWay,
    least: Least,
}

impl Way {
    fn new(at: Stamp, first: OneWay) -> Way {
        Way {
            first,
            least: Least::new(at, 0),
        }
    }

    fn push(&mut self, at: Stamp, d: OneWay) -> i64 {
        let v = d.over(self.first);
        self.least.push(at, v);
        v
    }

    /// The least, as tenths of a millisecond.
    fn base(&self) -> i64 {
        self.first.0 as i32 as i64 + self.least.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xorshift::Xorshift;

    /// Over two clocks with any offset (wrapping included), delays each way drawn above a base
    /// each way: RTprop is the two bases' sum to the tenth, the spread the most excess of the
    /// arrivals under a round trip late, and a block's deadline the base delay and the spread
    /// after its last byte is through, on the relay's clock.
    #[test]
    fn the_offset_cancels() {
        let mut rng = Xorshift::new(0xA54F_F53A_5F1D_36F1);
        for _ in 0..50 {
            let offset = rng.next() as u32;
            let (down, up) = (rng.range(10, 1500) as i64, rng.range(10, 1500) as i64);
            let at = |daemon: u32| Stamp(daemon.wrapping_add(offset));
            let mut t: u32 = rng.next() as u32;
            let mut p = Path::new(at(t), Stamp(t.wrapping_add(up as u32 + 7)));
            p.up(at(t), Stamp(t.wrapping_add(up as u32)));
            p.down(OneWay::of(Stamp(t), at(t).plus(down)), Stamp(t));
            let mut most = 0i64;
            for _ in 0..500 {
                t = t.wrapping_add(rng.range(1, 50) as u32);
                let excess = match rng.below(10) {
                    0 => (down + up) + rng.below(3000) as i64,
                    _ => rng.below(200) as i64,
                };
                if excess < down + up {
                    most = most.max(excess);
                }
                p.down(OneWay::of(Stamp(t), at(t).plus(down + excess)), Stamp(t));
                p.up(
                    at(t),
                    Stamp(t.wrapping_add((up + rng.below(100) as i64) as u32)),
                );
            }
            assert_eq!(p.rtprop(), Some(from_tenths(down + up)));
            assert_eq!(p.spread(), from_tenths(most));
            let sending = Duration::from_millis(rng.below(30));
            assert_eq!(
                p.due(Stamp(t).plus(tenths(sending))),
                at(t).plus(down + tenths(sending) + most)
            );
        }
    }

    /// A path that erases `p` of what it is sent and carries at most `capacity` of what
    /// arrives: blocks sent back to back at the pacing rate, each delivering at the lesser of
    /// its sending rate and the capacity (BBR's sample), reach BtlBw within one probe of the
    /// capacity from ten symbols a round trip, and hold it there, never past the capacity
    /// times the cycle's highest gain.
    #[test]
    fn btlbw_finds_the_capacity() {
        let mut rng = Xorshift::new(0x510E_527F_ADE6_82D1);
        for case in 0..40 {
            let loss = rng.below(300) as f64 / 1000.0;
            let capacity = 1e5 * (1.0 + rng.below(200) as f64);
            let rtt = rng.range(500, 3000) as i64;
            let mut p = Path::new(Stamp(0), Stamp(0));
            p.up(Stamp(0), Stamp(0));
            p.down(OneWay::of(Stamp(0), Stamp(rtt as u32)), Stamp(0));
            let mut t = Stamp(0);
            let mut flights: VecDeque<(Stamp, Stamp, Vec<Piece>)> = VecDeque::new();
            let mut last = 0.0;
            let size = DATAGRAM;
            for _ in 0..20_000 {
                let pacing = p.pacing();
                p.frame(Track::Audio, t);
                let sending = Duration::from_secs_f64((20 * size) as f64 / pacing);
                // Twenty messages paced over the sending time, those the loss spares queued
                // through the capacity in order.
                let (mut pieces, mut free) = (Vec::new(), 0.0f64);
                for j in 0..20 {
                    let left = sending.as_secs_f64() * j as f64 / 20.0;
                    if (j as f64 + 0.5) / 20.0 < loss {
                        continue;
                    }
                    free = free.max(left) + size as f64 / capacity;
                    pieces.push(Piece {
                        arrived: tenths(Duration::from_secs_f64(free)),
                        left: tenths(Duration::from_secs_f64(left)),
                        bytes: size,
                    });
                }
                pieces.sort_unstable();
                let start = t;
                t = t.plus(tenths(sending));
                flights.push_back((t.plus(rtt), start, pieces));
                while flights.front().is_some_and(|(at, _, _)| at.to(t) >= 0) {
                    let (at, start, pieces) = flights.pop_front().unwrap();
                    p.delivered(start, &pieces, at);
                }
                last = p.btlbw();
            }
            // The best sample is the whole block's: its bytes after the first over their
            // crossing, and a tenth more for the stamps' resolution.
            let n = (0..20).filter(|&j| (j as f64 + 0.5) / 20.0 >= loss).count() as f64;
            let crossing = n * size as f64 / capacity;
            let carried = (n - 1.0) * size as f64 / (crossing + TENTH.as_secs_f64());
            assert!(
                last >= carried * 0.9 && last <= capacity * CYCLE[0] * 1.01,
                "case {case}: BtlBw {last}, the path carries {carried}"
            );
        }
    }
}
