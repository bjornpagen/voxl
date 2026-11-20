//! How long the pipeline's steps took, as distributions: every time is counted in the bucket
//! it falls in, so `voxl stats` says a step's median and its tail, which a sum hides (one
//! frame that waited an hour and 3,600 that waited a second have the same sum).
//!
//! A bucket is a time's leading bits: every time under [`STEPS`] microseconds has a bucket
//! of its own, and above that the times of one bucket share their [`BITS`] + 1 most
//! significant bits. So a bucket is never wider than a [`STEPS`]th of the times in it, and
//! every `u64` of microseconds has one: nothing is clamped, and no time is out of range.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// The bits of a time, below its leading one, that pick its bucket: a bucket is at most
/// 2^-BITS of its times wide.
///
/// @limitKind derived
/// @owner the daemon's `voxl stats`
/// @unit bits
/// @scope every histogram
/// @constraint a quantile read off a bucket is its lower bound, under the true one by less
/// than the bucket's width: the width must be under the least difference a decision here
/// was made on
/// @source src/av1.rs's `COST_UPDATES` kept a change measured as 43.3 to 20.5 ms and
/// `SPEED`'s deltas are told apart as 15.1 to 18.7 ms; the least difference this tree's
/// tuning reports as a result is typing's 8.6 against 9.4 ms a frame, 9.3%. 3 bits is
/// 12.5%, which hides it; 4 is 6.25%, which shows it
/// @verified 2026-11-19
const BITS: u32 = 4;

/// The buckets an octave has, and the times under it that each have their own.
const STEPS: u64 = 1 << BITS;

/// Every bucket: [`STEPS`] for the times under [`STEPS`], and [`STEPS`] for each of the
/// 64 - [`BITS`] octaves from there to `u64::MAX`.
const BUCKETS: usize = ((u64::BITS - BITS + 1) << BITS) as usize;

/// The bucket of a time of `micros`.
fn bucket(micros: u64) -> usize {
    // How far its leading bits are shifted down: none under two octaves of STEPS.
    let shift = (micros | 1).ilog2().saturating_sub(BITS);
    ((u64::from(shift) << BITS) + (micros >> shift)) as usize
}

/// The least time of bucket `index`, in microseconds.
fn least(index: usize) -> u64 {
    let (octave, step) = ((index as u64) >> BITS, (index as u64) & (STEPS - 1));
    match octave.checked_sub(1) {
        None => step,
        Some(shift) => (STEPS + step) << shift,
    }
}

/// The times a step took, counted by bucket, and their sum. Counted from any thread; read
/// as one pass over the buckets, which another thread may count into meanwhile: a reading
/// is every time counted before it began, and some of those counted while it ran.
pub struct Histogram {
    buckets: [AtomicU64; BUCKETS],
    /// Every time counted, summed, in microseconds: wraps after 584,000 years of them.
    sum: AtomicU64,
}

impl Default for Histogram {
    fn default() -> Histogram {
        Histogram {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            sum: AtomicU64::new(0),
        }
    }
}

/// A part of a whole: `of` in every `in_all`.
#[derive(Clone, Copy, Debug)]
struct Share {
    of: u64,
    in_all: u64,
}

/// The median.
const HALF: Share = Share { of: 1, in_all: 2 };
/// The tail `voxl stats` says: the time 99 in 100 are within.
const TAIL: Share = Share {
    of: 99,
    in_all: 100,
};

/// A histogram as one pass read it: each bucket's count.
struct Reading(Vec<u64>);

impl Reading {
    fn count(&self) -> u64 {
        self.0.iter().sum()
    }

    /// The least time of the bucket that holds the time `share` of them are within: of `n`
    /// times in order, the one at place `ceil(n * share)`, counted from 1. None of no times.
    fn within(&self, share: Share) -> Option<u64> {
        let n = u128::from(self.count());
        let place = (n * u128::from(share.of)).div_ceil(u128::from(share.in_all));
        let mut before = 0u128;
        self.0.iter().enumerate().find_map(|(index, count)| {
            before += u128::from(*count);
            (*count > 0 && before >= place).then(|| least(index))
        })
    }
}

impl Histogram {
    /// A step took `time`: counted. A time past `u64::MAX` microseconds is counted as that.
    pub fn took(&self, time: Duration) {
        let micros = u64::try_from(time.as_micros()).unwrap_or(u64::MAX);
        self.buckets[bucket(micros)].fetch_add(1, Ordering::Relaxed);
        self.sum.fetch_add(micros, Ordering::Relaxed);
    }

    fn read(&self) -> Reading {
        Reading(
            self.buckets
                .iter()
                .map(|b| b.load(Ordering::Relaxed))
                .collect(),
        )
    }

