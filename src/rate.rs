//! Rate control: the bitrate the stream is steered to, and the bits each frame may spend.

use crate::hub::{Client, Hub, Viewer};
use crate::path::{Bits, Capacity, Kbps};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// What one client's acks say about its path now.
#[derive(Clone, Copy)]
struct PathNow {
    /// Delay queued on the path (ms).
    queue_ms: f64,
    /// What the path can carry, once its frames have told.
    capacity: Option<Kbps>,
    /// Its oldest unacknowledged frame is overdue.
    overdue: bool,
}

/// Delay-based bitrate adaptation. The path's capacity is measured by the frames
/// themselves (path.rs), so the target follows it directly: up to just under it while
/// nothing queues, down below it when something does. Nothing has to press against the
/// cap for the cap to rise, and an idle screen doesn't lower it.
pub struct RateCtl {
    /// Always within `bounds`.
    pub target: Kbps,
    bounds: Bounds,
    /// When it last decided.
    decided: Option<Instant>,
    /// What it last did to the target.
    moved: Moved,
}

/// What the controller last did to the target, and when: all it needs of the past to
/// know whether it may move the target again. A rise waits longer after a fall than a fall
/// or a rise waits after their own kind, so after a rise a fall may come at once, and
/// after a fall the rise before it is long enough ago.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Moved {
    Never,
    Lowered(Instant),
    Raised(Instant),
}

const _: () = assert!(
    RAISE_AFTER_LOWER.as_nanos() >= LOWER_EVERY.as_nanos()
        && RAISE_AFTER_LOWER.as_nanos() >= RAISE_EVERY.as_nanos()
);

impl Moved {
    /// A queue may lower the target `LOWER_EVERY` after it last did.
    fn may_lower(self, now: Instant) -> bool {
        match self {
            Moved::Lowered(at) => now.saturating_duration_since(at) >= LOWER_EVERY,
            Moved::Never | Moved::Raised(_) => true,
        }
    }

    /// A clear path may raise it `RAISE_AFTER_LOWER` after a fall, and `RAISE_EVERY` after
    /// a rise.
    fn may_raise(self, now: Instant) -> bool {
        match self {
            Moved::Never => true,
            Moved::Lowered(at) => now.saturating_duration_since(at) >= RAISE_AFTER_LOWER,
            Moved::Raised(at) => now.saturating_duration_since(at) >= RAISE_EVERY,
        }
    }
}

/// The bitrate cap of a path nothing is known about yet.
const START: Kbps = Kbps::new(8000.0);
/// How much of the measured capacity the stream may fill. The rest is headroom for the
/// path's own variation, so a steady stream sits below the queue instead of in it.
const CAPACITY_SHARE: f64 = 0.9;
/// A path with more than this queued is congested; with no more than `QUEUE_CLEAR_MS`,
/// clear. Between the two the target stays where it is.
const QUEUE_CONGESTED_MS: f64 = 30.0;
const QUEUE_CLEAR_MS: f64 = 15.0;
/// A frame unacknowledged this long past the round trip is overdue.
const OVERDUE_MS: f64 = 80.0;
/// The round trip assumed for a client whose pings haven't been answered yet.
pub const RTT_GUESS_MS: u32 = 40;

// The controller's numbers. Each was run one step either side (an interval or a change
// halved and doubled) on rate_tests' model: one simulated link with a rate, a round trip
// and a queue served in order, at 60 fps, seeded. No rate test fails either side unless
// the comment says so. The rows, and what they read now:
// - follow: how long a 40 Mbit/s link that slows to 4 takes to bring the target under
//   4000 kbit/s (3.8 s); after: a frame's mean wait from then on (16.5 ms);
// - late 3: the worst wait of a frame under video-like motion on 2.5 to 12 Mbit/s links
//   with acks up to 3 ms late (0 ms); late 6: the least share of the link that motion
//   uses with acks up to 6 ms late (0.78);
// - climb: how long the target takes from the floor to a 30000 kbit/s ceiling on a fast
//   path (2.2 s, or 3.0 s just after a fall).

