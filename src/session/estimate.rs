//! The path a session's lanes cross, guessed from the tooks: its round trip and its rate.
//!
//! - The round trip: the least time from a frame's send to its took, among frames sent
//!   with nothing else on the way and any frame that came sooner than those.
//! - The rate: the most bytes told taken a second, over a frame's time on the way. Every
//!   frame's figure counts if it is no less than the greatest remembered; a lower one
//!   counts only from a frame that pressed the path ([`Ahead`]).
//!
//! Both are guesses, and they stand only until whatever fronts the lane states its path
//! itself: the transport measures both, a packet at a time, and this module guesses them a
//! frame at a time. Then this module goes, and `link` is given the path as it is stated.
//! What an independent review measured of the guess, on this module's own simulated paths:
//! - it does not follow a path that slows by less than about a third: a lower rate counts
//!   only from a late frame, and a frame is late only past twice the round trip. After a
//!   fall to 90, 85 or 80 % the rate stood 1.16 to 1.39 times over the path's for good;
//! - it sticks low when small frames and whole screens mix: a frame larger than the window
//!   goes alone, a frame alone shows less than the path's rate, and nothing raises it. On a
//!   path of 1,860 KB/s, three frames of 2 KB to one of 300 KB used 60 to 98 % of the path,
//!   by no rule in the round trip;
//! - in steady use it runs 1.02 to 1.4 times over the path's rate (a frame the path had
//!   begun before counts whole, and only figures no less than the greatest are kept), so a
//!   frame is on the way for the round trip, [`RESPONSE`] and its own time times that, not
//!   within them;
//! - one lone frame that a stall held up, after a still screen longer than the round
//!   trip's memory, becomes the round trip (667 ms for a path of 236 ms) until the next
//!   took that comes sooner.
//!
//! The tests below hold the guesses to what they do, on paths that step around these.
//!
//! It outlives a lane: what a lane was sent goes with the lane (`link::Outstanding`), and
//! what its tooks showed of the path stays for the next. As in `link`, time is an argument
//! and a lane's events come in time order.

use super::link::{BytesPerSecond, Gate, Known, Outstanding, Path, Stats, Taken, RESPONSE};
use crate::wire::Sequence;
use std::cmp::Reverse;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How long a round trip seen stays the path's: the window of the least round trip. A path's
/// round trip changes under a session (a route change), so the least ever seen would leave
/// every frame late on a path grown longer. The window is long enough to hold a frame sent
/// alone that no stall delayed, and no longer than a longer path may keep the lane throttled
/// before the estimate follows it: simulated on a path whose round trip grew from 106 to
/// 243 ms under a scroll, the scroll lost no frames a second in the ten seconds until it
/// did (`video`'s `sessions_over_the_measured_paths`).
///
/// @limitKind published
/// @owner BBR's estimator of the same quantity, the least round trip of a path
/// @unit seconds
/// @scope the window of the path's round trip
/// @constraint a default of another estimator, adopted for the same trade (long enough to
/// see the path's own round trip through its queues, short enough to follow a route change);
/// not measured on this path
/// @source https://www.ietf.org/archive/id/draft-ietf-ccwg-bbr-03.txt "BBR.MinRTTFilterLen: A
/// constant specifying the length of the BBR.min_rtt min filter window,
/// BBR.MinRTTFilterLen is 10 secs."
/// @verified 2026-11-18
const PATH_MEMORY: Duration = Duration::from_secs(10);

