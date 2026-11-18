//! What the reports of a datagram lane tell of the path under it: the delay toward the relay
//! (its base and its spread, which a block's deadline waits), the erasures in send order (the
//! fits a block's repairs are sized by) and each track's frame rate. The rate the path carries
//! is not here: the transport under the lane owns it, and the daemon reads it from its own
//! socket ([`crate::session::sender`]).

use super::model::{Came, Correction, Erasures, Runs};
use super::wire::{tenths, LateArrival, OneWay, Seq, Stamp, Track};
use std::collections::VecDeque;
use std::time::Duration;

/// The delays the spread remembers, and the reported symbols between refits of the erasure
/// fit ([`super::link::Link`]). Stated as a precision: the standard error of a share read from
/// this many is at most one percentage point (at a share of one half, the worst):
/// `sqrt(0.25 / MEMORY) = 0.01`.
pub const MEMORY: usize = 2500;

/// How long the base delay's min filter remembers each delay (BBR draft §4.5.3:
/// "BBR.RTpropFilterLen ... 10 secs"): a queue the path holds for a while must not become its
/// base.
pub const BASE_WINDOW: Duration = Duration::from_secs(10);

/// The gaps between a track's frames its rate is read from. Stated as a precision: read from
/// this many gaps a rate has a relative standard error of a tenth (`1 / sqrt(100)`, the gaps
/// taken as a Poisson process's); the miss chance a block may have is inversely proportional
/// to the rate, and the chance falls geometrically with each repair, so a tenth of it moves m
/// by less than one repair.
pub const RATE_MEMORY: usize = 100;

/// The delays toward the relay: the way, and the most excess over the base of the last
/// [`MEMORY`] (the spread). On a datagram lane nothing waits on a resend: every arrival's
/// excess is the path's jitter or a queue under it, which a deadline must wait.
struct Delays {
    way: Way,
    spread: Extreme<true>,
}

/// The path, as the reports tell it.
pub struct Path {
    erasures: Erasures,
    up: Way,
    /// The delays toward the relay, once one is reported.
    down: Option<Delays>,
    /// Each track's frame rate: video's, then audio's.
    rates: [Rate; 2],
    /// The relay's measured round trip, from its attach: the spread until a report.
    round_trip: Duration,
}

impl Path {
    /// A session's path, from its lane's attach (stamped `sent` by the relay and `received` by
    /// the daemon, with the relay's measured `round_trip`).
    pub fn new(sent: Stamp, received: Stamp, round_trip: Duration) -> Path {
        Path {
            erasures: Erasures::new(),
            up: Way::new(received, OneWay::of(sent, received)),
            down: None,
            rates: [Rate::new(), Rate::new()],
            round_trip,
        }
    }

    /// The two fits ([`Erasures::fits`]).
    pub fn fits(&self) -> [Runs; 2] {
        self.erasures.fits()
    }

    /// The base round trip, once a delay each way has been seen.
    pub fn rtprop(&self) -> Option<Duration> {
        self.down
            .as_ref()
            .map(|down| super::wire::from_tenths(down.way.base() + self.up.base()))
    }

    /// The spread of arrivals over the base delay down; before any is reported, the relay's
    /// measured round trip, which the base taken from the delays toward the daemon (as if
    /// those took no time) is short of by at most that.
    pub fn spread(&self) -> Duration {
        match &self.down {
            None => self.round_trip,
            Some(down) => match down.spread.get() {
                Some(most) => super::wire::from_tenths(most - down.way.least.get()),
                None => Duration::ZERO,
            },
        }
    }

    /// What a block's deadline waits past the moment the socket accepted it, in tenths, on the
    /// relay's clock less the daemon's: the base delay down and the spread. Before any delay
    /// toward the relay is known, the base one-way delay is taken as the offset the delays
    /// toward the daemon show, as if those took no time.
    pub fn after(&self) -> i64 {
        let base = match &self.down {
            Some(down) => down.way.base(),
            None => -self.up.base(),
        };
        base + tenths(self.spread())
    }

    /// A frame of `track` starts to leave at `start`: its track's rate.
    pub fn frame(&mut self, track: Track, start: Stamp) {
        self.rates[Kind::of(track) as usize].push(start);
    }

    /// Frames of a track a second: video updates, or Opus packets.
    pub fn rate(&self, kind: Kind) -> f64 {
        self.rates[kind as usize].get()
    }

    /// A report's (or an attach's) delay toward the daemon.
    pub fn up(&mut self, sent: Stamp, received: Stamp) {
        self.up.push(received, OneWay::of(sent, received));
    }

    /// A piece's or repair's delay toward the relay, reported (on time or late) at `at`.
    pub fn down(&mut self, d: OneWay, at: Stamp) {
        let down = self.down.get_or_insert_with(|| Delays {
            way: Way::new(at, d),
            spread: Extreme::new(),
        });
        let v = down.way.push(at, d);
        down.spread.push(v);
    }

    /// What became of a block's symbols, in send order, the blocks in the order they were sent.
    pub fn block(&mut self, block: Seq, came: impl Iterator<Item = Came>) {
        self.erasures.block(block, came);
    }

    /// A symbol came after its block's deadline, reported at `at`: its delay, and that it came.
    pub fn late(&mut self, late: LateArrival, at: Stamp) -> Correction {
        self.down(late.delay, at);
        self.erasures.late(late.block, late.position)
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

/// The least of the samples of the last [`BASE_WINDOW`], of which there is always one (the
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
        let window = tenths(BASE_WINDOW);
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
    /// arrivals, and a deadline waits the base delay and the spread, on the relay's clock.
    #[test]
    fn the_offset_cancels() {
        let mut rng = Xorshift::new(0xA54F_F53A_5F1D_36F1);
        for _ in 0..50 {
            let offset = rng.next() as u32;
            let (down, up) = (rng.range(10, 1500) as i64, rng.range(10, 1500) as i64);
            let at = |daemon: u32| Stamp(daemon.wrapping_add(offset));
            let mut t: u32 = rng.next() as u32;
            let mut p = Path::new(at(t), Stamp(t.wrapping_add(up as u32 + 7)), Duration::ZERO);
            p.up(at(t), Stamp(t.wrapping_add(up as u32)));
            p.down(OneWay::of(Stamp(t), at(t).plus(down)), Stamp(t));
            let mut most = 0i64;
            for _ in 0..500 {
                t = t.wrapping_add(rng.range(1, 50) as u32);
                let excess = rng.below(3000) as i64;
                most = most.max(excess);
                p.down(OneWay::of(Stamp(t), at(t).plus(down + excess)), Stamp(t));
                p.up(
                    at(t),
                    Stamp(t.wrapping_add((up + rng.below(100) as i64) as u32)),
                );
            }
            assert_eq!(p.rtprop(), Some(super::super::wire::from_tenths(down + up)));
            assert_eq!(p.spread(), super::super::wire::from_tenths(most));
            assert_eq!(Stamp(t).plus(p.after()), at(t).plus(down + most));
        }
    }
}
