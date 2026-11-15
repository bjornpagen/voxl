//! What a client's acks say about the path to it: how long a frame takes when nothing
//! is queued, how fast the path delivers, and how much is queued on it now.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// The smallest send->ack delay of this long is the path's own: round trip plus decode,
/// with nothing queued. Long enough that a burst of big frames can't become the baseline,
/// short enough to follow a path that changes. The window is measured back from the
/// newest sample, not from now: a screen that sits still says nothing new about the
/// path, and what was learned before the stillness still holds after it.
const BASE_WINDOW: Duration = Duration::from_secs(30);
/// Capacity is read from the rate samples of this long, measured back from the newest:
/// long enough that a stretch of frames that all queued still has, behind it, frames
/// that didn't.
const CAPACITY_WINDOW: Duration = Duration::from_secs(10);
/// The capacity is the rate this many of those samples reached. A frame can't cross
/// faster than the path carries it, so on a path without jitter the fastest sample is
/// the capacity; on a real one an ack that comes a few ms early reads as a path
/// several times faster, and one or two of those must not set the rate.
const CAPACITY_AGREE: usize = 3;
/// Delays are only known to a few ms (the clock, the client's decoder, the network's
/// own jitter). A frame the path is thought to carry in less than this holds no rate:
/// its delay is all jitter. One that should take longer and didn't proves a rate of its
/// size over this long and no more, so a path faster than thought reads low until
/// bigger frames cross it, and never high.
const MIN_WIRE_MS: f64 = 5.0;
/// Before any rate is known, the size (beyond the baseline's frame) that can tell one
/// whatever it took: on a fast path nothing takes `MIN_WIRE_MS`, and the first rate has
/// to come from a frame's size over that floor.
const PROBE_BYTES: usize = 16 << 10;
/// A frame too small to tell a rate by its size can still show a slow path by its time:
/// sent with nothing ahead of it, it took `MIN_WIRE_MS` or more. One such frame is as
/// likely a late ack as a slow path, so they count only as a run: `CAPACITY_AGREE` or
/// more in a row, with no frame between them that was on time, over at least this long.
/// Jitter comes in spikes and leaves frames on time between them; a slow path does not.
const SLOW_RUN: Duration = Duration::from_secs(1);
/// And only when the run reads under this share of the known capacity: what was learned
/// before it is then of a path that is gone.
const SLOWED_SHARE: f64 = 0.5;
/// The queue estimate halves over this long without an ack: nothing in flight means
/// nothing is queued, however the last frame fared.
const QUEUE_HALF_LIFE: Duration = Duration::from_millis(150);

/// A rate, in kbit/s.
#[derive(Clone, Copy, PartialEq, PartialOrd, Debug)]
pub struct Kbps(f64);

impl Kbps {
    pub const fn new(kbps: f64) -> Self {
        Kbps(kbps)
    }
    pub fn get(self) -> f64 {
        self.0
    }
    pub fn bps(self) -> f64 {
        self.0 * 1000.0
    }
    /// What this rate carries in `time`.
    pub fn over(self, time: Duration) -> Bits {
        Bits(self.bps() * time.as_secs_f64())
    }
    pub fn times(self, factor: f64) -> Self {
        Kbps(self.0 * factor)
    }
    pub fn min(self, other: Self) -> Self {
        Kbps(self.0.min(other.0))
    }
    pub fn max(self, other: Self) -> Self {
        Kbps(self.0.max(other.0))
    }
}

/// An amount of data, in bits. Below zero it is a debt.
#[derive(Clone, Copy, PartialEq, PartialOrd, Debug)]
pub struct Bits(f64);

impl Bits {
    pub const ZERO: Bits = Bits(0.0);
    pub fn of_bytes(bytes: usize) -> Self {
        Bits(bytes as f64 * 8.0)
    }
    #[cfg(test)]
    pub fn bytes(self) -> f64 {
        self.0 / 8.0
    }
    pub fn times(self, factor: f64) -> Self {
        Bits(self.0 * factor)
    }
    pub fn min(self, other: Self) -> Self {
        Bits(self.0.min(other.0))
    }
    pub fn max(self, other: Self) -> Self {
        Bits(self.0.max(other.0))
    }
    /// The rate that sends this much in every frame at `fps`: how x264's VBV is told
    /// what one frame may take.
    pub fn a_frame_at(self, fps: u32) -> Kbps {
        Kbps(self.0 * fps as f64 / 1000.0)
    }
}

impl std::ops::Add for Bits {
    type Output = Bits;
    fn add(self, other: Bits) -> Bits {
        Bits(self.0 + other.0)
    }
}

impl std::ops::Sub for Bits {
    type Output = Bits;
    fn sub(self, other: Bits) -> Bits {
        Bits(self.0 - other.0)
    }
}

/// What the paths of everyone watching can carry: one stream serves them all, so the
/// slowest decides.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Capacity {
    /// Nobody is watching.
    NoViewers,
    /// Somebody's path has not shown its rate yet.
    Unknown,
    /// The slowest path's rate.
    Known(Kbps),
}

