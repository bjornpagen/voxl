//! When each frame passed each point between the screen changing and its messages
//! leaving: the last frames, kept in memory, read through `/stats/trace`.
//!
//! The encode loop records a frame once it is coded and about to be handed to the
//! viewers; a viewer's socket task adds when it wrote the frame. Nothing here allocates,
//! waits or writes anywhere but the ring, so recording costs the stream a few clock
//! readings and stores a frame.
//!
//! Each viewer also has a `Trace`: the frames it was sent and the acks that came back,
//! which is what its path did to them. The traces of the last viewers to leave are kept.

use crate::hub::Hold;
use crate::window::Flight;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// The frames kept: 68 s at 60 fps, so a scene of 10 to 30 s and what came before it can
/// be read once it is over, in 512 KB (128 bytes a frame). At half, a 30 s scene read at
/// its end loses its first seconds; at double, the ring takes 1 MB for scenes nobody
/// runs. A viewer's trace keeps as many of its frames, 288 KB (72 bytes a frame): on a
/// slow link, which carried 214 frames in 48 s (4.5 a second), 15 minutes of it.
pub const FRAMES_KEPT: usize = 4096;

/// The viewers that have left whose traces are kept: the last 4. The stall that ends in a
/// reconnect is what a trace is most wanted for, and by the time anyone reads it the viewer
/// that stalled has gone. Asked for in review, unmeasured. At 288 KB a trace, 1.2 MB at
/// most. At half, two short connections of a run of reconnects push out the stall that
/// began it; at double, 2.3 MB kept for connections nobody reads.
pub const LEFT_KEPT: usize = 4;

/// The points a frame passes up to its hand-off to the viewers, in order, as `/stats`
/// names them. A viewer's socket task writing the frame comes after.
pub const MARKS: [&str; 12] = [
    "damage",
    "capture",
    "grabbed",
    "converted",
    "queued",
    "taken",
    "dispatched",
    "woke",
    "x264",
    "encoded",
    "collected",
    "sent",
];

/// The capture loop's part of a frame.
#[derive(Clone, Copy, Debug)]
pub struct CaptureMarks {
    /// The capture loop saw the first damage the frame carries; its capture, if it
    /// carries none (a sharpening pass, a keyframe of a still screen).
    pub damage: Instant,
    /// Every wait was over and the capture began.
    pub capture: Instant,
    /// The changed rows were grabbed.
    pub grabbed: Instant,
    /// They were converted into the frame's buffer.
    pub converted: Instant,
    /// The frame was handed to the encode loop.
    pub queued: Instant,
    /// How many rows were converted.
    pub rows: u32,
}

#[cfg(test)]
impl CaptureMarks {
    /// A frame captured at `at`, nothing converted, with no wait anywhere.
    pub fn at(at: Instant) -> CaptureMarks {
        CaptureMarks {
            damage: at,
            capture: at,
            grabbed: at,
            converted: at,
            queued: at,
            rows: 0,
        }
    }
}

/// A stripe worker's part of a frame.
#[derive(Clone, Copy, Debug)]
pub struct Coded {
    /// The worker took the stripe.
    pub woke: Instant,
    /// It handed the picture to x264.
    pub x264: Instant,
    /// x264 returned the stripe's bytes.
    pub encoded: Instant,
}

impl Coded {
    /// Of two stripes of a frame, the one coded last: the frame waits for it.
    pub fn later(self, other: Coded) -> Coded {
        match other.encoded > self.encoded {
            true => other,
            false => self,
        }
    }
}

/// A frame as the encode loop hands it to the viewers.
#[derive(Clone, Copy, Debug)]
pub struct Frame {
    pub captured: CaptureMarks,
    /// The encode loop took the frame.
    pub taken: Instant,
    /// It began handing the frame's stripes to their workers.
    pub dispatched: Instant,
    /// The stripe coded last.
    pub coded: Coded,
    /// The encode loop had every stripe back.
    pub collected: Instant,
    /// It handed the frame's messages to the viewers' socket tasks.
    pub sent: Instant,
    /// What the frame's messages hold.
    pub bytes: usize,
}