/// The controller decides at most this often. Upstream's (e88355c), unmeasured. At 50 ms
/// late 3 waits 53 ms; at 200 ms follow takes 5.0 s and after is 20.1 ms. With no interval
/// at all late 6 uses 0.91 of the link, and late 3 waits 41 ms.
const DECIDE_EVERY: Duration = Duration::from_millis(100);
/// A queue lowers the target at most this often. Upstream's (e88355c), unmeasured. At
/// 150 ms follow takes 2.6 s and late 3 waits 33 ms; at 600 ms follow takes 7.4 s, after
/// is 29.2 ms and late 6 uses 0.67 of the link.
const LOWER_EVERY: Duration = Duration::from_millis(300);
/// A queue lowers the target to this share of itself. Upstream's (e88355c), unmeasured.
/// Half the cut (0.925): follow takes 7.7 s, with 34 cuts where it now takes 18, and after
/// is 35.6 ms. Twice the cut (0.70): follow takes 1.7 s with 8 cuts, after is 5.7 ms, and
/// climb from a fall takes 3.2 s.
const LOWER_TO: f64 = 0.85;
/// The target rises no sooner than this after a queue lowered it. Upstream's (e88355c),
/// unmeasured. At 500 ms late 3 waits 34 ms and follow takes 19 cuts; at 2 s late 3 waits
/// 56 ms, late 6 uses 0.74 of the link, and climb from a fall takes 4.0 s.
const RAISE_AFTER_LOWER: Duration = Duration::from_secs(1);
/// While nothing queues the target rises at most this often. Upstream's (e88355c),
/// unmeasured. At 100 ms (every decision) climb takes 1.1 s; at 400 ms it takes 4.4 s,
/// and a new viewer is left at the start (`a_new_viewer_is_not_left_at_the_floor` fails).
const RAISE_EVERY: Duration = Duration::from_millis(200);
/// Each rise multiplies the target by this. Ours (9c3cfad), in place of upstream's 1.06
/// when the target began to follow the measured capacity; unmeasured. At 1.125 climb
/// takes 4.0 s and a new viewer is left at the start (the same test fails); at 1.5 climb
/// takes 1.2 s. With none (1.0) five rate tests fail, and climb takes 56 s.
const RAISE_BY: f64 = 1.25;
/// And adds this, so a target near nothing (`--min-kbps 0`) climbs too. Upstream's
/// (e88355c), unmeasured. From 1 kbit/s to the cautious start takes 2.6 s: 3.2 s at 50,
/// 2.0 s at 200. With none it takes 8.0 s, and a new viewer is left at the start.
const RAISE_STEP: Kbps = Kbps::new(100.0);

/// What the target may be: never under a floor, and never over a ceiling if there is
/// one. Where the two cross, the ceiling wins: it is the most a person lets the stream
/// send (Selkies' `--video-bitrate` sets it alone), where the floor only keeps the
/// controller from starving the picture.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Bounds {
    /// Never over the ceiling.
    floor: Kbps,
    ceiling: Option<Kbps>,
}

impl Bounds {
    /// A floor over the ceiling is the ceiling.
    pub fn new(floor: Kbps, ceiling: Option<Kbps>) -> Self {
        let floor = ceiling.into_iter().fold(floor, Kbps::min);
        Bounds { floor, ceiling }
    }

    /// None: no ceiling. Encoders open without a rate cap, so nothing applies the
    /// target: a viewer on a slow path gets every frame at its CRF.
    pub fn ceiling(self) -> Option<Kbps> {
        self.ceiling
    }

    /// `rate`, or the bound it is past.
    fn hold(self, rate: Kbps) -> Kbps {
        self.ceiling
            .into_iter()
            .fold(rate.max(self.floor), Kbps::min)
    }
}

impl RateCtl {
    /// Carries on from `resume`, the last pipeline's target: one rebuilt for a new
    /// screen size streams over the same paths as before.
    pub fn new(bounds: Bounds, resume: Option<Kbps>) -> Self {
        RateCtl {
            target: bounds.hold(resume.unwrap_or(START)),
            bounds,
            decided: None,
            moved: Moved::Never,
        }
    }

    /// Steers by what the clients' paths say now; what they can carry together.
    pub fn update(&mut self, hub: &Hub, viewers: &[(Arc<Client>, Viewer)]) -> Capacity {
        let paths: Vec<PathNow> = viewers
            .iter()
            .filter(|(_, viewer)| *viewer == Viewer::Started)
            .map(|(c, _)| {
                let (_, age) = c.backlog();
                let rtt = c.rtt_ms().unwrap_or(RTT_GUESS_MS) as f64;
                PathNow {
                    queue_ms: c.queue_ms(),
                    capacity: c.capacity(),
                    overdue: age as f64 > rtt + OVERDUE_MS,
                }
            })
            .collect();
        let capacity = self.step(Instant::now(), &paths);
        hub.set_target(self.target);
        capacity
    }

    /// One decision, at most every `DECIDE_EVERY`, from what every watched path says now;
    /// what they can carry together.
    fn step(&mut self, now: Instant, paths: &[PathNow]) -> Capacity {
        let capacity = Capacity::of(paths.iter().map(|p| p.capacity));
        if self
            .decided
            .is_some_and(|t| now.saturating_duration_since(t) < DECIDE_EVERY)
        {
            return capacity;
        }
        self.decided = Some(now);
        // Where the target goes while nothing queues, and the most it may be now.
        let aim = self.bounds.hold(match capacity {
            Capacity::NoViewers => return capacity,
            // An unknown path gets the cautious start, whatever was reached before
            // it: no more than it, and up to it, since frames at a target far under
            // it are too small to show the path's rate, and the target would stay
            // there.
            Capacity::Unknown => START,
            Capacity::Known(c) => c.times(CAPACITY_SHARE),
        });
        // A path that got slower is followed down at once, queue or no queue.
        self.target = self.target.min(aim);
        let congested = paths
            .iter()
            .any(|p| p.queue_ms > QUEUE_CONGESTED_MS || p.overdue);
        let clear = paths.iter().all(|p| p.queue_ms <= QUEUE_CLEAR_MS);
        if congested {
            if self.moved.may_lower(now) {
                self.target = self.bounds.hold(self.target.times(LOWER_TO));
                self.moved = Moved::Lowered(now);
                log::debug!("rate: congested, target {:.0} kbps", self.target.get());
            }
        } else if clear && self.target < aim && self.moved.may_raise(now) {
            self.target = Kbps::new(self.target.get() * RAISE_BY + RAISE_STEP.get()).min(aim);
            self.moved = Moved::Raised(now);
        }
        capacity
    }
}

