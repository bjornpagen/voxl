//! The window: what a viewer's path carries in the delay a frame may add, and so what may
//! be on its way to that viewer at once.
//!
//! The transport is in order and loses nothing, so the server knows what it has sent that
//! the viewer has not acknowledged, and when each frame left and came back. That is enough
//! to hold the stream to the path with no estimate of a rate and nothing steered:
//!
//! - **One value**, `carries`: the bytes the path carries in `LATENESS`, from a packet up.
//!   A path that carries less than a packet in it is outside the window's range.
//! - **One queue**, a time: the path is taken to be busy with what has left until `busy`,
//!   carrying `carries` in each `LATENESS`. It says what a frame may spend, nothing more.
//! - **One reading a frame**, of bytes that really crossed the path, as a TCP sender reads
//!   its delivery rate: the bytes that came back from the last ack before the frame left
//!   until its own, over the longer of the time between those acks and the time between
//!   their frames' leaving, never under `LATENESS / 2`. Every byte counted left and came back
//!   within those times, so acks that bunch are read over the time their frames took to
//!   leave, a burst over the time its acks took to come, and no reading is more than the
//!   path delivered. A frame that left an empty path reads its own bytes over its whole
//!   trip. Which of the two times is longer says what set the pace: the path, or the stream.
//!   With it, how long the frame waited behind what was queued: its trip past the fastest
//!   ping's and its own crossing. Only pings say the fastest trip, and no reading uses it: a
//!   ping's own lateness would be an error in every reading.
//! - **One move a round** (the time one frame takes to go and come back), never to less than
//!   half the window or more than twice it. Where the path set the pace and every frame
//!   waited more than `LATENESS / 2`, to what it delivered, less as much again as the queue
//!   is over that, over twice the round. Where the window held the stream back (every
//!   frame took all its room) and the path kept up, up by as much as the queue is short of
//!   it, over twice the round: if the path carries no more, one round adds a quarter of
//!   `LATENESS` of queue. A round in which the stream sent less than the window let
//!   through says only that the path carries at least what it delivered.
//! - **What a frame may spend** is what the path carries in `LATENESS`, less what is
//!   queued ahead of it. During a stall nothing comes back, the frames on their way grow
//!   late, and nothing more goes out.

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

/// In one round the window falls to no less than this part of itself, and rises to no more
/// than this many times itself. A round a stall fell in reads far too low; a link that
/// really slowed is followed down by this a round. A frame that crossed alone may read twice
/// its bytes, a keyframe eight times the window's: against a first ping that came back
/// late, it would lift the window by all of that.
pub const FALL: usize = 2;

/// How long a ping that left an empty path is remembered for the path's fastest trip. A
/// path whose own delay rises reads as late for this long after the last such ping before
/// the rise; under motion that never pauses no ping leaves an empty path, and it stays so
/// until the first pause.
const FASTEST_FOR: Duration = Duration::from_secs(30);

/// The pings `FASTEST_FOR` holds: one every `PING_EVERY`. Room for them is made when a
/// viewer joins, so no ping grows it.
const PINGS_FOR: usize =
    (FASTEST_FOR.as_millis() / crate::hub::PING_EVERY.as_millis()) as usize + 1;

/// How the window was sent through in a round: whether it held the stream back.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Sent {
    /// No frame has left in it.
    Nothing,
    /// Every frame that left took all its room, to within a packet: the stream wanted more
    /// than the window let through.
    Filled,
    /// One left with room to spare: the stream sent what it had.
    Short,
}

/// The frames that come back from `began`: it ends at the first of them, acknowledged by
/// its own id, that left after it began (by then every frame on its way when it began has
/// come back), or that came back as long as a round trip and `LATENESS` after it began.
struct Round {
    began: Instant,
    /// What its frames said, once one has.
    read: Option<Reading>,
    sent: Sent,
}

/// What the frames of a round said.
#[derive(Clone, Copy, Debug)]
struct Reading {
    /// The most any said the path carries in `LATENESS`, what set its pace, and the bytes
    /// it counted.
    says: usize,
    pace: Pace,
    counted: usize,
    /// The least time any waited behind what was queued.
    queue: Duration,
}

/// What set the pace of a frame's reading.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Pace {
    /// Its acks came back further apart than its frames left: the path did.
    Path,
    /// They did not, or it crossed alone: the stream did.
    Stream,
}

impl Round {
    fn new(began: Instant) -> Round {
        Round {
            began,
            read: None,
            sent: Sent::Nothing,
        }
    }
}

pub struct Window {
    /// The bytes the path carries in `LATENESS`. Never under `least` or over `most`.
    carries: usize,
    /// The least `carries` has been.
    lowest: usize,
    least: usize,
    most: usize,
    /// The path's fastest round trip, once a ping has come back: the least trip of the pings
    /// in `pings`, once two are there; until then the least of any. No ping is faster than
    /// the path's own trip, so any ping faster than it lowers it, whatever it left behind.
    fastest: Option<Duration>,
    /// (came back at, trip) of the pings of the last `FASTEST_FOR` that left an empty path
    /// or came back faster than `fastest`, oldest first.
    pings: VecDeque<(Instant, Duration)>,
    /// Until when the path is busy with what has left, carrying `carries` in each
    /// `LATENESS`.
    busy: Instant,
    /// (left at, bytes, what had come back by then) of each frame that has left and not
    /// come back, oldest first.
    flying: VecDeque<(Instant, usize, Before)>,
    /// The bytes that have come back, ever: what each frame's reading counts from.
    delivered: usize,
    /// When the frame that came back last did, and when it had left; since nothing was on
    /// its way, when the first frame after that left, for both: the acks' time then holds
    /// that frame's trip, as a TCP sender's does after an idle path.
    acked: Instant,
    sent: Instant,
    round: Round,
}

