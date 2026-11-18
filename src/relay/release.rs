//! What the page is given of the frames the lanes complete, and when.
//!
//! The screen is one video stream whose updates refer only to pictures the page has
//! reported decoded, so every update the relay completes is decodable: updates go to the
//! page in their order, each as soon as every earlier one is decided, and every completed
//! update goes. One that missed its deadline (datagram lanes only) is passed over: the relay
//! knows it missed once every frame before the next update it holds is decided, and that
//! update's number is past it. On a stream lane frames arrive whole and in order, and each
//! goes at once.
//!
//! An Opus packet refers to nothing. On a datagram lane it goes to the page at its deadline,
//! in order: the daemon's deadline is the moment by which the path's measured spread has
//! delivered it, so packets reach the page as evenly as they were sent, and the page's
//! player needs no jitter allowance of its own; the cost is that spread, on every packet. On
//! a stream lane it goes as it arrives: the stream paces it. One whole only after a later
//! one went is dropped (audio plays in order).

use crate::lanes::wire::{Member, Number, Seq, Stamp, Track};
use std::collections::VecDeque;

/// What goes to the page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Give<'a> {
    /// An update: its 9-byte header and the picture.
    Video(&'a [u8]),
    /// An Opus packet.
    Audio(&'a [u8]),
}

/// When an Opus packet may go to the page: at once (a stream lane's), or at its block's
/// deadline (a datagram lane's).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum When {
    Now,
    At(Stamp),
}

/// A whole update held until every earlier one is decided.
struct Held {
    number: Number,
    frame: Seq,
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
            held: VecDeque::new(),
            undecided: 0,
            audio: None,
            sounds: VecDeque::new(),
        }
    }

    /// Gives the page what is due, in order, while every frame before the next held update
    /// is decided.
    fn pump(&mut self, give: &mut impl FnMut(Give<'_>)) {
        while let Some(h) = self.held.front() {
            // The updates before it were not completed: once every frame before this one is
            // decided, they missed.
            if h.number > self.next && self.undecided < h.frame.get() {
                return;
            }
            let h = self.held.pop_front().expect("the front, just read");
            give(Give::Video(&h.bytes));
            self.next = h.number.next();
        }
    }

    /// A frame the lanes completed: given now, or held.
    pub fn whole(&mut self, member: &Member<'_>, when: When, give: &mut impl FnMut(Give<'_>)) {
        let bytes = member.bytes;
        match member.track {
            Track::Audio => {
                if self.audio.is_some_and(|a| a >= member.frame) {
                    return;
                }
                match when {
                    When::Now => {
                        self.audio = Some(member.frame);
                        give(Give::Audio(bytes));
                    }
                    When::At(due) => {
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
                }
            }
            Track::Video { number } => {
                let at = self.held.partition_point(|h| h.number < number);
                self.held.insert(
                    at,
                    Held {
                        number,
                        frame: member.frame,
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

    /// Every frame up to `last` is decided (a datagram lane's block, completed or missed; a
    /// stream lane's frame, completed): an update among them that was not completed is
    /// passed over once a later update is held.
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
    use std::time::Instant;

    /// A frame as the fake daemon coded it (a block of one): its number and track as the
    /// encoder made them.
    struct Coded {
        frame: Seq,
        track: Track,
        /// Its bytes: its frame number, so what the page is given names it.
        bytes: [u8; 8],
    }

    /// Over random sessions of updates and Opus packets, each completed at a random time
    /// before it is decided, or never (missed), frames decided in order: the page gets every
    /// completed update, each once, in number order, and each only once every frame before
    /// it is decided or the update before it went; no completed update is held back once
    /// that is so.
    #[test]
    fn every_completed_update_goes_to_the_page_in_order() {
        let mut rng = Xorshift::new(0x853C49E6748FEA9B);
        let due = {
            let now = Instant::now();
            Stamp::since(now, now)
        };
        let mut missed_then_given = 0;
        for _ in 0..3_000 {
            let frames = rng.range(1, 200) as usize;
            let loss = rng.below(5);
            let mut encoder = Encoder::new();
            let mut next = Number::first();
            let mut coded: Vec<Coded> = Vec::new();
            // Per frame: the step it is completed at, or never.
            let mut whole_at: Vec<Option<usize>> = Vec::new();
            let mut release = Release::new();
            let mut given: Vec<Number> = Vec::new();
            let mut decided = 0usize;
            let mut step = 0usize;
            while decided < frames {
                step += 1;
                if coded.len() < frames {
                    let f = coded.len();
                    let track = match rng.below(4) {
                        0 => Track::Audio,
                        _ => {
                            let number = next;
                            next = number.next();
                            Track::Video { number }
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
                        bytes,
                    });
                    let lost = rng.below(10) < loss;
                    whole_at.push((!lost).then(|| step + rng.below(6) as usize));
                }
                let give = |g: Give<'_>, decided: usize, given: &mut Vec<Number>| {
                    let Give::Video(b) = g else { return };
                    let f = u64::from_be_bytes(b.try_into().unwrap()) as usize;
                    let Track::Video { number } = coded[f].track else {
                        panic!("an Opus packet given as video")
                    };
                    assert!(given.last().is_none_or(|&l| l < number), "out of order");
                    // Every frame before it is decided, or it is the next number.
                    let next_number = given.last().map_or(Number::first(), |l| l.next());
                    assert!(
                        decided >= f || number == next_number,
                        "update {number:?} given before the frames before it were decided"
                    );
                    given.push(number);
                };
                for f in decided..coded.len() {
                    if whole_at[f] == Some(step) {
                        let c = &coded[f];
                        let member = Member {
                            frame: c.frame,
                            track: c.track,
                            bytes: &c.bytes,
                        };
                        release.whole(&member, When::At(due), &mut |g| {
                            give(g, decided, &mut given)
                        });
                    }
                }
                while decided < coded.len()
                    && rng.below(3) == 0
                    && whole_at[decided].is_none_or(|w| w <= step)
                {
                    let c = &coded[decided];
                    let d = decided + 1;
                    release.decided(c.frame, &mut |g| give(g, d, &mut given));
                    decided += 1;
                }
            }
            // Every completed update went, and nothing else.
            let want: Vec<Number> = coded
                .iter()
                .filter_map(|c| match c.track {
                    Track::Video { number } if whole_at[c.frame.get() as usize].is_some() => {
                        Some(number)
                    }
                    _ => None,
                })
                .collect();
            if want.len()
                < coded
                    .iter()
                    .filter(|c| matches!(c.track, Track::Video { .. }))
                    .count()
                && !want.is_empty()
            {
                missed_then_given += 1;
            }
            assert_eq!(given, want);
        }
        assert!(missed_then_given > 500, "{missed_then_given}");
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
                    release.whole(&member, When::At(at(c.1)), &mut give);
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
