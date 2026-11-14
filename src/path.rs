//! What a client's acks say about the path to it: how long a frame takes when nothing
//! is queued, how fast the path delivers, and how much is queued on it now.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// The smallest send->ack delay of this long is the path's own: round trip plus decode,
/// with nothing queued. Long enough that a burst of big frames can't become the baseline,
/// short enough to follow a path that changes. Both windows are measured back from the
/// newest sample, not from now: a screen that sits still says nothing new about the
/// path, and what was learned before the stillness still holds after it.
const BASE_WINDOW: Duration = Duration::from_secs(30);
/// The best delivery rate of this long is what the path can carry.
const CAPACITY_WINDOW: Duration = Duration::from_secs(10);
/// A frame smaller than this is delivered within the jitter of the clock and of the
/// client's decoder: its delay holds no rate.
const PROBE_BYTES: usize = 16 << 10;
/// A capacity sample needs a baseline that has seen this many acks, or the first big
/// frame of a session would be measured against itself.
const BASE_SAMPLES: u32 = 8;
/// The queue estimate halves over this long without an ack: nothing in flight means
/// nothing is queued, however the last frame fared.
const QUEUE_HALF_LIFE: Duration = Duration::from_millis(150);

/// The extreme (smallest or largest) value over a sliding window of time.
struct Windowed {
    /// (when, value, the size of the frame it was measured on), values monotonic from
    /// the front: the front is the extreme.
    q: VecDeque<(Instant, f64, usize)>,
    window: Duration,
    /// Whether `a` beats `b` for the front.
    better: fn(f64, f64) -> bool,
}

impl Windowed {
    fn new(window: Duration, better: fn(f64, f64) -> bool) -> Self {
        Windowed {
            q: VecDeque::new(),
            window,
            better,
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
    fn push(&mut self, now: Instant, v: f64, bytes: usize) {
        self.expire(now);
        while self.q.back().is_some_and(|&(_, b, _)| !(self.better)(b, v)) {
            self.q.pop_back();
        }
        self.q.push_back((now, v, bytes));
    }
    fn get(&self) -> Option<(f64, usize)> {
        self.q.front().map(|&(_, v, bytes)| (v, bytes))
    }
}

/// Send->ack delays turned into the three things flow control needs.
///
/// A frame's delay beyond the baseline is the time its extra bytes took on the wire
/// (how much bigger it is than the baseline's frame, over the path's capacity) plus
/// whatever it waited behind. So the largest extra-size-over-excess of recent frames is
/// the capacity, measured by the frames themselves, with no need for the stream to
/// press against its cap first; and what is left of a delay once the frame's own wire
/// time is taken off is the queue.
///
/// The baseline needs, in every window, one frame that waited behind nothing. Flow
/// control gives it that: it drains a queue within seconds, and a desktop is still most
/// of the time. Without one the baseline reads high, and capacity with it.
pub struct PathEst {
    base: Windowed,
    base_samples: u32,
    capacity: Windowed,
    queue_ms: f64,
    queue_at: Instant,
}

impl PathEst {
    pub fn new(now: Instant) -> Self {
        PathEst {
            base: Windowed::new(BASE_WINDOW, |a, b| a < b),
            base_samples: 0,
            capacity: Windowed::new(CAPACITY_WINDOW, |a, b| a > b),
            queue_ms: 0.0,
            queue_at: now,
        }
    }

    /// A frame of `bytes`, sent at `sent`, was acknowledged at `now`.
    pub fn on_ack(&mut self, now: Instant, sent: Instant, bytes: usize) {
        let delay_ms = now.saturating_duration_since(sent).as_secs_f64() * 1000.0;
        self.base.push(now, delay_ms, bytes);
        self.base_samples = self.base_samples.saturating_add(1);
        let (base, base_bytes) = self.base.get().unwrap_or((delay_ms, bytes));
        let excess_ms = delay_ms - base;
        let extra_bits = bytes.saturating_sub(base_bytes) as f64 * 8.0;
        if bytes >= base_bytes + PROBE_BYTES
            && self.base_samples >= BASE_SAMPLES
            && excess_ms >= 1.0
        {
            self.capacity
                .push(now, extra_bits / (excess_ms / 1000.0), bytes);
        }
        let wire_ms = self
            .capacity
            .get()
            .map_or(0.0, |(bps, _)| extra_bits / bps * 1000.0);
        let q = (excess_ms - wire_ms).max(0.0);
        self.queue_ms = self.queue_ms(now, false) * 0.7 + q * 0.3;
        self.queue_at = now;
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

    /// What the path can carry (bit/s), once frames big enough to tell have been acked.
    pub fn capacity_bps(&self) -> Option<f64> {
        self.capacity.get().map(|(bps, _)| bps)
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
    }

    impl Link {
        fn new(rate: f64, rtt_ms: u64, now: Instant) -> Self {
            Link {
                rate,
                rtt: Duration::from_millis(rtt_ms),
                free: now,
            }
        }
        /// When a frame of `bytes` sent at `sent` is acknowledged, and how long it queued.
        fn send(&mut self, sent: Instant, bytes: usize) -> (Instant, Duration) {
            let start = self.free.max(sent);
            let wire = Duration::from_secs_f64(bytes as f64 * 8.0 / self.rate);
            self.free = start + wire;
            (self.free + self.rtt, start - sent)
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
            est.on_ack(acked, sent, bytes);
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

    #[test]
    fn capacity_comes_from_the_frames_themselves() {
        // Any link, any round trip, a mostly still screen with the odd burst of big
        // frames that never fills the link: the estimate lands on the link's rate.
        let mut rng = Rng(0x9E3779B97F4A7C15);
        for _ in 0..200 {
            let rate = rng.range(2_000_000, 80_000_000) as f64;
            let rtt = rng.range(5, 200);
            let t0 = Instant::now();
            let mut link = Link::new(rate, rtt, t0);
            let mut est = PathEst::new(t0);
            let mut frames = trickle(t0, 30, 16, 600);
            let mut t = t0 + Duration::from_millis(600);
            for _ in 0..rng.range(2, 6) {
                // Still for a while, then a few big frames, far enough apart that none
                // waits behind another.
                t += Duration::from_millis(rng.range(300, 3000));
                for _ in 0..rng.range(1, 5) {
                    let bytes = rng.range(20_000, 120_000) as usize;
                    frames.push((t, bytes));
                    t += Duration::from_secs_f64(bytes as f64 * 8.0 / rate)
                        + Duration::from_millis(rng.range(20, 60));
                }
            }
            run(&mut est, &mut link, &frames);
            let got = est.capacity_bps().expect("big frames were acked");
            assert!(
                (got - rate).abs() <= rate * 0.001,
                "rate {rate}, rtt {rtt} ms: estimated {got}"
            );
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
                est.on_ack(acked, t, bytes);
            }
            if let Some(got) = est.capacity_bps() {
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
                est.on_ack(acked, sent, bytes);
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
        let got = est.capacity_bps().unwrap();
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
}
