//! The oracle a viewer's path is tested against, by the window's tests and the capture
//! loop's: a path of a rate and a round trip whose bottleneck serves in order and stalls,
//! and whose answers come back in order and as late as a real page's did; and a page seated
//! on one, as the server sees it through its `Client`.

use crate::hub::{Acked, Client, Hold, Hub, Token, PING_EVERY};
use crate::timeline::FrameId;
use crate::window::LATENESS;
use crate::xorshift::Xorshift;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How late answers come back, past the path's own trip: as the page's acks in
/// `acks-late-us.txt` did, in their order from anywhere among them, those without their
/// slowest hundredth, or not at all.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Late {
    Page,
    PageBelowTop,
    None,
}

/// The page's measured lates, in their order, and those under the 99th hundredth of them.
pub(crate) fn measured() -> &'static (Vec<Duration>, Vec<Duration>) {
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
pub(crate) const PING: usize = 40;

/// The oracle: what a viewer's path does with what the server sends it.
pub(crate) struct Path {
    /// Bytes a second through its bottleneck, and its own round trip.
    pub(crate) rate: f64,
    pub(crate) trip: Duration,
    /// (from, to): its bottleneck carries nothing.
    pub(crate) stalls: Vec<(Instant, Instant)>,
    /// (from, to): the viewer holds its answers, and sends them all at `to`.
    pub(crate) held: Vec<(Instant, Instant)>,
    /// When its bottleneck has carried everything it was given.
    pub(crate) free: Instant,
    /// When the last answer on its way back comes back: answers come back in order.
    pub(crate) last: Instant,
    /// How late answers come back, and where among the measured lates the next is.
    pub(crate) late: Late,
    pub(crate) next: usize,
    pub(crate) rng: Xorshift,
}

impl Path {
    /// A path that carries `carries` bytes in `LATENESS`, a round trip of `trip`, from `t0`,
    /// its answers as late as `late` says, drawn from `seed`.
    pub(crate) fn new(t0: Instant, carries: f64, trip: Duration, late: Late, seed: u64) -> Path {
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

    /// How late the next answer comes back.
    pub(crate) fn late(&mut self) -> Duration {
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
    pub(crate) fn through(&mut self, at: Instant, bytes: usize) -> Instant {
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
    pub(crate) fn answer(&mut self, through: Instant, held: Duration) -> Instant {
        let sent = through + self.trip / 2 + held;
        let sent = match self.held.iter().find(|h| h.0 <= sent && sent < h.1) {
            Some(&(_, to)) => to,
            None => sent,
        };
        self.last = self.last.max(sent + self.trip / 2 + self.late());
        self.last
    }

    /// What the path carries in `LATENESS`.
    pub(crate) fn carries(&self) -> usize {
        (self.rate * LATENESS.as_secs_f64()) as usize
    }
}

/// What a page sends back on its path.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Up {
    /// The ack of frame `id`, with what the page says it held the frame.
    Ack(u32, Duration),
    /// The reply to the ping so named.
    Pong(Token),
    /// `kf`: a decoder broke.
    Keyframe,
}

/// What the page does with a frame that reaches it, as web/src/frames.ts has it: it acks
/// every frame once, in order, whatever became of its pictures.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Page {
    /// It holds it this long (decoding, drawing), then acks it.
    Holds(Duration),
    /// A decoder breaks on it, this long after it arrived: the page asks for a keyframe,
    /// then acks it.
    Breaks(Duration),
}

/// A page on a path, as the server sees it through its `Client`: the pings the server sends
/// it every `PING_EVERY` from its connect, the frames it is sent, and what comes back.
pub(crate) struct Seat {
    pub(crate) client: Arc<Client>,
    pub(crate) path: Path,
    /// What is on its way back, in the order it comes: when each reaches the server.
    up: VecDeque<(Instant, Up)>,
    /// When the next ping goes.
    ping: Instant,
}

impl Seat {
    /// A page connects to `hub` at `t0` over `path`, as the server greets it: its first
    /// ping goes first, then `ahead` bytes of hello and cursor, and it says it watches.
    pub(crate) fn connect(hub: &Hub, t0: Instant, path: Path, ahead: usize) -> Seat {
        let client = hub.add_client(tokio::sync::mpsc::unbounded_channel().0, t0);
        let mut seat = Seat {
            client,
            path,
            up: VecDeque::new(),
            ping: t0,
        };
        seat.pings(t0);
        seat.path.through(t0, ahead);
        seat.client.watch();
        seat
    }

    /// The pings due by `now` go, each behind what is queued on the path.
    pub(crate) fn pings(&mut self, now: Instant) {
        while self.ping <= now {
            let token = Token::random();
            self.client.ping(self.ping, token);
            let through = self.path.through(self.ping, PING);
            let back = self.path.answer(through, Duration::ZERO);
            self.up.push_back((back, Up::Pong(token)));
            self.ping += PING_EVERY;
        }
    }

    /// What reaches the server by `now` arrives there, in order: each ack names the oldest
    /// frame on its way (the page acks every frame once, in order), a pong is a round trip
    /// if it answers a ping still out, and `kf` asks for a keyframe. When the last `kf`
    /// arrived, if one did.
    pub(crate) fn hears(&mut self, now: Instant) -> Option<Instant> {
        let mut asked = None;
        while let Some(&(at, up)) = self.up.front().filter(|u| u.0 <= now) {
            self.up.pop_front();
            match up {
                Up::Ack(id, held) => {
                    let acked = self.client.on_ack(at, id, Hold(held));
                    assert_eq!(acked, Acked::Oldest, "the page's ack of frame {id}");
                }
                Up::Pong(token) => {
                    if let Some((trip, ahead)) = self.client.pings.lock().answer(at, token) {
                        self.client.pinged(at, trip, ahead);
                    }
                }
                Up::Keyframe => {
                    self.client.ask_keyframe();
                    asked = Some(at);
                }
            }
        }
        asked
    }

    /// The server sent frame `id`, of `bytes`, at `at`, and the page does what `page` says
    /// with it. When it reached the page.
    pub(crate) fn frame(&mut self, at: Instant, id: FrameId, bytes: usize, page: Page) -> Instant {
        let through = self.path.through(at, bytes);
        let held = match page {
            Page::Holds(held) => held,
            Page::Breaks(held) => {
                let back = self.path.answer(through, held);
                self.up.push_back((back, Up::Keyframe));
                held
            }
        };
        let back = self.path.answer(through, held);
        self.up.push_back((back, Up::Ack(id.get(), held)));
        through + self.path.trip / 2
    }

    /// The server sent `bytes` at `at` that the page does not answer (a close). When it
    /// reached the page.
    pub(crate) fn message(&mut self, at: Instant, bytes: usize) -> Instant {
        self.path.through(at, bytes) + self.path.trip / 2
    }

    /// When the next answer reaches the server, or the next ping goes, whichever is first.
    pub(crate) fn next(&self) -> Instant {
        self.up.front().map_or(self.ping, |u| u.0.min(self.ping))
    }
}
