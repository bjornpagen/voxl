//! Delivery, as it is told: what a session's lane was sent and has not been told taken, and
//! whether a capture may start, given the path the lane crosses.
//!
//! The daemon's socket says nothing of what arrived: the transport's flow-control window
//! accepts megabytes past a hole in the stream. So the page says when it took each of the
//! lane's frames (a video update or an Opus packet: both cross the same path and press the
//! same window), the relay says so on the control stream (`wire::Up::Took`), and what is on
//! the way is exactly what was sent and no took has named yet ([`Outstanding`]).
//!
//! A lane's gate ([`Gate`], [`Outstanding::gate`]) is open, and the lane takes the next
//! capture, when what is on the way is within what the path takes in its round trip and
//! [`RESPONSE`], and no frame sent is later than any frame that window lets go can be. The
//! path's round trip and rate are not found here, and nothing here guesses them: the
//! transport the lane crosses measures both, a packet at a time, to pace itself, and states
//! them to the lane ([`Path`], `statement`). A lane whose path nothing states is sent one
//! frame at a time: with nothing to hold what is on the way against, the next goes when
//! nothing is. Each lane's gate is its own: a capture goes while any watching lane's is
//! open, and is sent only to those (`hub::planned`); a lane whose gate is shut is passed by,
//! and holds no other back.
//!
//! Every frame is coded whole, at one quality (`av1`): the gate alone bounds what is in
//! flight on its lane, by passing the lane by while it is shut, never by shrinking a frame.
//!
//! Everything here is a function of state and one event, with the time an argument:
//! nothing reads a clock. A lane's events come in time order: each is given a time no
//! earlier than the one before it. The session's lock orders them and the clock is read
//! under it (`hub::Client`), so a later time less an earlier one is taken as it is.

use crate::wire::{Sequence, Track};
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

/// What is known of the path a lane crosses: what the lane's transport states of it, and
/// nothing else. It is the lane's own: the next lane's is whatever comes with that lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Path {
    /// No statement came with the lane, and none can come later: its transport states no
    /// path (one paced to a rate its owner declared measures none), or what handed the
    /// lane over passes no descriptor. Nothing will ever be known of it.
    Unstated,
    /// A statement came with the lane, and says nothing now: its transport has yet to
    /// measure the path, or the statement has ended.
    Unknown,
    /// The path as the statement last said it.
    Known(Known),
}

/// A path's least round trip and the rate it delivers, as its transport states them.
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

/// Whether a lane takes a capture now: a capture planned while it is open is sent it, and
/// goes for it; one planned while it is shut goes on without it (`hub::planned`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
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

/// A frame (a video update or an Opus packet) sent on a lane and not yet told taken.
#[derive(Clone, Copy, Debug)]
pub struct Sent {
    pub sequence: Sequence,
    /// What it is: an update's time to its took and a packet's are told apart.
    pub track: Track,
    pub at: Instant,
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
    /// A frame of `track` and `bytes`, named `sequence`, went onto the lane at `now`.
    pub fn sent(&mut self, sequence: Sequence, track: Track, bytes: u64, now: Instant) {
        self.frames.push_back(Sent {
            sequence,
            track,
            at: now,
            bytes,
        });
    }

    /// The page took the frame named `sequence`, and so every frame the lane carried before
    /// it: they are on the way no longer. If no frame on the way has that name, nothing
    /// is taken: a took of a lane since replaced, or of nothing ever sent. The took came at
    /// `now`: `taken` is told each frame it took, its track and how long it was on the way,
    /// from its going onto the lane to this took (`voxl stats`' `took`). This is where a took
    /// meets its send, so it is the one place that time is known.
    pub fn took(
        &mut self,
        sequence: Sequence,
        now: Instant,
        mut taken: impl FnMut(Track, Duration),
    ) {
        if let Some(at) = self.frames.iter().position(|s| s.sequence == sequence) {
            for frame in self.frames.drain(..=at) {
                taken(frame.track, now - frame.at);
            }
        }
    }

    /// The bytes on the way.
    pub fn bytes(&self) -> u64 {
        self.frames.iter().map(|s| s.bytes).sum()
    }