impl Frame {
    /// Its instants, in the order of `MARKS`.
    fn instants(&self) -> [Instant; MARKS.len()] {
        let (c, d) = (self.captured, self.coded);
        [
            c.damage,
            c.capture,
            c.grabbed,
            c.converted,
            c.queued,
            self.taken,
            self.dispatched,
            d.woke,
            d.x264,
            d.encoded,
            self.collected,
            self.sent,
        ]
    }
}

/// A slot's state, in the low bits of its `seq`, beside the frame's number.
const EMPTY: u64 = 0;
const WRITING: u64 = 1;
const DONE: u64 = 2;

/// One frame's record. The encode loop is its one writer; a reader copies it and keeps
/// the copy only if `seq` said the same frame, done, before and after.
struct Slot {
    /// The frame's number << 2 | its state.
    seq: AtomicU64,
    rows: AtomicU32,
    bytes: AtomicU64,
    /// Microseconds from the ring's start to each of `MARKS`.
    at: [AtomicU64; MARKS.len()],
    /// The frame's number << 32 | microseconds from its damage until the last of its
    /// messages was written to the last socket: 0 until one was. A socket task changes it
    /// only while the number is its frame's, so one that writes a frame long gone leaves
    /// the slot's next frame alone.
    written: AtomicU64,
}

/// A frame read from the ring.
#[derive(Debug, PartialEq, serde::Serialize)]
pub struct Recorded {
    pub frame: u32,
    /// Microseconds from the ring's start to the frame's damage.
    pub damage_us: u64,
    /// Microseconds from its damage to each later mark (`MARKS` but the first).
    pub after_us: [u64; MARKS.len() - 1],
    /// Microseconds from its damage until the last of its messages was written to the
    /// last viewer's socket, once one was.
    pub written_us: Option<u64>,
    pub rows: u32,
    pub bytes: u64,
}

/// A frame's number: the server's one count of the frames it has sent, which goes on from
/// stream to stream and never starts again. Only `Timeline::number` makes one, so every
/// frame a viewer is sent and every frame the timeline records has a number no other
/// frame had (until the count wraps, after 2^32 frames: 2.3 years at 60 a second).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameId(u32);

impl FrameId {
    /// As a video message's header carries it, and as a viewer's ack names it.
    pub fn get(self) -> u32 {
        self.0
    }
}

pub struct Timeline {
    start: Instant,
    slots: Box<[Slot]>,
    /// The number of the newest frame numbered: the one count of frames, kept with the
    /// hub for as long as voxl runs.
    newest: AtomicU32,
}

impl Timeline {
    pub fn new(start: Instant) -> Self {
        Timeline {
            start,
            slots: (0..FRAMES_KEPT)
                .map(|_| Slot {
                    seq: AtomicU64::new(EMPTY),
                    rows: AtomicU32::new(0),
                    bytes: AtomicU64::new(0),
                    at: std::array::from_fn(|_| AtomicU64::new(0)),
                    written: AtomicU64::new(0),
                })
                .collect(),
            newest: AtomicU32::new(0),
        }
    }

    /// Microseconds from the ring's start, which is voxl's (the hub's making), to `at`: the
    /// one clock `/stats` and `/stats/trace` give every time on, monotonic as `Instant` is.
    pub fn us(&self, at: Instant) -> u64 {
        at.saturating_duration_since(self.start).as_micros() as u64
    }

    fn slot(&self, frame: u32) -> &Slot {
        &self.slots[frame as usize % FRAMES_KEPT]
    }

    /// The next frame's number. A stream that starts (a viewer joins after nobody watched,
    /// the screen is resized) takes up the count where the last stream left it, so an ack
    /// for a frame of an earlier stream names no frame of a later one.
    pub fn number(&self) -> FrameId {
        FrameId(self.newest.fetch_add(1, Ordering::AcqRel).wrapping_add(1))
    }

    /// Frame `id` is handed to the viewers. Called by the encode loop alone.
    pub fn record(&self, id: FrameId, f: &Frame) {
        let frame = id.get();
        let slot = self.slot(frame);
        let seq = (frame as u64) << 2;
        slot.seq.store(seq | WRITING, Ordering::Relaxed);
        fence(Ordering::Release);
        for (at, instant) in slot.at.iter().zip(f.instants()) {
            at.store(self.us(instant), Ordering::Relaxed);
        }
        slot.rows.store(f.captured.rows, Ordering::Relaxed);
        slot.bytes.store(f.bytes as u64, Ordering::Relaxed);
        slot.written.store((frame as u64) << 32, Ordering::Relaxed);
        slot.seq.store(seq | DONE, Ordering::Release);
    }

