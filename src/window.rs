//! The window: what a viewer's path carries in the delay a frame may add, and so what may
//! be on its way to that viewer at once.
//!
//! The transport is in order and loses nothing, so the server knows what it has sent that
//! the viewer has not acknowledged, and when each frame left and came back. That is enough
//! to hold the stream to the path with no estimate of a rate and nothing steered:
//!
//! - **One reading.** While frames come back one behind the other the path is busy with
//!   their bytes and nothing else: each took, to cross, the time between the frame ahead
//!   coming back and itself (or, if nothing was ahead, since it would have come back had
//!   it weighed nothing). So over a round, the bytes that came back and the time they kept
//!   the path busy say what it carries: `bytes * LATENESS / busy` in `LATENESS`. Anything
//!   else that held a frame up (a stall, a busy client) only made `busy` longer, so a
//!   reading is never more than the path carries; and it is trusted no further than twice
//!   the load the path was really under, since frames say nothing of loads they did not
//!   meet.
//! - **One value**, `carries`: the bytes the path carries in `LATENESS`. It moves once a
//!   round (the time one frame takes to go and come back), and only in a round that tested
//!   it, with a load of at least half of it: to that round's reading, and never to less
//!   than half of itself. So typing cannot move it, a stall halves it once, and a path
//!   that never carried a load is never credited with one.
//! - **What a frame may spend** is what the path carries in `LATENESS`, less what is still
//!   queued ahead of it by that same measure; and never more than leaves the unacknowledged
//!   bytes within what the path holds on its way there and back plus that queue. During a
//!   stall nothing comes back, so nothing more goes out.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How late a frame may come back, after the path's fastest round trip: what the stream
/// may add to the path's own delay, under sustained motion and for a burst after stillness
/// alike.
pub const LATENESS: Duration = Duration::from_millis(60);

/// One packet's payload on an Ethernet path. The least a path is taken to carry, and the
/// least room a frame is sent into: a typed character's frame fits in one.
pub const PACKET: usize = 1460;

/// What a new viewer's path is taken to carry: ten packets, what a TCP connection starts
/// with (RFC 6928), and for the same reason: nothing is known about the path yet.
pub const START: usize = 10 * PACKET;

/// The frames a window keeps room for, of those that left in the last `LATENESS`: the capture
/// loop captures at most twice in its least interval (a change and a pass), 3.5 ms at 240
/// frames a second, so at most 35 frames leave in `LATENESS`. Room for 64 is made when a
/// viewer joins, so that no frame grows it: one frame leaving allocates nothing. A bench that
/// sends frames faster than any capture loop still fits: the allocation test, a frame every
/// 2 ms, measured 31 of them in `LATENESS`. At half (32) that is all the margin there is;
/// at double, 1.5 KB a viewer more for nothing.
const LATELY: usize = 64;

/// How long the fastest round trip is remembered. A path whose own delay rises reads as
/// late for this long, and then is the new fastest.
const FASTEST_FOR: Duration = Duration::from_secs(30);

/// A reading is trusted to this many times the load its frames met.
const TRUST: usize = 2;

/// In one round the window falls to no less than this part of itself. A round a stall fell
/// in reads far too low, and motion wins a fall back by `TRUST` a round; a link that really
/// slowed is followed down by this a round.
const FALL: usize = 2;

/// The smallest round trip over `FASTEST_FOR`, and the bytes of the frame it was measured
/// on (a ping's: none), whose own time on the path is part of it.
struct Fastest {
    /// (when, trip, bytes), trips rising from the front: the front is the smallest.
    trips: VecDeque<(Instant, Duration, usize)>,
}

impl Fastest {
    /// A `trip` of `bytes`, taken at `now`; the fastest there is now, which is no slower.
    fn take(&mut self, now: Instant, trip: Duration, bytes: usize) -> (Duration, usize) {
        while self
            .trips
            .front()
            .is_some_and(|&(at, _, _)| now.saturating_duration_since(at) > FASTEST_FOR)
        {
            self.trips.pop_front();
        }
        while self.trips.back().is_some_and(|&(_, t, _)| t >= trip) {
            self.trips.pop_back();
        }
        self.trips.push_back((now, trip, bytes));
        self.get().unwrap_or((trip, bytes))
    }

