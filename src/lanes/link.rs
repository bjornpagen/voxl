//! The daemon's view of its lanes: the [`Path`] model fed by reports, and from it each block's
//! plan.
//!
//! - **m** is the least whose miss chance, exact under the runs fitted to the reports in send
//!   order ([`super::model`]), meets the freeze requirement ([`FREEZE_EVERY`],
//!   [`Link::target`]). It reads the runs as they stood at the last refit (every [`MEMORY`]
//!   reported symbols), so that one fit sizes a stretch of blocks. What leaves is the screen's
//!   data and that m (and a probe's padding): the pacing rate is a ceiling, never a target to
//!   fill, and when it cannot carry both the encoders' data target shrinks ([`Link::data`]),
//!   never the repairs.
//! - **The wanted lanes** are the most symbols that left within one stall over the last
//!   [`MEMORY`] symbols ([`Link::wanted`]). Dealt in turn, a stream lane stalled by a loss then
//!   holds back none of the pieces after it, and the erasures stay the path's. A block whose
//!   pieces could still land on a stalled lane has a deadline that waits the stall out
//!   ([`Wait::Stall`]), with its own erasure fit.

use super::model::{Arrival, Lanes, Tables};
use super::path::{Kind, Path, Piece, Wait, MEMORY};
use super::wire::{symbols, tenths, Fate, Head, Report, Stamp, Track, HEADER, SYMBOL, WINDOW};
use std::collections::VecDeque;
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
/// evidence: `lanes::sim::tests::the_loop_freezes_at_most_once_per_busy_interval_on_datagram_lanes`
///   and its stream and connection twins, which run the full loop for production's workload and
///   a busy screen's at 100 and 200 ms, with desktop audio off and on, and assert the picture's
///   freezes and the sound's dropouts counted are each within the Poisson bound of one per
///   FREEZE_EVERY, printing the misses against the model's expectation.
/// revalidate: when the owner restates the requirement, or the loss model, a track's rate
///   measure or the release of missed frames changes.
pub const FREEZE_EVERY: Duration = Duration::from_secs(8 * 60);

/// How a block is sent.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Plan {
    /// Its repairs.
    pub m: u16,
    /// Its deadline on the relay's clock, for its head.
    pub due: Stamp,
    /// How long it takes to leave at the pacing rate: its messages are paced over this.
    pub sending: Duration,
    /// The lanes it is dealt over.
    pub lanes: Lanes,
    /// The lane its first piece goes on.
    first: usize,
    /// What its deadline waits.
    wait: Wait,
    /// The chance it misses, by the loss model.
    pub miss: f64,
    /// When it starts to leave, on the daemon's clock: its messages leave evenly over
    /// `sending` from then.
    pub start: Stamp,
}

impl Plan {
    /// The lane (by place in the set) message `i` of the block goes on.
    pub fn lane(&self, i: usize) -> usize {
        self.lanes.lane(self.first, i)
    }

    /// What the daemon keeps of the block until its report: with its members' tracks, in
    /// order, so that a miss names the updates it leaves stale (in a vector the daemon may
    /// take back from the flight and reuse).
    pub fn flight(&self, head: &Head, tracks: Vec<Track>) -> Flight {
        Flight {
            head: *head,
            tracks,
            lanes: self.lanes,
            first: self.first,
            wait: self.wait,
            sending: self.sending,
            start: self.start,
        }
    }
}

/// A block sent and not yet reported.
#[derive(Clone, Debug, PartialEq)]
pub struct Flight {
    pub head: Head,
    pub tracks: Vec<Track>,
    pub lanes: Lanes,
    first: usize,
    wait: Wait,
    pub sending: Duration,
    pub start: Stamp,
}

impl Flight {
    /// The lane (by place in the set) message `i` of the block went on.
    pub fn lane(&self, i: usize) -> usize {
        self.lanes.lane(self.first, i)
    }
}