impl Capacity {
    /// From each watched path's own capacity (None: not known yet).
    pub fn of(paths: impl IntoIterator<Item = Option<Kbps>>) -> Capacity {
        paths
            .into_iter()
            .fold(Capacity::NoViewers, |all, path| match (all, path) {
                (Capacity::Unknown, _) | (_, None) => Capacity::Unknown,
                (Capacity::NoViewers, Some(c)) => Capacity::Known(c),
                (Capacity::Known(slowest), Some(c)) => Capacity::Known(slowest.min(c)),
            })
    }
}

/// The smallest value over a sliding window of time.
struct Lowest {
    /// (when, value, the size of the frame it was measured on), values rising from the
    /// front: the front is the smallest.
    q: VecDeque<(Instant, f64, usize)>,
    window: Duration,
}

impl Lowest {
    fn new(window: Duration) -> Self {
        Lowest {
            q: VecDeque::new(),
            window,
        }
    }
    fn expire(&mut self, now: Instant) {
        while self
            .q
            .front()
            .is_some_and(|&(t, _, _)| now.saturating_duration_since(t) > self.window)
        {
            self.q.pop_front();
        }
    }
    /// Takes a value in and returns the smallest of the window, which holds it.
    fn push(&mut self, now: Instant, v: f64, bytes: usize) -> (f64, usize) {
        self.expire(now);
        while self.q.back().is_some_and(|&(_, b, _)| b >= v) {
            self.q.pop_back();
        }
        self.q.push_back((now, v, bytes));
        let &(_, v, bytes) = self.q.front().unwrap_or(&(now, v, bytes));
        (v, bytes)
    }
    fn get(&self) -> Option<(f64, usize)> {
        self.q.front().map(|&(_, v, bytes)| (v, bytes))
    }
}

/// Send->ack delays turned into the three things flow control needs.
///
/// A frame's delay beyond the baseline is the time its extra bytes took on the wire
/// (how much bigger it is than the baseline's frame, over the path's capacity) plus
/// whatever it waited behind. So the best extra-size-over-excess that several recent
/// frames agree on is the capacity, measured by the frames themselves, with no need for
/// the stream to press against its cap first; and what is left of a delay once the
/// frame's own wire time is taken off is the queue.
///
/// The baseline is the smallest delay of the window, so it follows a round trip that
/// grows even while the screen never stops moving. It needs, in every window, one frame
/// that waited behind nothing; flow control gives it that. Where it matters that is not
/// assumed but checked against `clean`, the smallest delay of frames sent with nothing
/// of ours ahead of them: such a frame did not queue, so a baseline no more than
/// `MIN_WIRE_MS` under it did not either.
pub struct PathEst {
    base: Lowest,
    clean: Lowest,
    /// When the frame before this one was acknowledged.
    acked: Option<Instant>,
    /// The latest rate samples, oldest first.
    rates: VecDeque<Rate>,
    /// The run of frames, each sent with nothing ahead of it, that were too small to
    /// tell a rate by size and still took time to cross (see `SLOW_RUN`).
    slow: Vec<Rate>,
    capacity: Option<f64>,
    queue_ms: f64,
    queue_at: Instant,
}

/// The rate one frame crossed at.
#[derive(Clone, Copy)]
struct Rate {
    at: Instant,
    bps: f64,
}

impl PathEst {
    pub fn new(now: Instant) -> Self {
        PathEst {
            base: Lowest::new(BASE_WINDOW),
            clean: Lowest::new(BASE_WINDOW),
            acked: None,
            rates: VecDeque::new(),
            slow: Vec::new(),
            capacity: None,
            queue_ms: 0.0,
            queue_at: now,
        }
    }