/// How long a frame may take to cross the path: what a burst after stillness may cost
/// in delay, once.
const LATENCY_BUDGET: Duration = Duration::from_millis(60);
/// A frame in debt may still spend this share of the target's part of one frame time:
/// the stream keeps moving while it pays back, and pays back because it is under one.
const DEBT_SHARE: f64 = 0.5;
/// The tokens pile up to at least this many frame times of the target, and to this alone
/// on a path not yet known. Ours (9c3cfad): the allowance upstream gave a frame, twice its
/// share of the target; unmeasured. At one frame five rate tests fail: frames on a path
/// not yet known are too small to show its rate, the target stays at the start on
/// 40 Mbit/s, and the first frame of a scroll after stillness gets 37% of what it wanted.
/// At four nothing fails, and late 3 waits 37 ms.
const DEPTH_FRAMES: f64 = 2.0;

/// The bits a frame may spend. Tokens come in at the target rate by the time that
/// really passed, and pile up while the screen is still to what the path carries in
/// `LATENCY_BUDGET`: so the first frames of a scroll after stillness go out sharp.
/// Everything sent is taken off, into debt when a frame took more than there was (a
/// keyframe's boost, several stripes moving at once), so over any stretch the stream
/// sends what the target carries in it, give or take one depth.
pub struct FrameBudget {
    tokens: Bits,
    /// The most the tokens pile up to, and the most that is ever owed.
    depth: Bits,
    at: Instant,
}

impl FrameBudget {
    pub fn new(now: Instant) -> Self {
        FrameBudget {
            tokens: Bits::ZERO,
            depth: Bits::ZERO,
            at: now,
        }
    }

    /// What a frame encoded at `now` may spend.
    pub fn allow(&mut self, now: Instant, target: Kbps, capacity: Capacity, fps: u32) -> Bits {
        let steady = target.over(Duration::from_secs_f64(1.0 / fps.max(1) as f64));
        let depth = match capacity {
            Capacity::Known(c) => c.over(LATENCY_BUDGET).max(steady.times(DEPTH_FRAMES)),
            // A path nothing is known about gets `DEPTH_FRAMES` of the target at most.
            Capacity::Unknown | Capacity::NoViewers => steady.times(DEPTH_FRAMES),
        };
        let passed = now.saturating_duration_since(self.at);
        self.at = now;
        self.depth = depth;
        self.tokens = (self.tokens + target.over(passed)).min(depth);
        self.tokens.max(steady.times(DEBT_SHARE))
    }

    /// A frame of `bytes` was sent. What it took beyond the tokens is owed, up to one
    /// depth: a picture the encoder cannot make as small as its share (a video at a
    /// target far under what it costs) would otherwise run up a debt for as long as it
    /// plays, and everything after it would pay.
    pub fn spend(&mut self, bytes: usize) {
        self.tokens = (self.tokens - Bits::of_bytes(bytes)).max(Bits::ZERO - self.depth);
    }
}

/// The rate controller, the frame budget and the path estimate together, against a
/// link that is the oracle: a rate, a round trip, and a queue that serves in order.
#[cfg(test)]
mod rate_tests {
    use super::*;
    use crate::path::PathEst;
    use std::collections::VecDeque;

    const FPS: u32 = 60;

    struct Rng(u64);
    impl Rng {
        fn range(&mut self, lo: u64, hi: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            lo + self.0 % (hi - lo)
        }
    }

    /// One viewer on one link, driven a frame time at a tick.
    struct Sim {
        now: Instant,
        /// The link's rate (bit/s) and round trip.
        rate: f64,
        rtt: Duration,
        /// When the link is free again.
        free: Instant,
        /// (acked at, sent at, bytes, sent with nothing ahead of it) of frames on their way.
        flying: VecDeque<(Instant, Instant, usize, bool)>,
        est: PathEst,
        rc: RateCtl,
        budget: FrameBudget,
        /// Worst time a frame waited behind others, since `reset`.
        worst_wait: Duration,
        /// Smallest share of what it wanted that a frame got, since `reset`.
        worst_share: f64,
        sent_bytes: usize,
        /// Every ack comes up to this late, at random.
        late: Duration,
        late_rng: Rng,
        /// The highest the estimate read, as a share of the link's rate.
        highest_read: f64,
    }