    /// What `voxl stats` says of it: how many times, their sum, the median and the time 99
    /// in 100 are within (each the least time of its bucket, so under the true one by less
    /// than a sixteenth; null of no times), and every bucket that holds any, as its least
    /// time and its count.
    pub fn stats(&self) -> serde_json::Value {
        let reading = self.read();
        let held: Vec<_> = reading
            .0
            .iter()
            .enumerate()
            .filter(|(_, count)| **count > 0)
            .map(|(index, count)| serde_json::json!([least(index), count]))
            .collect();
        serde_json::json!({
            "count": reading.count(),
            "sum_us": self.sum.load(Ordering::Relaxed),
            "p50_us": reading.within(HALF),
            "p99_us": reading.within(TAIL),
            "buckets_us": held,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xorshift::Xorshift;

    /// A time of any size: a generated number of bits, then generated bits.
    fn any_time(rng: &mut Xorshift) -> u64 {
        rng.next() >> rng.below(64)
    }

    /// `micros` with only its five leading bits kept, written apart from `bucket`: halved
    /// until it is under two octaves of steps, and doubled back.
    fn leading(micros: u64) -> u64 {
        let (mut v, mut halved) = (micros, 0);
        while v >= 2 * STEPS {
            v /= 2;
            halved += 1;
        }
        v << halved
    }

    /// A time's bucket is its five leading bits, and nothing else of it. Over every time
    /// under 4,096 microseconds, the times either side of every power of two, and generated
    /// times of every size: the bucket's least time is the time's leading bits, so the time
    /// is in its bucket and over the least by less than a sixteenth of itself; a greater
    /// time's bucket is no earlier; the next bucket begins past the time; and every bucket
    /// there is is some time's, the last `u64::MAX`'s.
    #[test]
    fn a_bucket_is_a_times_leading_bits() {
        let mut rng = Xorshift::new(0x7f4a_7c15_9e37_79b9);
        let edges = (0..64).flat_map(|b| {
            let p = 1u64 << b;
            [p - 1, p, p + 1]
        });
        let times: Vec<u64> = (0..4096)
            .chain(edges)
            .chain((0..200_000).map(|_| any_time(&mut rng)))
            .chain([u64::MAX])
            .collect();
        let mut reached = vec![false; BUCKETS];
        for &t in &times {
            let b = bucket(t);
            reached[b] = true;
            assert_eq!(least(b), leading(t), "{t}");
            assert!(
                t - least(b) <= t >> BITS,
                "{t} in a bucket from {}",
                least(b)
            );
            if b + 1 < BUCKETS {
                assert!(least(b + 1) > t, "{t}");
            }
            let more = t.saturating_add(any_time(&mut rng));
            assert!(bucket(more) >= b, "{t} then {more}");
        }
        assert_eq!(bucket(u64::MAX), BUCKETS - 1);
        // Every bucket is reached by its own least time, which the times above need not hold.
        for index in 0..BUCKETS {
            assert_eq!(bucket(least(index)), index);
        }
        assert!(reached.iter().filter(|r| **r).count() > BUCKETS * 9 / 10);
    }

    /// What `voxl stats` says of a histogram is what was counted: over generated runs of
    /// times of every size (none, one, and up to 400, some runs all in a narrow band), kept
    /// apart in order, the count is theirs, the sum theirs, the median and the 99th
    /// percentile are the leading bits of the times at places `ceil(n/2)` and
    /// `ceil(99n/100)` in order, null of none, and the buckets are exactly the times'
    /// leading bits, each with how many times have them, in order.
    #[test]
    fn a_reading_is_what_was_counted() {
        let mut rng = Xorshift::new(0x1b87_3593_cc9e_2d51);
        let (mut empty, mut narrow) = (0, 0);
        for run in 0..2_000 {
            let h = Histogram::default();
            let n = match rng.below(8) {
                0 => 0,
                1 => 1,
                2.. => rng.range(2, 401),
            };
            let band = rng.below(3) == 0;
            let mut times: Vec<u64> = (0..n)
                .map(|_| match band {
                    true => rng.range(8_000, 12_000),
                    false => any_time(&mut rng) >> 8,
                })
                .collect();
            for &t in &times {
                h.took(Duration::from_micros(t));
            }
            times.sort_unstable();
            let at = |of: u64, in_all: u64| {
                let place = (n * of).div_ceil(in_all);
                place
                    .checked_sub(1)
                    .map(|i| leading(times[i as usize]))
                    .map_or(serde_json::Value::Null, Into::into)
            };
            let mut buckets: Vec<(u64, u64)> = Vec::new();
            for &t in &times {
                match buckets.last_mut() {
                    Some((from, count)) if *from == leading(t) => *count += 1,
                    Some(_) | None => buckets.push((leading(t), 1)),
                }
            }
            let want = serde_json::json!({
                "count": n,
                "sum_us": times.iter().sum::<u64>(),
                "p50_us": at(1, 2),
                "p99_us": at(99, 100),
                "buckets_us": buckets,
            });
            assert_eq!(h.stats(), want, "run {run}");
            empty += usize::from(n == 0);
            narrow += usize::from(band && n > 1);
        }
        assert!(empty > 100 && narrow > 300, "{empty} {narrow}");
    }
}