    /// A viewer's socket task wrote a message of frame `frame` to its socket at `now`.
    pub fn written(&self, frame: u32, now: Instant) {
        let slot = self.slot(frame);
        if slot.seq.load(Ordering::Acquire) != (frame as u64) << 2 | DONE {
            return;
        }
        let damage = slot.at[0].load(Ordering::Relaxed);
        // Written more than 71 minutes after its damage: recorded as never.
        let Ok(after) = u32::try_from(self.us(now).saturating_sub(damage)) else {
            return;
        };
        let mut was = slot.written.load(Ordering::Relaxed);
        while (was >> 32) as u32 == frame && (was as u32) < after {
            let now = (frame as u64) << 32 | after as u64;
            match slot
                .written
                .compare_exchange_weak(was, now, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return,
                Err(current) => was = current,
            }
        }
    }

    /// The newest `n` frames still in the ring, oldest first. A frame being recorded as
    /// it is read is left out.
    pub fn newest(&self, n: usize) -> Vec<Recorded> {
        let newest = self.newest.load(Ordering::Acquire);
        let n = n.min(FRAMES_KEPT) as u32;
        (0..n)
            .rev()
            .filter_map(|back| self.read(newest.wrapping_sub(back)))
            .collect()
    }

    fn read(&self, frame: u32) -> Option<Recorded> {
        let slot = self.slot(frame);
        let seq = slot.seq.load(Ordering::Acquire);
        if seq != (frame as u64) << 2 | DONE {
            return None;
        }
        let at: [u64; MARKS.len()] = std::array::from_fn(|i| slot.at[i].load(Ordering::Relaxed));
        let (rows, bytes) = (
            slot.rows.load(Ordering::Relaxed),
            slot.bytes.load(Ordering::Relaxed),
        );
        let written = slot.written.load(Ordering::Relaxed);
        fence(Ordering::Acquire);
        if slot.seq.load(Ordering::Relaxed) != seq {
            return None;
        }
        let damage_us = at[0];
        let written_us = ((written >> 32) as u32 == frame && written as u32 != 0)
            .then_some(written as u32 as u64);
        Some(Recorded {
            frame,
            damage_us,
            after_us: std::array::from_fn(|i| at[i + 1] - damage_us),
            written_us,
            rows,
            bytes,
        })
    }
}

/// A frame as the encode loop sent it to one viewer, and its ack.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sent {
    /// What the window keeps of it while it is on its way: its number, its bytes, and when
    /// the encode loop handed it to the viewer's socket task.
    pub frame: Flight,
    /// Every stripe of it is a keyframe, as `/stats` counts keyframes.
    pub key: bool,
    pub ack: Option<Ack>,
}

/// A frame's ack.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ack {
    /// When it arrived.
    pub at: Instant,
    /// What the viewer said it held the frame.
    pub hold: Hold,
}

/// The last `FRAMES_KEPT` frames sent to one viewer, and their acks. Made with room for
/// all of them when the viewer joins, so recording allocates nothing; it goes when the
/// viewer does.
#[derive(Clone)]
pub struct Trace {
    /// The frame sent `k`-th is at `k % FRAMES_KEPT`.
    ring: Vec<Sent>,
    /// How many frames the viewer has been sent.
    sent: u64,
    /// How many of them, from the first, have been acked.
    acked: u64,
}

impl Trace {
    pub fn with_room() -> Trace {
        Trace {
            ring: Vec::with_capacity(FRAMES_KEPT),
            sent: 0,
            acked: 0,
        }
    }

    fn slot(k: u64) -> usize {
        (k % FRAMES_KEPT as u64) as usize
    }