/// How many round trips a rate seen stays the path's: the window of the greatest rate. A
/// path's rate changes under a session faster than its round trip (a fading link), and a
/// rate remembered too long keeps too much on the way; the window is long enough to hold a
/// frame that crossed the path at its full rate, between its stalls. A rate is forgotten
/// only when another is seen, so a pause forgets nothing: the rate a path showed under load
/// is still its rate after any time in which nothing pressed it.
///
/// @limitKind published
/// @owner BBR's estimator of the same quantity, the greatest delivery rate of a path
/// @unit round trips, as the link knows the path's
/// @scope the window of the path's rate
/// @constraint a default of another estimator, adopted for the same trade (long enough to
/// ride out a short stall, short enough to follow the path); not measured on this path
/// @source https://www.ietf.org/archive/id/draft-cardwell-iccrg-bbr-congestion-control-00.txt
/// "BtlBwFilterLen: A constant specifying the length of the BBR.BtlBw max filter window for
/// BBR.BtlBwFilter, BtlBwFilterLen is 10 packet-timed round trips."
/// @verified 2026-11-18
const RATE_MEMORY_TRIPS: u32 = 10;

/// Whether a frame went onto a path with nothing of the session's on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Company {
    /// Nothing sent was untaken: its time to its took is the path's round trip and its own
    /// bytes' time, with none of the session's queue in it.
    Alone,
    /// Sent behind frames not yet taken.
    Behind,
}

/// How much was on the way before a frame when it was sent, against what the link then
/// knew of the path: with whether the frame then came late, what decides whether it pressed
/// the path, so that the bytes told taken while it was on the way are what the path can do.
///
/// It is what tells a path slower than thought from a path that stalled under a light
/// sender. A frame behind more than the path takes in [`RESPONSE`] that comes late waited
/// longer than its rate explains: the rate is less. A frame behind less (one sent alone,
/// however large; typing) cannot have waited [`RESPONSE`] for the path's rate: if it is late
/// the path stalled, and its figure says only that the path is no slower than that. And a
/// frame that is not late pressed nothing, however much was before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ahead {
    /// No less than the path takes in [`RESPONSE`], at the rate then known: its rate
    /// counts, whatever it says, if the frame is late.
    Response,
    /// Less: its rate counts only if it is no less than the greatest in memory (the rule
    /// BBR feeds its filter by: "if (rs.delivery_rate >= BBR.BtlBw || !
    /// rs.is_app_limited)", draft-cardwell-iccrg-bbr-congestion-control-00, 4.1.1.5).
    Little,
}

/// What is noted of a frame as it goes, to read when it is told taken.
#[derive(Clone, Copy, Debug)]
pub struct Note {
    company: Company,
    ahead: Ahead,
    /// The bytes told taken, in all, when it was sent.
    taken_before: u64,
}

/// A value seen at a time.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Seen<V> {
    at: Instant,
    value: V,
}

/// The least of the values seen within a memory of the newest. Only a value seen moves it:
/// with none coming it stands.
#[derive(Clone, Debug)]
struct Least<V> {
    /// The value seen last.
    newest: Seen<V>,
    /// The values before it, within memory of it, each less than every value after it:
    /// oldest first, so they rise, and the first is the least of all.
    lesser: VecDeque<Seen<V>>,
}

impl<V: Copy + Ord> Least<V> {
    fn of(first: Seen<V>) -> Least<V> {
        Least {
            newest: first,
            lesser: VecDeque::new(),
        }
    }

    /// A value seen: it is the newest, the values it does not undercut and that are within
    /// `memory` of it stay, and the rest are forgotten.
    fn see(&mut self, seen: Seen<V>, memory: Duration) {
        while self.lesser.back().is_some_and(|s| s.value >= seen.value) {
            self.lesser.pop_back();
        }
        if self.newest.value < seen.value {
            self.lesser.push_back(self.newest);
        }
        self.newest = seen;
        let remembered = |s: &Seen<V>| seen.at - s.at <= memory;
        while self.lesser.front().is_some_and(|s| !remembered(s)) {
            self.lesser.pop_front();
        }
    }

    /// The least value in memory.
    fn least(&self) -> V {
        match self.lesser.front() {
            Some(least) => least.value,
            None => self.newest.value,
        }
    }
}

