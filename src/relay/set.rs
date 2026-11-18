//! Which lanes a session holds, as rules on events (the lanes the daemon asks for, a lane's
//! confirmation, a lane's end), apart from the sockets that carry them:
//!
//! - A set has one lane of datagrams; or every connection the description names, opened
//!   whole; or `min(asked, cap)` stream lanes, one until the daemon first asks.
//! - Lanes are opened in the order of their places, each once in its generation. Asked for
//!   more, the set opens the lanes it lacks in the same generation; asked for fewer, it closes
//!   those past the number.
//! - A lane that ends after every lane of its set attached opens the set again, whole and in
//!   order, as the next generation (the daemon drops the old one's lanes). One that ends
//!   before is a transport refusing lanes, and ends the session: so a set is reopened only
//!   after the daemon confirmed each of its lanes, never in a loop the relay drives alone.
//! - The end or the confirmation of a lane of an earlier generation, or of one closed since,
//!   changes nothing.

use crate::lanes::model::Lanes;
use crate::lanes::wire::{Generation, LaneIndex};

/// How many lanes a set has, by the description's kind.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Wanted {
    /// One lane of datagrams.
    One,
    /// Every connection, always.
    All(Lanes),
    /// As many stream lanes as the daemon asks for, up to the cap.
    UpTo(Lanes),
}

/// A lane's attach: sent at `T` and unconfirmed, or confirmed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Attaching<T> {
    Sent(T),
    Attached,
}

/// A lane of the set: its place, its attach, and what carries it (`P`, dropped when the lane
/// is closed).
pub struct Slot<T, P> {
    pub lane: LaneIndex,
    pub attaching: Attaching<T>,
    pub carrier: P,
}

/// What a lane's end does.
#[derive(Debug, PartialEq)]
pub enum Ended {
    /// It was of an earlier generation, or closed: nothing.
    Stale,
    /// The set is opened again as this generation: every lane it wants is missing.
    Reopen(Generation),
    /// The set had not attached: the session ends.
    End,
}

pub struct Set<T, P> {
    wanted: Wanted,
    /// The stream lanes the daemon last asked for: one until it asks.
    asked: Lanes,
    generation: Generation,
    slots: Vec<Slot<T, P>>,
}

impl<T: Copy, P> Set<T, P> {
    pub fn new(wanted: Wanted) -> Set<T, P> {
        Set {
            wanted,
            asked: Lanes::ONE,
            generation: Generation::first(),
            slots: Vec::new(),
        }
    }

    pub fn generation(&self) -> Generation {
        self.generation
    }

    pub fn slots(&self) -> &[Slot<T, P>] {
        &self.slots
    }

    pub fn slots_mut(&mut self) -> &mut [Slot<T, P>] {
        &mut self.slots
    }

    /// The lanes the set has when nothing is missing.
    fn count(&self) -> Lanes {
        match self.wanted {
            Wanted::One => Lanes::ONE,
            Wanted::All(lanes) => lanes,
            Wanted::UpTo(cap) => self.asked.min(cap),
        }
    }

    /// The lanes to open now, in order: those past the ones the set holds.
    pub fn missing(&self) -> impl Iterator<Item = LaneIndex> {
        LaneIndex::all(self.count()).skip(self.slots.len())
    }

    /// Lane `lane`, the first missing one, was opened and its attach sent at `sent`.
    pub fn opened(&mut self, lane: LaneIndex, sent: T, carrier: P) {
        debug_assert_eq!(lane.get(), self.slots.len(), "lanes open in order");
        self.slots.push(Slot {
            lane,
            attaching: Attaching::Sent(sent),
            carrier,
        });
    }

    /// The daemon asks for `lanes` lanes: stream lanes past the number close.
    pub fn asked(&mut self, lanes: Lanes) {
        self.asked = lanes;
        let count = self.count().get();
        self.slots.truncate(count);
    }

    /// The daemon confirmed `lane` of `generation`: when its attach went, if it is a lane of
    /// the set that was waiting for it.
    pub fn attached(&mut self, lane: LaneIndex, generation: Generation) -> Option<T> {
        if generation != self.generation {
            return None;
        }
        let slot = self.slots.iter_mut().find(|s| s.lane == lane)?;
        match std::mem::replace(&mut slot.attaching, Attaching::Attached) {
            Attaching::Sent(at) => Some(at),
            Attaching::Attached => None,
        }
    }