    /// The encode loop handed `frame` to the viewer's socket task; `key`: every stripe of it
    /// is a keyframe.
    pub fn sent(&mut self, frame: Flight, key: bool) {
        let sent = Sent {
            frame,
            key,
            ack: None,
        };
        match self.ring.len() < FRAMES_KEPT {
            true => self.ring.push(sent),
            false => self.ring[Self::slot(self.sent)] = sent,
        }
        self.sent += 1;
    }

    /// At `at`, the ack of the oldest frame sent and not yet acked arrived, with `hold`. The
    /// hub tells it so as the frame comes back to the window, which holds what the trace
    /// was sent and not yet told acked, in the same order: so the ack is that frame's. It is
    /// marked if the ring still keeps it.
    pub fn acked(&mut self, at: Instant, hold: Hold) {
        let k = self.acked;
        self.acked += 1;
        if (self.sent - self.ring.len() as u64..self.sent).contains(&k) {
            self.ring[Self::slot(k)].ack = Some(Ack { at, hold });
        }
    }

    /// The frames kept, oldest first.
    pub fn frames(&self) -> impl Iterator<Item = &Sent> {
        // Until the ring is full, the next slot is its length: nothing is older.
        let (newer, older) = self.ring.split_at(Self::slot(self.sent));
        older.iter().chain(newer)
    }
}

/// A viewer that has left: when, and its trace, which may yet take a frame the encode loop
/// was handing out as it left.
pub struct Leaver {
    pub id: u64,
    pub at: Instant,
    pub trace: Arc<Mutex<Trace>>,
}

/// The last `LEFT_KEPT` viewers to leave, oldest first. Made with the hub with room for all
/// of them, so a viewer leaving allocates nothing: its trace moves in, and the oldest out.
pub struct Leavers(VecDeque<Leaver>);

impl Leavers {
    pub fn with_room() -> Leavers {
        Leavers(VecDeque::with_capacity(LEFT_KEPT))
    }

    pub fn left(&mut self, leaver: Leaver) {
        if self.0.len() == LEFT_KEPT {
            self.0.pop_front();
        }
        self.0.push_back(leaver);
    }

    pub fn iter(&self) -> impl Iterator<Item = &Leaver> {
        self.0.iter()
    }
}

/// A frame sent to a viewer as `/stats/trace` gives it, its times on the timeline's clock.
#[derive(Debug, serde::Serialize)]
pub struct SentRow {
    frame: u32,
    sent_us: u64,
    bytes: usize,
    key: bool,
    ack: Option<AckRow>,
}

#[derive(Debug, serde::Serialize)]
pub struct AckRow {
    at_us: u64,
    hold: HoldRow,
}

/// The hold an ack reported for a frame, as `/stats/trace` gives it.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HoldRow {
    /// The ack named the frame and said it held it this many microseconds.
    SaidUs(u128),
    /// It named the frame and said nothing of a hold (an older viewer, or the recorder).
    Unsaid,
    /// It named the frame with a hold that is not a time.
    Unreadable,
}