/// A report that does not fit its block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mismatch {
    /// It is of another block.
    Block,
    /// It has another count of symbols.
    Symbols,
}

/// The daemon's whole view.
pub struct Link {
    path: Path,
    /// The miss chances the blocks are sized by, both fits' tables for each deadline, and the
    /// reported symbols since they were fitted: a table lives from one refit to the next, so
    /// a plan pays only for block sizes no plan of the fit asked before.
    tables: [Tables; 2],
    since: usize,
    /// The miss chance of the last video plan.
    miss: f64,
    /// The blocks planned in the last [`MEMORY`] symbols whose count of symbols
    /// leaving within one stall can still be the largest: (symbols planned before it, that
    /// count).
    largest: VecDeque<(u64, NonZeroUsize)>,
    /// The blocks that started within the last stall: when, and their symbols; and the sum.
    stalled: VecDeque<(Stamp, usize)>,
    within: usize,
    /// The symbols planned so far, and the lane the next goes on.
    planned: u64,
    next: usize,
    /// The frames' bytes and the bytes they take on the wire, of the blocks planned since the
    /// runs were fitted and in the stretch before.
    share: [(usize, usize); 2],
    /// The bytes planned and not yet reported.
    in_flight: usize,
    /// When the bottleneck, crossed at BtlBw, is through every block planned so far.
    through: Stamp,
    /// The last block's deadline, on the relay's clock: none goes before it (the relay
    /// decides a block nothing of which came once a later block's deadline has passed,
    /// [`super::code::Decoder::decide`]).
    due: Option<Stamp>,
    /// What the last plan's deadline waited, and how long past the spread.
    deadline: Wait,
    wait: Duration,
    /// A report's arrived pieces, reused from report to report.
    pieces: Vec<Piece>,
}

impl Link {
    /// A session's link, from its first lane's attach (stamped `sent` by the relay and
    /// `received` by the daemon).
    pub fn new(sent: Stamp, received: Stamp) -> Link {
        let path = Path::new(sent, received);
        Link {
            tables: [
                Tables::new(path.fits(Wait::Spread)),
                Tables::new(path.fits(Wait::Stall)),
            ],
            path,
            since: 0,
            miss: 0.0,
            largest: VecDeque::new(),
            stalled: VecDeque::new(),
            within: 0,
            planned: 0,
            next: 0,
            share: [(0, 0); 2],
            in_flight: 0,
            through: received,
            due: None,
            deadline: Wait::Spread,
            wait: Duration::ZERO,
            pieces: Vec::new(),
        }
    }