    /// None until something has come back.
    fn get(&self) -> Option<(Duration, usize)> {
        self.trips.front().map(|&(_, t, b)| (t, b))
    }
}

/// The frames that came back since `began`. It ends at the first of them that left after
/// it began (by then every frame on its way when it began has come back), or once it has
/// gone on as long as a round trip and `LATENESS`.
struct Round {
    began: Instant,
    /// The bytes that came back in it and the time they kept the path busy.
    bytes: usize,
    busy: Duration,
    /// The most the path was under as one of its frames left.
    met: usize,
}

impl Round {
    fn new(began: Instant) -> Self {
        Round {
            began,
            bytes: 0,
            busy: Duration::ZERO,
            met: 0,
        }
    }
}

pub struct Window {
    /// The bytes the path carries in `LATENESS`. Never under `least` or over `most`.
    carries: usize,
    least: usize,
    most: usize,
    fastest: Fastest,
    round: Round,
    /// When the last frame came back, and the bytes that have come back one behind the
    /// other up to it: the load the path has been under without a break.
    last: Option<(Instant, usize)>,
    /// (left at, bytes) of each frame that has left and not come back, oldest first.
    flying: VecDeque<(Instant, usize)>,
    /// The bytes still on their way through the path at `as_of`, had it carried them as
    /// it is taken to since.
    queued: usize,
    as_of: Instant,
    /// (left at, bytes) of the frames that left within the last `LATENESS`, oldest first.
    lately: VecDeque<(Instant, usize)>,
}

/// What a frame leaving finds on its way to the viewer, a viewer being taken to have
/// stalled once its oldest frame on its way has been on its way for some time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Way {
    /// Room for this many bytes (`room`).
    Room(usize),
    /// No room for a packet until an ack makes some, or until `stalls`, when the oldest
    /// frame on its way will have been on its way for that time.
    Full { stalls: Instant },
    /// The oldest frame on its way has been on its way for that time.
    Stalled,
}

/// What a frame was part of as it left: when it left, and the bytes the path was asked to
/// carry in the `LATENESS` up to then (itself included).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Left {
    pub at: Instant,
    pub asked: usize,
}

/// `bytes * a / b`, for durations.
pub fn scale(bytes: usize, a: Duration, b: Duration) -> usize {
    (bytes as u128 * a.as_nanos() / b.as_nanos().max(1)).min(usize::MAX as u128) as usize
}

impl Window {
    /// A viewer's window from `now`. Its path is taken to carry between `least` and
    /// `most` bytes in `LATENESS` (`--min-kbps` and `--max-kbps`); never under one packet,
    /// and where the two cross `most` wins.
    pub fn new(now: Instant, least: usize, most: usize) -> Self {
        let most = most.max(PACKET);
        let least = least.clamp(PACKET, most);
        Window {
            carries: START.clamp(least, most),
            least,
            most,
            fastest: Fastest {
                trips: VecDeque::new(),
            },
            round: Round::new(now),
            last: None,
            flying: VecDeque::new(),
            queued: 0,
            as_of: now,
            lately: VecDeque::with_capacity(LATELY),
        }
    }

    /// The bytes the path carries in `LATENESS`.
    pub fn carries(&self) -> usize {
        self.carries
    }

    /// The fastest round trip of the last `FASTEST_FOR`, once something has come back.
    pub fn fastest(&self) -> Option<Duration> {
        self.fastest.get().map(|(trip, _)| trip)
    }

    /// The bytes that have left and not come back.
    pub fn flying(&self) -> usize {
        self.flying.iter().map(|&(_, b)| b).sum()
    }

    /// The bytes queued in the path at `now`, as far as can be told; never more than are.
    /// Two things tell. What has left since the queue was last known empty, less what the
    /// path carries in the time since: right for a burst, but it takes the path to have
    /// carried all along, and a path that stalled or slowed has more queued than that. And
    /// the frames that left so long ago that they would be back even behind the fullest
    /// queue the path is allowed, and are not: those are queued, but for the oldest, which
    /// may be all but through.
    fn queued(&self, now: Instant) -> usize {
        let through = scale(
            self.carries,
            now.saturating_duration_since(self.as_of),
            LATENESS,
        );
        let sent = self.queued.saturating_sub(through);
        sent.max(self.overdue(now))
    }