    /// Whether a capture may start at `now`, on `path`. With nothing on the way, it may.
    /// While nothing is known of the path (it is not stated, or not yet, or no longer)
    /// there is nothing to hold what is on the way against, so a capture waits until
    /// nothing is: one frame at a time. Once it is known: no frame is late, and what is on
    /// the way is within the window.
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
            (None, Path::Unstated | Path::Unknown | Path::Known(_)) => Gate::Open,
            (Some(_), Path::Unstated | Path::Unknown) => Gate::Full,
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
    use super::{BytesPerSecond, Gate, Known, Outstanding, Path, Stats, RESPONSE};
    use crate::wire::{Sequence, Track};
    use crate::xorshift::Xorshift;
    use std::collections::VecDeque;
    use std::time::{Duration, Instant};

    /// A lane's link as its sender holds it: what is on the way, and the path as it was
    /// last stated.
    pub(crate) struct Link {
        on: Outstanding,
        pub(crate) path: Path,
    }

    impl Link {
        pub(crate) fn new(path: Path) -> Link {
            Link {
                on: Outstanding::default(),
                path,
            }
        }

        /// An update goes onto the lane.
        pub(crate) fn sent(&mut self, sequence: Sequence, bytes: u64, now: Instant) {
            self.on.sent(sequence, Track::Video, bytes, now);
        }

        pub(crate) fn took(&mut self, sequence: Sequence, now: Instant) {
            self.on.took(sequence, now, |_, _| {});
        }

        pub(crate) fn gate(&self, now: Instant) -> Gate {
            self.on.gate(self.path, now)
        }

        pub(crate) fn stats(&self, now: Instant) -> Stats {
            self.on.stats(self.path, now)
        }
    }

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
        pub(crate) tooks: VecDeque<(Instant, Sequence)>,
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

        /// The path as a transport that has measured it states it: its round trip and its
        /// rate, as they are now.
        pub(crate) fn known(&self) -> Known {
            Known {
                round_trip: self.delay,
                rate: BytesPerSecond(self.rate),
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
            sequence: Sequence,
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
            self.tooks.push_back((told, sequence));
            told
        }

        /// The tooks due by `now` are told to `took`, each with its time, in order.
        pub(crate) fn tell(&mut self, now: Instant, mut took: impl FnMut(Sequence, Instant)) {
            while let Some((at, sequence)) =
                self.tooks.front().copied().filter(|(at, _)| *at <= now)
            {
                self.tooks.pop_front();
                took(sequence, at);
            }
        }
    }

    /// A frame a test sent: when, when it was told taken, its bytes, and whether it went
    /// into a steady stream.
    struct Frame {
        sent: Instant,
        took: Instant,
        bytes: u64,
        steady: bool,
    }

    /// How long a frame takes to code, in these tests: less than any frame's time on any
    /// path here, so a viewer that wants every frame always has the next ready.
    const CODING: Duration = Duration::from_millis(5);

    /// A viewer that wants every frame: a frame of `size()` goes whenever the gate lets
    /// one, a frame's coding time apart at the least, from `from` for `long`. The gate is
    /// never late, which the caller's path gives it no cause to be.
    fn stream(
        link: &mut Link,
        path: &mut Simulated,
        (from, long): (Instant, Duration),
        mut size: impl FnMut() -> u64,
    ) -> Vec<Frame> {
        let mut frames: Vec<Frame> = Vec::new();
        let (mut now, mut next) = (from, 0);
        while now < from + long {
            path.tell(now, |sequence, at| link.took(sequence, at));
            match link.gate(now) {
                Gate::Open => {}
                Gate::Full => {
                    // Held: nothing opens the gate but a took.
                    let (told, _) = *path.tooks.front().expect("a held gate has a took to come");
                    now = now.max(told);
                    continue;
                }
                Gate::Late => panic!("a frame late on a path that is as it was stated"),
            }
            let (sequence, bytes) = (Sequence(next), size());
            next += 1;
            let steady = path.steady(now);
            link.sent(sequence, bytes, now);
            let took = path.send(sequence, bytes, Duration::ZERO, now);
            frames.push(Frame {
                sent: now,
                took,
                bytes,
                steady,
            });
            now += CODING;
        }
        frames
    }

    /// A path of any rate from 20 KB/s to 40 MB/s and any round trip from 1 to 400 ms.
    fn any_path(rng: &mut Xorshift, now: Instant) -> Simulated {
        let rate = 20_000u64 << rng.below(11);
        let rate = rate + rng.below(rate);
        Simulated::new(rate, Duration::from_micros(rng.range(1_000, 400_000)), now)
    }