/// What the tooks have shown of the path.
///
/// Its round trip (`fastest`): the least time from a frame's send to its took, within
/// [`PATH_MEMORY`], among frames sent alone and any frame taken sooner than the least in
/// memory. A frame sent alone waited behind nothing of the session's, and a frame that did
/// wait and still came sooner shows the path no longer than that; so the session's own queue
/// never raises it, and with no such frame coming (frames behind one another, under
/// continuous motion) it stands. It carries the time the path took for its own frame's
/// bytes: it is never shorter than the path's round trip, and longer by that time.
///
/// Its rate: the greatest, within [`RATE_MEMORY_TRIPS`] round trips, of the bytes told
/// taken while a frame was on the way over that frame's time on the way. Every such figure
/// is true of the path: the bytes were taken in that time. It is the path's rate when the
/// path was busy throughout, less when it was not, and over it by at most one frame's bytes
/// in that time (a frame the path had begun before counts whole). Which figures count:
/// any that is no less than the greatest in memory; and a lower one only from a frame that
/// pressed the path ([`Ahead`]: late, and behind more than the path was thought to take in
/// [`RESPONSE`]), so a path grown much slower under a heavy sender is followed, while
/// neither typing nor a silence that a light sender met reads as a slow path, and no pause
/// forgets what the path showed under load. What this does not do, the head of this module
/// says.
#[derive(Clone, Debug, Default)]
enum Shown {
    /// No frame sent alone has been taken yet: nothing is known of the path.
    #[default]
    Nothing,
    Path {
        fastest: Least<Duration>,
        /// The greatest rate in memory: the least of them, the other way up.
        rate: Least<Reverse<BytesPerSecond>>,
    },
}

/// The path as its two memories have it now.
fn known(fastest: &Least<Duration>, rate: &Least<Reverse<BytesPerSecond>>) -> Known {
    let Reverse(rate) = rate.least();
    Known {
        round_trip: fastest.least(),
        rate,
    }
}

/// A session's guess of its path: what its lanes' tooks have shown.
#[derive(Clone, Debug, Default)]
pub struct Estimate {
    /// The bytes told taken, in all.
    taken: u64,
    shown: Shown,
}

impl Estimate {
    /// The path, as far as the tooks have shown it.
    pub fn path(&self) -> Path {
        match &self.shown {
            Shown::Nothing => Path::Unknown,
            Shown::Path { fastest, rate } => Path::Known(known(fastest, rate)),
        }
    }

    /// A frame of `bytes`, named `sequence`, goes onto the lane at `now`, behind what is
    /// `on` the way: it is on the way too, with what is noted of it.
    pub fn sent(&self, on: &mut Outstanding, sequence: Sequence, bytes: u64, now: Instant) {
        let company = match on.is_empty() {
            true => Company::Alone,
            false => Company::Behind,
        };
        let ahead = match self.path() {
            // Nothing is known to hold it against: the first frame's took shows the path.
            Path::Unknown => Ahead::Little,
            Path::Known(known) => match on.bytes() >= known.rate.carries(RESPONSE) {
                true => Ahead::Response,
                false => Ahead::Little,
            },
        };
        let note = Note {
            company,
            ahead,
            taken_before: self.taken,
        };
        on.sent(sequence, bytes, note, now);
    }

    /// The relay said at `now` that the page took the update named `sequence`: it and the
    /// frames before it are `on` the way no longer, and its time on the way shows the path.
    pub fn took(&mut self, on: &mut Outstanding, sequence: Sequence, now: Instant) {
        let Some(Taken { named, bytes }) = on.took(sequence) else {
            return;
        };
        self.taken += bytes;
        let Note {
            company,
            ahead,
            taken_before,
        } = named.note;
        // The frame named was on the way this long, and this much was told taken meanwhile:
        // itself, and whatever was on the way before it.
        let trip = now - named.at;
        let rate = Seen {
            at: now,
            value: Reverse(BytesPerSecond::of(self.taken - taken_before, trip)),
        };
        let trip = Seen {
            at: now,
            value: trip,
        };
        match (&mut self.shown, company) {
            (Shown::Nothing, Company::Behind) => {}
            (Shown::Nothing, Company::Alone) => {
                self.shown = Shown::Path {
                    fastest: Least::of(trip),
                    rate: Least::of(rate),
                }
            }
            (
                Shown::Path {
                    fastest,
                    rate: greatest,
                },
                Company::Alone | Company::Behind,
            ) => {
                let late = trip.value > known(fastest, greatest).due(named.bytes);
                match (company, trip.value < fastest.least()) {
                    (Company::Alone, true | false) | (Company::Behind, true) => {
                        fastest.see(trip, PATH_MEMORY)
                    }
                    (Company::Behind, false) => {}
                }
                let memory = fastest.least() * RATE_MEMORY_TRIPS;
                match (ahead, late, rate.value <= greatest.least()) {
                    (Ahead::Response, true, true | false)
                    | (Ahead::Response, false, true)
                    | (Ahead::Little, true | false, true) => greatest.see(rate, memory),
                    (Ahead::Response, false, false) | (Ahead::Little, true | false, false) => {}
                }
            }
        }
    }