    /// The bytes of the frames on their way at `now` that would be back even behind the
    /// fullest queue the path is allowed, but for the oldest.
    fn overdue(&self, now: Instant) -> usize {
        let there_and_back = self.fastest.get().map_or(Duration::ZERO, |(trip, _)| trip);
        self.flying
            .iter()
            .skip(1)
            .filter(|&&(left, _)| now.saturating_duration_since(left) > there_and_back + LATENESS)
            .map(|&(_, b)| b)
            .sum()
    }

    /// When the queue the path is taken to hold at `now` is empty, so that a frame leaving
    /// then finds all of `carries`: the moment what has left since it was last empty is
    /// through at what the path carries (at or before `now` if it is), or None while frames
    /// on their way are overdue, which only their acks clear.
    pub fn empties(&self, now: Instant) -> Option<Instant> {
        let ns = (LATENESS.as_nanos() * self.queued as u128).div_ceil(self.carries as u128);
        let drained = Duration::from_nanos(ns.min(u64::MAX as u128) as u64);
        (self.overdue(now) == 0).then_some(self.as_of + drained)
    }

    /// What a frame leaving at `now` may spend: what the path carries in `LATENESS`, less
    /// what is queued ahead of it; or None while that leaves no room for a packet. With
    /// nothing on its way a frame may always go. In a stall nothing comes back, the queue
    /// stands, and nothing more goes out.
    pub fn room(&self, now: Instant) -> Option<usize> {
        if self.flying.is_empty() {
            return Some(self.carries);
        }
        let room = self.carries.saturating_sub(self.queued(now));
        (room >= PACKET).then_some(room)
    }

    /// What a frame leaving at `now` finds on its way to the viewer, once one whose oldest
    /// frame on its way has been on its way for `stall` is taken to have stalled.
    pub fn way(&self, now: Instant, stall: Duration) -> Way {
        let Some(&(oldest, _)) = self.flying.front() else {
            return Way::Room(self.carries);
        };
        let stalls = oldest + stall;
        match self.room(now) {
            _ if now >= stalls => Way::Stalled,
            Some(room) => Way::Room(room),
            None => Way::Full { stalls },
        }
    }

    /// When the newest frame on its way left, while one is on its way.
    pub fn newest(&self) -> Option<Instant> {
        self.flying.back().map(|&(at, _)| at)
    }

    /// A frame of `bytes` leaves at `now`.
    pub fn leave(&mut self, now: Instant, bytes: usize) -> Left {
        let through = scale(
            self.carries,
            now.saturating_duration_since(self.as_of),
            LATENESS,
        );
        self.queued = self.queued.saturating_sub(through) + bytes;
        self.as_of = now;
        self.flying.push_back((now, bytes));
        while self
            .lately
            .front()
            .is_some_and(|&(at, _)| now.saturating_duration_since(at) > LATENESS)
        {
            self.lately.pop_front();
        }
        self.lately.push_back((now, bytes));
        Left {
            at: now,
            asked: self.lately.iter().map(|&(_, b)| b).sum(),
        }
    }

    /// Whatever was on its way is not coming back: the viewer starts over.
    pub fn forget(&mut self, now: Instant) {
        self.flying.clear();
        self.queued = 0;
        self.as_of = now;
        self.last = None;
        self.round = Round::new(now);
    }

    /// A ping came back at `now`, `trip` after it left. A ping carries nothing, so its
    /// round trip is the path's own: the fastest is measured against it, and a frame's
    /// run then holds all the time its bytes took to cross.
    pub fn pinged(&mut self, now: Instant, trip: Duration) {
        self.fastest.take(now, trip, 0);
    }