    /// On a path that is as its transport stated it, what is in flight is bounded as the
    /// head of this module says, and the path is kept busy. A viewer wants every frame of
    /// a desktop that never stops moving, over generated paths (20 KB/s to 40 MB/s, round
    /// trips of 1 to 400 ms), each stated from the first as it is, and frames of generated
    /// sizes after a keyframe a fifth of a second of the path. On a virtual clock, with the
    /// path's own word as the oracle:
    /// - no frame is ever late: every one is told taken within what it is due in
    ///   (`Known::due`: twice the round trip, the response time and its own bytes' time);
    /// - every frame that goes into a steady stream is told taken a round trip sooner:
    ///   within the round trip, the response time and its own bytes' time;
    /// - the stream runs without a stop: in every second after the first, the path is told
    ///   to have taken its rate, to two frames.
    #[test]
    fn on_a_path_as_stated_no_frame_is_late_and_the_path_is_kept_busy() {
        let mut rng = Xorshift::new(0x7137_449f_23ef_65cd);
        let start = Instant::now();
        let (mut steady, mut from_rest) = (0, 0);
        for case in 0..60 {
            let mut path = any_path(&mut rng, start);
            let (known, rate, delay) = (path.known(), path.rate, path.delay);
            let mut link = Link::new(Path::Known(known));
            // Frames up to a fortieth of a second of the path: a stream it carries dozens a
            // second of, and that a frame every coding time overfills.
            let most = (rate / 40).max(2_000);
            let mut sizes = rng.clone();
            let long = Duration::from_secs(20);
            let mut first = Some(rate / 5);
            let frames = stream(&mut link, &mut path, (start, long), || {
                first.take().unwrap_or_else(|| 500 + sizes.below(most))
            });
            // The path's times are whole nanoseconds, each rounded down.
            let slack = Duration::from_nanos(2);
            for f in &frames {
                let (lag, own) = (f.took - f.sent, path.time(f.bytes));
                assert!(
                    lag <= known.due(f.bytes) + slack,
                    "case {case}: {lag:?} at {rate} B/s, {delay:?}"
                );
                from_rest += usize::from(lag > delay + RESPONSE + own + slack);
                if f.steady {
                    assert!(
                        lag <= delay + RESPONSE + own + slack,
                        "case {case}: a steady frame, {lag:?} at {rate} B/s, {delay:?}"
                    );
                    steady += 1;
                }
            }
            for second in 1..19 {
                let from = start + Duration::from_secs(second);
                let taken: u64 = frames
                    .iter()
                    .filter(|f| f.took > from && f.took <= from + Duration::from_secs(1))
                    .map(|f| f.bytes)
                    .sum();
                assert!(
                    taken + 2 * (most + 500) >= rate,
                    "case {case}: {taken} B taken in second {second} at {rate} B/s"
                );
            }
        }
        assert!(
            steady > 5_000 && from_rest > 20,
            "{steady} steady frames, {from_rest} past the steady bound"
        );
    }

    /// A lane whose path nothing states is sent one frame at a time, and none waits longer
    /// than that takes. Over generated paths and frames, on a lane whose path is not stated
    /// and on one whose statement has said nothing: every frame goes with nothing on the
    /// way, exactly when the frame before was told taken or, if that was sooner, when it
    /// was coded.
    #[test]
    fn a_lane_nothing_states_the_path_of_is_sent_one_frame_at_a_time() {
        let mut rng = Xorshift::new(0x2f0b_4a3c_9e1d_7755);
        let start = Instant::now();
        let (mut waited, mut ready) = (0, 0);
        for case in 0..40 {
            for unknown in [Path::Unstated, Path::Unknown] {
                let mut path = any_path(&mut rng, start);
                let mut link = Link::new(unknown);
                let most = (path.rate / 40).max(2_000);
                let mut sizes = rng.clone();
                let long = Duration::from_secs(5);
                let frames = stream(&mut link, &mut path, (start, long), || {
                    500 + sizes.below(most)
                });
                for pair in frames.windows(2) {
                    let (before, f) = (&pair[0], &pair[1]);
                    let coded = before.sent + CODING;
                    assert_eq!(f.sent, before.took.max(coded), "case {case}, {unknown:?}");
                    waited += usize::from(before.took > coded);
                    ready += usize::from(before.took <= coded);
                }
                assert!(frames.len() > 1, "case {case}: {} frames", frames.len());
            }
        }
        assert!(waited > 1_000 && ready > 40, "{waited} {ready}");
    }
}