    /// Whether a capture may start at `now`, with `on` the way.
    pub fn gate(&self, on: &Outstanding, now: Instant) -> Gate {
        on.gate(self.path(), now)
    }

    /// What `voxl stats` says of the link at `now`, with `on` the way.
    pub fn stats(&self, on: &Outstanding, now: Instant) -> Stats {
        on.stats(self.path(), now)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::session::link::tests::Simulated;
    use crate::xorshift::Xorshift;

    /// A session's link as its sender holds it while one lane lasts: what is on the way,
    /// and the guess of the path.
    #[derive(Default)]
    pub(crate) struct Link {
        on: Outstanding,
        estimate: Estimate,
    }

    impl Link {
        pub(crate) fn sent(&mut self, sequence: Sequence, bytes: u64, now: Instant) {
            self.estimate.sent(&mut self.on, sequence, bytes, now);
        }

        pub(crate) fn took(&mut self, sequence: Sequence, now: Instant) {
            self.estimate.took(&mut self.on, sequence, now);
        }

        pub(crate) fn gate(&self, now: Instant) -> Gate {
            self.estimate.gate(&self.on, now)
        }

        pub(crate) fn stats(&self, now: Instant) -> Stats {
            self.estimate.stats(&self.on, now)
        }
    }

    /// A frame a test sent: when, when it was told taken, and what the link showed when it
    /// was sent.
    struct Frame {
        sent: Instant,
        took: Instant,
        bytes: u64,
        /// What the link knew of the path when it was sent.
        shown: Path,
        /// It went into a steady stream.
        steady: bool,
    }

    /// A viewer that wants every frame: a frame of `size()` goes whenever the gate lets
    /// one, a frame's coding time apart at the least, from `from` for `long`.
    fn stream(
        link: &mut Link,
        path: &mut Simulated,
        next: &mut u32,
        (from, long): (Instant, Duration),
        mut size: impl FnMut() -> u64,
    ) -> Vec<Frame> {
        let coding = Duration::from_millis(5);
        let mut frames: Vec<Frame> = Vec::new();
        let mut now = from;
        while now < from + long {
            path.tell(now, |sequence, at| link.took(sequence, at));
            if link.gate(now) != Gate::Open {
                // Held: nothing opens the gate but a took.
                let (told, _) = *path.tooks.front().expect("a held gate has a took to come");
                now = now.max(told);
                continue;
            }
            let (sequence, bytes) = (Sequence(*next), size());
            *next += 1;
            let stats = link.stats(now);
            let steady = path.steady(now);
            link.sent(sequence, bytes, now);
            let took = path.send(sequence, bytes, Duration::ZERO, now);
            frames.push(Frame {
                sent: now,
                took,
                bytes,
                shown: stats.path,
                steady,
            });
            now += coding;
        }
        frames
    }

    /// Frames sent alone, as typing sends them: one every `gap`, each 2 KB, each told taken
    /// with `jitter()` more than the path's own time, from `from` for `long`.
    fn typing(
        link: &mut Link,
        path: &mut Simulated,
        next: &mut u32,
        (from, long): (Instant, Duration),
        gap: Duration,
        mut jitter: impl FnMut() -> Duration,
    ) {
        let mut now = from;
        while now < from + long {
            path.tell(now, |sequence, at| link.took(sequence, at));
            if link.gate(now) == Gate::Open {
                let sequence = Sequence(*next);
                *next += 1;
                link.sent(sequence, 2_000, now);
                path.send(sequence, 2_000, jitter(), now);
            }
            now += gap;
        }
        path.tell(from + long, |sequence, at| link.took(sequence, at));
    }

    /// A path of any rate from 20 KB/s to 40 MB/s and any round trip from 1 to 400 ms.
    fn any_path(rng: &mut Xorshift, now: Instant) -> Simulated {
        let rate = 20_000u64 << rng.below(11);
        let rate = rate + rng.below(rate);
        Simulated::new(rate, Duration::from_micros(rng.range(1_000, 400_000)), now)
    }

    fn known(link: &Link, at: Instant) -> Known {
        match link.stats(at).path {
            Path::Known(known) => known,
            Path::Unknown => panic!("the path was not seen"),
        }
    }

    /// Whether the rate `shown` is within a tenth under `path_rate`, or over it.
    fn nears(shown: Path, path_rate: u64) -> bool {
        match shown {
            Path::Known(known) => known.rate.0 >= path_rate / 10 * 9,
            Path::Unknown => false,
        }
    }

    fn fastest(link: &Link, at: Instant) -> Duration {
        known(link, at).round_trip
    }

    fn rate(link: &Link, at: Instant) -> u64 {
        known(link, at).rate.0
    }

    /// What is in flight is bounded as the head of this module says. A viewer wants every
    /// frame of a desktop that never stops moving, over generated paths (20 KB/s to 40 MB/s,
    /// round trips of 1 to 400 ms) and frames of generated sizes. On a virtual clock, with
    /// the path's own word as the oracle:
    /// - every frame that goes into a steady stream is told taken within the path's time
    ///   for the window the link had when it was sent and the frame's own bytes' time;
    /// - any frame at all, the first after a pause included, is within the path's round
    ///   trip more than that: what goes at once from rest is at most one round trip of the
    ///   rate more;
    /// - the stream runs without a stop: in every second once the link's rate has neared
    ///   the path's, the path is told to have taken its rate, to two frames;
    /// - the link's rate is never over the path's by more than one frame's bytes in a round
    ///   trip, and ends no lower than the path's;
    /// - the link's round trip is never under the path's, and never over it by more than
    ///   the largest frame's time on the path: no frame's wait behind others is in it.
    #[test]
    fn a_frame_is_taken_within_the_round_trip_the_response_and_its_own_time() {
        let mut rng = Xorshift::new(0x7137_449f_23ef_65cd);
        let start = Instant::now();
        let (mut steady, mut from_idle) = (0, 0);
        for case in 0..60 {
            let mut path = any_path(&mut rng, start);
            let (path_rate, delay) = (path.rate, path.delay);
            let mut link = Link::default();
            let mut next = 0;
            // Frames up to a fortieth of a second of the path: a stream it carries dozens a
            // second of, and that a frame every coding time overfills.
            let most = (path_rate / 40).max(2_000);
            let mut sizes = rng.clone();
            let long = Duration::from_secs(20);
            // The session's first frame: a keyframe, a fifth of a second of the path.
            let mut first = Some(path_rate / 5);
            let frames = stream(&mut link, &mut path, &mut next, (start, long), || {
                first.take().unwrap_or_else(|| 500 + sizes.below(most))
            });
            let slack = Duration::from_micros(1);
            for f in &frames {
                let Path::Known(known) = f.shown else {
                    continue;
                };
                let (fastest, rate) = (known.round_trip, known.rate.0);
                // The keyframe's own time is in the round trip until a smaller frame shows
                // a shorter one: frames from then on are held to the stream's sizes.
                if f.sent <= frames[1].took {
                    continue;
                }
                let over = BytesPerSecond::of(most + 500, delay).0;
                assert!(
                    rate <= path_rate + over,
                    "case {case}: a rate of {rate}, the path's {path_rate}"
                );
                assert!(
                    fastest >= delay && fastest <= delay + path.time(most + 500) + slack,
                    "case {case}: a round trip of {fastest:?}, the path's {delay:?}"
                );
                let (lag, own) = (f.took - f.sent, path.time(f.bytes));
                let window = path.time(known.window());
                assert!(
                    lag <= window + own + delay + slack,
                    "case {case}: {lag:?} at {path_rate} B/s, {delay:?}"
                );
                from_idle += usize::from(lag > window + own + slack);
                if f.steady {
                    assert!(
                        lag <= window + own + slack,
                        "case {case}: a steady frame, {lag:?} at {path_rate} B/s, {delay:?}"
                    );
                    steady += 1;
                }
            }
            let found = frames
                .iter()
                .find(|f| nears(f.shown, path_rate))
                .unwrap_or_else(|| panic!("case {case}: the rate never neared the path's"));
            let found = (found.sent - start).as_secs() + 1;
            assert!(
                found < 10,
                "case {case}: the path's rate found after {found} s"
            );
            for second in found..19 {
                let from = start + Duration::from_secs(second);
                let taken: u64 = frames
                    .iter()
                    .filter(|f| f.took > from && f.took <= from + Duration::from_secs(1))
                    .map(|f| f.bytes)
                    .sum();
                assert!(
                    taken + 2 * (most + 500) >= path_rate,
                    "case {case}: {taken} B taken in second {second} at {path_rate} B/s"
                );
            }
            path.tell(start + long + Duration::from_secs(3600), |sequence, at| {
                link.took(sequence, at)
            });
            let known = rate(&link, start + long);
            assert!(
                known >= path_rate - path_rate / 1000,
                "case {case}: the rate ends at {known}, the path's is {path_rate}"
            );
        }
        assert!(
            steady > 5_000 && from_idle > 20,
            "{steady} steady frames, {from_idle} past the steady bound"
        );
    }

    /// The round trip follows the path's, and only the path's. Over generated paths, a
    /// session types (a small frame alone, every round trip and a half), and the path's
    /// round trip steps to a generated other one, longer or shorter:
    /// - shorter: the link's round trip is the new one from the first frame told taken
    ///   after the step;
    /// - longer: it is the new one once the path's memory has passed since the last frame
    ///   of the old path was told taken, and not before the memory has passed since the
    ///   step: a frame held up once is not a path grown longer.
    #[test]
    fn the_round_trip_follows_the_paths() {
        let mut rng = Xorshift::new(0xb5c0_fbcf_ec4d_3b2f);
        let start = Instant::now();
        let (mut longer, mut shorter) = (0, 0);
        for case in 0..100 {
            let mut path = any_path(&mut rng, start);
            let before = path.delay;
            let after = Duration::from_micros(rng.range(1_000, 400_000));
            let own = path.time(2_000) + Duration::from_micros(1);
            let gap = before.max(after) * 3 / 2 + own;
            let mut link = Link::default();
            let mut next = 0;
            let quiet = || Duration::ZERO;
            let step = start + Duration::from_secs(5);
            typing(
                &mut link,
                &mut path,
                &mut next,
                (start, step - start),
                gap,
                quiet,
            );
            let old = fastest(&link, step);
            assert!(old >= before && old <= before + own);
            path.delay = after;
            // One frame of the new path told taken.
            typing(&mut link, &mut path, &mut next, (step, 2 * gap), gap, quiet);
            let first = fastest(&link, step + 2 * gap);
            match after < before {
                true => {
                    assert!(
                        first <= after + own,
                        "case {case}: {first:?} after one frame"
                    );
                    shorter += 1;
                }
                false => {
                    assert_eq!(first, old, "case {case}: one frame moved it");
                    // Up to the memory since the step it stands; past the memory since the
                    // old path's last frame it has followed.
                    let nearly = PATH_MEMORY - 3 * gap;
                    typing(
                        &mut link,
                        &mut path,
                        &mut next,
                        (step + 2 * gap, nearly),
                        gap,
                        quiet,
                    );
                    assert_eq!(
                        fastest(&link, step + nearly),
                        old,
                        "case {case}: it rose early"
                    );
                    let rest = (step + 2 * gap + nearly, 6 * gap);
                    typing(&mut link, &mut path, &mut next, rest, gap, quiet);
                    let now = fastest(&link, step + PATH_MEMORY + 3 * gap);
                    assert!(
                        now >= after && now <= after + own,
                        "case {case}: {now:?} once the memory passed, the path's is {after:?}"
                    );
                    longer += usize::from(after > before + own);
                }
            }
        }
        assert!(
            longer > 20 && shorter > 20,
            "{longer} longer, {shorter} shorter"
        );
    }

    /// The rate corrects itself both ways, and typing does not read as a slow path. Over
    /// generated paths, a desktop moves without a stop; then its session types for a while
    /// (small frames alone, each told taken with a generated jitter of up to 20 ms); then
    /// the path's rate steps to a generated other one, a tenth to ten times, and the desktop
    /// moves again, every frame the same size. In the last second of each stream the path
    /// is used whole: the bytes told taken are the path's rate for that second, to two
    /// frames; the link's rate ends each stream at the path's; and the typing between
    /// leaves the rate where the stream left it. A rate read as bytes over the time frames
    /// were outstanding fails this: after typing it knows only the round trip.
    #[test]
    fn the_rate_corrects_itself_both_ways() {
        let mut rng = Xorshift::new(0xe9b5_dba5_8189_dbbc);
        let start = Instant::now();
        let (mut faster, mut slower) = (0, 0);
        for case in 0..60 {
            let mut path = any_path(&mut rng, start);
            let mut link = Link::default();
            let mut next = 0;
            let mut from = start;
            let long = Duration::from_secs(8);
            for moving in 0..2 {
                let path_rate = path.rate;
                let frame = (path_rate / 50).max(1_000);
                let frames = stream(&mut link, &mut path, &mut next, (from, long), || frame);
                let last = from + long - Duration::from_secs(1);
                let taken: u64 = frames
                    .iter()
                    .filter(|f| f.took > last && f.took <= from + long)
                    .map(|f| f.bytes)
                    .sum();
                assert!(
                    taken + 2 * frame >= path_rate,
                    "case {case}, stream {moving}: {taken} B taken in the last second at \
                     {path_rate} B/s"
                );
                from += long + Duration::from_secs(1);
                path.tell(from, |sequence, at| link.took(sequence, at));
                let known = rate(&link, from);
                let over = BytesPerSecond::of(frame, path.delay).0;
                assert!(
                    known <= path_rate + over && known >= path_rate - path_rate / 1000,
                    "case {case}, stream {moving}: the rate ends at {known}, the path's is \
                     {path_rate}"
                );
                // Typing: frames alone, in no hurry, their tooks jittered.
                let gap = path.delay * 3 / 2 + path.time(2_000) + Duration::from_millis(20);
                let mut noise = rng.clone();
                let jitter = move || Duration::from_micros(noise.below(20_000));
                let typed = Duration::from_secs(6);
                typing(&mut link, &mut path, &mut next, (from, typed), gap, jitter);
                from += typed + Duration::from_secs(1);
                path.tell(from, |sequence, at| link.took(sequence, at));
                assert_eq!(
                    rate(&link, from),
                    known,
                    "case {case}: typing moved the rate"
                );
                // The path's rate steps: a tenth to ten times, down as often as up.
                let tenths = match rng.below(2) {
                    0 => rng.range(1, 8),
                    1.. => rng.range(12, 100),
                };
                path.rate = (path_rate * tenths / 10).max(2_000);
                path.free = from;
                faster += usize::from(moving == 0 && tenths > 15);
                slower += usize::from(moving == 0 && tenths < 7);
            }
        }
        assert!(
            faster > 10 && slower > 3,
            "{faster} faster, {slower} slower"
        );
    }
}
