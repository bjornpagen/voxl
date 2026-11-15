//! When each frame passed each point between the screen changing and its messages
//! leaving: the last frames, kept in memory, read through `/stats/trace`.
//!
//! The encode loop records a frame once it is coded and about to be handed to the
//! viewers; a viewer's socket task adds when it wrote the frame. Nothing here allocates,
//! waits or writes anywhere but the ring, so recording costs the stream a few clock
//! readings and stores a frame.

use std::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

/// The frames kept: 68 s at 60 fps, so a scene of 10 to 30 s and what came before it can
/// be read once it is over, in 512 KB (128 bytes a frame). At half, a 30 s scene read at
/// its end loses its first seconds; at double, the ring takes 1 MB for scenes nobody
/// runs.
pub const FRAMES_KEPT: usize = 4096;

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

pub struct Timeline {
    start: Instant,
    slots: Box<[Slot]>,
    /// The number of the newest frame recorded.
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

    /// Frame number `frame` is handed to the viewers. Called by the encode loop alone,
    /// with frames numbered one after another.
    pub fn record(&self, frame: u32, f: &Frame) {
        let slot = self.slot(frame);
        let id = (frame as u64) << 2;
        slot.seq.store(id | WRITING, Ordering::Relaxed);
        fence(Ordering::Release);
        for (at, instant) in slot.at.iter().zip(f.instants()) {
            at.store(self.us(instant), Ordering::Relaxed);
        }
        slot.rows.store(f.captured.rows, Ordering::Relaxed);
        slot.bytes.store(f.bytes as u64, Ordering::Relaxed);
        slot.written.store((frame as u64) << 32, Ordering::Relaxed);
        slot.seq.store(id | DONE, Ordering::Release);
        self.newest.store(frame, Ordering::Release);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    struct Rng(u64);
    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }
    }

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
    /// exactly the model's newest frames.
    #[test]
    fn the_ring_returns_the_newest_frames_as_recorded() {
        let start = Instant::now();
        let ring = Timeline::new(start);
        let mut rng = Rng(0x2545F4914F6CDD1D);
        // The model: frame number -> (what was recorded, its write).
        let mut model: Vec<(u32, Recorded)> = vec![];
        let mut at = 0u64;
        let first = rng.below(u32::MAX as u64) as u32;
        for k in 0..3 * FRAMES_KEPT as u32 + 777 {
            let id = first.wrapping_add(k);
            at += rng.below(40_000);
            let gaps: [u64; MARKS.len() - 1] = std::array::from_fn(|_| rng.below(9_000));
            let (rows, bytes) = (rng.below(2000) as u32, rng.below(1 << 20) as usize);
            let f = frame(start, at, gaps, rows, bytes);
            ring.record(id, &f);
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
                for id in 1..=FRAMES {
                    ring.record(id, &said(id));
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
}