    /// A frame of `bytes`, sent at `sent`, was acknowledged at `now`. `alone`: nothing
    /// sent before it was still unacknowledged when it went.
    pub fn on_ack(&mut self, now: Instant, sent: Instant, bytes: usize, alone: bool) {
        let delay_ms = now.saturating_duration_since(sent).as_secs_f64() * 1000.0;
        let (base, base_bytes) = self.base.push(now, delay_ms, bytes);
        let excess_ms = delay_ms - base;
        // The baseline did not queue: a frame known not to have was no slower.
        let trusted = |clean: &Lowest| {
            clean
                .get()
                .is_some_and(|(clean, _)| clean <= base + MIN_WIRE_MS)
        };
        // This frame did not queue behind ours. Either nothing was unacknowledged when
        // it went, which is a fact whatever the path. Or the frame before it had left
        // the bottleneck by then: that frame's ack came one baseline after it left, less
        // the baseline frame's own time on the wire, which takes a known capacity to
        // say. The second is what lets a path with a long round trip, where a frame is
        // rarely sent alone, keep saying so.
        let wire_of_base = self
            .capacity
            .filter(|_| trusted(&self.clean))
            .map(|bps| Duration::from_secs_f64(base_bytes as f64 * 8.0 / bps));
        let clear = alone
            || wire_of_base.is_some_and(|wire| {
                self.acked.is_none_or(|acked| {
                    sent + Duration::from_secs_f64(base / 1000.0) >= acked + wire
                })
            });
        if clear {
            self.clean.push(now, delay_ms, bytes);
        }
        let base_clean = trusted(&self.clean);
        self.acked = Some(now);
        let extra_bits = bytes.saturating_sub(base_bytes) as f64 * 8.0;
        let rate = Rate {
            at: now,
            bps: extra_bits / (excess_ms.max(MIN_WIRE_MS) / 1000.0),
        };
        // Big enough to tell a rate: the path should take `MIN_WIRE_MS` to carry it.
        // Before any rate is known, a fixed size, and only against a baseline that
        // queued behind nothing: a session's first frames may all have.
        let by_size = match self.capacity {
            Some(bps) => extra_bits >= bps * MIN_WIRE_MS / 1000.0,
            None => base_clean && bytes >= base_bytes + PROBE_BYTES,
        };
        if by_size {
            self.slow.clear();
            // The window is measured back from the newest sample, and never empties
            // below what has to agree: after a still spell one sample alone is not
            // the capacity.
            while self.rates.len() >= CAPACITY_AGREE
                && self
                    .rates
                    .front()
                    .is_some_and(|r| now.saturating_duration_since(r.at) > CAPACITY_WINDOW)
            {
                self.rates.pop_front();
            }
            self.rates.push_back(rate);
            self.capacity = self.agreed();
        } else if clear && base_clean {
            if extra_bits > 0.0 && excess_ms >= MIN_WIRE_MS {
                self.slow.push(rate);
                let long = self
                    .slow
                    .first()
                    .is_some_and(|first| now.saturating_duration_since(first.at) >= SLOW_RUN);
                let slowed = self
                    .capacity
                    .is_none_or(|known| self.slow.iter().all(|r| r.bps < known * SLOWED_SHARE));
                if long && self.slow.len() >= CAPACITY_AGREE && slowed {
                    self.rates = self.slow.drain(..).collect();
                    self.capacity = self.agreed();
                }
            } else {
                // On time: whatever made the frames before it late was not the path.
                self.slow.clear();
            }
        }
        let wire_ms = self.capacity.map_or(0.0, |bps| extra_bits / bps * 1000.0);
        let q = (excess_ms - wire_ms).max(0.0);
        self.queue_ms = self.queue_ms(now, false) * 0.7 + q * 0.3;
        self.queue_at = now;
    }

    /// The rate `CAPACITY_AGREE` of the samples reached; until that many have been
    /// seen, the slowest of them.
    fn agreed(&self) -> Option<f64> {
        let mut best = [0.0f64; CAPACITY_AGREE];
        for r in &self.rates {
            // `best` holds the fastest samples, fastest first.
            if let Some(i) = best.iter().position(|&b| r.bps > b) {
                best[i..].rotate_right(1);
                best[i] = r.bps;
            }
        }
        (!self.rates.is_empty()).then(|| best[self.rates.len().min(CAPACITY_AGREE) - 1])
    }

    /// Delay queued on the path now (ms). With nothing in flight (`idle`) the last
    /// estimate fades: no ack is coming to correct it.
    pub fn queue_ms(&self, now: Instant, idle: bool) -> f64 {
        if !idle {
            return self.queue_ms;
        }
        let since = now.saturating_duration_since(self.queue_at).as_secs_f64();
        self.queue_ms * 0.5f64.powf(since / QUEUE_HALF_LIFE.as_secs_f64())
    }