    /// Lane `lane` of `generation` ended.
    pub fn ended(&mut self, lane: LaneIndex, generation: Generation) -> Ended {
        let current = generation == self.generation && self.slots.iter().any(|s| s.lane == lane);
        if !current {
            return Ended::Stale;
        }
        if self
            .slots
            .iter()
            .any(|s| !matches!(s.attaching, Attaching::Attached))
        {
            return Ended::End;
        }
        self.slots.clear();
        self.generation = self.generation.next();
        Ended::Reopen(self.generation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xorshift::Xorshift;

    /// Over random runs of events (the daemon asking for a number of lanes, confirming a lane
    /// of this or an earlier generation, a lane of any generation ending, the missing lanes
    /// opening), for each kind of set:
    /// - once the missing lanes are opened, the set holds exactly its places 0 to its count,
    ///   in order, where the count is 1 for datagrams, the cap for connections, and for
    ///   streams one until the first ask, then the least of the newest ask and the cap;
    /// - a lane is opened only where the set has none, so no place opens twice in a
    ///   generation;
    /// - the end of a lane of the set reopens it as the next generation, with every place
    ///   missing, exactly when every lane of the set was confirmed, and otherwise ends the
    ///   session; the end of any other lane changes nothing;
    /// - between two reopenings the daemon confirmed at least one lane, so the relay never
    ///   reopens on its own.
    #[test]
    fn a_set_holds_its_count_and_reopens_only_once_attached() {
        let mut rng = Xorshift::new(0x94D049BB133111EB);
        let mut reopened = 0;
        for _ in 0..5_000 {
            let cap = Lanes::new(rng.range(1, 12) as u16).unwrap();
            let wanted = match rng.below(3) {
                0 => Wanted::One,
                1 => Wanted::All(cap),
                _ => Wanted::UpTo(cap),
            };
            let mut set: Set<u64, ()> = Set::new(wanted);
            let mut asked: Option<Lanes> = None;
            let mut confirmed_since_reopen = 0;
            for step in 0..200u64 {
                let count = match wanted {
                    Wanted::One => 1,
                    Wanted::All(c) => c.get(),
                    Wanted::UpTo(c) => asked.map_or(1, |a| a.get().min(c.get())),
                };
                match rng.below(5) {
                    0 => {
                        let n = Lanes::new(rng.range(1, 16) as u16).unwrap();
                        asked = Some(n);
                        set.asked(n);
                        let count = match wanted {
                            Wanted::UpTo(c) => n.get().min(c.get()),
                            _ => count,
                        };
                        assert!(set.slots().len() <= count);
                    }
                    1 => {
                        let lane = LaneIndex::all(Lanes::new(16).unwrap())
                            .nth(rng.below(16) as usize)
                            .unwrap();
                        let generation = match rng.below(4) {
                            0 => Generation::first(),
                            _ => set.generation(),
                        };
                        let waiting = generation == set.generation()
                            && set.slots().iter().any(|s| {
                                s.lane == lane && !matches!(s.attaching, Attaching::Attached)
                            });
                        let sent = set.attached(lane, generation);
                        assert_eq!(sent.is_some(), waiting);
                        if waiting {
                            confirmed_since_reopen += 1;
                        }
                    }
                    2 => {
                        let lane = LaneIndex::all(Lanes::new(16).unwrap())
                            .nth(rng.below(16) as usize)
                            .unwrap();
                        let generation = match rng.below(4) {
                            0 => Generation::first(),
                            _ => set.generation(),
                        };
                        let before = set.generation();
                        let ours =
                            generation == before && set.slots().iter().any(|s| s.lane == lane);
                        let all = set
                            .slots()
                            .iter()
                            .all(|s| matches!(s.attaching, Attaching::Attached));
                        match set.ended(lane, generation) {
                            Ended::Stale => assert!(!ours),
                            Ended::End => {
                                assert!(ours && !all);
                                break;
                            }
                            Ended::Reopen(g) => {
                                assert!(ours && all);
                                assert_eq!(g, before.next());
                                assert_eq!(set.generation(), g);
                                assert!(set.slots().is_empty());
                                assert!(
                                    confirmed_since_reopen > 0,
                                    "reopened with nothing confirmed"
                                );
                                confirmed_since_reopen = 0;
                                reopened += 1;
                            }
                        }
                    }
                    _ => {
                        let missing: Vec<LaneIndex> = set.missing().collect();
                        for lane in missing {
                            assert!(set.slots().iter().all(|s| s.lane != lane), "opened twice");
                            set.opened(lane, step, ());
                        }
                        let places: Vec<usize> = set.slots().iter().map(|s| s.lane.get()).collect();
                        assert_eq!(places, (0..count).collect::<Vec<_>>());
                    }
                }
            }
        }
        assert!(reopened > 1000, "{reopened}");
    }
}
