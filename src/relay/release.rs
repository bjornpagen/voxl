//! What the page is given of the frames the decoder makes whole, and when.
//!
//! The screen is one H.264 stream, whose updates refer to earlier ones. When an update
//! misses its deadline the daemon is told (its report), and invalidates it in its encoder
//! (`x264_encoder_invalidate_reference`, which takes it and every later picture out of the
//! references); every update carries its epoch, the newest missed update the encoder had
//! invalidated when it coded it. So updates go to the page in their order, each once every
//! earlier one is decided: decoded and given to the page, or missed. One of an epoch older
//! than the newest miss before it may refer to that miss, or to an update dropped after
//! it, and is dropped too.
//!
//! A block that misses is reported by its frame numbers alone: the relay knows an update in
//! it missed once every frame before the next update it holds is decided, and that update's
//! number is past it.
//!
//! An Opus packet refers to nothing. It goes to the page at its deadline, in order: the
//! daemon's deadline is the moment by which the path's measured spread has delivered it,
//! so packets reach the page as evenly as they were sent, and the page's player needs no
//! jitter allowance of its own. The cost is that spread, on every packet: a packet waits
//! from its arrival to its deadline. One whole only after a later one went is dropped.

use crate::lanes::wire::{Epoch, Member, Number, Seq, Stamp, Track};
use std::collections::VecDeque;