    /// A frame of `bytes` that `left` came back at `back` (the time the viewer held it
    /// taken off).
    pub fn came_back(&mut self, left: Left, back: Instant, bytes: usize) {
        self.flying.pop_front();
        if self.flying.is_empty() {
            // Everything is through, whatever the queue was taken to be.
            self.queued = 0;
            self.as_of = back;
        }
        let trip = back.saturating_duration_since(left.at);
        let (fastest, weighed) = self.fastest.take(back, trip, bytes);
        // When it would have come back alone, weighing what the fastest trip's frame did.
        let alone = left.at + fastest;
        let (carried, busy, unbroken) = match self.last {
            // The frame ahead was not through yet: this one took the time since it was.
            Some((ahead, unbroken)) if ahead > alone => (
                bytes,
                back.saturating_duration_since(ahead),
                unbroken + bytes,
            ),
            // Nothing was ahead. The fastest trip's own bytes took their time inside it, so
            // that many of this frame's are not part of what the time since `alone` carried.
            Some(_) | None => (
                bytes.saturating_sub(weighed),
                back.saturating_duration_since(alone),
                bytes,
            ),
        };
        self.last = Some((back, unbroken));
        self.round.bytes += carried;
        self.round.busy += busy;
        // What the path was really under when the frame left.
        self.round.met = self.round.met.max(unbroken).max(left.asked);
        // The round ends with the first frame back that left after it began; or, when the
        // path is so over-full that this takes longer, after as long as a round should
        // take: what has come back by then says what the path carries now.
        let long_enough = back.saturating_duration_since(self.round.began) >= fastest + LATENESS;
        if left.at >= self.round.began || long_enough {
            let Round {
                bytes, busy, met, ..
            } = self.round;
            // A round that did not test what the path carries, or whose frames were no
            // heavier than the fastest trip's, says nothing.
            if met.saturating_mul(2) >= self.carries && bytes > 0 {
                let trusted = met.saturating_mul(TRUST);
                let reading = match busy {
                    Duration::ZERO => trusted,
                    busy => scale(bytes, LATENESS, busy).min(trusted),
                };
                self.carries = reading
                    .max(self.carries / FALL)
                    .clamp(self.least, self.most);
            }
            self.round = Round::new(back);
        }
    }
}

/// The window against a link that is the oracle: a rate, a round trip, and a queue that
/// serves in order; with stalls, and with acknowledgements that come late.
#[cfg(test)]
mod tests {
    use super::*;

    struct Rng(u64);
    impl Rng {
        fn range(&mut self, lo: u64, hi: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            lo + self.0 % (hi - lo)
        }
    }

    const FRAME: Duration = Duration::from_micros(16_667);

    /// A frame on its way.
    struct Flying {
        acked: Instant,
        left: Left,
        bytes: usize,
    }

    /// One viewer on one link, a frame time at a tick.
    struct Sim {
        now: Instant,
        /// The link: bytes a second, its round trip, when it is free again, and the
        /// stalls (from, to) in which it carries nothing.
        rate: f64,
        rtt: Duration,
        free: Instant,
        stalls: Vec<(Instant, Instant)>,
        /// Each acknowledgement comes up to this late, at random.
        jitter: Duration,
        rng: Rng,
        flying: VecDeque<Flying>,
        /// (comes back at, left at) of the pings on their way.
        pings: VecDeque<(Instant, Instant)>,
        ticks: u32,
        in_flight: usize,
        window: Window,
        /// Since `reset`: how late each frame came back (beyond the round trip), the bytes
        /// sent, the smallest share of what it wanted that a frame got, the most that was
        /// in flight, and the biggest load a frame carried.
        lates: Vec<Duration>,
        sent: usize,
        worst_share: f64,
        most_in_flight: usize,
        most_load: usize,
    }

    impl Sim {
        fn new(rate: f64, rtt_ms: u64) -> Self {
            let now = Instant::now();
            Sim {
                now,
                rate,
                rtt: Duration::from_millis(rtt_ms),
                free: now,
                stalls: Vec::new(),
                jitter: Duration::ZERO,
                rng: Rng(0x9E3779B97F4A7C15),
                flying: VecDeque::new(),
                pings: VecDeque::new(),
                ticks: 0,
                in_flight: 0,
                window: Window::new(now, 0, usize::MAX),
                lates: Vec::new(),
                sent: 0,
                worst_share: 1.0,
                most_in_flight: 0,
                most_load: 0,
            }
        }

        fn reset(&mut self) {
            self.lates.clear();
            self.sent = 0;
            self.worst_share = 1.0;
        }

        /// When the link has carried `bytes` more, starting no sooner than `from`: its
        /// rate, less the stalls on the way.
        fn carried(&self, from: Instant, bytes: usize) -> Instant {
            let mut at = from;
            let mut left = Duration::from_secs_f64(bytes as f64 / self.rate);
            for &(a, b) in &self.stalls {
                if b <= at {
                    continue;
                }
                if a > at {
                    let run = a - at;
                    if run >= left {
                        return at + left;
                    }
                    left -= run;
                }
                at = b;
            }
            at + left
        }