    impl Sim {
        fn new(rate: f64, rtt_ms: u64, max_kbps: f64) -> Self {
            let now = Instant::now();
            Sim {
                now,
                rate,
                rtt: Duration::from_millis(rtt_ms),
                free: now,
                flying: VecDeque::new(),
                est: PathEst::new(now),
                rc: RateCtl::new(
                    Bounds::new(Kbps::new(2000.0), Some(Kbps::new(max_kbps))),
                    None,
                ),
                budget: FrameBudget::new(now),
                worst_wait: Duration::ZERO,
                worst_share: 1.0,
                sent_bytes: 0,
                late: Duration::ZERO,
                late_rng: Rng(0x9E3779B97F4A7C15),
                highest_read: 0.0,
            }
        }

        fn reset(&mut self) {
            self.worst_wait = Duration::ZERO;
            self.worst_share = 1.0;
            self.sent_bytes = 0;
        }

        /// One frame time passes; the screen wants to send `want` bytes (0: it is still).
        fn tick(&mut self, want: usize) {
            self.now += Duration::from_secs_f64(1.0 / FPS as f64);
            while self.flying.front().is_some_and(|f| f.0 <= self.now) {
                let (acked, sent, bytes, alone) = self.flying.pop_front().unwrap();
                self.est.on_ack(acked, sent, bytes, alone);
                if let Some(c) = self.est.capacity() {
                    self.highest_read = self.highest_read.max(c.bps() / self.rate);
                }
            }
            let oldest = self
                .flying
                .front()
                .map_or(Duration::ZERO, |f| self.now.duration_since(f.1));
            let path = PathNow {
                queue_ms: self.est.queue_ms(self.now, self.flying.is_empty()),
                capacity: self.est.capacity(),
                overdue: oldest > self.rtt + Duration::from_millis(80),
            };
            self.rc.step(self.now, &[path]);
            // The capture loop's flow control: no new frame while the oldest is this late.
            if want == 0 || oldest > self.rtt + Duration::from_millis(120) {
                return;
            }
            // x264 under the rate cap: a frame holds at most what the budget allows.
            let room = self
                .budget
                .allow(self.now, self.rc.target, Capacity::of([path.capacity]), FPS)
                .bytes() as usize;
            let bytes = want.min(room).max(200);
            self.budget.spend(bytes);
            let start = self.free.max(self.now);
            self.worst_wait = self.worst_wait.max(start - self.now);
            self.worst_share = self.worst_share.min(bytes as f64 / want as f64);
            self.free = start + Duration::from_secs_f64(bytes as f64 * 8.0 / self.rate);
            let alone = self.flying.is_empty();
            let late = match self.late.as_micros() as u64 {
                0 => Duration::ZERO,
                most => Duration::from_micros(self.late_rng.range(0, most + 1)),
            };
            // Acks arrive in the order their frames were sent.
            let acked =
                (self.free + self.rtt + late).max(self.flying.back().map_or(self.now, |f| f.0));
            self.flying.push_back((acked, self.now, bytes, alone));
            self.sent_bytes += bytes;
        }

        fn run(&mut self, ticks: u32, mut want: impl FnMut(u32) -> usize) {
            for i in 0..ticks {
                self.tick(want(i));
            }
        }
    }

    /// A desktop at rest: a cursor and a clock, a few hundred bytes now and then.
    fn rest(i: u32) -> usize {
        if i.is_multiple_of(7) {
            600
        } else {
            0
        }
    }

    #[test]
    fn a_scroll_after_stillness_is_sharp_on_a_link_with_room() {
        // The case this exists for: a desktop that is mostly still, on a link faster
        // than the scroll needs (60 KB a frame at 60 fps is 29 Mbit/s), scrolled now and
        // then. Every scroll frame gets what it asks for, from the first one, and
        // nothing queues.
        let mut rng = Rng(0x2545F4914F6CDD1D);
        for _ in 0..40 {
            let rate = rng.range(36_000_000, 80_000_000) as f64;
            let mut sim = Sim::new(rate, rng.range(20, 150), 30_000.0);
            // Still, then one scroll that teaches the path, then stillness again.
            sim.run(120, rest);
            sim.run(40, |_| 45_000);
            sim.run(60 * rng.range(1, 20) as u32, rest);
            sim.reset();
            let want = rng.range(30_000, 60_000) as usize;
            sim.run(45, |_| want);
            assert!(
                sim.worst_share >= 0.999,
                "rate {rate}: a scroll frame got {:.0}% of what it wanted",
                sim.worst_share * 100.0
            );
            assert!(
                sim.worst_wait <= LATENCY_BUDGET,
                "rate {rate}: a frame waited {:?}",
                sim.worst_wait
            );
        }
    }