    /// How to send a block of `bytes` whose members are of `tracks` over `lanes`, starting
    /// to leave at `start`.
    pub fn plan(
        &mut self,
        bytes: NonZeroUsize,
        tracks: &[Track],
        lanes: Lanes,
        start: Stamp,
    ) -> Plan {
        for track in tracks {
            self.path.frame(*track, start);
        }
        let sources = symbols(bytes);
        let k = sources.get();
        // The symbols that leave within one stall of this block's start, before it: the
        // stall seen, but never less than a resend takes, so that lanes are kept apart before
        // any resend has been seen.
        let stall = tenths(self.path.lane_stall());
        while let Some(&(at, n)) = self.stalled.front() {
            if at.to(start) < stall {
                break;
            }
            self.within -= n;
            self.stalled.pop_front();
        }
        let message = bytes.get().min(SYMBOL) + HEADER;
        let pacing = self.path.pacing();
        // The repairs under a deadline that waits `deadline`: the least whose miss chance, by
        // that deadline's own erasure fit, meets the miss target of every member ([`Link::target`]), never
        // past the in-flight cap (the sources go whole, repairs as the room allows).
        let size = |deadline: Wait| {
            let wait = match deadline {
                Wait::Stall => self.path.stall().saturating_sub(self.path.spread()),
                Wait::Spread => Duration::ZERO,
            };
            let room = self.path.in_flight(wait).saturating_sub(self.in_flight);
            let most = (WINDOW - k).min((room / message).saturating_sub(k));
            let target = self.target(tracks);
            (deadline, wait, most, target)
        };
        // The deadline waits the spread alone when every piece's lane was last dealt more
        // than a stall before: the symbols dealt in the last stall and the block's own fit in
        // the lanes. Otherwise a stalled lane may hold one of its pieces, and the deadline
        // waits the stall out. On datagram lanes nothing waits on a resend, the stall is zero
        // and the two are one.
        let (spread, stall) = (size(Wait::Spread), size(Wait::Stall));
        let at_spread = self.tables[Wait::Spread as usize].repairs(sources, spread.3, spread.2);
        let fits = self.within + k + at_spread.m + self.path.pad(k + at_spread.m) <= lanes.get();
        let ((deadline, wait, most, _), choice) = match fits {
            true => (spread, at_spread),
            false => (
                stall,
                self.tables[Wait::Stall as usize].repairs(sources, stall.3, stall.2),
            ),
        };
        self.wait = wait;
        self.deadline = deadline;
        if tracks.iter().any(|t| matches!(t, Track::Video { .. })) {
            self.miss = choice.miss;
        }
        let m = (choice.m + self.path.pad(k + choice.m)).min(most) as u16;
        let n = sources.saturating_add(m as usize);
        let wire = bytes.get() + m as usize * (message - HEADER) + n.get() * HEADER;
        let sending = Duration::from_secs_f64(wire as f64 / pacing);
        // Its last byte is through the bottleneck once it has left and the bottleneck, at
        // BtlBw, has carried it and every block before it: a probe paces past BtlBw and
        // queues, and the deadline waits out that queue rather than count its pieces late.
        let crossing = tenths(Duration::from_secs_f64(wire as f64 / self.path.btlbw()));
        let queued = match start.to(self.through) {
            ahead if ahead > 0 => self.through,
            _ => start,
        };
        self.through = match start.plus(tenths(sending)) {
            left if queued.plus(crossing).to(left) > 0 => left,
            _ => queued.plus(crossing),
        };
        self.stalled.push_back((start, n.get()));
        self.within += n.get();
        let needed = NonZeroUsize::new(self.within).unwrap_or(n);
        while self.largest.back().is_some_and(|&(_, back)| back <= needed) {
            self.largest.pop_back();
        }
        self.share[0].0 += bytes.get();
        self.share[0].1 += wire;
        self.in_flight += wire;
        self.largest.push_back((self.planned, needed));
        // Dealt in turn from where the last block stopped; past the lanes there are now, from
        // the first.
        let first = match self.next < lanes.get() {
            true => self.next,
            false => 0,
        };
        self.next = (first + n.get()) % lanes.get();
        self.planned += n.get() as u64;
        while self.largest.len() > 1
            && self
                .largest
                .front()
                .is_some_and(|&(at, _)| at + (MEMORY as u64) < self.planned)
        {
            self.largest.pop_front();
        }
        // Its deadline: when its last byte is through, the base delay, the spread and the
        // wait, but never before the last block's, as deadlines go in block order.
        let own = self.path.due(self.through).plus(tenths(wait));
        let due = match self.due {
            Some(last) if own.to(last) > 0 => last,
            _ => own,
        };
        self.due = Some(due);
        Plan {
            m,
            due,
            sending,
            lanes,
            first,
            wait: deadline,
            miss: match (m as usize) < choice.m {
                // Fewer repairs than chosen: the chance the block misses with those.
                true => self.tables[deadline as usize].miss(sources, m as usize),
                false => choice.miss,
            },
            start,
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

    /// The lanes the daemon wants: the most symbols that left within one stall lately (a
    /// block's own among them), within what a lane index numbers. Dealt in turn, no two of
    /// them then share a lane, so a stream lane's stall holds back only its own lost piece,
    /// and the erasures stay the path's. The daemon tells the relay whenever this changes.
    pub fn wanted(&self) -> Lanes {
        match self.largest.front() {
            Some(&(_, n)) => Lanes::at_most(n),
            None => Lanes::ONE,
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
        self.in_flight -= (0..n).map(|i| head.message_len(i)).sum::<usize>();
        let sending = tenths(flight.sending);
        let left = |i: usize| sending * i as i64 / n as i64;
        self.pieces.clear();
        match report.fate {
            Fate::Unseen => (0..n).for_each(|_| self.path.erasure(flight.wait, Arrival::Erased)),
            Fate::Seen { arrivals, .. } => {
                for (i, a) in arrivals.iter().enumerate() {
                    let Some(d) = a else {
                        self.path.erasure(flight.wait, Arrival::Erased);
                        continue;
                    };
                    self.path.erasure(flight.wait, Arrival::Arrived);
                    self.path.down(d, received);
                    self.pieces.push(Piece {
                        arrived: left(i) + self.path.past_base(d),
                        left: left(i),
                        bytes: head.message_len(i),
                    });
                }
            }
        }
        for d in report.late.iter() {
            self.path.down(d, received);
        }
        // Pushed in the order they left.
        self.path.delivered(flight.start, &self.pieces, received);
        self.since += n;
        if self.since >= MEMORY {
            self.since = 0;
            self.tables = [
                Tables::new(self.path.fits(Wait::Spread)),
                Tables::new(self.path.fits(Wait::Stall)),
            ];
            self.share = [(0, 0), self.share[0]];
        }
        Ok(())
    }

    /// The pacing rate, bytes a second.
    pub fn pacing(&self) -> f64 {
        self.path.pacing()
    }

    /// The part of the pacing the frames themselves may take, for the encoders to aim at: the
    /// pacing times the share of the frames' bytes in what the blocks planned lately took on
    /// the wire (repairs and headers take the rest); all of it before any block is planned.
    pub fn data(&self) -> f64 {
        let (bytes, wire) = (
            self.share[0].0 + self.share[1].0,
            self.share[0].1 + self.share[1].1,
        );
        match wire {
            0 => self.pacing(),
            _ => self.pacing() * bytes as f64 / wire as f64,
        }
    }

    /// The most one of `lanes` lanes may hold unreported, in bytes: its share of
    /// [`Path::in_flight`], never less than one message.
    pub fn lane_bound(&self, lanes: Lanes) -> usize {
        (self.path.in_flight(self.wait) / lanes.get()).max(super::wire::DATAGRAM)
    }

    /// What `voxl stats` shows of the link.
    pub fn stats(&self) -> Stats {
        let recovery = self.path.recovery(Duration::ZERO);
        Stats {
            loss: self.path.fits(self.deadline)[0].loss(),
            btlbw: self.path.btlbw(),
            rtprop: self.path.rtprop(),
            spread: self.path.spread(),
            recovery,
            pacing: self.path.pacing(),
            stale: (self.miss * self.path.rate(Kind::Video) * recovery.as_secs_f64()).min(1.0),
        }
    }
}

/// What the link reads of the path, for `voxl stats`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stats {
    /// The long-run share of symbols erased ([`Runs::loss`]).
    pub loss: f64,
    /// BtlBw, bytes a second.
    pub btlbw: f64,
    /// RTprop, once one is measured. The one-way delays are not: each holds the two clocks'
    /// offset, and only their sum is free of it.
    pub rtprop: Option<Duration>,
    /// The spread of arrivals that waited on no resend.
    pub spread: Duration,
    /// How long after a block starts to leave its miss is mended by the next (its own
    /// sending time comes on top).
    pub recovery: Duration,
    /// The pacing rate, bytes a second.
    pub pacing: f64,
    /// The share of time the picture is expected to be stale: the last video plan's chance of
    /// a miss times the update rate times the recovery, a miss leaving it stale that long.
    pub stale: f64,
}