        /// One frame time passes; the screen wants to send `want` bytes (0: it is still).
        fn tick(&mut self, want: usize) {
            self.now += FRAME;
            self.ticks += 1;
            // A ping twice a second, behind whatever is on the link, as the server sends.
            if self.ticks.is_multiple_of(30) {
                let stalled = self.carried(self.free.max(self.now), 0);
                self.pings.push_back((stalled + self.rtt, self.now));
            }
            while self.pings.front().is_some_and(|p| p.0 <= self.now) {
                let (back, left) = self.pings.pop_front().unwrap();
                self.window.pinged(back, back - left);
            }
            while self.flying.front().is_some_and(|f| f.acked <= self.now) {
                let f = self.flying.pop_front().unwrap();
                self.in_flight -= f.bytes;
                self.lates
                    .push((f.acked - f.left.at).saturating_sub(self.rtt));
                self.window.came_back(f.left, f.acked, f.bytes);
            }
            if want == 0 {
                return;
            }
            let Some(room) = self.window.room(self.now) else {
                return;
            };
            // The encoder under its cap: a frame holds at most the room it was given.
            let bytes = want.min(room);
            self.worst_share = self.worst_share.min(bytes as f64 / want as f64);
            let left = self.window.leave(self.now, bytes);
            self.in_flight += bytes;
            self.most_in_flight = self.most_in_flight.max(self.in_flight);
            self.most_load = self.most_load.max(self.in_flight);
            self.free = self.carried(self.free.max(self.now), bytes);
            let jitter = match self.jitter.as_micros() as u64 {
                0 => Duration::ZERO,
                most => Duration::from_micros(self.rng.range(0, most + 1)),
            };
            // Acknowledgements come in the order their frames left.
            let acked = (self.free + self.rtt + jitter)
                .max(self.flying.back().map_or(self.now, |f| f.acked));
            self.flying.push_back(Flying { acked, left, bytes });
            self.sent += bytes;
        }

        fn run(&mut self, ticks: u32, mut want: impl FnMut(u32) -> usize) {
            for i in 0..ticks {
                self.tick(want(i));
            }
        }

        /// The `share` of frames since `reset` that came back no later than this.
        fn late(&self, share: f64) -> Duration {
            let mut l = self.lates.clone();
            l.sort();
            l[((l.len() - 1) as f64 * share) as usize]
        }