    #[test]
    fn the_first_frames_of_a_scroll_are_whole_on_a_link_the_scroll_overfills() {
        // A slower link can't carry the whole scroll at full quality, but what piled up
        // during the stillness pays for its first frame in full, within the latency
        // budget; the frames after it get what the link sustains.
        let mut rng = Rng(0x94D049BB133111EB);
        for _ in 0..40 {
            let rate = rng.range(8_000_000, 20_000_000) as f64;
            let mut sim = Sim::new(rate, rng.range(20, 150), 30_000.0);
            sim.run(120, rest);
            sim.run(40, |_| 45_000);
            sim.run(60 * rng.range(2, 20) as u32, rest);
            sim.reset();
            sim.run(1, |_| 45_000);
            assert!(
                sim.worst_share >= 0.999,
                "rate {rate}: the first scroll frame got {:.0}% of what it wanted",
                sim.worst_share * 100.0
            );
            sim.run(120, |_| 45_000);
            assert!(
                sim.worst_wait <= LATENCY_BUDGET + Duration::from_millis(40),
                "rate {rate}: a frame waited {:?}",
                sim.worst_wait
            );
        }
    }

    #[test]
    fn a_new_viewer_is_not_left_at_the_floor() {
        // The target is kept across viewers. One slow viewer drives it to the floor
        // and leaves; the next one's path is unknown, and frames at the floor are too
        // small to show its rate. The target has to come back to the cautious start by
        // itself, and from there the path shows what it carries: here after half a
        // second of a desktop at rest, which gives the estimate its baseline.
        let mut rng = Rng(0xA0761D6478BD642F);
        for _ in 0..40 {
            let rate = rng.range(20_000_000, 80_000_000) as f64;
            let mut sim = Sim::new(rate, rng.range(20, 100), 30_000.0);
            sim.rc.target = sim.rc.bounds.floor;
            sim.run(30, rest);
            sim.run(60 * 3, |_| 60_000);
            assert!(
                sim.rc.target.get() >= 0.5 * rate / 1000.0 || sim.rc.target.get() >= 25_000.0,
                "rate {rate}: target {} three seconds into a scroll",
                sim.rc.target.get()
            );
        }
        // And with nothing but the unknown path to go by, it reaches the start, not more.
        let now = Instant::now();
        let mut rc = RateCtl::new(
            Bounds::new(Kbps::new(2000.0), Some(Kbps::new(30_000.0))),
            Some(Kbps::new(2000.0)),
        );
        let unknown = PathNow {
            queue_ms: 0.0,
            capacity: None,
            overdue: false,
        };
        for i in 0..100 {
            rc.step(now + Duration::from_millis(100 * i), &[unknown]);
        }
        assert_eq!(rc.target, START);
    }

    /// Where a controller starts: the last pipeline's target, or the cautious start;
    /// never under the floor, and never over a ceiling if there is one, which wins over
    /// the floor.
    #[test]
    fn a_controller_starts_under_its_ceiling_if_it_has_one() {
        let kbps = Kbps::new;
        let floor = kbps(2000.0);
        for (max, resume, start) in [
            (None, None, START),
            (None, Some(kbps(50_000.0)), kbps(50_000.0)),
            (None, Some(kbps(500.0)), floor),
            (Some(kbps(30_000.0)), None, START),
            (Some(kbps(30_000.0)), Some(kbps(50_000.0)), kbps(30_000.0)),
            (Some(kbps(5000.0)), None, kbps(5000.0)),
            (Some(kbps(1000.0)), None, kbps(1000.0)),
            (Some(kbps(1000.0)), Some(kbps(500.0)), kbps(1000.0)),
        ] {
            let rc = RateCtl::new(Bounds::new(floor, max), resume);
            assert_eq!(rc.target, start, "ceiling {max:?}, resuming {resume:?}");
        }
    }

    /// Ten seconds of decisions every 100 ms on one path. With a ceiling the target
    /// stops at it. With none (`--max-kbps 0`) it goes where the path lets it: the
    /// cautious start while the path is unknown, the path's share once it is known. Under
    /// a queue it falls to the floor either way.
    #[test]
    fn the_target_is_held_by_the_ceiling_or_by_the_path() {
        let (floor, fast) = (Kbps::new(2000.0), Kbps::new(100_000.0));
        let path = |capacity, queue_ms| PathNow {
            queue_ms,
            capacity,
            overdue: false,
        };
        let steer = |max, path| {
            let mut rc = RateCtl::new(Bounds::new(floor, max), None);
            let t0 = Instant::now();
            for i in 0..100 {
                rc.step(t0 + Duration::from_millis(100 * i), &[path]);
            }
            rc.target
        };
        let ceiling = Some(Kbps::new(30_000.0));
        assert_eq!(steer(ceiling, path(Some(fast), 0.0)), Kbps::new(30_000.0));
        assert_eq!(
            steer(None, path(Some(fast), 0.0)),
            fast.times(CAPACITY_SHARE)
        );
        assert_eq!(steer(ceiling, path(None, 0.0)), START);
        assert_eq!(
            steer(Some(Kbps::new(5000.0)), path(None, 0.0)),
            Kbps::new(5000.0)
        );
        assert_eq!(steer(None, path(None, 0.0)), START);
        assert_eq!(steer(ceiling, path(Some(fast), 50.0)), floor);
        assert_eq!(steer(None, path(Some(fast), 50.0)), floor);
    }