/// What had come back when a frame left: what its reading counts from.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Before {
    /// Nothing was on its way: it had the path to itself.
    Nothing,
    /// Frames were: `delivered` bytes had come back, the last at `acked`, which had left at
    /// `sent`.
    Frames {
        delivered: usize,
        acked: Instant,
        sent: Instant,
    },
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

/// What was on its way to the viewer as a ping left.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Ahead {
    /// Nothing: it had the path to itself.
    Nothing,
    /// Frames, which it may have queued behind.
    Frames,
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
            lowest: START.clamp(least, most),
            least,
            most,
            fastest: None,
            pings: VecDeque::with_capacity(PINGS_FOR),
            busy: now,
            flying: VecDeque::new(),
            delivered: 0,
            acked: now,
            sent: now,
            round: Round::new(now),
        }
    }

    /// The bytes the path carries in `LATENESS`.
    pub fn carries(&self) -> usize {
        self.carries
    }

    /// The least the path has been taken to carry in `LATENESS`.
    pub fn lowest(&self) -> usize {
        self.lowest
    }

    /// The path's fastest round trip, once a ping has come back.
    pub fn fastest(&self) -> Option<Duration> {
        self.fastest
    }

    /// The bytes that have left and not come back.
    pub fn flying(&self) -> usize {
        self.flying.iter().map(|&(_, b, _)| b).sum()
    }

    /// What a ping leaving now finds ahead of it on the path.
    pub fn ahead(&self) -> Ahead {
        match self.flying.is_empty() {
            true => Ahead::Nothing,
            false => Ahead::Frames,
        }
    }

    /// The time the path takes to carry `bytes` at `carries`, to the nanosecond below.
    fn time(&self, bytes: usize) -> Duration {
        let ns = bytes as u128 * LATENESS.as_nanos() / self.carries as u128;
        Duration::from_nanos(ns.min(u64::MAX as u128) as u64)
    }

    /// The bytes the path is still busy with at `now`, at `carries`: every byte not yet
    /// through, so none until `busy` and some right up to it. With `time`, the queue a frame
    /// leaves is all it was given exactly, and a move keeps it: for any `carries` under
    /// 60 MB (a byte a nanosecond of `LATENESS`), `time` loses less than one byte.
    fn model(&self, now: Instant) -> usize {
        let ns = self.busy.saturating_duration_since(now).as_nanos();
        let bytes = (self.carries as u128 * ns).div_ceil(LATENESS.as_nanos());
        bytes.min(usize::MAX as u128) as usize
    }

    /// The bytes queued in the path at `now`, as far as can be told; never more than are.
    /// Two things tell. The model queue, which takes the path to have carried all along,
    /// and a path that stalled or slowed has more queued than that. And the frames that left
    /// so long ago that they would be back even behind the fullest queue the path is
    /// allowed, and are not: those are queued, but for the oldest, which may be all but
    /// through.
    fn queued(&self, now: Instant) -> usize {
        self.model(now).max(self.overdue(now))
    }

    /// The bytes of the frames on their way at `now` that would be back even behind the
    /// fullest queue the path is allowed, but for the oldest. Until a ping has said how
    /// fast the path is, none.
    fn overdue(&self, now: Instant) -> usize {
        let Some(fastest) = self.fastest else {
            return 0;
        };
        self.flying
            .iter()
            .skip(1)
            .filter(|&&(left, _, _)| now.saturating_duration_since(left) > fastest + LATENESS)
            .map(|&(_, b, _)| b)
            .sum()
    }

    /// When the queue the path is taken to hold at `now` is empty, so that a frame leaving
    /// then finds all of `carries`: `busy`, or None while frames on their way are overdue,
    /// which only their acks clear.
    pub fn empties(&self, now: Instant) -> Option<Instant> {
        (self.overdue(now) == 0).then_some(self.busy)
    }

    /// What a frame leaving at `now` may spend: what the path carries in `LATENESS`, less
    /// what is queued ahead of it; or None while that leaves no room for a packet. In a
    /// stall nothing comes back, the frames on their way grow overdue, and nothing more
    /// goes out.
    pub fn room(&self, now: Instant) -> Option<usize> {
        let room = self.carries.saturating_sub(self.queued(now));
        (room >= PACKET).then_some(room)
    }

    /// What a frame leaving at `now` finds on its way to the viewer, once one whose oldest
    /// frame on its way has been on its way for `stall` is taken to have stalled.
    pub fn way(&self, now: Instant, stall: Duration) -> Way {
        let Some(&(oldest, _, _)) = self.flying.front() else {
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
        self.flying.back().map(|&(at, _, _)| at)
    }

    /// A frame of `bytes` leaves at `now`: the path is busy until what is queued ahead of
    /// it and itself are through, and it keeps what had come back, to count its reading
    /// from.
    pub fn leave(&mut self, now: Instant, bytes: usize) {
        let queued = self.queued(now);
        let filled = queued + bytes + PACKET > self.carries;
        self.round.sent = match (self.round.sent, filled) {
            (Sent::Nothing | Sent::Filled, true) => Sent::Filled,
            (Sent::Nothing | Sent::Filled, false) | (Sent::Short, _) => Sent::Short,
        };
        let before = match self.flying.is_empty() {
            true => {
                (self.acked, self.sent) = (now, now);
                Before::Nothing
            }
            false => Before::Frames {
                delivered: self.delivered,
                acked: self.acked,
                sent: self.sent,
            },
        };
        self.busy = now + self.time(queued + bytes);
        self.flying.push_back((now, bytes, before));
    }

    /// Whatever was on its way is not coming back: the viewer starts over.
    pub fn forget(&mut self, now: Instant) {
        self.flying.clear();
        self.busy = now;
        self.round = Round::new(now);
    }

    /// A ping came back at `now`, `trip` after it left, with `ahead` on its way as it left.
    /// A ping carries nothing: no ping is faster than the path's own trip, so one faster
    /// than `fastest` lowers it, whatever was ahead of it; and of those that had the path
    /// to themselves, the least of the last `FASTEST_FOR` is nearest it. That least may
    /// raise it only once two pings say so: one ping's own lateness would be an error in
    /// every reading.
    pub fn pinged(&mut self, now: Instant, trip: Duration, ahead: Ahead) {
        let lower = self.fastest.is_some_and(|fastest| trip < fastest);
        if ahead == Ahead::Nothing || lower {
            self.pings.push_back((now, trip));
        }
        while self
            .pings
            .front()
            .is_some_and(|&(at, _)| now.saturating_duration_since(at) > FASTEST_FOR)
        {
            self.pings.pop_front();
        }
        let least = self.pings.iter().map(|&(_, trip)| trip).min();
        self.fastest = match (self.pings.len() >= 2, least) {
            (true, Some(least)) => Some(least),
            (true, None) | (false, None) => self.fastest,
            (false, Some(least)) => Some(self.fastest.map_or(least, |f| f.min(least))),
        };
    }

    /// The oldest frame on its way came back: it was at the viewer at `at` (the ack's
    /// arrival, less what the viewer says it held it). Once a ping has said how fast the path
    /// is, it reads the path (see the module's doc).
    pub fn came_back(&mut self, at: Instant) {
        let Some((left, bytes, before)) = self.flying.pop_front() else {
            return;
        };
        self.delivered += bytes;
        (self.acked, self.sent) = (at, left);
        if self.flying.is_empty() {
            // Everything is through, whatever the queue was taken to be.
            self.busy = self.busy.min(at);
        }
        let Some(fastest) = self.fastest else {
            return;
        };
        let late = at.saturating_duration_since(left + fastest);
        let queue = late.saturating_sub(self.time(bytes));
        let (counted, took, pace) = match before {
            // It had the path to itself: its own bytes, over its whole trip.
            Before::Nothing => (bytes, at.saturating_duration_since(left), Pace::Stream),
            Before::Frames {
                delivered,
                acked,
                sent,
            } => {
                let sending = left.saturating_duration_since(sent);
                let acking = at.saturating_duration_since(acked);
                let pace = match acking > sending {
                    true => Pace::Path,
                    false => Pace::Stream,
                };
                (self.delivered - delivered, sending.max(acking), pace)
            }
        };
        let says = scale(counted, LATENESS, took.max(LATENESS / 2));
        self.round.read = Some(match self.round.read {
            Some(r) if r.says >= says => Reading {
                queue: r.queue.min(queue),
                ..r
            },
            Some(r) => Reading {
                says,
                pace,
                counted,
                queue: r.queue.min(queue),
            },
            None => Reading {
                says,
                pace,
                counted,
                queue,
            },
        });
        let round = at.saturating_duration_since(self.round.began);
        if left >= self.round.began || round >= fastest + LATENESS {
            if let Some(read) = self.round.read {
                let to = self.verdict(read, round);
                let to = to.clamp(self.carries / FALL, self.carries.saturating_mul(FALL));
                self.moves(at, to.clamp(self.least, self.most));
            }
            self.round = Round::new(at);
        }
    }

    /// Where a round of `round` that read `read` moves the window.
    fn verdict(&self, read: Reading, round: Duration) -> usize {
        let Reading {
            says,
            pace,
            counted,
            queue,
        } = read;
        // The queue's distance from `LATENESS / 2`, over twice the round: as much again of
        // the window is what one round adds or takes away.
        let half = (LATENESS / 2).as_nanos() as i128;
        let step = |from: usize, short: i128| {
            let from = from as i128;
            (from + from * short / (2 * round.as_nanos().max(1) as i128)).max(0) as usize
        };
        let short = half - queue.as_nanos() as i128;
        // The window held the stream back, or a reading counted half of it: it was tested.
        let tested = match self.round.sent {
            Sent::Filled => true,
            Sent::Nothing | Sent::Short => counted.saturating_mul(2) >= self.carries,
        };
        match (tested, pace, short >= 0) {
            // The path set the pace and held every frame past `LATENESS / 2`: it carries
            // what it delivered, and the window gives up the queue's excess as well; but not
            // under `START` while the path delivered that much: nothing says it carries less.
            (true, Pace::Path, false) => step(says, short).max(says.min(START)),
            // The path kept up with all the window let through.
            (true, Pace::Path, true) => step(says.max(self.carries), short),
            // The stream set the pace: the path kept up whatever the frames' lateness says
            // (a path whose own delay rose looks queued until a ping knows it).
            (true, Pace::Stream, _) => step(says.max(self.carries), half),
            // The stream sent what it had: the path carries at least what it delivered.
            (false, Pace::Path | Pace::Stream, _) => says.max(self.carries),
        }
    }

    /// At `at` the window moves to `carries`. That changes how fast the queue drains, never
    /// how many bytes it holds: `busy` moves so that the queue at `at` is the same bytes at
    /// the new `carries` as at the old. Left where it was, a fall to half would count half
    /// the bytes really queued, and open room into the path just found slower.
    fn moves(&mut self, at: Instant, carries: usize) {
        let queued = self.model(at);
        self.carries = carries;
        self.lowest = self.lowest.min(carries);
        // Past `at` there was nothing queued, and still is not.
        self.busy = self.busy.min(at) + self.time(queued);
    }
}