impl Timeline {
    /// A viewer's frames as `/stats/trace` gives them, oldest first.
    pub fn rows(&self, trace: &Trace) -> Vec<SentRow> {
        let row = |s: &Sent| SentRow {
            frame: s.frame.id.get(),
            sent_us: self.us(s.frame.left),
            bytes: s.frame.bytes,
            key: s.key,
            ack: s.ack.map(|a| AckRow {
                at_us: self.us(a.at),
                hold: match a.hold {
                    Hold::Said(held) => HoldRow::SaidUs(held.as_micros()),
                    Hold::Unsaid => HoldRow::Unsaid,
                    Hold::Unreadable => HoldRow::Unreadable,
                },
            }),
        };
        trace.frames().map(row).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xorshift::Xorshift;
    use std::time::Duration;

    /// A frame whose marks follow its damage at `start + at`, `gaps` apart.
    fn frame(
        start: Instant,
        at: u64,
        gaps: [u64; MARKS.len() - 1],
        rows: u32,
        bytes: usize,
    ) -> Frame {
        let mut t = start + Duration::from_micros(at);
        let mut next = gaps.into_iter().map(|g| {
            t += Duration::from_micros(g);
            t
        });
        let damage = start + Duration::from_micros(at);
        let mut n = || next.next().unwrap();
        Frame {
            captured: CaptureMarks {
                damage,
                capture: n(),
                grabbed: n(),
                converted: n(),
                queued: n(),
                rows,
            },
            taken: n(),
            dispatched: n(),
            coded: Coded {
                woke: n(),
                x264: n(),
                encoded: n(),
            },
            collected: n(),
            sent: n(),
            bytes,
        }
    }

    /// What the ring is asked to remember, and what it should say: the model is every
    /// frame recorded, with the latest write each one had while it was the newest frame
    /// of its number in the ring and within 2^32 us of its damage. Over a long run of
    /// frames, writes from viewers (the newest frames, older ones, frames long gone and
    /// frames not yet recorded, in any order) and reads of any length, the ring returns
    /// exactly the model's newest frames. The count starts within a ring's length of its
    /// wrap at 2^32, so the run records, writes and reads across it, and the crossing is
    /// counted.
    #[test]
    fn the_ring_returns_the_newest_frames_as_recorded() {
        let start = Instant::now();
        let ring = Timeline::new(start);
        let mut rng = Xorshift::new(0x2545F4914F6CDD1D);
        // The model: frame number -> (what was recorded, its write).
        let mut model: Vec<(u32, Recorded)> = vec![];
        let mut at = 0u64;
        // The count a ring's length or less short of its wrap.
        let first = 0u32.wrapping_sub(1 + rng.below(FRAMES_KEPT as u64) as u32);
        ring.newest.store(first.wrapping_sub(1), Ordering::Relaxed);
        let mut crossed = 0;
        for k in 0..3 * FRAMES_KEPT as u32 + 777 {
            let number = ring.number();
            let id = number.get();
            assert_eq!(
                id,
                first.wrapping_add(k),
                "frames are numbered one after another"
            );
            crossed += u32::from(id == 0);
            at += rng.below(40_000);
            let gaps: [u64; MARKS.len() - 1] = std::array::from_fn(|_| rng.below(9_000));
            let (rows, bytes) = (rng.below(2000) as u32, rng.below(1 << 20) as usize);
            let f = frame(start, at, gaps, rows, bytes);
            ring.record(number, &f);
            let mut after = [0; MARKS.len() - 1];
            let mut sum = 0;
            for (a, g) in after.iter_mut().zip(gaps) {
                sum += g;
                *a = sum;
            }
            model.push((
                id,
                Recorded {
                    frame: id,
                    damage_us: at,
                    after_us: after,
                    written_us: None,
                    rows,
                    bytes: bytes as u64,
                },
            ));
            for _ in 0..rng.below(4) {
                // A frame among the newest, an older one, one long gone, or one not yet
                // recorded; written soon after its damage, or 72 minutes after it.
                let back = match rng.below(4) {
                    0 => rng.below(4),
                    1 => rng.below(FRAMES_KEPT as u64),
                    2 => FRAMES_KEPT as u64 + rng.below(1000),
                    _ => 0,
                };
                let ahead = rng.below(4) == 0;
                let target = match ahead {
                    true => id.wrapping_add(1 + rng.below(9) as u32),
                    false => id.wrapping_sub(back as u32),
                };
                let late = match rng.below(50) {
                    0 => 72 * 60 * 1_000_000,
                    _ => rng.below(200_000),
                };
                let Some(i) = model.iter().rposition(|(m, _)| *m == target) else {
                    ring.written(target, start + Duration::from_micros(at + late));
                    continue;
                };
                let damage = model[i].1.damage_us;
                let now = damage + late;
                ring.written(target, start + Duration::from_micros(now));
                let in_ring = model.len() - i <= FRAMES_KEPT;
                let r = &mut model[i].1;
                if in_ring && late < 1 << 32 && late > 0 {
                    r.written_us = Some(r.written_us.map_or(late, |w| w.max(late)));
                }
            }
            if rng.below(97) == 0 || k == 3 * FRAMES_KEPT as u32 + 776 {
                let n = rng.below(2 * FRAMES_KEPT as u64) as usize;
                let want: Vec<&Recorded> = model
                    .iter()
                    .rev()
                    .take(n.min(FRAMES_KEPT))
                    .rev()
                    .map(|(_, r)| r)
                    .collect();
                let got = ring.newest(n);
                assert_eq!(got.len(), want.len(), "the newest {n} after frame {id}");
                for (g, w) in got.iter().zip(want) {
                    assert_eq!(g, w);
                }
            }
        }
        assert_eq!(crossed, 1, "the count crossed its wrap");
    }

    /// A reader never takes a frame that is being recorded as it reads: every frame it
    /// returns is one frame's, whole. The writer records frames whose every field says
    /// which frame it is, viewers write them as they go, and a reader reads the whole
    /// ring all the while, so it reads slots as they are being written over.
    #[test]
    fn a_reader_never_sees_a_frame_half_written() {
        const FRAMES: u32 = 200_000;
        let start = Instant::now();
        let ring = Timeline::new(start);
        let said = |id: u32| {
            frame(
                start,
                id as u64 * 16,
                std::array::from_fn(|i| (id as u64 + i as u64) % 7),
                id,
                id as usize * 3,
            )
        };
        let done = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(|| {
                for _ in 0..FRAMES {
                    let number = ring.number();
                    let id = number.get();
                    ring.record(number, &said(id));
                    ring.written(id, start + Duration::from_micros(id as u64 * 16 + 99));
                }
                done.store(true, Ordering::Release);
            });
            s.spawn(|| {
                let mut seen = 0;
                for pass in 0.. {
                    if done.load(Ordering::Acquire) {
                        break;
                    }
                    // Mostly the oldest frames, whose slots the writer takes next; now and
                    // then the whole ring.
                    let newest = ring.newest.load(Ordering::Acquire);
                    let oldest = (0..4)
                        .filter_map(|k| ring.read(newest.wrapping_sub(FRAMES_KEPT as u32 - 1 - k)));
                    let all = match pass % 1000 {
                        0 => ring.newest(FRAMES_KEPT),
                        _ => vec![],
                    };
                    for r in oldest.chain(all) {
                        let want = ring_view(&said(r.frame), start);
                        assert_eq!((r.damage_us, r.after_us), want, "frame {}", r.frame);
                        assert_eq!((r.rows, r.bytes), (r.frame, r.frame as u64 * 3));
                        assert!(r.written_us.is_none_or(|w| w == 99), "frame {}", r.frame);
                        seen += 1;
                    }
                }
                assert!(seen > 0, "the reader read nothing");
            });
        });
    }

    fn ring_view(f: &Frame, start: Instant) -> (u64, [u64; MARKS.len() - 1]) {
        let at = f.instants().map(|i| (i - start).as_micros() as u64);
        (at[0], std::array::from_fn(|i| at[i + 1] - at[0]))
    }

    /// A viewer as the test sees it: the client the hub made for it, every frame it was
    /// ever sent with its ack, how many of them have been acked, and the step from which its
    /// acks get through again.
    struct Watcher {
        client: std::sync::Arc<crate::hub::Client>,
        sent: Vec<Sent>,
        acked: usize,
        heard_from: u32,
    }

    /// The rows `/stats/trace` gives for frames as the model has them, written apart from
    /// `Timeline::rows`.
    fn model_rows(timeline: &Timeline, kept: &[Sent]) -> serde_json::Value {
        let us = |at| timeline.us(at);
        kept.iter()
            .map(|s| {
                let ack = s.ack.map(|a| {
                    let hold = match a.hold {
                        Hold::Said(held) => {
                            serde_json::json!({"said_us": held.as_micros() as u64})
                        }
                        Hold::Unsaid => "unsaid".into(),
                        Hold::Unreadable => "unreadable".into(),
                    };
                    serde_json::json!({"at_us": us(a.at), "hold": hold})
                });
                serde_json::json!({
                    "frame": s.frame.id.get(),
                    "sent_us": us(s.frame.left),
                    "bytes": s.frame.bytes,
                    "key": s.key,
                    "ack": ack,
                })
            })
            .collect()
    }

    /// Each viewer's trace holds exactly the last `FRAMES_KEPT` frames it was sent, oldest
    /// first, each with its ack as the model says: the k-th ack is the k-th frame's, as the
    /// hub tells the trace of each frame the window takes back; the hub lists exactly the
    /// viewers present, and the last `LEFT_KEPT` to leave with when each left, their traces
    /// as they stood; and their rows in `/stats/trace` say the same. Generated over several
    /// laps of the ring: viewers join and leave (one now and then after laps of its ring), a
    /// frame goes to a viewer as it leaves, a viewer gone already leaves again, a viewer is
    /// sent most frames but not every one, and goes deaf for spells long enough that frames
    /// leave its ring before their acks come. Frames are numbered by the hub's timeline;
    /// acks come with a hold said, unsaid or unreadable. The run must reach every one of
    /// those, and a leaver pushed out by later ones.
    #[test]
    fn each_viewer_keeps_its_last_frames_and_their_acks() {
        const STEPS: u32 = 64_000;
        let hub = crate::hub::Hub::new(crate::hub::Paths::ANY);
        let mut rng = Xorshift::new(0x94D0_49BB_1331_11EB);
        let mut at = Instant::now();
        let mut watchers: Vec<Watcher> = vec![];
        // The last to leave, with when each left and at which step, oldest first.
        let mut leavers: VecDeque<(Watcher, Instant, u32)> = VecDeque::new();
        // How often each situation the run must reach came up.
        let (mut left, mut marked, mut gone_before) = (0usize, 0, 0);
        let (mut late, mut again, mut longest_left, mut fell_open) = (0, 0, 0, 0);
        // Frames that left a viewer's ring with no ack.
        let open_fallen = |w: &Watcher| {
            let gone = &w.sent[..w.sent.len().saturating_sub(FRAMES_KEPT)];
            gone.iter().filter(|s| s.ack.is_none()).count()
        };
        for step in 0..STEPS {
            at += Duration::from_micros(rng.below(30_000));
            match rng.below(10_000) {
                0..=29 if watchers.len() < 4 => watchers.push(Watcher {
                    client: hub.add_client(tokio::sync::mpsc::unbounded_channel().0, at),
                    sent: vec![],
                    acked: 0,
                    heard_from: step,
                }),
                // The first stays a long while (a leave in 20,000 steps); the others come
                // and go.
                30..=60 if watchers.len() > 1 || rng.below(2) == 0 => {
                    let w = match rng.below(62) {
                        0 => 0,
                        _ if watchers.len() > 1 => 1 + rng.below(watchers.len() as u64 - 1),
                        _ => continue,
                    };
                    let w = watchers.remove(w as usize);
                    hub.remove_client(w.client.id, at);
                    longest_left = longest_left.max(w.sent.len());
                    if leavers.len() == LEFT_KEPT {
                        if let Some((w, _, _)) = leavers.pop_front() {
                            fell_open += open_fallen(&w);
                        }
                    }
                    leavers.push_back((w, at, step));
                    left += 1;
                }
                // One that left already leaves again, and leaves nothing more.
                61..=62 if !leavers.is_empty() => {
                    let w = rng.below(leavers.len() as u64) as usize;
                    hub.remove_client(leavers[w].0.client.id, at);
                    again += 1;
                }
                63..=72 if !watchers.is_empty() => {
                    let w = rng.below(watchers.len() as u64) as usize;
                    let w = &mut watchers[w];
                    // A short spell, or one long enough that more than a ring of frames
                    // goes out before its acks come again.
                    let spell = match rng.below(8) {
                        0 => 9_000 + rng.below(3_000),
                        _ => rng.below(1_000),
                    };
                    w.heard_from = w.heard_from.max(step + spell as u32);
                }
                73..=5972 => {
                    let number = hub.timeline.number();
                    let (bytes, key) = (rng.below(1 << 20) as usize, rng.below(30) == 0);
                    for w in &mut watchers {
                        // One waiting for a keyframe is not sent the frames before it.
                        if rng.below(12) == 0 {
                            continue;
                        }
                        let frame = Flight {
                            id: number,
                            bytes,
                            left: at,
                        };
                        w.client.trace.lock().sent(frame, key);
                        w.sent.push(Sent {
                            frame,
                            key,
                            ack: None,
                        });
                    }
                    // The encode loop read the viewers just before the last one left.
                    if let Some((w, _, _)) = leavers.back_mut().filter(|l| l.2 + 2 >= step) {
                        let frame = Flight {
                            id: number,
                            bytes,
                            left: at,
                        };
                        w.client.trace.lock().sent(frame, key);
                        w.sent.push(Sent {
                            frame,
                            key,
                            ack: None,
                        });
                        late += 1;
                    }
                }
                5973.. if !watchers.is_empty() => {
                    let w = rng.below(watchers.len() as u64) as usize;
                    let w = &mut watchers[w];
                    if step < w.heard_from || w.acked == w.sent.len() {
                        continue;
                    }
                    // The acks that arrive together, as TCP brings them: up to 64, each the
                    // oldest frame's not yet acked.
                    for _ in 0..(1 + rng.below(64) as usize).min(w.sent.len() - w.acked) {
                        let hold = match rng.below(3) {
                            0 => Hold::Unsaid,
                            1 => Hold::Said(Duration::from_micros(rng.below(1 << 40))),
                            _ => Hold::Unreadable,
                        };
                        w.client.trace.lock().acked(at, hold);
                        let kept = w.acked + FRAMES_KEPT >= w.sent.len();
                        w.sent[w.acked].ack = Some(Ack { at, hold });
                        w.acked += 1;
                        marked += usize::from(kept);
                        gone_before += usize::from(!kept);
                    }
                }
                _ => {}
            }
            if rng.below(400) == 0 || step == STEPS - 1 {
                let rows = rng.below(8) == 0 || step == STEPS - 1;
                let traces = hub.traces();
                let ids: Vec<u64> = traces.iter().map(|(id, _)| *id).collect();
                let want: Vec<u64> = watchers.iter().map(|w| w.client.id).collect();
                assert_eq!(ids, want, "the viewers listed at step {step}");
                let gone = hub.left_traces();
                let ids: Vec<(u64, Instant)> = gone.iter().map(|(id, at, _)| (*id, *at)).collect();
                let want: Vec<(u64, Instant)> = leavers
                    .iter()
                    .map(|(w, at, _)| (w.client.id, *at))
                    .collect();
                assert_eq!(ids, want, "the leavers listed at step {step}");
                let all = traces.iter().map(|(id, trace)| (id, trace));
                let all = all.chain(gone.iter().map(|(id, _, trace)| (id, trace)));
                let models = watchers.iter().chain(leavers.iter().map(|l| &l.0));
                for ((id, trace), w) in all.zip(models) {
                    let kept = &w.sent[w.sent.len().saturating_sub(FRAMES_KEPT)..];
                    let got: Vec<&Sent> = trace.frames().collect();
                    assert_eq!(got.len(), kept.len(), "viewer {id} at step {step}");
                    if let Some(i) = (0..kept.len()).find(|&i| *got[i] != kept[i]) {
                        panic!(
                            "viewer {id} at step {step}, frame {i} of {}: {:?} against {:?}",
                            kept.len(),
                            got[i],
                            kept[i]
                        );
                    }
                    if rows {
                        let rows = serde_json::to_value(hub.timeline.rows(trace)).unwrap();
                        assert!(
                            rows == model_rows(&hub.timeline, kept),
                            "viewer {id}'s rows"
                        );
                    }
                }
            }
        }
        let everyone = || watchers.iter().chain(leavers.iter().map(|l| &l.0));
        let fell_open = fell_open + everyone().map(open_fallen).sum::<usize>();
        let reached = [
            ("a viewer left", left),
            (
                "a leaver was pushed out by later ones",
                left.saturating_sub(LEFT_KEPT),
            ),
            ("a frame went to a viewer as it left", late),
            ("a viewer gone already left again", again),
            (
                "a viewer left after a lap of its ring",
                longest_left.saturating_sub(FRAMES_KEPT),
            ),
            ("a frame left the ring with no ack", fell_open),
            ("an ack marked a frame the ring keeps", marked),
            (
                "an ack came for a frame that had left the ring",
                gone_before,
            ),
        ];
        for (what, n) in reached {
            assert!(n > 0, "never reached: {what}");
        }
        assert!(
            everyone().any(|w| w.sent.len() > 3 * FRAMES_KEPT) || longest_left > 3 * FRAMES_KEPT,
            "no viewer went three times round its ring"
        );
    }
}