    /// `--max-kbps 1000` with `--min-kbps` at its 2000: the target starts at the ceiling,
    /// and a queue lowering it must not lift it to the floor. (RED's finding 20.)
    #[test]
    fn a_queue_never_raises_the_target_over_its_ceiling() {
        let ceiling = Kbps::new(1000.0);
        let mut rc = RateCtl::new(Bounds::new(Kbps::new(2000.0), Some(ceiling)), None);
        let t0 = Instant::now();
        let queued = PathNow {
            queue_ms: 50.0,
            capacity: Some(Kbps::new(100_000.0)),
            overdue: false,
        };
        for i in 0..10 {
            rc.step(t0 + Duration::from_millis(100 * i), &[queued]);
            assert!(
                rc.target <= ceiling,
                "target {:?} over the ceiling {ceiling:?}",
                rc.target
            );
        }
    }

    /// Whatever the flags and the paths say, the target stays between the floor and the
    /// ceiling, and where the two cross, at the ceiling: random bounds (crossing, and
    /// with no ceiling), random resumes, and 30 s of random paths (unknown, slow, fast,
    /// queued, overdue) every 100 ms.
    #[test]
    fn the_target_never_leaves_its_bounds() {
        let kbps = |k: u64| Kbps::new(k as f64);
        let mut rng = Rng(0x2545F4914F6CDD1D);
        let t0 = Instant::now();
        for _ in 0..300 {
            let floor = kbps(rng.range(0, 20_000));
            let ceiling = (rng.range(0, 4) > 0).then(|| kbps(rng.range(1, 40_000)));
            let resume = (rng.range(0, 2) > 0).then(|| kbps(rng.range(0, 100_000)));
            let mut rc = RateCtl::new(Bounds::new(floor, ceiling), resume);
            let (low, high) = match ceiling {
                Some(c) if c < floor => (c, c),
                Some(c) => (floor, c),
                None => (floor, Kbps::new(f64::INFINITY)),
            };
            for i in 0..300 {
                let path = PathNow {
                    queue_ms: rng.range(0, 60) as f64,
                    capacity: (rng.range(0, 3) > 0).then(|| kbps(rng.range(500, 100_000))),
                    overdue: rng.range(0, 10) == 0,
                };
                rc.step(t0 + Duration::from_millis(100 * i), &[path]);
                assert!(
                    low <= rc.target && rc.target <= high,
                    "floor {floor:?}, ceiling {ceiling:?}: target {:?}",
                    rc.target
                );
            }
        }
    }

    #[test]
    fn late_acks_on_a_long_path_do_not_read_as_a_faster_one() {
        // A round trip longer than the gap between a desktop's frames at rest, so that
        // few frames are sent with nothing unacknowledged, and every ack a few ms late:
        // rest, then scrolls, then rest again. A late ack makes a frame look slower,
        // never the path faster.
        let mut rng = Rng(0xE7037ED1A0B428DB);
        for late_ms in [1, 3, 6] {
            for _ in 0..30 {
                let rate = rng.range(20_000_000, 60_000_000) as f64;
                let mut sim = Sim::new(rate, rng.range(120, 180), 30_000.0);
                sim.late = Duration::from_millis(late_ms);
                for _ in 0..4 {
                    sim.run(60 * 2, rest);
                    sim.run(60 * 2, |_| 62_000);
                }
                assert!(
                    sim.highest_read <= 1.15,
                    "rate {rate}, acks up to {late_ms} ms late: read {:.0}% of the link",
                    sim.highest_read * 100.0
                );
            }
        }
    }

    #[test]
    fn stillness_does_not_lower_the_target() {
        // Minutes of a still screen with a big frame now and then: the path is as fast
        // as it was, and the target says so.
        let mut sim = Sim::new(40_000_000.0, 100, 30_000.0);
        sim.run(120, rest);
        sim.run(40, |_| 45_000);
        sim.run(600, rest);
        let learned = sim.rc.target.get();
        assert!(
            learned >= 0.85 * 30_000.0,
            "target {learned} after one scroll"
        );
        for _ in 0..20 {
            sim.run(600, rest);
            sim.run(3, |_| 80_000);
        }
        assert!(
            sim.rc.target.get() >= learned,
            "target fell from {learned} to {}",
            sim.rc.target.get()
        );
    }