/// What goes to the page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Give<'a> {
    /// An update: its 9-byte header and Annex-B.
    Video(&'a [u8]),
    /// An Opus packet.
    Audio(&'a [u8]),
}

/// A whole update held until every earlier one is decided.
struct Held {
    number: Number,
    frame: Seq,
    epoch: Epoch,
    bytes: Vec<u8>,
}

/// A whole Opus packet held until its deadline.
struct Sound {
    frame: Seq,
    due: Stamp,
    bytes: Vec<u8>,
}

/// The relay's order of release, for one session.
pub struct Release {
    /// The number of the next update the page may be given; every earlier one was decided.
    next: Number,
    /// The update just after the newest missed one: an update coded once its encoder had
    /// invalidated that miss has an epoch of at least the one before it. None: no miss yet.
    clean: Option<Number>,
    /// Whole updates at `next` or past it, by number.
    held: VecDeque<Held>,
    /// The oldest frame not yet decided, by its number.
    undecided: u64,
    /// The newest Opus packet given; none before the first.
    audio: Option<Seq>,
    /// Whole Opus packets after it, by number, until their deadlines.
    sounds: VecDeque<Sound>,
}

impl Release {
    pub fn new() -> Release {
        Release {
            next: Number::first(),
            clean: None,
            held: VecDeque::new(),
            undecided: 0,
            audio: None,
            sounds: VecDeque::new(),
        }
    }

    /// Whether an update coded in `epoch` refers to no missed update.
    fn passes(&self, epoch: Epoch) -> bool {
        match (self.clean, epoch) {
            (None, _) => true,
            (Some(_), Epoch::Start) => false,
            (Some(clean), Epoch::After(x)) => x.next() >= clean,
        }
    }

    /// Gives the page what is due, in order, while every frame before the next held update
    /// is decided.
    fn pump(&mut self, give: &mut impl FnMut(Give<'_>)) {
        while let Some(h) = self.held.front() {
            if h.number > self.next {
                // The updates between were not made whole: once every frame before this one
                // is decided, they are missed, the one just before it the newest.
                if self.undecided < h.frame.get() {
                    return;
                }
                self.clean = Some(h.number);
                self.next = h.number;
            }
            let h = self.held.pop_front().expect("the front, just read");
            if self.passes(h.epoch) {
                give(Give::Video(&h.bytes));
            }
            self.next = h.number.next();
        }
    }

    /// A frame the decoder made whole (a member of a whole block due at `due`): given now,
    /// held, or dropped.
    pub fn whole(&mut self, member: &Member<'_>, due: Stamp, give: &mut impl FnMut(Give<'_>)) {
        let bytes = member.bytes;
        match member.track {
            Track::Audio => {
                if self.audio.is_some_and(|a| a >= member.frame) {
                    return;
                }
                let at = self.sounds.partition_point(|s| s.frame < member.frame);
                self.sounds.insert(
                    at,
                    Sound {
                        frame: member.frame,
                        due,
                        bytes: bytes.to_vec(),
                    },
                );
            }
            Track::Video { number, epoch } => {
                // One already decided missed comes too late for the page.
                if number < self.next {
                    return;
                }
                let at = self.held.partition_point(|h| h.number < number);
                self.held.insert(
                    at,
                    Held {
                        number,
                        frame: member.frame,
                        epoch,
                        bytes: bytes.to_vec(),
                    },
                );
                self.pump(give);
            }
        }
    }

    /// Gives the page the Opus packets whose deadlines are past at `now`, in order.
    pub fn due(&mut self, now: Stamp, give: &mut impl FnMut(Give<'_>)) {
        while self.sounds.front().is_some_and(|s| now.to(s.due) <= 0) {
            let s = self.sounds.pop_front().expect("the front, just read");
            self.audio = Some(s.frame);
            give(Give::Audio(&s.bytes));
        }
    }

    /// When the next held Opus packet is due; none while none is held.
    pub fn next_due(&self) -> Option<Stamp> {
        self.sounds.front().map(|s| s.due)
    }

    /// The decoder decided every frame up to `last` (a block, whole or missed): an update
    /// among them that was not made whole is known missed once a later update is held.
    pub fn decided(&mut self, last: Seq, give: &mut impl FnMut(Give<'_>)) {
        self.undecided = last.get() + 1;
        self.pump(give);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lanes::code::Encoder;
    use crate::lanes::wire::Stamp;
    use crate::xorshift::Xorshift;
    use std::collections::BTreeSet;
    use std::time::Instant;

    /// A frame as the fake daemon coded it (a block of one): its number and track as the
    /// encoder made them, and what an update refers to, which is the oracle's.
    struct Coded {
        frame: Seq,
        track: Track,
        refers: Vec<Number>,
        /// Its bytes: its frame number, so what the page is given names it.
        bytes: [u8; 8],
    }

    /// Over random sessions, a fake daemon codes updates and Opus packets, each update
    /// referring to some of the updates its encoder still holds (none for a keyframe), and
    /// invalidates a missed update (that update and every later one it holds) when the
    /// report of its miss reaches it, some frames after the miss, reports arriving in order.
    /// Each frame is made whole at a random time before it is decided, or never (missed);
    /// frames are decided in order.
    ///
    /// The page is never given an update that refers to one it was not given, gets updates
    /// in order and each at most once, and gets Opus packets in order, each at its deadline. And nothing is held
    /// back for nothing: every whole update whose epoch is at or past each miss before it
    /// is given.
    #[test]
    fn the_page_never_gets_an_update_referring_to_one_it_did_not_get() {
        let mut rng = Xorshift::new(0x853C49E6748FEA9B);
        let due = {
            let now = Instant::now();
            Stamp::since(now, now)
        };
        let mut gave_after_miss = 0;
        for _ in 0..3_000 {
            let frames = rng.range(1, 200) as usize;
            let loss = rng.below(5);
            let mut encoder = Encoder::new();
            // The encoder: the updates it may refer to, its epoch, its next number.
            let mut refs: Vec<Number> = Vec::new();
            let mut epoch = Epoch::Start;
            let mut next = Number::first();
            // Reports of misses on their way to the daemon, in order: the frame they reach
            // it before, and the update.
            let mut reports: Vec<(usize, Number)> = Vec::new();
            let mut coded: Vec<Coded> = Vec::new();
            // Per frame: the step it is made whole at, or never.
            let mut whole_at: Vec<Option<usize>> = Vec::new();
            let mut missed: Vec<Number> = Vec::new();
            let mut release = Release::new();
            // What the page was given, and in what order.
            let mut page: BTreeSet<Number> = BTreeSet::new();
            let mut last: Option<Number> = None;
            let mut audio: Option<u64> = None;
            let mut decided = 0usize;
            let mut step = 0usize;
            let check = |g: Give<'_>,
                         coded: &[Coded],
                         page: &mut BTreeSet<Number>,
                         last: &mut Option<Number>,
                         audio: &mut Option<u64>| {
                let b = match g {
                    Give::Video(b) | Give::Audio(b) => b,
                };
                let f = u64::from_be_bytes(b.try_into().unwrap());
                let c = &coded[f as usize];
                match (g, c.track) {
                    (Give::Audio(_), Track::Audio) => {
                        assert!(audio.is_none_or(|a| a < f), "an Opus packet out of order");
                        *audio = Some(f);
                    }
                    (Give::Video(_), Track::Video { number, .. }) => {
                        for r in &c.refers {
                            assert!(page.contains(r), "{number:?} refers to {r:?}, not given");
                        }
                        assert!(last.is_none_or(|n| n < number), "updates out of order");
                        *last = Some(number);
                        page.insert(number);
                    }
                    _ => panic!("a frame given as another track"),
                }
            };
            while decided < frames {
                step += 1;
                if coded.len() < frames {
                    let f = coded.len();
                    // Reports that have reached the daemon invalidate their update and every
                    // later one.
                    reports.retain(|&(by, x)| {
                        if by > f {
                            return true;
                        }
                        refs.retain(|&r| r < x);
                        epoch = epoch.max(Epoch::After(x));
                        false
                    });
                    let (track, refers) = match rng.below(4) {
                        0 => (Track::Audio, vec![]),
                        _ => {
                            let number = next;
                            next = number.next();
                            let refers: Vec<Number> = match rng.below(10) {
                                0 => vec![],
                                _ => refs.iter().copied().filter(|_| rng.below(2) == 0).collect(),
                            };
                            refs.push(number);
                            (Track::Video { number, epoch }, refers)
                        }
                    };
                    let bytes = (f as u64).to_be_bytes();
                    let head = *encoder
                        .assemble([(track, &bytes[..])].into_iter())
                        .unwrap()
                        .code(0, due)
                        .unwrap()
                        .head();
                    assert_eq!(head.first.get(), f as u64);
                    coded.push(Coded {
                        frame: head.first,
                        track,
                        refers,
                        bytes,
                    });
                    let lost = rng.below(10) < loss;
                    whole_at.push((!lost).then(|| step + rng.below(6) as usize));
                }
                for f in decided..coded.len() {
                    if whole_at[f] == Some(step) {
                        let c = &coded[f];
                        let mut give =
                            |g: Give<'_>| check(g, &coded, &mut page, &mut last, &mut audio);
                        let member = Member {
                            frame: c.frame,
                            track: c.track,
                            bytes: &c.bytes,
                        };
                        release.whole(&member, due, &mut give);
                        release.due(due, &mut give);
                    }
                }
                // The oldest frames are decided, now and then, once made whole if they are.
                while decided < coded.len()
                    && rng.below(3) == 0
                    && whole_at[decided].is_none_or(|w| w <= step)
                {
                    let c = &coded[decided];
                    if let (None, Track::Video { number, .. }) = (whole_at[decided], c.track) {
                        missed.push(number);
                        // The control stream keeps reports in order.
                        let after = reports.last().map_or(0, |r| r.0);
                        let by = after.max(coded.len() + rng.below(5) as usize);
                        reports.push((by, number));
                    }
                    let mut give = |g: Give<'_>| check(g, &coded, &mut page, &mut last, &mut audio);
                    release.decided(c.frame, &mut give);
                    decided += 1;
                }
            }
            // Nothing held back for nothing.
            for c in &coded {
                let Track::Video { number, epoch } = c.track else {
                    continue;
                };
                let before = missed.iter().copied().filter(|&x| x < number).max();
                let whole = whole_at[c.frame.get() as usize].is_some();
                let due = whole && before.is_none_or(|x| epoch >= Epoch::After(x));
                assert_eq!(page.contains(&number), due, "update {number:?}");
                if due && before.is_some() {
                    gave_after_miss += 1;
                }
            }
        }
        // The oracle above is not met only by giving nothing after a miss.
        assert!(gave_after_miss > 1000, "{gave_after_miss}");
    }

    /// Over random runs of Opus packets, each due a random while after the one before and
    /// made whole at a random time before its deadline, or never: the page gets a packet
    /// only once its deadline is past, in order, each at most once, and every whole packet
    /// once all deadlines are past.
    #[test]
    fn an_opus_packet_goes_to_the_page_at_its_deadline() {
        let mut rng = Xorshift::new(0x6A09E667F3BCC909);
        let origin = Instant::now();
        let at = |tenths: u64| {
            Stamp::since(
                origin,
                origin + crate::lanes::wire::from_tenths(tenths as i64),
            )
        };
        for _ in 0..2_000 {
            let mut encoder = Encoder::new();
            let mut release = Release::new();
            let n = rng.range(1, 60) as usize;
            let mut dues = Vec::new();
            let mut t = 0u64;
            for _ in 0..n {
                t += rng.range(1, 400);
                dues.push(t);
            }
            let coded: Vec<(Seq, u64, Option<u64>, [u8; 8])> = dues
                .iter()
                .enumerate()
                .map(|(f, &due)| {
                    let bytes = (f as u64).to_be_bytes();
                    let head = *encoder
                        .assemble([(Track::Audio, &bytes[..])].into_iter())
                        .unwrap()
                        .code(0, at(due))
                        .unwrap()
                        .head();
                    let whole = (rng.below(5) != 0).then(|| due.saturating_sub(rng.below(800)));
                    (head.first, due, whole, bytes)
                })
                .collect();
            // Events in time order: wholes, and looks at the clock.
            let mut events: Vec<(u64, Option<usize>)> = coded
                .iter()
                .enumerate()
                .filter_map(|(f, c)| c.2.map(|w| (w, Some(f))))
                .collect();
            for _ in 0..n {
                events.push((rng.below(t + 1), None));
            }
            events.push((t, None));
            events.sort();
            let mut given: Vec<u64> = Vec::new();
            for (now, what) in events {
                let mut give = |g: Give<'_>| {
                    let Give::Audio(b) = g else {
                        panic!("audio given as video")
                    };
                    let f = u64::from_be_bytes(b.try_into().unwrap());
                    assert!(
                        coded[f as usize].1 <= now,
                        "packet {f} given before its deadline"
                    );
                    assert!(
                        given.last().is_none_or(|&l| l < f),
                        "packet {f} out of order"
                    );
                    given.push(f);
                };
                if let Some(f) = what {
                    let c = &coded[f];
                    let member = Member {
                        frame: c.0,
                        track: Track::Audio,
                        bytes: &c.3,
                    };
                    release.whole(&member, at(c.1), &mut give);
                }
                release.due(at(now), &mut give);
            }
            // A packet whole after a later one went is dropped; every other whole one went.
            let mut last = None;
            let mut want = Vec::new();
            let mut order: Vec<usize> = (0..n).filter(|&f| coded[f].2.is_some()).collect();
            order.sort_by_key(|&f| coded[f].1);
            for f in order {
                if last.is_none_or(|l| l < f) {
                    want.push(f as u64);
                    last = Some(f);
                }
            }
            assert_eq!(given, want);
        }
    }
}
