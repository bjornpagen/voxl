//! Delivery, as it is told: what a session's lane was sent and has not been told taken, and
//! whether a capture may start, given the path the lane crosses.
//!
//! The daemon's socket says nothing of what arrived: the transport's flow-control window
//! accepts megabytes past a hole in the stream. So the page says when it took each update,
//! the relay says so on the control stream (`wire::Up::Took`), and what is on the way is
//! exactly what was sent and no took has named yet ([`Outstanding`]).
//!
//! A capture may start ([`Gate`], [`Outstanding::gate`]) when what is on the way is within
//! what the path takes in its round trip and [`RESPONSE`], and no frame sent is later than
//! any frame that window lets go can be. The path's round trip and rate are not found
//! here: whoever knows them says them ([`Path`]). Today that is `estimate`, which guesses
//! them from the tooks.
//!
//! Every frame is coded whole, at one quality (`vp9`): the gate alone bounds what is in
//! flight, by holding the next capture, never by shrinking a frame.
//!
//! Everything here is a function of state and one event, with the time an argument:
//! nothing reads a clock. A lane's events come in time order: each is given a time no
//! earlier than the one before it. The session's lock orders them and the clock is read
//! under it (`hub::Client`), so a later time less an earlier one is taken as it is.

use super::estimate::Note;
use crate::wire::Version;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How long a change may take to appear, beyond the path's own round trip, before it reads
/// as lag: how much of the path's time may be on the way beyond its round trip, and how far
/// past its round trip a frame may be before the lane is late.
///
/// @limitKind published
/// @owner the response-time literature (Miller 1968; Card, Robertson and Mackinlay 1991), as
/// Nielsen states it
/// @unit milliseconds
/// @scope every link's window, and every lane's lateness
/// @constraint advisory threshold: the limit of a response that feels instantaneous
/// @source https://www.nngroup.com/articles/response-times-3-important-limits/ "0.1 second
/// is about the limit for having the user feel that the system is reacting instantaneously"
/// @verified 2026-11-18
pub const RESPONSE: Duration = Duration::from_millis(100);

/// Bytes a second.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct BytesPerSecond(pub u64);

const NANOS_PER_SECOND: u128 = 1_000_000_000;

impl BytesPerSecond {
    /// The rate of `bytes` in `time`, rounded down; at most `u64::MAX`. A time the clock
    /// shows as none is taken as its least step, a nanosecond.
    pub fn of(bytes: u64, time: Duration) -> BytesPerSecond {
        let nanos = time.max(Duration::from_nanos(1)).as_nanos();
        let rate = u128::from(bytes) * NANOS_PER_SECOND / nanos;
        BytesPerSecond(rate.min(u128::from(u64::MAX)) as u64)
    }

    /// The bytes it carries in `time`, rounded down; at most `u64::MAX`.
    pub fn carries(self, time: Duration) -> u64 {
        let bytes = u128::from(self.0) * time.as_nanos() / NANOS_PER_SECOND;
        bytes.min(u128::from(u64::MAX)) as u64
    }

    /// The time it takes for `bytes`, rounded down: without end at no rate at all, and at
    /// most `Duration::MAX`.
    pub fn takes(self, bytes: u64) -> Duration {
        match (u128::from(bytes) * NANOS_PER_SECOND).checked_div(u128::from(self.0)) {
            Some(nanos) => Duration::from_nanos(nanos.min(u128::from(u64::MAX)) as u64),
            None => Duration::MAX,
        }
    }
}

/// What is known of the path a lane crosses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Path {
    /// Nothing, yet.
    Unknown,
    Known(Known),
}

/// A path's least round trip and the rate it delivers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Known {
    #[serde(rename = "round_trip_us", serialize_with = "micros")]
    pub round_trip: Duration,
    #[serde(rename = "rate_bytes_per_second")]
    pub rate: BytesPerSecond,
}

/// A time as `voxl stats` says it: in microseconds.
fn micros<S: serde::Serializer>(time: &Duration, to: S) -> Result<S::Ok, S::Error> {
    to.serialize_u128(time.as_micros())
}

impl Known {
    /// The most that may be on the way when a capture starts: what the path takes in its round
    /// trip and [`RESPONSE`].
    ///
    /// On a path in steady use, its round trip's worth is in flight, and [`RESPONSE`] of it
    /// waits. The window turns over once a round trip, which is the rate and [`RESPONSE`] over
    /// the round trip more: so a rate thought too low is outrun, and the frames that outrun it
    /// show the path's.
    pub fn window(self) -> u64 {
        self.rate.carries(self.round_trip + RESPONSE)
    }