    #[test]
    fn sustained_motion_settles_under_a_slow_link() {
        // Video-like motion that always wants more than the link carries: the stream
        // settles under the link's rate, uses most of it, and the queue stays short.
        let mut rng = Rng(0x9E6C63D0876A9A47);
        for _ in 0..40 {
            let rate = rng.range(2_500_000, 12_000_000) as f64;
            let rtt = rng.range(20, 150);
            let mut sim = Sim::new(rate, rtt, 30_000.0);
            sim.run(120, rest);
            sim.run(60 * 20, |_| 120_000);
            sim.reset();
            sim.run(60 * 20, |_| 120_000);
            let sent = sim.sent_bytes as f64 * 8.0 / 20.0;
            assert!(
                sent <= rate * 1.0 && sent >= rate * 0.6,
                "rate {rate}, rtt {rtt}: sent {sent} bit/s"
            );
            assert!(
                sim.worst_wait <= Duration::from_millis(250),
                "rate {rate}, rtt {rtt}: a frame waited {:?}",
                sim.worst_wait
            );
        }
    }

    #[test]
    fn a_link_that_slows_is_followed_down() {
        // The path changes under a running stream (another network, a busy link).
        let mut sim = Sim::new(40_000_000.0, 60, 30_000.0);
        sim.run(120, rest);
        sim.run(60 * 5, |_| 60_000);
        assert!(
            sim.rc.target.get() > 20_000.0,
            "target {} on 40 Mbit/s",
            sim.rc.target.get()
        );
        sim.rate = 4_000_000.0;
        sim.run(60 * 15, |_| 60_000);
        sim.reset();
        sim.run(60 * 10, |_| 60_000);
        let sent = sim.sent_bytes as f64 * 8.0 / 10.0;
        assert!(sent <= 4_000_000.0, "sent {sent} bit/s on a 4 Mbit/s link");
        assert!(
            sim.worst_wait <= Duration::from_millis(250),
            "a frame waited {:?} after the link slowed",
            sim.worst_wait
        );
    }

    #[test]
    fn a_frame_gets_its_steady_share_and_no_more_than_the_latency_budget() {
        // Whatever the tokens, a frame may spend at least its share in debt of the
        // target's part of one frame time, and at most what the path carries in the
        // latency budget.
        let mut rng = Rng(0xBF58476D1CE4E5B9);
        let t0 = Instant::now();
        let frame_time = Duration::from_secs_f64(1.0 / FPS as f64);
        for _ in 0..500 {
            let mut b = FrameBudget::new(t0);
            let target = Kbps::new(rng.range(500, 30_000) as f64);
            let capacity = match rng.range(0, 3) {
                0 => Capacity::Unknown,
                _ => Capacity::Known(Kbps::new(rng.range(500, 100_000) as f64)),
            };
            let mut now = t0;
            for _ in 0..200 {
                now += Duration::from_micros(rng.range(100, 3_000_000));
                let may = b.allow(now, target, capacity, FPS);
                let steady = target.over(frame_time);
                let least = steady.times(DEBT_SHARE);
                assert!(may >= least, "{may:?} under the share in debt {least:?}");
                let most = match capacity {
                    Capacity::Known(c) => c.over(LATENCY_BUDGET).max(steady.times(2.0)),
                    Capacity::Unknown | Capacity::NoViewers => steady.times(2.0),
                };
                assert!(may <= most, "{may:?} over the latency budget {most:?}");
                b.spend(rng.range(0, 400_000) as usize);
            }
        }
    }

    /// Over any stretch, what frames spend is at most what the target carries in it,
    /// plus the depth, plus one frame's floor: at the capture loop's own pace (0.85 of
    /// a frame time apart, its `min_interval`), and with two stripes that each take
    /// their share of twice the frame's budget. What a frame takes beyond the tokens is
    /// a debt, and the frames after it pay it.
    #[test]
    fn a_long_run_of_frames_averages_the_target() {
        let t0 = Instant::now();
        let target = Kbps::new(9_000.0);
        let capacity = Capacity::Known(Kbps::new(10_000.0));
        let frame_time = Duration::from_secs_f64(1.0 / FPS as f64);
        let steady = target.over(frame_time);
        let depth = Kbps::new(10_000.0)
            .over(LATENCY_BUDGET)
            .max(steady.times(2.0));
        let mut over = vec![];
        // (frame times between frames, what a frame spends over what it may)
        for (pace, spends) in [(0.85, 1.0), (1.0, 2.0)] {
            let mut b = FrameBudget::new(t0);
            let (mut now, mut sent) = (t0, Bits::ZERO);
            for _ in 0..600 {
                now += frame_time.mul_f64(pace);
                let may = b.allow(now, target, capacity, FPS);
                let bytes = (may.bytes() * spends) as usize;
                b.spend(bytes);
                sent = sent + Bits::of_bytes(bytes);
            }
            let most = target.over(now - t0) + depth + steady;
            if sent > most {
                over.push((pace, spends, sent.bytes() / most.bytes()));
            }
        }
        assert!(
            over.is_empty(),
            "(pace, spends, sent over the most): {over:?}"
        );
    }

