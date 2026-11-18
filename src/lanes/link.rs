//! A datagram lane's view, from its reports: the erasure fits each block's repairs are sized
//! by, the delays a block's deadline waits, and each track's rate. A stream lane has none of
//! this: its transport delivers every frame, in order, and no frame has a deadline.
//!
//! - **m** is the least whose miss chance, exact under the larger of two fits of the reported
//!   erasures ([`super::model::Tables`]), meets the freeze requirement ([`FREEZE_EVERY`],
//!   [`Link::target`]). The fits are refitted every [`MEMORY`] reported symbols, so that one
//!   fit sizes a stretch of blocks.
//! - **The deadline** of a block waits the base delay toward the relay and the spread past the
//!   moment the lane's socket accepted the block ([`Link::after`]).
//! - **The data's share** of what the lane carries ([`Link::share`]): the frames' bytes over
//!   the bytes their blocks put on the wire, repairs and headers with them. The encoder's
//!   target is the lane's drain rate times it.

use super::model::{Arrival, Tables};
use super::path::{Kind, Path, MEMORY};
use super::wire::{symbols, Fate, Head, Report, Stamp, Track, HEADER, SYMBOL, WINDOW};
use std::num::NonZeroUsize;
use std::time::Duration;

/// How often a busy screen may freeze, picture or sound: at most once in this long. A video
/// update missed freezes the picture until the update after the miss is known; an Opus packet
/// missed is a dropout of the sound. At f frames a second of a track, a block may then miss
/// with chance r* = 1 / (f × FREEZE_EVERY) ([`Link::target`]), so that f × r* of the track's
/// blocks a second miss, one per FREEZE_EVERY. How long a freeze lasts does not enter: it is
/// the freeze, not its length, the requirement counts.
///
/// kind: requirement. The owner's UX requirement, in its natural unit (one freeze per N
///   minutes of busy screen, N = 8), stated 2026-11-18; it replaces a share-of-time target
///   whose recovery time cancelled out of the derivation, and audio's share of packets lost.
/// unit: time of a busy screen (one whose picture changes, or whose sound plays) per freeze.
/// scope: every block, at the measured frame rate of each track it carries; a block with both
///   takes the smaller r*.
/// evidence: `lanes::sim::tests::the_loop_freezes_at_most_once_per_busy_interval`, which runs
///   the full loop for production's workload and a busy screen's at 100 and 200 ms, with
///   desktop audio off and on, and asserts the picture's freezes and the sound's dropouts
///   counted are each within the Poisson bound of one per FREEZE_EVERY, and the misses within
///   it of the model's expectation.
/// revalidate: when the owner restates the requirement, or the loss model, a track's rate
///   measure or the release of missed frames changes.
pub const FREEZE_EVERY: Duration = Duration::from_secs(8 * 60);

/// A block's repairs, and the chance it misses with them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Plan {
    pub m: u16,
    pub miss: f64,
}

/// A block sent and not yet reported: its head, its members' tracks in order (in a vector the
/// daemon may take back from the flight and reuse), and its plan's chance of a miss.
#[derive(Clone, Debug, PartialEq)]
pub struct Flight {
    pub head: Head,
    pub tracks: Vec<Track>,
    pub miss: f64,
}

/// A report that does not fit its block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mismatch {
    /// It is of another block.
    Block,
    /// It has another count of symbols.
    Symbols,
}

/// The datagram lane's view.
pub struct Link {
    path: Path,
    /// The miss chances the blocks are sized by, and the reported symbols since they were
    /// fitted: the tables live from one refit to the next, so a plan pays only for block sizes
    /// no plan of the fit asked before.
    tables: Tables,
    since: usize,
    /// The miss chance of the last video plan.
    miss: f64,
    /// The frames' bytes and the bytes they take on the wire, of the blocks planned since the
    /// fits were refitted and in the stretch before.
    share: [(usize, usize); 2],
}

impl Link {
    /// A session's link, from its lane's attach (stamped `sent` by the relay and `received` by
    /// the daemon, with the relay's measured `round_trip`).
    pub fn new(sent: Stamp, received: Stamp, round_trip: Duration) -> Link {
        let path = Path::new(sent, received, round_trip);
        Link {
            tables: Tables::new(path.fits()),
            path,
            since: 0,
            miss: 0.0,
            share: [(0, 0); 2],
        }
    }