/// The window against one oracle: a path of a rate and a round trip whose bottleneck serves
/// in order and stalls, and whose acks and pongs come back as late as a real page's did,
/// driven as the server drives a viewer's window from the moment the viewer connects.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::xorshift::Xorshift;

    const FRAME: Duration = Duration::from_micros(16_667);

    /// How late answers come back, past the path's own trip: as the page's acks in
    /// `acks-late-us.txt` did, in their order from anywhere among them, those without their
    /// slowest hundredth, or not at all.
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Late {
        Page,
        PageBelowTop,
        None,
    }

    /// The page's measured lates, in their order, and those under the 99th hundredth of them.
    fn measured() -> &'static (Vec<Duration>, Vec<Duration>) {
        static MEASURED: std::sync::OnceLock<(Vec<Duration>, Vec<Duration>)> =
            std::sync::OnceLock::new();
        MEASURED.get_or_init(|| {
            let all: Vec<Duration> = include_str!("acks-late-us.txt")
                .lines()
                .filter(|l| !l.starts_with('#'))
                .map(|l| Duration::from_micros(l.parse().unwrap()))
                .collect();
            let mut sorted = all.clone();
            sorted.sort();
            let top = sorted[sorted.len() * 99 / 100];
            let below = all.iter().copied().filter(|&l| l < top).collect();
            (all, below)
        })
    }

    /// What a ping weighs on the path: its JSON, a token of 16 hex digits.
    const PING: usize = 40;

    /// The oracle: what a viewer's path does with what the server sends it.
    struct Path {
        /// Bytes a second through its bottleneck, and its own round trip.
        rate: f64,
        trip: Duration,
        /// (from, to): its bottleneck carries nothing.
        stalls: Vec<(Instant, Instant)>,
        /// (from, to): the viewer holds its answers, and sends them all at `to`.
        held: Vec<(Instant, Instant)>,
        /// When its bottleneck has carried everything it was given.
        free: Instant,
        /// When the last answer on its way back comes back: answers come back in order.
        last: Instant,
        /// How late answers come back, and where among the measured lates the next is.
        late: Late,
        next: usize,
        rng: Xorshift,
    }

    impl Path {
        /// How late the next answer comes back.
        fn late(&mut self) -> Duration {
            let (all, below) = measured();
            let lates = match self.late {
                Late::Page => all,
                Late::PageBelowTop => below,
                Late::None => return Duration::ZERO,
            };
            self.next += 1;
            lates[self.next % lates.len()]
        }

        /// `bytes` given at `at`: when its bottleneck has carried them.
        fn through(&mut self, at: Instant, bytes: usize) -> Instant {
            let mut t = self.free.max(at);
            let mut left = Duration::from_secs_f64(bytes as f64 / self.rate);
            for &(a, b) in &self.stalls {
                if b <= t {
                    continue;
                }
                if a > t {
                    if a - t >= left {
                        break;
                    }
                    left -= a - t;
                }
                t = b;
            }
            self.free = t + left;
            self.free
        }

        /// What was through at `through`, answered `held` after it reached the viewer: when
        /// the answer comes back, late by a draw, behind every answer before it.
        fn answer(&mut self, through: Instant, held: Duration) -> Instant {
            let sent = through + self.trip / 2 + held;
            let sent = match self.held.iter().find(|h| h.0 <= sent && sent < h.1) {
                Some(&(_, to)) => to,
                None => sent,
            };
            self.last = self.last.max(sent + self.trip / 2 + self.late());
            self.last
        }

        /// What the path carries in `LATENESS`.
        fn carries(&self) -> usize {
            (self.rate * LATENESS.as_secs_f64()) as usize
        }
    }

    /// What the screen wants sent in a frame time.
    #[derive(Clone, Copy)]
    enum Want {
        Nothing,
        /// A frame of this many bytes, or less if its room is less.
        Frame(usize),
        /// A keyframe of this many bytes, or less if what the path carries in `KEY_TIME` is
        /// less: the capture loop's budget, whatever the room.
        Key(usize),
    }

    /// The server's side of one viewer, as video.rs and hub.rs drive its window, from the
    /// moment it connects: a ping then and every `PING_EVERY`, a frame a frame time as its
    /// room allows, and the acks and pongs the oracle brings back.
    struct Viewer {
        now: Instant,
        window: Window,
        path: Path,
        /// Frames on their way: when the ack of each comes back, and what the viewer says it
        /// held it.
        frames: VecDeque<(Instant, Duration)>,
        /// Pongs on their way: when each comes back, when its ping left, and what was on its
        /// way ahead of it then. The first may be later than the viewer's answers after it:
        /// it sent it late.
        pongs: VecDeque<(Instant, Instant, Ahead)>,
        ping: Instant,
        rng: Xorshift,
        /// Since `reset`: the bytes sent, and the longest a frame waited behind the queue.
        sent: usize,
        worst: Duration,
        /// The most `fastest` has been over the path's own trip, and the window over what
        /// the path carries (`carries` over it, less one), since `reset`.
        over: Duration,
        past: f64,
    }

    impl Viewer {
        /// A viewer connects at `t0` over `path`: the first ping goes first, its pong coming
        /// back `extra` later than the path brings it, then the hello and cursor (`ahead`
        /// bytes). It says it watches as its socket opens (the hub forgets what is on its
        /// way), and the capture loop sends it its keyframe at once (and forgets again).
        fn connect(t0: Instant, mut path: Path, ahead: usize, extra: Duration) -> Viewer {
            let mut window = Window::new(t0, 0, usize::MAX);
            let through = path.through(t0, PING);
            let pong = path.answer(through, Duration::ZERO) + extra;
            path.through(t0, ahead);
            let watch = t0 + path.trip + path.late();
            window.forget(watch);
            let rng = Xorshift::new(path.rng.next());
            Viewer {
                now: watch,
                window,
                frames: VecDeque::new(),
                pongs: VecDeque::from([(pong, t0, Ahead::Nothing)]),
                ping: t0 + crate::hub::PING_EVERY,
                rng,
                path,
                sent: 0,
                worst: Duration::ZERO,
                over: Duration::ZERO,
                past: 0.0,
            }
        }

        /// Forget what was measured.
        fn reset(&mut self) {
            self.sent = 0;
            self.worst = Duration::ZERO;
            self.over = Duration::ZERO;
            self.past = 0.0;
        }

        /// The next answer due by `by`, if any: its time, and whether it is a pong.
        fn next(&self, by: Instant) -> Option<(Instant, bool)> {
            let ack = self.frames.front().map(|&(at, _)| (at, false));
            let pong = self.pongs.iter().map(|&(at, _, _)| (at, true)).min();
            ack.into_iter().chain(pong).filter(|a| a.0 <= by).min()
        }

        /// One frame time: what came back in it comes back, pings go, and the frame the
        /// screen wants leaves.
        fn tick(&mut self, want: Want) {
            let end = self.now + FRAME;
            while self.ping <= end {
                let through = self.path.through(self.ping, PING);
                let pong = self.path.answer(through, Duration::ZERO);
                self.pongs.push_back((pong, self.ping, self.window.ahead()));
                self.ping += crate::hub::PING_EVERY;
            }
            while let Some((at, pong)) = self.next(end) {
                match pong {
                    true => {
                        let i = self.pongs.iter().position(|p| p.0 == at).unwrap();
                        let (_, left, ahead) = self.pongs.remove(i).unwrap();
                        self.window.pinged(at, at - left, ahead);
                    }
                    false => {
                        let (_, held) = self.frames.pop_front().unwrap();
                        self.window.came_back(at - held);
                    }
                }
            }
            self.now = end;
            if let Some(fastest) = self.window.fastest() {
                self.over = self.over.max(fastest.saturating_sub(self.path.trip));
            }
            let past = self.window.carries() as f64 / self.path.carries() as f64 - 1.0;
            self.past = self.past.max(past);
            let bytes = match want {
                Want::Nothing => return,
                Want::Key(bytes) => bytes.min(scale(
                    self.window.carries(),
                    crate::video::KEY_TIME,
                    LATENESS,
                )),
                Want::Frame(bytes) => match self.window.room(self.now) {
                    Some(room) => bytes.min(room),
                    None => return,
                },
            };
            let waits = self.path.free.saturating_duration_since(self.now);
            self.worst = self.worst.max(waits);
            self.window.leave(self.now, bytes);
            let through = self.path.through(self.now, bytes);
            let held = Duration::from_micros(self.rng.below(5_000));
            let ack = self.path.answer(through, held);
            self.frames.push_back((ack, held));
            self.sent += bytes;
        }
    }

    /// A path that carries `carries` bytes in `LATENESS`, a round trip of `trip`, from `t0`.
    fn path(t0: Instant, carries: f64, trip: Duration, late: Late, seed: u64) -> Path {
        let mut rng = Xorshift::new(seed);
        Path {
            rate: carries / LATENESS.as_secs_f64(),
            trip,
            stalls: Vec::new(),
            held: Vec::new(),
            free: t0,
            last: t0,
            late,
            next: rng.below(measured().0.len() as u64) as usize,
            rng,
        }
    }

    /// What a path from `START` to 1 Gbit/s carries in `LATENESS`, evenly spread in its
    /// logarithm.
    fn any_carries(rng: &mut Xorshift) -> f64 {
        let most = 1e9 / 8.0 * LATENESS.as_secs_f64();
        START as f64 * (most / START as f64).powf(rng.below(1001) as f64 / 1000.0)
    }

    /// The share `q` of a lateness model's draws come back no later than this.
    fn quantile(late: Late, q: f64) -> Duration {
        let (all, below) = measured();
        let lates = match late {
            Late::Page => all,
            Late::PageBelowTop => below,
            Late::None => return Duration::ZERO,
        };
        let mut sorted = lates.clone();
        sorted.sort();
        sorted[((sorted.len() - 1) as f64 * q) as usize]
    }

    /// The lateness models, one a case in turn.
    const LATES: [Late; 3] = [Late::None, Late::Page, Late::PageBelowTop];

    /// A cold load: a new viewer of a page of text on any path that carries from `START` to
    /// 1 Gbit/s, from 2 to 300 ms, its acks and pongs as late as one of the lateness models
    /// says, its connect ping ahead of a hello and cursor of up to 17 KB. Its keyframe (60 to
    /// 400 KB), the passes that fill the page in (frames of up to 20 KB as their room allows,
    /// for 1 to 10 s, some frame times with nothing), then 222 B frames at 60 a second, to a
    /// minute. Among the cases the first pong comes back late by up to 120 ms, or after the
    /// keyframe's ack, when nothing is read until a later frame.
    ///
    /// The window never falls under `START`, or under what the path carries if that is less,
    /// but by what a late ack can stretch a reading: an ack late by `δ`, all but the model's
    /// slowest hundredth, stretches a reading's time (of `LATENESS / 2` at least) by `δ`, so the
    /// bound is that times `(LATENESS / 2) / (LATENESS / 2 + δ)`.
    ///
    /// And no frame waits behind the queue longer than `3 * LATENESS`, plus `E`, the most the
    /// fastest ping's trip was over the path's own (a frame's queue is measured `E` short, so
    /// the window sees a queue only past `H = LATENESS / 2 + E`), plus what the window adds
    /// while `E` hides it. Over the path by `x` (its most `carries` over what the path
    /// carries, less one), the queue grows by `x` of every moment, for as long as it takes a
    /// whole round of frames to show it: two rounds at most, each the trip and the queue. So
    /// `wait <= 3 * LATENESS + E + 2 * (trip + wait) * x`, which bounds the wait only while
    /// `x < 1/2`; past that it says nothing. No bound from the constants alone closes it: a
    /// round grows the window by up to `LATENESS / (4 * round)` while the queue is hidden, and
    /// the `n`-th such round adds about `n * LATENESS / 4` of queue, so up to
    /// `sqrt(8 * H / LATENESS)` of them fit under `H`, and `x` may reach
    /// `(1 + LATENESS / (4 * trip))^n - 1`, over a half on short trips (on a 34 ms trip
    /// with `E` 43 ms, 3.3). So the property takes the `x` the window reached.
    #[test]
    fn a_cold_load_keeps_the_window_at_start_or_what_the_path_carries() {
        let mut rng = Xorshift::new(0x3C6E_F372_FE94_F82B);
        let ms = Duration::from_millis;
        let (mut late_pong, mut pong_after_key, mut over) = (0, 0, 0);
        for case in 0..201 {
            let t0 = Instant::now();
            let carries = any_carries(&mut rng);
            let trip = ms(2 + rng.below(299));
            let late = LATES[case % LATES.len()];
            let path = path(t0, carries, trip, late, rng.next());
            let extra = ms(rng.below(121));
            // In half the cases the first pong is held until after the keyframe's ack.
            let after_key = rng.below(2) == 1;
            let held = match after_key {
                false => extra,
                true => Duration::from_secs(3600),
            };
            let mut v = Viewer::connect(t0, path, rng.below(17_000) as usize, held);
            v.tick(Want::Key(60_000 + rng.below(340_000) as usize));
            let key = v.frames.back().map(|a| a.0);
            if let (Some(key), true) = (key, after_key) {
                let first = v.pongs.iter_mut().find(|p| p.1 == t0).unwrap();
                first.0 = key + extra;
            }
            match (v.pongs.iter().map(|p| p.0).min(), key) {
                (Some(pong), Some(key)) if pong > key => pong_after_key += 1,
                _ => late_pong += 1,
            }
            let fill = 60 + rng.below(540);
            let chunk = 1_000 + rng.below(19_000) as usize;
            for i in 0..3600 {
                let want = match i < fill {
                    true if rng.below(4) == 0 => Want::Nothing,
                    true => Want::Frame(chunk),
                    false => Want::Frame(222),
                };
                v.tick(want);
            }
            // An ack late by up to the model's 99th hundredth stretches a path-paced
            // reading's time, of `LATENESS / 2` at least, by that much: a fall may land that
            // much under what the path carries.
            let stretched =
                (LATENESS / 2).as_secs_f64() / (LATENESS / 2 + quantile(late, 0.99)).as_secs_f64();
            let floor = (START.min(v.path.carries()) as f64 * stretched) as usize;
            let what = format!(
                "case {case}: {carries:.0} B in LATENESS, {trip:?}, {late:?}, first pong {extra:?} \
                 late, after the keyframe's ack: {after_key}: window at its lowest {}, worst \
                 wait {:?}, fastest over the trip by {:?}",
                v.window.lowest(),
                v.worst,
                v.over
            );
            assert!(v.window.lowest() >= floor, "{what}");
            let lag = (trip + v.worst).mul_f64(2.0 * v.past);
            assert!(
                v.worst <= LATENESS * 3 + v.over + lag,
                "{what}, over it by {:.2}",
                v.past
            );
            over += usize::from(v.worst > LATENESS * 3 + v.over);
        }
        assert!(
            late_pong > 50 && pong_after_key > 50 && over > 0,
            "{late_pong} first pongs before the keyframe's ack, {pong_after_key} after, {over} \
             waits past 3 LATENESS and E"
        );
    }

    /// A move changes how fast the queue drains, never how many bytes it holds: from any
    /// window and any queue, to any other window, the bytes queued at the move are the same
    /// just after it as just before.
    #[test]
    fn a_move_keeps_the_bytes_queued() {
        let mut rng = Xorshift::new(0x1F83_D9AB_FB41_BD6B);
        for _ in 0..10_000 {
            let at = Instant::now();
            let mut w = Window::new(at, 0, usize::MAX);
            w.carries = PACKET + rng.below(8_000_000) as usize;
            w.busy = at + Duration::from_nanos(rng.below(2_000_000_000));
            let before = w.model(at);
            let to = PACKET + rng.below(8_000_000) as usize;
            w.moves(at, to);
            assert_eq!((w.carries, w.model(at)), (to, before));
        }
    }

    /// A round moves the window once, within the rule written here. A round ends at the
    /// first frame back that left after it began, or that came back a round trip and
    /// `LATENESS` after it began, once a ping has come back; every other ack leaves the window
    /// bit-equal. A round moves the window to no less than half of it and no more than twice
    /// it, and below it only if every frame back in it waited longer than `LATENESS / 2` past
    /// the fastest ping's trip and its own crossing; and the bytes queued are the same across
    /// a move. Over generated runs of pings and frames (frames that take all their room or any
    /// size), rises, falls and rounds that move nothing are all seen.
    #[test]
    fn a_round_moves_the_window_once_within_half_and_twice() {
        const MOST: usize = 7_500_000;
        let mut rng = Xorshift::new(0x5BE0_CD19_137E_2179);
        let ms = |n: u64| Duration::from_millis(n);
        let (mut rises, mut falls, mut held) = (0, 0, 0);
        for _ in 0..300 {
            let t0 = Instant::now();
            // Up to what 1 Gbit/s carries in `LATENESS`: the queue's bytes are kept across a
            // move to the byte under 60 MB.
            let mut w = Window::new(t0, 0, MOST);
            // Frames on their way: when each left, its bytes, and when its ack comes back.
            let mut flying = VecDeque::<(Instant, usize, Instant)>::new();
            let (mut now, mut acks, mut ping) = (t0, t0, t0 + ms(rng.below(2_000)));
            // The trip pings and acks take, plus up to 20 ms or up to 600 ms.
            let trip = 20 + rng.below(280);
            let jitter = [20, 600][rng.below(2) as usize];
            // The connect ping, with nothing on its way.
            w.pinged(t0, ms(trip + rng.below(jitter)), Ahead::Nothing);
            let gap = 1 + rng.below(40);
            // Whether frames take all their room, as motion's do, or any size up to 30 KB.
            let fill = rng.below(2) == 0;
            // The rule's own round: when it began, and whether every frame that came back in
            // it by its own ack waited past `LATENESS / 2`.
            let (mut began, mut queued) = (t0, true);
            for _ in 0..600 {
                now += ms(1 + rng.below(gap));
                if now >= ping {
                    let ahead = w.ahead();
                    w.pinged(now, ms(trip + rng.below(jitter)), ahead);
                    ping += crate::hub::PING_EVERY;
                }
                // The next ack: it names the oldest frame on its way.
                while let Some(&(left, bytes, at)) = flying.front().filter(|f| f.2 <= now) {
                    flying.pop_front();
                    let (carries, before) = (w.carries, w.model(at));
                    w.came_back(at);
                    let Some(fastest) = w.fastest else {
                        assert_eq!(w.carries, carries);
                        continue;
                    };
                    let late = at.saturating_duration_since(left + fastest);
                    let crossing = scale(bytes, LATENESS, Duration::from_nanos(carries as u64));
                    let wait = late.saturating_sub(Duration::from_nanos(crossing as u64));
                    queued &= wait > LATENESS / 2;
                    let ends = left >= began || at - began >= fastest + LATENESS;
                    if !ends {
                        assert_eq!(w.carries, carries);
                        continue;
                    }
                    let (low, high) = ((carries / FALL).max(PACKET), (carries * FALL).min(MOST));
                    assert!(
                        (low..=high).contains(&w.carries),
                        "{carries} to {}: {low} to {high}",
                        w.carries
                    );
                    assert!(w.carries >= carries || queued, "{carries} to {}", w.carries);
                    if !flying.is_empty() {
                        assert_eq!(w.model(at), before, "the queue across a move");
                    }
                    rises += usize::from(w.carries > carries);
                    falls += usize::from(w.carries < carries);
                    held += usize::from(w.carries == carries);
                    (began, queued) = (at, true);
                }
                let bytes = match (fill, w.room(now)) {
                    (true, Some(room)) => room,
                    (true, None) => continue,
                    (false, _) => 100 + rng.below(30_000) as usize,
                };
                w.leave(now, bytes);
                acks = acks.max(now + ms(trip + rng.below(jitter)));
                flying.push_back((now, bytes, acks));
            }
        }
        assert!(
            rises > 100 && falls > 100 && held > 100,
            "{rises} rises, {falls} falls, {held} rounds read and held"
        );
    }

    /// The path's fastest trip, as the rule written here says it: the least trip of the
    /// pings of the last `FASTEST_FOR` that left an empty path or came back faster than it,
    /// once two are there; until then, the least of those of them that came back. A ping
    /// that left behind frames before any had the path to itself says nothing. Over
    /// generated runs of pings, a fifth of them clean, pings come and go from the record,
    /// and it both rises and falls.
    #[test]
    fn the_fastest_trip_is_the_least_of_the_pings_that_had_the_path() {
        let mut rng = Xorshift::new(0x9B05_688C_2B3E_6C1F);
        let (mut rises, mut falls) = (0, 0);
        for _ in 0..200 {
            let t0 = Instant::now();
            let mut w = Window::new(t0, 0, usize::MAX);
            let mut pings: Vec<(Instant, Duration)> = Vec::new();
            let mut now = t0;
            for _ in 0..400 {
                now += Duration::from_millis(100 + rng.below(1_500));
                let trip = Duration::from_millis(20 + rng.below(400));
                let ahead = match rng.below(5) {
                    0 => Ahead::Nothing,
                    _ => Ahead::Frames,
                };
                let was = w.fastest;
                w.pinged(now, trip, ahead);
                if ahead == Ahead::Nothing || was.is_some_and(|f| trip < f) {
                    pings.push((now, trip));
                }
                pings.retain(|&(at, _)| now - at <= FASTEST_FOR);
                let least = pings.iter().map(|p| p.1).min();
                let want = match (pings.len() >= 2, was, least) {
                    (true, _, least) => least,
                    (false, was, Some(least)) => Some(was.map_or(least, |f| f.min(least))),
                    (false, was, None) => was,
                };
                assert_eq!(w.fastest, want);
                rises += usize::from(w.fastest > was && was.is_some());
                falls += usize::from(w.fastest < was);
            }
        }
        assert!(rises > 100 && falls > 100, "{rises} rises, {falls} falls");
    }

    /// What changes on a path under load.
    #[derive(Clone, Copy, Debug)]
    enum Change {
        /// Nothing: the load runs on from its cold start.
        None,
        /// Its bottleneck carries nothing for this long.
        Stall(Duration),
        /// Its rate becomes this many tenths of what it was.
        Rate(u64),
        /// Its own round trip grows by this much, for good.
        Trip(Duration),
        /// The viewer holds its answers this long, and then sends them all.
        Held(Duration),
    }

    /// Saturating demand from any state: a viewer that connects as production's do, on any
    /// path (0.2 to 100 Mbit/s, 2 to 300 ms), under motion that wants more than any path
    /// carries, its answers as late as one of the lateness models says; after 10 s one change
    /// (a stall of 0.5 to 3 s, a rate from a tenth to ten times, a round trip 40 to 300 ms
    /// longer, answers held 0.2 to 2 s), or none; then 30 s more, past any memory the window
    /// has. Over the last 30 s the path is used at least `LATENESS / (3 * LATENESS / 2 + δ)`, δ
    /// the lateness of the model's middle answer, and no frame waits behind the queue longer
    /// than `3 * LATENESS` and the lateness of all but the model's slowest hundredth.
    #[test]
    fn saturating_demand_from_any_state_uses_the_path() {
        let mut rng = Xorshift::new(0xA54F_F53A_5F1D_36F1);
        let ms = Duration::from_millis;
        let mut changes = [0; 5];
        for case in 0..201 {
            let t0 = Instant::now();
            let (slow, fast): (f64, f64) = (0.2e6 / 8.0, 100e6 / 8.0);
            let rate = slow * (fast / slow).powf(rng.below(1001) as f64 / 1000.0);
            let trip = ms(2 + rng.below(299));
            let late = LATES[case % LATES.len()];
            let path = path(t0, rate * LATENESS.as_secs_f64(), trip, late, rng.next());
            let mut v = Viewer::connect(t0, path, rng.below(17_000) as usize, Duration::ZERO);
            v.tick(Want::Key(usize::MAX));
            for _ in 0..600 {
                v.tick(Want::Frame(usize::MAX));
            }
            let at = v.now;
            let change = match rng.below(5) {
                0 => Change::None,
                1 => Change::Stall(ms(500 + rng.below(2501))),
                2 => Change::Rate(1 + rng.below(100)),
                3 => Change::Trip(ms(40 + rng.below(261))),
                _ => Change::Held(ms(200 + rng.below(1801))),
            };
            match change {
                Change::None => changes[0] += 1,
                Change::Stall(d) => {
                    v.path.stalls.push((at, at + d));
                    changes[1] += 1;
                }
                Change::Rate(tenths) => {
                    v.path.rate *= tenths as f64 / 10.0;
                    changes[2] += 1;
                }
                Change::Trip(d) => {
                    v.path.trip += d;
                    changes[3] += 1;
                }
                Change::Held(d) => {
                    v.path.held.push((at, at + d));
                    changes[4] += 1;
                }
            }
            for _ in 0..600 {
                v.tick(Want::Frame(usize::MAX));
            }
            // A path whose own delay rose is known for it only once pings have had it to
            // themselves and the trips of before are forgotten: the motion pauses once the
            // last ping that had the path to itself before the rise, at the start, is
            // `FASTEST_FOR` old. The window then grows back by at least
            // `1 + LATENESS / (4 * round)` a round, a round no longer than the trip and twice
            // `LATENESS`: it is given that many rounds to reach the path.
            if let Change::Trip(_) = change {
                while v.now < t0 + FASTEST_FOR + LATENESS {
                    v.tick(Want::Frame(usize::MAX));
                }
                for _ in 0..150 {
                    v.tick(Want::Nothing);
                }
                let round = (v.path.trip + LATENESS * 2).as_secs_f64();
                let grow = 1.0 + LATENESS.as_secs_f64() / (4.0 * round);
                let short = v.path.carries() as f64 / v.window.carries() as f64;
                let rounds = short.max(1.0).ln() / grow.ln();
                let until = v.now + Duration::from_secs_f64(rounds.ceil() * round);
                while v.now < until {
                    v.tick(Want::Frame(usize::MAX));
                }
            }
            for _ in 0..1200 {
                v.tick(Want::Frame(usize::MAX));
            }
            v.reset();
            let from = v.now;
            for _ in 0..1800 {
                v.tick(Want::Frame(usize::MAX));
            }
            let used = v.sent as f64 / (v.path.rate * (v.now - from).as_secs_f64());
            // The window lets the queue grow to `LATENESS / 2` past the fastest trip before
            // it gives way, learns of it a middle answer's lateness late, and gives way as far
            // under the path as it went over: at worst the path idles that long in every
            // `LATENESS` and that.
            let middle = quantile(late, 0.5);
            let least = LATENESS.as_secs_f64() / (LATENESS + LATENESS / 2 + middle).as_secs_f64();
            let what = format!(
                "case {case}: {:.0} B in LATENESS, {trip:?}, {late:?}, {change:?}: used \
                 {used:.2}, worst wait {:?}, window {} B",
                v.path.carries(),
                v.worst,
                v.window.carries()
            );
            assert!(used >= least, "{what}");
            assert!(v.worst <= LATENESS * 3 + quantile(late, 0.99), "{what}");
        }
        assert!(changes.iter().all(|&n| n > 25), "{changes:?}");
    }
}