    /// FrameBudget's doc: over any stretch the stream sends what the
    /// target carries in it, give or take one depth; so a debt is at most a depth, and
    /// stillness pays it back. x264 cannot make a frame smaller than what the picture
    /// costs at its highest quantizer, and on a full-screen video at a low target that
    /// is over the target's share. `spend` takes all of it off, with no bound, and the
    /// debt grows for as long as the video plays.
    #[test]
    fn a_debt_is_never_more_than_a_depth() {
        let t0 = Instant::now();
        let target = Kbps::new(2_000.0);
        let capacity = Kbps::new(3_000.0);
        let known = Capacity::Known(capacity);
        let frame_time = Duration::from_secs_f64(1.0 / FPS as f64);
        let steady = target.over(frame_time);
        let depth = capacity.over(LATENCY_BUDGET).max(steady.times(2.0));
        // A minute of frames x264 cannot make smaller than one and a half shares.
        let mut b = FrameBudget::new(t0);
        let mut now = t0;
        for _ in 0..60 * 60 {
            now += frame_time;
            let may = b.allow(now, target, known, FPS);
            b.spend(may.max(steady.times(1.5)).bytes() as usize);
        }
        assert!(
            b.tokens >= Bits::ZERO - depth,
            "a debt of {:.0} KB after a minute, a depth is {:.0} KB: {:.0} s of the target to pay",
            -b.tokens.bytes() / 1000.0,
            depth.bytes() / 1000.0,
            -b.tokens.bytes() * 8.0 / target.bps()
        );
    }

    /// RateCtl::step and the end of RateCtl::update as they were before the paths'
    /// capacity was computed once: the oracle for that refactor.
    fn old_step(rc: &mut RateCtl, now: Instant, paths: &[PathNow]) {
        if rc
            .decided
            .is_some_and(|t| now.saturating_duration_since(t) < DECIDE_EVERY)
        {
            return;
        }
        rc.decided = Some(now);
        let aim = rc
            .bounds
            .hold(match Capacity::of(paths.iter().map(|p| p.capacity)) {
                Capacity::NoViewers => return,
                Capacity::Unknown => START,
                Capacity::Known(c) => c.times(CAPACITY_SHARE),
            });
        rc.target = rc.target.min(aim);
        let congested = paths
            .iter()
            .any(|p| p.queue_ms > QUEUE_CONGESTED_MS || p.overdue);
        let clear = paths.iter().all(|p| p.queue_ms <= QUEUE_CLEAR_MS);
        if congested {
            if rc.moved.may_lower(now) {
                rc.target = rc.bounds.hold(rc.target.times(LOWER_TO));
                rc.moved = Moved::Lowered(now);
            }
        } else if clear && rc.target < aim && rc.moved.may_raise(now) {
            rc.target = Kbps::new(rc.target.get() * RAISE_BY + RAISE_STEP.get()).min(aim);
            rc.moved = Moved::Raised(now);
        }
    }

    fn old_update_capacity(paths: &[PathNow]) -> Capacity {
        Capacity::of(paths.iter().map(|p| p.capacity))
    }

    /// A gap between decisions: often a whole number of 50 ms, so every interval the
    /// controller waits is hit exactly, else anything up to half a second.
    fn gap(rng: &mut Rng) -> Duration {
        match rng.range(0, 2) {
            0 => Duration::from_millis(50 * rng.range(0, 25)),
            _ => Duration::from_micros(rng.range(0, 500_000)),
        }
    }

    /// What up to three watched paths say: queues on and around both thresholds,
    /// capacities unknown or anything, and now and then a frame overdue.
    fn paths(rng: &mut Rng) -> Vec<PathNow> {
        (0..rng.range(0, 4))
            .map(|_| PathNow {
                queue_ms: [15.0, 30.0, rng.range(0, 60) as f64][rng.range(0, 3) as usize],
                capacity: (rng.range(0, 3) > 0).then(|| Kbps::new(rng.range(1, 100_000) as f64)),
                overdue: rng.range(0, 10) == 0,
            })
            .collect()
    }

    /// The capacity computed once decides as the two computations did, and is what
    /// update returned: the same targets, times and moves at the same instants, over
    /// random bounds, resumes, paths and gaps.
    #[test]
    fn one_capacity_decides_as_two_did() {
        let kbps = |k: u64| Kbps::new(k as f64);
        let mut rng = Rng(0xD1B54A32D192ED03);
        let t0 = Instant::now();
        for _ in 0..2000 {
            let floor = kbps(rng.range(0, 20_000));
            let ceiling = (rng.range(0, 4) > 0).then(|| kbps(rng.range(1, 40_000)));
            let resume = (rng.range(0, 2) > 0).then(|| kbps(rng.range(0, 100_000)));
            let mut new = RateCtl::new(Bounds::new(floor, ceiling), resume);
            let mut old = RateCtl::new(Bounds::new(floor, ceiling), resume);
            let mut now = t0;
            for _ in 0..200 {
                now += gap(&mut rng);
                let paths = paths(&mut rng);
                let capacity = new.step(now, &paths);
                old_step(&mut old, now, &paths);
                assert_eq!(capacity, old_update_capacity(&paths));
                assert_eq!(
                    (new.target, new.decided, new.moved),
                    (old.target, old.decided, old.moved)
                );
            }
        }
    }
}