        /// What the link carries in `LATENESS`.
        fn fits(&self) -> f64 {
            self.rate * LATENESS.as_secs_f64()
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

    /// Motion that wants more than any link here carries.
    const HEAVY: usize = 4_000_000;

    /// Sustained motion that wants everything, on any link, settles where the link is
    /// used and frames come back within about twice `LATENESS` (the queue it allows, and
    /// the frame in it that is all but through): from 150 kbit/s to 80 Mbit/s, round trips
    /// of 2 to 300 ms.
    #[test]
    fn sustained_motion_fills_the_link_and_adds_about_the_lateness() {
        let mut rng = Rng(0x2545F4914F6CDD1D);
        for _ in 0..200 {
            let rate = rng.range(19_000, 10_000_000) as f64;
            let mut sim = Sim::new(rate, rng.range(2, 300));
            sim.run(600, |_| HEAVY);
            sim.reset();
            sim.run(1200, |_| HEAVY);
            let used = sim.sent as f64 / (rate * 20.0);
            let what = format!(
                "{rate} B/s, {:?}: used {used:.2}, late p50 {:?} max {:?}, window {} of {:.0}",
                sim.rtt,
                sim.late(0.5),
                sim.late(1.0),
                sim.window.carries(),
                sim.fits()
            );
            // A link that carries only a couple of packets in `LATENESS` spends much of
            // its time waiting for a packet of room.
            let least = if sim.fits() < 4.0 * PACKET as f64 {
                0.5
            } else {
                0.8
            };
            assert!(used > least, "{what}");
            assert!(sim.late(0.5) <= LATENESS * 2, "{what}");
            assert!(sim.late(1.0) <= LATENESS * 3, "{what}");
        }
    }

    /// The window is never more than twice a load the path has carried, whatever is sent
    /// and however its acknowledgements come; and never more is in flight than the window
    /// allowed when the last frame left.
    #[test]
    fn the_window_is_never_more_than_twice_a_load_the_path_carried() {
        let mut rng = Rng(0x9E3779B97F4A7C15);
        for _ in 0..300 {
            let mut sim = Sim::new(rng.range(19_000, 10_000_000) as f64, rng.range(2, 300));
            sim.jitter = Duration::from_millis(rng.range(0, 3000));
            for _ in 0..rng.range(1, 6) {
                let from = sim.now + Duration::from_millis(rng.range(0, 20_000));
                sim.stalls
                    .push((from, from + Duration::from_millis(rng.range(100, 4000))));
            }
            sim.stalls.sort();
            let mut most = START;
            for _ in 0..2000 {
                let want = match rng.range(0, 4) {
                    0 => 0,
                    1 => rng.range(200, 2000) as usize,
                    2 => rng.range(2000, 60_000) as usize,
                    _ => HEAVY,
                };
                sim.tick(want);
                most = most.max(sim.most_load * TRUST);
                assert!(sim.window.carries() <= most);
                assert!(sim.window.carries() >= PACKET);
            }
        }
    }

    /// Typing does not move the window, however late each of its frames comes back, while
    /// they come back one at a time: a frame that met less than half of what the path
    /// carries did not test it.
    #[test]
    fn frames_that_did_not_test_the_window_do_not_move_it() {
        let mut rng = Rng(0xD1B54A32D192ED03);
        for _ in 0..200 {
            let mut sim = Sim::new(rng.range(19_000, 10_000_000) as f64, rng.range(2, 300));
            // A scroll first, so the window is the path's and not the start.
            sim.run(600, |_| HEAVY);
            // The scroll's last round ends with the first frame that leaves after the
            // scroll has all come back.
            sim.run(180, |i| if i % 60 == 59 { 700 } else { 0 });
            let before = sim.window.carries();
            // A key every 200 ms, each frame up to 150 ms late.
            sim.jitter = Duration::from_millis(rng.range(0, 150));
            let small = (before / 2).saturating_sub(1).min(700);
            sim.run(1200, |i| if i % 12 == 0 { small } else { 0 });
            assert_eq!(
                sim.window.carries(),
                before,
                "{} B/s, {:?}, jitter {:?}, keys of {small}",
                sim.rate,
                sim.rtt,
                sim.jitter
            );
        }
    }

    /// A scroll after stillness gets what it asks for from its first frame, on a link with
    /// room: stillness does not shrink the window.
    #[test]
    fn a_scroll_after_stillness_is_whole_from_its_first_frame() {
        let mut rng = Rng(0x2545F4914F6CDD1D);
        for _ in 0..100 {
            // 60 KB a frame at 60 fps is 3.6 MB/s.
            let rate = rng.range(4_500_000, 10_000_000) as f64;
            let mut sim = Sim::new(rate, rng.range(20, 150));
            sim.run(120, rest);
            sim.run(180, |_| 60_000);
            sim.run(rng.range(60, 3600) as u32, rest);
            sim.reset();
            sim.run(60, |_| 60_000);
            assert_eq!(sim.worst_share, 1.0, "{rate} B/s, {:?}", sim.rtt);
            assert!(sim.late(1.0) <= LATENESS, "{rate} B/s: {:?}", sim.late(1.0));
        }
    }

    /// A viewer in motion from its first frame learns a fast path: the window is found by
    /// the frames themselves, with nothing held back for a path not yet known.
    #[test]
    fn motion_from_the_first_frame_finds_a_fast_path() {
        let mut rng = Rng(0x9E3779B97F4A7C15);
        for _ in 0..100 {
            let rate = rng.range(4_500_000, 125_000_000) as f64;
            let mut sim = Sim::new(rate, rng.range(1, 100));
            sim.run(60, |_| 60_000);
            sim.reset();
            sim.run(60, |_| 60_000);
            assert_eq!(sim.worst_share, 1.0, "{rate} B/s, {:?}", sim.rtt);
        }
    }

    /// A stall halves the window at most a few times and motion after it wins it back:
    /// within two seconds of the stall's end the link is used again.
    #[test]
    fn a_stall_is_not_a_slow_path() {
        let mut rng = Rng(0xD1B54A32D192ED03);
        for _ in 0..200 {
            let rate = rng.range(19_000, 10_000_000) as f64;
            let mut sim = Sim::new(rate, rng.range(2, 300));
            sim.run(600, |_| HEAVY);
            let from = sim.now + Duration::from_millis(rng.range(0, 500));
            let to = from + Duration::from_millis(rng.range(100, 4000));
            sim.stalls.push((from, to));
            // In a stall nothing comes back, and soon nothing more goes out. What is on
            // its way is what was when the stall began (a round trip and a queue's worth)
            // and what left until the frames on their way were later than the window allows
            // (as much again, and the queue that fills while it waits): never more than
            // the path is taken to carry in two round trips and four times `LATENESS`,
            // however long the stall lasts.
            // (By the most the path was taken to carry on the way: the window falls while
            // the stall lasts, and what left before it fell is still out.)
            let mut carries = 0;
            while sim.now < to {
                carries = carries.max(sim.window.carries());
                let most = scale(carries, sim.rtt * 2 + LATENESS * 4, LATENESS) + PACKET;
                sim.tick(HEAVY);
                assert!(
                    sim.in_flight <= most,
                    "{rate} B/s: {} of {most}",
                    sim.in_flight
                );
            }
            sim.run(120, |_| HEAVY);
            sim.reset();
            sim.run(600, |_| HEAVY);
            let used = sim.sent as f64 / (rate * 10.0);
            assert!(
                used > 0.9,
                "{rate} B/s, {:?}, stall {:?}: used {used:.2}",
                sim.rtt,
                to - from
            );
        }
    }

    /// A link that slows to a tenth or a quarter is followed down, and one that quickens
    /// up: five seconds after the change the link is used and frames come back within
    /// about twice `LATENESS`.
    #[test]
    fn a_link_that_changes_is_followed() {
        let mut rng = Rng(0x2545F4914F6CDD1D);
        for _ in 0..200 {
            let rate = rng.range(60_000, 5_000_000) as f64;
            let to = rate * [0.1, 0.25, 4.0, 10.0][rng.range(0, 4) as usize];
            let mut sim = Sim::new(rate, rng.range(2, 300));
            sim.run(600, |_| HEAVY);
            sim.rate = to;
            sim.run(300, |_| HEAVY);
            sim.reset();
            sim.run(600, |_| HEAVY);
            let used = sim.sent as f64 / (to * 10.0);
            let what = format!(
                "{rate} to {to} B/s, {:?}: used {used:.2}, late p50 {:?}",
                sim.rtt,
                sim.late(0.5)
            );
            // A link that carries only a couple of packets in `LATENESS` spends much of
            // its time waiting for a packet of room.
            let least = if to * LATENESS.as_secs_f64() < 4.0 * PACKET as f64 {
                0.5
            } else {
                0.8
            };
            assert!(used > least, "{what}");
            // A queue left over from before the change stands at the most the window
            // allows before it sees it: `LATENESS`, the frame in it, and a frame time.
            // (A link that carries less than two packets in `LATENESS` is as late as its
            // packets make it.)
            if to * LATENESS.as_secs_f64() >= 2.0 * PACKET as f64 {
                assert!(sim.late(0.5) <= LATENESS * 5 / 2, "{what}");
            }
        }
    }

    /// Acknowledgements that come late at random (a busy server, a path with jitter) do
    /// not starve the stream: with every one up to 40 ms late, motion still uses most of
    /// the link.
    #[test]
    fn late_acknowledgements_do_not_starve_the_stream() {
        let mut rng = Rng(0x9E3779B97F4A7C15);
        for _ in 0..200 {
            let rate = rng.range(19_000, 10_000_000) as f64;
            let mut sim = Sim::new(rate, rng.range(2, 300));
            sim.jitter = Duration::from_millis(rng.range(1, 40));
            sim.run(600, |_| HEAVY);
            sim.reset();
            sim.run(1200, |_| HEAVY);
            let used = sim.sent as f64 / (rate * 20.0);
            assert!(
                used > 0.5,
                "{rate} B/s, {:?}, jitter {:?}: used {used:.2}",
                sim.rtt,
                sim.jitter
            );
        }
    }
}