    /// How long after its send a frame of `bytes` is due at the latest, on the path as it is
    /// known: a frame not taken by then is late.
    ///
    /// The window before it may all wait for the path, when it was let go at once onto a path
    /// at rest: that is taken in the round trip and [`RESPONSE`]. Then the frame itself, in its
    /// own bytes' time at the path's rate; then the way back, within the round trip. In steady
    /// use a frame comes a round trip sooner (what is in flight does not wait), but a frame is
    /// not late for being the last of a burst the window itself let go: held to the sooner
    /// time, every burst from rest froze the stream until all of it was taken, and the next
    /// began the same (measured in this module's tests: 28 frames a second of the 50 the path
    /// carried, each up to 680 ms on the way).
    ///
    /// Without end on a path of no rate at all: the sum is held to the longest time there is.
    pub fn due(self, bytes: u64) -> Duration {
        (self.round_trip * 2 + RESPONSE).saturating_add(self.rate.takes(bytes))
    }
}

/// Whether a capture may start.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Gate {
    /// What is on the way is within the window, and none of it is late.
    Open,
    /// More is on the way than the gate lets be ([`Outstanding::gate`]).
    Full,
    /// A frame has not been taken by when any frame the window lets go is due: the path
    /// has stalled, or holds more than was thought.
    Late,
}

/// A frame sent on a lane and not yet told taken.
#[derive(Clone, Copy, Debug)]
pub struct Sent {
    pub version: Version,
    pub at: Instant,
    pub bytes: u64,
    /// What the estimator noted of it as it went, to read when it is told taken: nothing
    /// here reads it.
    pub note: Note,
}

/// What a took took: the frame it named, and the bytes of that frame and of every frame
/// sent before it, which the lane carried first.
#[derive(Clone, Copy, Debug)]
pub struct Taken {
    pub named: Sent,
    pub bytes: u64,
}

/// What a lane was sent and has not been told taken, oldest first. It is the lane's: a new
/// lane starts with nothing on the way, whatever the lane before it was sent.
#[derive(Clone, Debug, Default)]
pub struct Outstanding {
    frames: VecDeque<Sent>,
}

/// What `voxl stats` says of a lane's link.
#[derive(Debug, PartialEq, Eq, serde::Serialize)]
pub struct Stats {
    pub outstanding_bytes: u64,
    pub outstanding_frames: usize,
    pub path: Path,
    pub gate: Gate,
}

impl Outstanding {
    /// A frame of `bytes`, named `version`, went onto the lane at `now`.
    pub fn sent(&mut self, version: Version, bytes: u64, note: Note, now: Instant) {
        self.frames.push_back(Sent {
            version,
            at: now,
            bytes,
            note,
        });
    }

    /// The page took the update named `version`, and so every update the lane carried
    /// before it: they are on the way no longer. None if no frame on the way has that
    /// name: a took of a lane since replaced, or of nothing ever sent, takes nothing.
    pub fn took(&mut self, version: Version) -> Option<Taken> {
        let at = self.frames.iter().position(|s| s.version == version)?;
        let named = self.frames[at];
        let bytes = self.frames.drain(..=at).map(|s| s.bytes).sum();
        Some(Taken { named, bytes })
    }

    /// The bytes on the way.
    pub fn bytes(&self) -> u64 {
        self.frames.iter().map(|s| s.bytes).sum()
    }

    /// Whether nothing is on the way.
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Whether a capture may start at `now`, on `path`. With nothing on the way, it may.
    /// While nothing is known of the path there is nothing to hold what is on the way
    /// against, so a capture waits until nothing is. Once it is known: no frame is late,
    /// and what is on the way is within the window.
    ///
    /// The window counts what is on the way, not the frame to come, whose size nothing
    /// knows before it is coded: so a frame larger than the whole window goes when nothing
    /// is on the way.
    pub fn gate(&self, path: Path, now: Instant) -> Gate {
        self.gate_of(self.bytes(), path, now)
    }

    /// The gate, with `bytes` the bytes on the way.
    fn gate_of(&self, bytes: u64, path: Path, now: Instant) -> Gate {
        match (self.frames.front(), path) {
            (None, Path::Unknown | Path::Known(_)) => Gate::Open,
            (Some(_), Path::Unknown) => Gate::Full,
            (Some(oldest), Path::Known(known)) => {
                let late = now - oldest.at > known.due(oldest.bytes);
                match (late, bytes <= known.window()) {
                    (true, true | false) => Gate::Late,
                    (false, false) => Gate::Full,
                    (false, true) => Gate::Open,
                }
            }
        }
    }