    /// The repairs of a block of `bytes` whose members are of `tracks`, starting to leave at
    /// `start`: the least whose miss chance meets the target of every member.
    pub fn plan(&mut self, bytes: NonZeroUsize, tracks: &[Track], start: Stamp) -> Plan {
        for track in tracks {
            self.path.frame(*track, start);
        }
        let sources = symbols(bytes);
        let k = sources.get();
        let choice = self
            .tables
            .repairs(sources, self.target(tracks), WINDOW - k);
        if tracks.iter().any(|t| matches!(t, Track::Video { .. })) {
            self.miss = choice.miss;
        }
        let n = k + choice.m;
        self.share[0].0 += bytes.get();
        self.share[0].1 += bytes.get() + choice.m * SYMBOL + n * HEADER;
        Plan {
            m: choice.m as u16,
            miss: choice.miss,
        }
    }

    /// The miss chance a block of `tracks` may have: the least its members allow, each
    /// r* = 1 / (f × [`FREEZE_EVERY`]) at its track's measured frame rate f; before a rate is
    /// measured, at most every block's own freeze.
    fn target(&self, tracks: &[Track]) -> f64 {
        let every = FREEZE_EVERY.as_secs_f64();
        tracks
            .iter()
            .map(|&track| 1.0 / (self.path.rate(Kind::of(track)) * every).max(1.0))
            .fold(1.0, f64::min)
    }

    /// What a block's deadline waits past the moment its lane's socket accepted it, in tenths,
    /// on the relay's clock less the daemon's ([`Path::after`]).
    pub fn after(&self) -> i64 {
        self.path.after()
    }

    /// The spread of arrivals over the base delay down.
    pub fn spread(&self) -> Duration {
        self.path.spread()
    }

    /// The frames' share of the bytes their blocks put on the wire lately; all of it before any
    /// block is planned.
    pub fn share(&self) -> f64 {
        let (bytes, wire) = (
            self.share[0].0 + self.share[1].0,
            self.share[0].1 + self.share[1].1,
        );
        match wire {
            0 => 1.0,
            _ => bytes as f64 / wire as f64,
        }
    }

    /// The report of `flight`'s block, received at `received` on the daemon's clock.
    pub fn reported(
        &mut self,
        flight: &Flight,
        report: &Report<'_>,
        received: Stamp,
    ) -> Result<(), Mismatch> {
        let head = &flight.head;
        if report.block != head.block {
            return Err(Mismatch::Block);
        }
        let n = head.n();
        if let Fate::Seen { arrivals, .. } = report.fate {
            if arrivals.n() != n {
                return Err(Mismatch::Symbols);
            }
        }
        self.path.up(report.sent, received);
        match report.fate {
            Fate::Unseen => (0..n).for_each(|_| self.path.erasure(Arrival::Erased)),
            Fate::Seen { arrivals, .. } => {
                for a in arrivals.iter() {
                    match a {
                        None => self.path.erasure(Arrival::Erased),
                        Some(d) => {
                            self.path.erasure(Arrival::Arrived);
                            self.path.down(d, received);
                        }
                    }
                }
            }
        }
        for d in report.late.iter() {
            self.path.down(d, received);
        }
        self.since += n;
        if self.since >= MEMORY {
            self.since = 0;
            // The new tables reckoned at the refit to twice the largest block the old ones
            // sized, so that a plan only reads them: it extends them, and allocates, only for
            // a block more than twice as large as any of the last fit's.
            let asked = self.tables.asked();
            self.tables = Tables::new(self.path.fits());
            self.tables.prepare(2 * asked);
            self.share = [(0, 0), self.share[0]];
        }
        Ok(())
    }

    /// What `voxl stats` shows of the link.
    pub fn stats(&self) -> Stats {
        Stats {
            loss: self.path.fits()[0].loss(),
            rtprop: self.path.rtprop(),
            spread: self.path.spread(),
            miss: self.miss,
            share: self.share(),
        }
    }
}

/// What the link reads of the path, for `voxl stats`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stats {
    /// The long-run share of symbols erased (the short fit's).
    pub loss: f64,
    /// The base round trip, once a delay each way is measured.
    pub rtprop: Option<Duration>,
    /// The spread of arrivals over the base delay down.
    pub spread: Duration,
    /// The last video block's chance of a miss.
    pub miss: f64,
    /// The frames' share of the wire.
    pub share: f64,
}