    /// What the path can carry, once frames big enough to tell have been acked.
    pub fn capacity(&self) -> Option<Kbps> {
        self.capacity.map(|bps| Kbps::new(bps / 1000.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path as the estimator's oracle: `rate` bit/s behind a queue that serves frames
    /// in order, and `rtt` of delay that has nothing to do with size.
    struct Link {
        rate: f64,
        rtt: Duration,
        /// When the bottleneck is free again.
        free: Instant,
        /// When the last frame sent was acknowledged, and whether it was sent with
        /// nothing unacknowledged ahead of it.
        acked: Instant,
        alone: bool,
    }

    impl Link {
        fn new(rate: f64, rtt_ms: u64, now: Instant) -> Self {
            Link {
                rate,
                rtt: Duration::from_millis(rtt_ms),
                free: now,
                acked: now,
                alone: true,
            }
        }
        /// When a frame of `bytes` sent at `sent` is acknowledged, and how long it queued.
        fn send(&mut self, sent: Instant, bytes: usize) -> (Instant, Duration) {
            let start = self.free.max(sent);
            let wire = Duration::from_secs_f64(bytes as f64 * 8.0 / self.rate);
            self.free = start + wire;
            self.alone = sent >= self.acked;
            self.acked = self.free + self.rtt;
            (self.acked, start - sent)
        }
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn range(&mut self, lo: u64, hi: u64) -> u64 {
            lo + self.next() % (hi - lo)
        }
    }

    /// Drives the estimator with a schedule of frames over a link; acks arrive in order.
    /// Returns the estimator and the time of the last ack.
    fn run(est: &mut PathEst, link: &mut Link, frames: &[(Instant, usize)]) -> Instant {
        let mut last = frames[0].0;
        for &(sent, bytes) in frames {
            let (acked, _) = link.send(sent, bytes);
            est.on_ack(acked, sent, bytes, link.alone);
            last = acked;
        }
        last
    }

    /// Small frames at a steady rate: typing, a cursor, a clock.
    fn trickle(from: Instant, n: usize, every_ms: u64, bytes: usize) -> Vec<(Instant, usize)> {
        (0..n)
            .map(|i| (from + Duration::from_millis(every_ms * i as u64), bytes))
            .collect()
    }

    /// A mostly still screen with the odd burst of big frames that never fills the link:
    /// `bursts` of 3 to 5 frames of `size()` bytes, far enough apart that none waits
    /// behind another.
    fn bursts(
        rng: &mut Rng,
        t0: Instant,
        rate: f64,
        mut size: impl FnMut(&mut Rng) -> usize,
    ) -> Vec<(Instant, usize)> {
        let mut frames = trickle(t0, 30, 16, 600);
        let mut t = t0 + Duration::from_millis(600);
        for _ in 0..rng.range(2, 6) {
            t += Duration::from_millis(rng.range(300, 3000));
            for _ in 0..rng.range(3, 6) {
                let bytes = size(rng);
                frames.push((t, bytes));
                t += Duration::from_secs_f64(bytes as f64 * 8.0 / rate)
                    + Duration::from_millis(rng.range(20, 60));
            }
        }
        frames
    }

    #[test]
    fn the_slowest_known_path_decides_and_an_unknown_one_makes_it_unknown() {
        // Against the sentence, for every way three paths can be: absent, not known
        // yet, or known at one of two rates.
        let rates = [None, Some(Kbps::new(3000.0)), Some(Kbps::new(40_000.0))];
        for case in 0..4usize.pow(3) {
            let paths: Vec<Option<Kbps>> = (0..3)
                .map(|i| case / 4usize.pow(i) % 4)
                .filter(|&s| s > 0)
                .map(|s| rates[s - 1])
                .collect();
            let want = if paths.is_empty() {
                Capacity::NoViewers
            } else if paths.contains(&None) {
                Capacity::Unknown
            } else {
                let slowest = paths
                    .iter()
                    .flatten()
                    .map(|k| k.get())
                    .fold(f64::MAX, f64::min);
                Capacity::Known(Kbps::new(slowest))
            };
            assert_eq!(Capacity::of(paths.clone()), want, "{paths:?}");
        }
    }

    #[test]
    fn capacity_comes_from_the_frames_themselves() {
        // Any link, any round trip, frames that take longer to cross than delays can be
        // told apart: the estimate lands on the link's rate.
        let mut rng = Rng(0x9E3779B97F4A7C15);
        for _ in 0..200 {
            let rate = rng.range(2_000_000, 80_000_000) as f64;
            let rtt = rng.range(5, 200);
            let t0 = Instant::now();
            let mut link = Link::new(rate, rtt, t0);
            let mut est = PathEst::new(t0);
            let least = (rate * 0.006 / 8.0) as u64 + 600;
            let frames = bursts(&mut rng, t0, rate, |rng| {
                rng.range(least.max(20_000), least.max(20_000) * 3) as usize
            });
            run(&mut est, &mut link, &frames);
            let got = est
                .capacity()
                .map(Kbps::bps)
                .expect("big frames were acked");
            assert!(
                (got - rate).abs() <= rate * 0.001,
                "rate {rate}, rtt {rtt} ms: estimated {got}"
            );
        }
    }

    #[test]
    fn a_slow_path_shows_its_rate_through_small_frames() {
        // A stream that never sends a big frame, on a path slow enough that its frames
        // take time to cross: the rate is read from them, where a fixed size to qualify
        // would wait for a frame that never comes.
        let mut rng = Rng(0x2545F4914F6CDD1D);
        for _ in 0..200 {
            let rate = rng.range(500_000, 4_000_000) as f64;
            let t0 = Instant::now();
            let mut link = Link::new(rate, rng.range(5, 35), t0);
            let mut est = PathEst::new(t0);
            let mut frames = trickle(t0, 30, 16, 300);
            let mut t = t0 + Duration::from_secs(1);
            for _ in 0..40 {
                // 5 to 12 ms of wire time each, each sent after the one before it was
                // acknowledged: before any rate is known that is the only way to know
                // a frame did not queue.
                let bytes = (rate * rng.range(5_000, 12_000) as f64 / 8e6) as usize + 300;
                frames.push((t, bytes));
                t += Duration::from_millis(rng.range(40, 200));
            }
            assert!(frames.iter().all(|f| f.1 < PROBE_BYTES), "rate {rate}");
            run(&mut est, &mut link, &frames);
            let got = est
                .capacity()
                .map(Kbps::bps)
                .expect("frames took time to cross");
            assert!(
                (got - rate).abs() <= rate * 0.001,
                "rate {rate}: estimated {got}"
            );
        }
    }

    #[test]
    fn frames_too_quick_to_time_read_low_never_high() {
        // Frames of any size on any link: one that crosses in under MIN_WIRE_MS proves
        // only its size over that long.
        let mut rng = Rng(0x94D049BB133111EB);
        for _ in 0..200 {
            let rate = rng.range(2_000_000, 200_000_000) as f64;
            let t0 = Instant::now();
            let mut link = Link::new(rate, rng.range(5, 200), t0);
            let mut est = PathEst::new(t0);
            let frames = bursts(&mut rng, t0, rate, |rng| {
                rng.range(20_000, 120_000) as usize
            });
            let biggest = frames.iter().map(|f| f.1).max().unwrap();
            run(&mut est, &mut link, &frames);
            let got = est
                .capacity()
                .map(Kbps::bps)
                .expect("big frames were acked");
            assert!(got <= rate * 1.001, "rate {rate}: estimated {got}");
            let provable = (biggest - 600) as f64 * 8.0 / (MIN_WIRE_MS / 1000.0);
            assert!(
                got <= provable * 1.001,
                "rate {rate}: {got} from {biggest} bytes"
            );
        }
    }

    #[test]
    fn jitter_moves_the_estimate_by_no_more_than_its_share_of_the_wire_time() {
        // Every ack late by up to 4 ms, at random, on frames that take 20 ms or more to
        // cross: the estimate stays within what 4 ms is of 20.
        let mut rng = Rng(0x2545F4914F6CDD1D);
        for _ in 0..200 {
            let rate = rng.range(2_000_000, 60_000_000) as f64;
            let t0 = Instant::now();
            let mut link = Link::new(rate, rng.range(5, 200), t0);
            let mut est = PathEst::new(t0);
            let least = (rate * 0.020 / 8.0) as u64 + 600;
            let frames = bursts(&mut rng, t0, rate, |rng| {
                rng.range(least, least * 3) as usize
            });
            for (sent, bytes) in frames {
                let (acked, _) = link.send(sent, bytes);
                est.on_ack(
                    acked + Duration::from_micros(rng.range(0, 4000)),
                    sent,
                    bytes,
                    link.alone,
                );
            }
            let got = est
                .capacity()
                .map(Kbps::bps)
                .expect("big frames were acked");
            assert!(
                got >= rate * 0.8 && got <= rate * 1.25,
                "rate {rate}: estimated {got}"
            );
        }
    }

    #[test]
    fn two_early_acks_do_not_set_the_rate() {
        // An ack now and then that comes long before its frame can have crossed (a
        // client that acknowledged part of it): fewer than CAPACITY_AGREE of them in
        // the window read as nothing.
        let mut rng = Rng(0xDA942042E4DD58B5);
        for _ in 0..200 {
            let rate = rng.range(2_000_000, 60_000_000) as f64;
            let rtt = rng.range(5, 200);
            let t0 = Instant::now();
            let mut link = Link::new(rate, rtt, t0);
            let mut est = PathEst::new(t0);
            let least = (rate * 0.020 / 8.0) as u64 + 600;
            let frames = bursts(&mut rng, t0, rate, |rng| {
                rng.range(least, least * 3) as usize
            });
            let early = [frames.len() - 2, frames.len() - 5];
            for (i, &(sent, bytes)) in frames.iter().enumerate() {
                let (acked, _) = link.send(sent, bytes);
                let acked = if early.contains(&i) {
                    sent + Duration::from_millis(rtt)
                        + (acked - sent - Duration::from_millis(rtt)) / 4
                } else {
                    acked
                };
                est.on_ack(acked, sent, bytes, link.alone);
            }
            let got = est
                .capacity()
                .map(Kbps::bps)
                .expect("big frames were acked");
            assert!(got <= rate * 1.001, "rate {rate}: estimated {got}");
        }
    }

    #[test]
    fn capacity_is_never_overestimated() {
        // Whatever is sent, however it queues: a frame can't arrive faster than the link
        // carries it, so no schedule may read as a faster link. The one thing the
        // baseline needs is a frame per window that waited behind nothing, which flow
        // control provides by draining the queue; here, a small frame on an empty link
        // every 15 s.
        let mut rng = Rng(0xD1B54A32D192ED03);
        for _ in 0..200 {
            let rate = rng.range(1_000_000, 60_000_000) as f64;
            let t0 = Instant::now();
            let mut link = Link::new(rate, rng.range(5, 200), t0);
            let mut est = PathEst::new(t0);
            run(&mut est, &mut link, &trickle(t0, 10, 16, 500));
            let (mut t, mut clean) = (t0 + Duration::from_millis(200), t0);
            for _ in 0..rng.range(20, 400) {
                t += Duration::from_millis(rng.range(1, 400));
                let bytes = if t.duration_since(clean) > Duration::from_secs(15) {
                    t = t.max(link.free) + Duration::from_millis(1);
                    clean = t;
                    500
                } else {
                    rng.range(200, 200_000) as usize
                };
                let (acked, _) = link.send(t, bytes);
                est.on_ack(acked, t, bytes, link.alone);
            }
            if let Some(got) = est.capacity().map(Kbps::bps) {
                assert!(got <= rate * 1.001, "rate {rate}: estimated {got}");
            }
        }
    }

    #[test]
    fn a_big_frame_alone_is_not_a_queue() {
        // One large frame on an idle link takes its own time to cross and waits behind
        // nothing: once the path's capacity is known, that is not congestion.
        let mut rng = Rng(0xA0761D6478BD642F);
        for _ in 0..100 {
            let rate = rng.range(4_000_000, 60_000_000) as f64;
            let t0 = Instant::now();
            let mut link = Link::new(rate, rng.range(10, 150), t0);
            let mut est = PathEst::new(t0);
            let mut frames = trickle(t0, 30, 16, 600);
            let mut t = t0 + Duration::from_secs(1);
            for _ in 0..12 {
                frames.push((t, rng.range(30_000, 90_000) as usize));
                t += Duration::from_millis(rng.range(400, 900));
            }
            let end = run(&mut est, &mut link, &frames);
            let q = est.queue_ms(end, false);
            assert!(
                q < 3.0,
                "rate {rate}: queue {q} ms from frames that never queued"
            );
        }
    }

    #[test]
    fn a_standing_queue_shows() {
        // Frames sent faster than the link carries them wait behind each other, and
        // the estimate follows the wait the link really imposed.
        let mut rng = Rng(0xE7037ED1A0B428DB);
        for _ in 0..100 {
            let rate = rng.range(2_000_000, 20_000_000) as f64;
            let t0 = Instant::now();
            let mut link = Link::new(rate, rng.range(10, 150), t0);
            let mut est = PathEst::new(t0);
            // Learn the path: small frames, then spaced big ones.
            let mut frames = trickle(t0, 30, 16, 600);
            let mut t = t0 + Duration::from_secs(1);
            for _ in 0..6 {
                frames.push((t, 40_000));
                t += Duration::from_millis(500);
            }
            run(&mut est, &mut link, &frames);
            // Then 1.5x the link's rate at 60 fps for a second.
            let bytes = (rate * 1.5 / 8.0 / 60.0) as usize;
            let mut waited = Duration::ZERO;
            let mut end = t;
            for i in 0..60 {
                let sent = t + Duration::from_micros(16_667 * i);
                let (acked, w) = link.send(sent, bytes);
                est.on_ack(acked, sent, bytes, link.alone);
                waited = w;
                end = acked;
            }
            let q = est.queue_ms(end, false);
            let real = waited.as_secs_f64() * 1000.0;
            assert!(real > 100.0, "the overload queued {real} ms");
            assert!(
                q > real * 0.5 && q < real * 1.1,
                "rate {rate}: estimated queue {q} ms, the link's {real} ms"
            );
        }
    }

    #[test]
    fn the_baseline_is_the_windows_minimum() {
        // A stretch of nothing but big frames must not become the baseline: when small
        // frames return within the window, their delay is still the path's own.
        let t0 = Instant::now();
        let mut link = Link::new(8_000_000.0, 60, t0);
        let mut est = PathEst::new(t0);
        let mut frames = trickle(t0, 30, 16, 600);
        // 12 s of big frames, each alone on the link (60 ms of wire time each).
        for i in 0..80 {
            frames.push((
                t0 + Duration::from_secs(1) + Duration::from_millis(150 * i),
                60_000,
            ));
        }
        let end = run(&mut est, &mut link, &frames);
        let q = est.queue_ms(end, false);
        assert!(q < 3.0, "queue {q} ms: the big frames became the baseline");
        let got = est.capacity().map(Kbps::bps).unwrap();
        assert!((got - 8_000_000.0).abs() < 200_000.0, "capacity {got}");
    }

    #[test]
    fn the_queue_estimate_fades_only_when_idle() {
        let t0 = Instant::now();
        let mut link = Link::new(4_000_000.0, 40, t0);
        let mut est = PathEst::new(t0);
        let mut frames = trickle(t0, 30, 16, 600);
        for i in 0..6 {
            frames.push((
                t0 + Duration::from_secs(1) + Duration::from_millis(400 * i),
                40_000,
            ));
        }
        // Overload: 30 frames of 40 KB in half a second on a 4 Mbit/s link.
        let burst = t0 + Duration::from_secs(4);
        for i in 0..30 {
            frames.push((burst + Duration::from_millis(16 * i), 40_000));
        }
        let end = run(&mut est, &mut link, &frames);
        let q = est.queue_ms(end, false);
        assert!(q > 100.0, "the burst queued: {q} ms");
        // Frames still in flight: the estimate stands, whatever time passes.
        assert_eq!(est.queue_ms(end + Duration::from_secs(2), false), q);
        // Nothing in flight: it halves every 150 ms.
        let later = est.queue_ms(end + Duration::from_millis(150), true);
        assert!(
            (later - q / 2.0).abs() < q * 0.01,
            "{later} after one half-life of {q}"
        );
        assert!(est.queue_ms(end + Duration::from_secs(2), true) < 1.0);
    }

    /// After longer than CAPACITY_WINDOW without a frame that tells (a still screen,
    /// typing), the next sample is not the capacity on its own: the window keeps what
    /// has to agree, so one early ack after a still spell reads as nothing, as two do
    /// among good samples (two_early_acks_do_not_set_the_rate).
    #[test]
    fn one_early_ack_after_a_still_spell_does_not_set_the_rate() {
        let rate = 20_000_000.0;
        let t0 = Instant::now();
        let mut link = Link::new(rate, 50, t0);
        let mut est = PathEst::new(t0);
        let mut frames = trickle(t0, 30, 16, 600);
        for i in 0..6 {
            frames.push((t0 + Duration::from_millis(1000 + 500 * i), 40_000));
        }
        let end = run(&mut est, &mut link, &frames);
        assert!((est.capacity().unwrap().bps() - rate).abs() <= rate * 0.001);
        // 11 s of small frames: none tells.
        let end = run(
            &mut est,
            &mut link,
            &trickle(end + Duration::from_millis(100), 110, 100, 600),
        );
        assert!((est.capacity().unwrap().bps() - rate).abs() <= rate * 0.001);
        // One 40 KB frame (16 ms on the wire), acknowledged a quarter of its wire time
        // after the round trip, as two_early_acks_do_not_set_the_rate's early acks are.
        let sent = end + Duration::from_millis(500);
        let (acked, _) = link.send(sent, 40_000);
        let rtt = Duration::from_millis(50);
        est.on_ack(
            sent + rtt + (acked - sent - rtt) / 4,
            sent,
            40_000,
            link.alone,
        );
        let got = est.capacity().map(Kbps::bps).unwrap();
        assert!(got <= rate * 1.001, "rate {rate}: estimated {got}");
    }

    /// A session that opens with a keyframe, and the frames flow control lets out while
    /// it is still on the wire, has among its first acks no frame that waited behind
    /// nothing but the keyframe itself. The baseline is only ever a frame sent with
    /// nothing ahead of it, so the frames that queued behind the keyframe are not it,
    /// and the first big frame on the idle link is not read against them.
    #[test]
    fn a_session_that_opens_queued_is_not_read_as_a_faster_path() {
        let rate = 10_000_000.0;
        let t0 = Instant::now();
        let mut link = Link::new(rate, 50, t0);
        let mut est = PathEst::new(t0);
        // A 200 KB keyframe (160 ms on the wire), then seven 1 KB frames at 60 fps
        // behind it: what flow control lets go on a 50 ms path (eight unacknowledged).
        let mut frames = vec![(t0, 200_000)];
        for i in 1..8u32 {
            frames.push((t0 + Duration::from_micros(16_667) * i, 1_000));
        }
        run(&mut est, &mut link, &frames);
        // The screen is still, then one 100 KB frame crosses the idle link.
        let sent = t0 + Duration::from_secs(1);
        let (acked, waited) = link.send(sent, 100_000);
        assert_eq!(waited, Duration::ZERO);
        est.on_ack(acked, sent, 100_000, link.alone);
        // Nothing may be read at all from a session like this; whatever is, is not over.
        let got = est.capacity().map(Kbps::bps);
        assert!(
            got.is_none_or(|got| got <= rate * 1.001),
            "rate {rate}: estimated {got:?}"
        );
    }

    /// A path that slows after its rate is known is read again from frames that take
    /// time to cross it, as a path not yet known is
    /// (a_slow_path_shows_its_rate_through_small_frames): a frame sent with nothing
    /// ahead of it that still took time has only the path to blame.
    #[test]
    fn a_path_that_slows_shows_it_through_frames_that_take_time_to_cross() {
        let t0 = Instant::now();
        let typing = |from: Instant| trickle(from, 110, 100, 4_000);
        // The oracle: the same frames on a path that was always 4 Mbit/s read its rate.
        let mut slow = Link::new(4_000_000.0, 50, t0);
        let mut known = PathEst::new(t0);
        let end = run(&mut known, &mut slow, &trickle(t0, 30, 16, 600));
        run(
            &mut known,
            &mut slow,
            &typing(end + Duration::from_millis(100)),
        );
        let oracle = known.capacity().map(Kbps::bps).unwrap();
        assert!((oracle - 4_000_000.0).abs() <= 4_000.0, "{oracle}");
        // A path that was 40 Mbit/s, learned from big frames, then slows to 4.
        let mut link = Link::new(40_000_000.0, 50, t0);
        let mut est = PathEst::new(t0);
        let mut frames = trickle(t0, 30, 16, 600);
        for i in 0..6 {
            frames.push((t0 + Duration::from_millis(1000 + 500 * i), 40_000));
        }
        let end = run(&mut est, &mut link, &frames);
        assert!((est.capacity().unwrap().bps() - 40_000_000.0).abs() <= 40_000.0);
        link.rate = 4_000_000.0;
        // 4 KB frames, 8 ms each on the slow link, every 100 ms for CAPACITY_WINDOW and
        // a second: typing.
        let from = end + Duration::from_millis(100);
        let last = run(&mut est, &mut link, &typing(from));
        let got = est.capacity().map(Kbps::bps).unwrap();
        assert!(
            got <= 4_000_000.0 * 1.25,
            "estimated {got} on a 4 Mbit/s link {:?} after it slowed",
            last - from
        );
    }

    /// BASE_WINDOW's doc: short enough to follow a path that changes.
    /// Since 4dc5b5a only a frame sent with nothing unacknowledged ahead of it can be
    /// the baseline, and under sustained motion on a path whose round trip is longer
    /// than a frame time no frame is: the baseline is the session's first frame for as
    /// long as the motion lasts, and a round trip that grows reads as a queue.
    #[test]
    fn a_longer_round_trip_under_motion_is_not_a_queue_for_longer_than_the_base_window() {
        let t0 = Instant::now();
        let mut link = Link::new(20_000_000.0, 30, t0);
        let mut est = PathEst::new(t0);
        // Half the path's rate at 60 fps: 20.8 KB frames, 8.3 ms each on the wire.
        let frames: Vec<(Instant, usize)> = (0..60 * 41)
            .map(|i| (t0 + Duration::from_micros(16_667 * i), 20_833))
            .collect();
        let (before, after) = frames.split_at(60 * 10);
        run(&mut est, &mut link, before);
        // The round trip grows by 50 ms (another route, a busier hop).
        link.rtt += Duration::from_millis(50);
        let end = run(&mut est, &mut link, after);
        let q = est.queue_ms(end, false);
        assert!(
            q <= 15.0,
            "{q:.1} ms of queue {:?} after the round trip grew, on a link that queues nothing",
            end - after[0].0
        );
    }

    /// MIN_WIRE_MS's doc: a frame the path is thought to carry in
    /// less than that holds no rate, its delay is all jitter. Since 4dc5b5a a frame
    /// sent alone that took MIN_WIRE_MS tells whatever its size, so a small frame whose
    /// ack is that late reads as a path of its size over MIN_WIRE_MS; and after a still
    /// spell the window keeps two old samples and the new one, so that one sample is
    /// the capacity.
    #[test]
    fn a_late_ack_on_a_small_frame_holds_no_rate() {
        let rate = 40_000_000.0;
        let t0 = Instant::now();
        let mut link = Link::new(rate, 50, t0);
        let mut est = PathEst::new(t0);
        let mut frames = trickle(t0, 30, 16, 600);
        for i in 0..6 {
            frames.push((t0 + Duration::from_millis(1000 + 500 * i), 40_000));
        }
        let end = run(&mut est, &mut link, &frames);
        assert!((est.capacity().unwrap().bps() - rate).abs() <= rate * 0.001);
        // 11 s of typing, acknowledged on time.
        let end = run(
            &mut est,
            &mut link,
            &trickle(end + Duration::from_millis(100), 110, 100, 600),
        );
        // One more keystroke, 100 bytes bigger, whose ack comes 6 ms late: what
        // reply 9 measured as the server's and the viewer's lag at its 10th percentile.
        let sent = end + Duration::from_millis(100);
        let (acked, _) = link.send(sent, 700);
        est.on_ack(acked + Duration::from_millis(6), sent, 700, link.alone);
        let got = est.capacity().map(Kbps::bps).unwrap();
        assert!(got >= rate * 0.999, "rate {rate}: estimated {got}");
    }

    /// capacity_is_never_overestimated: a frame cannot arrive faster
    /// than the link carries it, so no schedule may read as a faster link, given a small
    /// frame on an empty link within the window; and acks are only ever late, which reads
    /// low, never high. Here every ack is on time but the session's first, and small
    /// frames cross the empty link all along. Since 4dc5b5a only a frame sent with nothing
    /// unacknowledged ahead of it can be the baseline, and on a path whose round trip is
    /// longer than the gap between a resting desktop's frames (a cursor, a clock) none of
    /// those is: the first frame's late ack stays the baseline, and every frame after it
    /// reads its wire time short by that lateness.
    #[test]
    fn on_a_long_path_frames_acked_on_time_do_not_read_it_faster() {
        let rate = 40_000_000.0;
        let t0 = Instant::now();
        let mut link = Link::new(rate, 140, t0);
        let mut est = PathEst::new(t0);
        // The session's first frame, its ack 2 ms late.
        let (acked, _) = link.send(t0, 600);
        assert!(link.alone);
        est.on_ack(acked + Duration::from_millis(2), t0, 600, link.alone);
        // A desktop at rest for 2 s: 600 B every 117 ms, each alone on the link and acked
        // on time, each sent while the one before is still unacknowledged.
        let rest = trickle(t0 + Duration::from_millis(117), 17, 117, 600);
        let end = run(&mut est, &mut link, &rest);
        // A scroll: 62 KB frames at 60 fps, 12.4 ms each on the wire, so none waits.
        let scroll = trickle(end, 60, 16, 62_000);
        run(&mut est, &mut link, &scroll);
        let got = est.capacity().map(Kbps::bps).unwrap();
        assert!(got <= rate * 1.001, "rate {rate}: estimated {got}");
    }

    /// A session in motion from its first frame, on a link slower than the motion: every
    /// frame after the first queues. Queueing is not a rate: nothing is read from such
    /// frames, where reading their wait as wire time gave a path of a few hundred bits
    /// a second.
    #[test]
    fn frames_that_all_queued_show_no_rate() {
        let mut rng = Rng(0xD6E8FEB86659FD93);
        for _ in 0..200 {
            let rate = rng.range(1_500_000, 6_000_000) as f64;
            let t0 = Instant::now();
            let mut link = Link::new(rate, rng.range(5, 200), t0);
            let mut est = PathEst::new(t0);
            let frames: Vec<(Instant, usize)> = (0..600)
                .map(|i| {
                    (
                        t0 + Duration::from_micros(16_667 * i),
                        rng.range(16_000, 24_000) as usize,
                    )
                })
                .collect();
            run(&mut est, &mut link, &frames);
            let got = est.capacity().map(Kbps::bps);
            assert!(
                got.is_none_or(|got| got >= rate * 0.5 && got <= rate * 1.001),
                "rate {rate}: estimated {got:?}"
            );
        }
    }
}