    /// What `voxl stats` says of it at `now`, on `path`: the bytes on the way, counted once.
    pub fn stats(&self, path: Path, now: Instant) -> Stats {
        let bytes = self.bytes();
        Stats {
            outstanding_bytes: bytes,
            outstanding_frames: self.frames.len(),
            path,
            gate: self.gate_of(bytes, path, now),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::wire::Version;
    use std::collections::VecDeque;
    use std::time::{Duration, Instant};

    /// A path a test sends frames through: it takes bytes at `rate`, one frame after
    /// another, and each is told taken `delay` after its last byte is through (the round
    /// trip, both ways), later by the frame's own jitter, and never before the frame before.
    /// With `holes`, it goes silent both ways for a length of time every period: nothing
    /// moves through it then, and nothing is told. It is the tests' oracle of a path, apart
    /// from whatever the link knows of it. Every time it is given is no earlier than its
    /// `epoch`, and each no earlier than the one before.
    pub(crate) struct Simulated {
        /// Bytes a second.
        pub(crate) rate: u64,
        pub(crate) delay: Duration,
        /// How often it goes silent, and for how long, counted from `epoch`.
        pub(crate) holes: Option<(Duration, Duration)>,
        epoch: Instant,
        /// When the path is through with what it was given.
        pub(crate) free: Instant,
        /// Since when it has been busy without a pause.
        busy_from: Instant,
        /// The tooks to come, in order: when, and of which frame.
        pub(crate) tooks: VecDeque<(Instant, Version)>,
    }

    impl Simulated {
        pub(crate) fn new(rate: u64, delay: Duration, now: Instant) -> Simulated {
            Simulated {
                rate,
                delay,
                holes: None,
                epoch: now,
                free: now,
                busy_from: now,
                tooks: VecDeque::new(),
            }
        }

        /// The time `bytes` take of it.
        pub(crate) fn time(&self, bytes: u64) -> Duration {
            let nanos = u128::from(bytes) * 1_000_000_000 / u128::from(self.rate);
            Duration::from_nanos(nanos as u64)
        }

        /// The silence `at` falls in, if any: when it began and when it ends.
        fn hole(&self, at: Instant) -> Option<(Instant, Instant)> {
            let (period, length) = self.holes?;
            let since = at - self.epoch;
            let n = (since.as_nanos() / period.as_nanos()) as u32;
            let began = self.epoch + period * n;
            (n > 0 && at < began + length).then_some((began, began + length))
        }

        /// The silence that begins next after `at`.
        fn next_hole(&self, at: Instant) -> Option<Instant> {
            let (period, _) = self.holes?;
            let since = at - self.epoch;
            let n = (since.as_nanos() / period.as_nanos()) as u32;
            Some(self.epoch + period * (n + 1))
        }

        /// When `work` of the path's time is done, begun at `from`: silences add to it.
        fn done(&self, from: Instant, work: Duration) -> Instant {
            let (mut at, mut left) = (from, work);
            loop {
                if let Some((_, ends)) = self.hole(at) {
                    at = ends;
                }
                match self.next_hole(at) {
                    Some(next) if at + left > next => {
                        left -= next - at;
                        at = next;
                    }
                    Some(_) | None => return at + left,
                }
            }
        }

        /// How much of the time from `from` to `to` the path was silent.
        pub(crate) fn silent(&self, from: Instant, to: Instant) -> Duration {
            let Some((period, length)) = self.holes else {
                return Duration::ZERO;
            };
            let first = ((from - self.epoch).as_nanos() / period.as_nanos()) as u32;
            // The silences that overlap `from..to`: each counts for the part within it.
            (first.max(1)..)
                .map(|n| self.epoch + period * n)
                .take_while(|began| *began < to)
                .filter(|began| *began + length > from)
                .map(|began| (began + length).min(to) - began.max(from))
                .sum()
        }

        /// Whether the path has been busy without a pause for its delay and more, and still
        /// is: a frame sent now goes into a steady stream.
        pub(crate) fn steady(&self, now: Instant) -> bool {
            now - self.busy_from >= self.delay && self.free > now
        }

        /// A frame is given to it at `now`: its took is to come, at the time returned.
        pub(crate) fn send(
            &mut self,
            version: Version,
            bytes: u64,
            jitter: Duration,
            now: Instant,
        ) -> Instant {
            if self.free <= now {
                (self.free, self.busy_from) = (now, now);
            }
            self.free = self.done(self.free, self.time(bytes));
            let told = self.free + self.delay + jitter;
            let told = self.hole(told).map_or(told, |(_, ends)| ends);
            let told = self.tooks.back().map_or(told, |(last, _)| told.max(*last));
            self.tooks.push_back((told, version));
            told
        }

        /// The tooks due by `now` are told to `took`, each with its time, in order.
        pub(crate) fn tell(&mut self, now: Instant, mut took: impl FnMut(Version, Instant)) {
            while let Some((at, version)) = self.tooks.front().copied().filter(|(at, _)| *at <= now)
            {
                self.tooks.pop_front();
                took(version, at);
            }
        }
    }
}
