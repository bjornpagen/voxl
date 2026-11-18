//! The code: a frame of k source symbols sent with m repairs over GF(2^8).
//!
//! Sources go as they are. Repair `j` of frame `f` is the sum over the frame's sources `i` of
//! `coefficient(f, j, i)` times source `i` (a shorter source counts as followed by zeros), and
//! is as long as the frame's longest source. The coefficients are never sent: both ends
//! compute them.
//!
//! The decoder holds each live frame as one linear system over its unknown sources, in reduced
//! row echelon form: a source that arrives is substituted into every row, a repair that
//! arrives is reduced by the rows and becomes one, and a row left with one unknown is that
//! source recovered. A frame leaves the decoder when its deadline passes: its rows go with it.
//!
//! Neither end allocates per symbol: the encoder's repairs and each frame's buffers are reused
//! from frame to frame, and grow only to the largest frame seen.

use super::gf;
use super::wire::{
    symbols, write_entry, Arrivals, Fate, Head, Lane, Late, Malformed, OneWay, Outcome, Report,
    Seq, Stamp, Symbol, Track, DELAY, LATE, SYMBOL, WINDOW,
};
use std::collections::VecDeque;
use std::num::{NonZeroU32, NonZeroU8, NonZeroUsize};

/// The coefficient of source `i` in repair `j` of `frame`: a pure function, never 0.
///
/// SplitMix64's finaliser (Steele, Lea and Flood, OOPSLA 2014) applied twice, to the frame and
/// then to the frame's hash with the repair and the position, reduced to 1..=255. A peer that
/// computed it differently would only look like a lossy path, so the tests pin it.
pub fn coefficient(frame: Seq, j: u16, i: u16) -> u8 {
    fn mix(mut z: u64) -> u64 {
        z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    (mix(mix(frame.get()) ^ ((j as u64) << 16 | i as u64)) % 255) as u8 + 1
}

/// Gathers frames into blocks and cuts blocks into their messages, numbering both.
pub struct Encoder {
    /// The next block's number, and its first frame's.
    block: Seq,
    frame: Seq,
    /// The block being assembled: its table and its members' bytes.
    bytes: Vec<u8>,
    members: u8,
    /// The coded block's repairs, one after another, each as long as its longest source.
    repairs: Vec<u8>,
}

/// A block assembled and not yet coded: its length is what the link plans it by.
pub struct Assembled<'e> {
    encoder: &'e mut Encoder,
    bytes: NonZeroUsize,
    members: NonZeroU8,
}

/// A block, encoded: its head and its n messages.
pub struct Coded<'a> {
    head: Head,
    bytes: &'a [u8],
    repairs: &'a [u8],
}

impl Encoder {
    pub fn new() -> Encoder {
        Encoder {
            block: Seq(0),
            frame: Seq(0),
            bytes: Vec::new(),
            members: 0,
            repairs: Vec::new(),
        }
    }

    /// A block of `members` (frames of these tracks and bytes, in this order): its table and
    /// bytes, to be planned and coded. A block of no members, of an empty member, of more than
    /// a one-byte count numbers or of more than [`WINDOW`] symbols is an error.
    pub fn assemble<'e, 'b>(
        &'e mut self,
        members: impl Iterator<Item = (Track, &'b [u8])> + Clone,
    ) -> Result<Assembled<'e>, Malformed> {
        let count = u8::try_from(members.clone().count()).map_err(|_| Malformed::TooLarge)?;
        let count = NonZeroU8::new(count).ok_or(Malformed::Empty)?;
        self.bytes.clear();
        for (track, bytes) in members.clone() {
            let len = u32::try_from(bytes.len()).map_err(|_| Malformed::TooLarge)?;
            if len == 0 {
                return Err(Malformed::Empty);
            }
            write_entry(&mut self.bytes, track, len);
        }
        for (_, bytes) in members {
            self.bytes.extend_from_slice(bytes);
        }
        let bytes = NonZeroUsize::new(self.bytes.len()).ok_or(Malformed::Empty)?;
        if symbols(bytes).get() > WINDOW {
            return Err(Malformed::TooLarge);
        }
        self.members = count.get();
        Ok(Assembled {
            encoder: self,
            bytes,
            members: count,
        })
    }
}

impl<'e> Assembled<'e> {
    /// Its length, table and members.
    pub fn bytes(&self) -> NonZeroUsize {
        self.bytes
    }

    /// Codes it with `m` repairs and deadline `due`, numbering it and its frames after the
    /// last. More repairs than [`WINDOW`] has room for with its sources is an error, and
    /// takes no number.
    pub fn code(self, m: u16, due: Stamp) -> Result<Coded<'e>, Malformed> {
        let e = self.encoder;
        let head = Head {
            block: e.block,
            first: e.frame,
            members: self.members,
            bytes: NonZeroU32::new(self.bytes.get() as u32).ok_or(Malformed::Empty)?,
            m,
            due,
        };
        if head.n() > WINDOW {
            return Err(Malformed::TooLarge);
        }
        e.block = Seq(e.block.get() + 1);
        e.frame = Seq(e.frame.get() + self.members.get() as u64);
        let width = head.repair_len();
        e.repairs.clear();
        e.repairs.resize(m as usize * width, 0);
        for (j, repair) in e.repairs.chunks_exact_mut(width).enumerate() {
            for (i, source) in e.bytes.chunks(SYMBOL).enumerate() {
                gf::mul_add(repair, coefficient(head.block, j as u16, i as u16), source);
            }
        }
        Ok(Coded {
            head,
            bytes: &e.bytes,
            repairs: &e.repairs,
        })
    }
}

impl Default for Encoder {
    fn default() -> Encoder {
        Encoder::new()
    }
}

impl<'a> Coded<'a> {
    pub fn head(&self) -> &Head {
        &self.head
    }

    /// Its message `i` of n (sources first, then repairs), leaving at `sent`.
    pub fn message(&self, i: usize, sent: Stamp) -> Lane<'a> {
        let k = self.head.k();
        match i < k {
            true => Lane::Piece(Symbol {
                head: self.head,
                sent,
                index: i as u16,
                data: &self.bytes[i * SYMBOL..(i * SYMBOL + self.head.source_len(i))],
            }),
            false => {
                let (j, w) = (i - k, self.head.repair_len());
                Lane::Repair(Symbol {
                    head: self.head,
                    sent,
                    index: j as u16,
                    data: &self.repairs[j * w..(j + 1) * w],
                })
            }
        }
    }
}

/// What became of a piece or repair the decoder took.
#[derive(Debug, PartialEq, Eq)]
pub enum Took<'a> {
    /// It made its frame whole: the frame, to forward now.
    Whole(Head, &'a [u8]),
    /// It is held: its frame is not whole yet.
    Held,
    /// Its frame was already whole, it had come before, or it told nothing new.
    Spare,
    /// It came after its frame's deadline: only its delay is kept, for the next report.
    Late,
}

/// Why the decoder refused a piece or repair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// Its head differs from the head its frame's earlier messages carried.
    Conflict,
    /// Its frame is [`WINDOW`] or more frames past the oldest undecided one.
    Ahead,
    /// Holding its frame would hold more than [`WINDOW`] symbols.
    Full,
}

/// A block decided: its report, and the newest frame decided with it (its own last member;
/// for a block nothing of which arrived, the frame before the next block that did), so that
/// every frame up to it is decided.
#[derive(Debug, PartialEq, Eq)]
pub struct Decided<'a> {
    pub report: Report<'a>,
    pub last: Seq,
}

/// One live frame: its sources as they arrive or are recovered, and its system.
struct Frame {
    head: Head,
    /// k slots of [`SYMBOL`] bytes: the frame, once whole, is the first `bytes` of them.
    data: Vec<u8>,
    known: Vec<bool>,
    unknown: usize,
    /// The rows: `rows` of them, each k coefficients and a repair's width of bytes, each with
    /// its pivot (a column no other row has), normalised to 1.
    coef: Vec<u8>,
    pay: Vec<u8>,
    pivot: Vec<usize>,
    rows: usize,
    /// Each of the n symbols' delay, if it arrived by the deadline.
    arrivals: Vec<Option<OneWay>>,
}

impl Frame {
    fn empty() -> Frame {
        Frame {
            head: Head {
                block: Seq(0),
                first: Seq(0),
                members: NonZeroU8::MIN,
                bytes: NonZeroU32::MIN,
                m: 0,
                due: Stamp(0),
            },
            data: Vec::new(),
            known: Vec::new(),
            unknown: 0,
            coef: Vec::new(),
            pay: Vec::new(),
            pivot: Vec::new(),
            rows: 0,
            arrivals: Vec::new(),
        }
    }

    /// Makes this (reused) frame the empty system of `head`.
    fn open(&mut self, head: Head) {
        let (k, m, w) = (head.k(), head.m as usize, head.repair_len());
        let rows = k.min(m);
        self.head = head;
        self.data.clear();
        self.data.resize(k * SYMBOL, 0);
        self.known.clear();
        self.known.resize(k, false);
        self.unknown = k;
        self.coef.clear();
        self.coef.resize(rows * k, 0);
        self.pay.clear();
        self.pay.resize(rows * w, 0);
        self.pivot.clear();
        self.pivot.resize(rows, 0);
        self.rows = 0;
        self.arrivals.clear();
        self.arrivals.resize(head.n(), None);
    }

    fn whole(&self) -> bool {
        self.unknown == 0
    }

    fn row(&mut self, r: usize) -> (&mut [u8], &mut [u8]) {
        let (k, w) = (self.head.k(), self.head.repair_len());
        (
            &mut self.coef[r * k..(r + 1) * k],
            &mut self.pay[r * w..(r + 1) * w],
        )
    }

    /// Row `r` += c · row `s`.
    fn add_row(&mut self, r: usize, c: u8, s: usize) {
        let (k, w) = (self.head.k(), self.head.repair_len());
        let (lo, hi) = (r.min(s), r.max(s));
        let (coef_lo, coef_hi) = self.coef.split_at_mut(hi * k);
        let (pay_lo, pay_hi) = self.pay.split_at_mut(hi * w);
        let (coef_lo, pay_lo) = (
            &mut coef_lo[lo * k..(lo + 1) * k],
            &mut pay_lo[lo * w..(lo + 1) * w],
        );
        let (coef_hi, pay_hi) = (&mut coef_hi[..k], &mut pay_hi[..w]);
        match r < s {
            true => {
                gf::mul_add(coef_lo, c, coef_hi);
                gf::mul_add(pay_lo, c, pay_hi);
            }
            false => {
                gf::mul_add(coef_hi, c, coef_lo);
                gf::mul_add(pay_hi, c, pay_lo);
            }
        }
    }

    /// Makes row `r`'s pivot its first non-zero column, normalised, cleared from every other
    /// row; or drops it if it has none (it told nothing new). Whether it stays.
    fn pivot_row(&mut self, r: usize) -> bool {
        let k = self.head.k();
        let Some(q) = self.coef[r * k..(r + 1) * k].iter().position(|&c| c != 0) else {
            self.drop_row(r);
            return false;
        };
        let inv = gf::inv(self.coef[r * k + q]);
        let (coef, pay) = self.row(r);
        gf::scale(coef, inv);
        gf::scale(pay, inv);
        self.pivot[r] = q;
        for s in 0..self.rows {
            let c = self.coef[s * k + q];
            if s != r && c != 0 {
                self.add_row(s, c, r);
            }
        }
        true
    }

    fn drop_row(&mut self, r: usize) {
        let last = self.rows - 1;
        if r != last {
            let (k, w) = (self.head.k(), self.head.repair_len());
            self.coef.copy_within(last * k..(last + 1) * k, r * k);
            self.pay.copy_within(last * w..(last + 1) * w, r * w);
            self.pivot[r] = self.pivot[last];
        }
        self.rows = last;
    }

    /// Source `i` is known: into its slot, and out of every row.
    fn source(&mut self, i: usize, data: &[u8]) {
        let k = self.head.k();
        self.data[i * SYMBOL..i * SYMBOL + data.len()].copy_from_slice(data);
        self.known[i] = true;
        self.unknown -= 1;
        let mut r = 0;
        while r < self.rows {
            let c = self.coef[r * k + i];
            if c != 0 {
                self.coef[r * k + i] = 0;
                let w = self.head.repair_len();
                gf::mul_add(
                    &mut self.pay[r * w..(r + 1) * w],
                    c,
                    &self.data[i * SYMBOL..i * SYMBOL + data.len()],
                );
                if self.pivot[r] == i && !self.pivot_row(r) {
                    continue;
                }
            }
            r += 1;
        }
    }

    /// Repair `j`, reduced into a row.
    fn repair(&mut self, j: u16, data: &[u8]) {
        let (k, w) = (self.head.k(), self.head.repair_len());
        let r = self.rows;
        self.rows += 1;
        let frame = self.head.block;
        self.pay[r * w..(r + 1) * w].copy_from_slice(data);
        for i in 0..k {
            let c = coefficient(frame, j, i as u16);
            match self.known[i] {
                true => {
                    self.coef[r * k + i] = 0;
                    let len = self.head.source_len(i);
                    let (src, pay) = (
                        &self.data[i * SYMBOL..i * SYMBOL + len],
                        &mut self.pay[r * w..(r + 1) * w],
                    );
                    gf::mul_add(pay, c, src);
                }
                false => self.coef[r * k + i] = c,
            }
        }
        for s in 0..r {
            let c = self.coef[r * k + self.pivot[s]];
            if c != 0 {
                self.add_row(r, c, s);
            }
        }
        self.pivot_row(r);
    }

    /// Every row left with one unknown is that source, recovered. In reduced row echelon form
    /// no other row has a recovered row's pivot, so one pass finds them all.
    fn harvest(&mut self) {
        let (k, w) = (self.head.k(), self.head.repair_len());
        let mut r = 0;
        while r < self.rows {
            let row = &self.coef[r * k..(r + 1) * k];
            if row.iter().filter(|&&c| c != 0).count() == 1 {
                let q = self.pivot[r];
                let len = self.head.source_len(q);
                self.data[q * SYMBOL..q * SYMBOL + len]
                    .copy_from_slice(&self.pay[r * w..r * w + len]);
                self.known[q] = true;
                self.unknown -= 1;
                self.drop_row(r);
                continue;
            }
            r += 1;
        }
    }
}

/// The relay's decoder.
pub struct Decoder {
    /// The frame numbered `base` and after, until the newest that has come; `None` for one
    /// nothing of which has come.
    live: VecDeque<Option<Frame>>,
    base: Seq,
    /// The symbols the live frames hold room for.
    held: usize,
    spare: Vec<Frame>,
    late: VecDeque<OneWay>,
    /// The newest block a piece of which came after its deadline: every block up to it is
    /// past its deadline, whatever else of them came.
    overdue: Option<Head>,
    /// The report being handed out.
    bits: Vec<u8>,
    delays: Vec<u8>,
    late_bytes: Vec<u8>,
}

impl Decoder {
    pub fn new() -> Decoder {
        Decoder {
            live: VecDeque::new(),
            base: Seq(0),
            held: 0,
            spare: Vec::new(),
            late: VecDeque::with_capacity(LATE),
            overdue: None,
            bits: Vec::new(),
            delays: Vec::new(),
            late_bytes: Vec::with_capacity(DELAY * LATE),
        }
    }

    fn late(&mut self, d: OneWay) -> Took<'static> {
        if self.late.len() == LATE {
            self.late.pop_front();
        }
        self.late.push_back(d);
        Took::Late
    }

    /// A piece or repair, arrived at `arrived` on the relay's clock.
    pub fn take(&mut self, lane: Lane<'_>, arrived: Stamp) -> Result<Took<'_>, Refused> {
        let s = *lane.symbol();
        let delay = OneWay::of(s.sent, arrived);
        if s.head.block < self.base {
            return Ok(self.late(delay));
        }
        if s.head.due.to(arrived) > 0 {
            if self.overdue.is_none_or(|o| o.block < s.head.block) {
                self.overdue = Some(s.head);
            }
            return Ok(self.late(delay));
        }
        let at = (s.head.block.get() - self.base.get()) as usize;
        if at >= WINDOW {
            return Err(Refused::Ahead);
        }
        if self.live.len() <= at {
            self.live.resize_with(at + 1, || None);
        }
        let frame = match &mut self.live[at] {
            Some(f) if f.head != s.head => return Err(Refused::Conflict),
            Some(f) => f,
            slot @ None => {
                if self.held + s.head.n() > WINDOW {
                    return Err(Refused::Full);
                }
                self.held += s.head.n();
                let mut f = self.spare.pop().unwrap_or_else(Frame::empty);
                f.open(s.head);
                slot.insert(f)
            }
        };
        let position = lane.position();
        if frame.arrivals[position].is_some() {
            return Ok(Took::Spare);
        }
        frame.arrivals[position] = Some(delay);
        if frame.whole() {
            return Ok(Took::Spare);
        }
        let before = frame.unknown;
        match lane {
            Lane::Piece(p) if frame.known[p.index as usize] => return Ok(Took::Spare),
            Lane::Piece(p) => frame.source(p.index as usize, p.data),
            Lane::Repair(r) => frame.repair(r.index, r.data),
        }
        frame.harvest();
        Ok(match () {
            _ if frame.whole() => {
                Took::Whole(frame.head, &frame.data[..frame.head.bytes.get() as usize])
            }
            _ if frame.unknown < before || frame.rows > 0 => Took::Held,
            _ => Took::Spare,
        })
    }

    /// When [`Decoder::decide`] next has a frame to decide, if any frame is held.
    pub fn wake(&self) -> Option<Stamp> {
        let live = self.live.iter().flatten().next().map(|f| f.head.due);
        let overdue = self.overdue.filter(|o| o.block >= self.base).map(|o| o.due);
        match (live, overdue) {
            (Some(l), Some(o)) => Some(if l.to(o) < 0 { o } else { l }),
            (l, o) => l.or(o),
        }
    }

    /// What decides the oldest undecided block by `now`, if anything: the head of the oldest
    /// later (or the same) block known past its deadline, whether a piece of it came on
    /// time or late.
    fn past(&self, now: Stamp) -> Option<Head> {
        let live = self
            .live
            .iter()
            .flatten()
            .next()
            .map(|f| f.head)
            .filter(|h| h.due.to(now) >= 0);
        let overdue = self.overdue.filter(|o| o.block >= self.base);
        match (live, overdue) {
            (Some(l), Some(o)) => Some(if l.block <= o.block { l } else { o }),
            (l, o) => l.or(o),
        }
    }

    /// The oldest undecided frame, if it is decided by `now`, with its report, leaving at
    /// `now`. Frames are decided in the order of their numbers; call it until it gives none.
    /// A block nothing of which came on time is decided once a piece of it or of a later one
    /// shows that block's deadline passed.
    pub fn decide(&mut self, now: Stamp) -> Option<Decided<'_>> {
        let next = match self.live.front() {
            // The oldest came on time: its own deadline decides it.
            Some(Some(f)) if f.head.due.to(now) < 0 => return None,
            Some(Some(f)) => f.head,
            _ => self.past(now)?,
        };
        let block = self.base;
        self.base = Seq(block.get() + 1);
        self.late_bytes.clear();
        for d in self.late.drain(..) {
            self.late_bytes.extend_from_slice(&d.0.to_be_bytes());
        }
        let late = Late::written(&self.late_bytes);
        match self.live.pop_front().flatten() {
            None => Some(Decided {
                report: Report {
                    block,
                    sent: now,
                    fate: Fate::Unseen,
                    late,
                },
                last: match next.block == block {
                    true => next.last(),
                    // A later block's frames follow this one's; a peer's head numbering its
                    // first frame 0 past block 0 decides no frame before it.
                    false => Seq(next.first.get().saturating_sub(1)),
                },
            }),
            Some(f) => {
                self.held -= f.head.n();
                let n = f.head.n();
                self.bits.clear();
                self.bits.resize(n.div_ceil(8), 0);
                self.delays.clear();
                for (i, a) in f.arrivals.iter().enumerate() {
                    if let Some(d) = a {
                        self.bits[i / 8] |= 1 << (i % 8);
                        self.delays.extend_from_slice(&d.0.to_be_bytes());
                    }
                }
                let outcome = match f.whole() {
                    true => Outcome::Decoded,
                    false => Outcome::Missed,
                };
                let head = f.head;
                self.spare.push(f);
                let arrivals = Arrivals::written(n as u16, &self.bits, &self.delays);
                Some(Decided {
                    report: Report {
                        block,
                        sent: now,
                        fate: Fate::Seen { outcome, arrivals },
                        late,
                    },
                    last: head.last(),
                })
            }
        }
    }
}

impl Default for Decoder {
    fn default() -> Decoder {
        Decoder::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lanes::model::{Arrival, Runs, Table};
    use crate::lanes::wire::{Epoch, Number, DATAGRAM};
    use crate::xorshift::Xorshift;
    use std::num::NonZeroUsize;

    /// The coefficients both ends compute, pinned: a change is a change of the protocol.
    #[test]
    fn the_coefficients_are_pinned() {
        let got: Vec<u8> = [
            (0, 0, 0),
            (0, 0, 1),
            (0, 1, 0),
            (1, 0, 0),
            (u64::MAX, 65535, 65535),
        ]
        .iter()
        .map(|&(f, j, i)| coefficient(Seq(f), j, i))
        .collect();
        assert_eq!(got, [16, 131, 62, 129, 207]);
        for f in 0..50 {
            for j in 0..20 {
                for i in 0..20 {
                    assert_ne!(coefficient(Seq(f), j, i), 0);
                }
            }
        }
    }

    /// A block of one Opus packet of `bytes`, coded.
    fn one<'e>(enc: &'e mut Encoder, bytes: &[u8], m: u16, due: Stamp) -> Coded<'e> {
        enc.assemble([(Track::Audio, bytes)].into_iter())
            .unwrap()
            .code(m, due)
            .unwrap()
    }

    fn any_frame(rng: &mut Xorshift, k_most: u64) -> Vec<u8> {
        let k = rng.range(1, k_most + 1) as usize;
        let len = (k - 1) * SYMBOL + rng.range(1, SYMBOL as u64 + 1) as usize;
        (0..len).map(|_| rng.next() as u8).collect()
    }

    fn track(rng: &mut Xorshift) -> Track {
        match rng.below(2) {
            0 => Track::Audio,
            _ => Track::Video {
                number: Number(rng.below(1000) + 10),
                epoch: Epoch::After(Number(rng.below(10))),
            },
        }
    }

    /// Frames of 1 to 40 symbols with up to 30 repairs, their messages erased at random,
    /// duplicated and shuffled together across several frames, each through the wire's
    /// encoding: a frame comes out whole exactly when its system has full rank, which needs at
    /// least k messages; when it does, its bytes are the frame's; and its report says, for
    /// each symbol, whether it arrived, and with what delay.
    #[test]
    fn decoding_is_byte_exact() {
        let mut rng = Xorshift::new(0x3243_F6A8_885A_308D);
        let mut enc = Encoder::new();
        let mut dec = Decoder::new();
        let mut buf = [0u8; DATAGRAM];
        let (mut whole_with_k, mut had_k) = (0, 0);
        for _round in 0..300 {
            // Each block's members, and its repairs.
            type Block = (Vec<(Track, Vec<u8>)>, u16);
            let blocks: Vec<Block> = (0..rng.range(1, 5))
                .map(|_| {
                    let members = (0..rng.range(1, 5))
                        .map(|_| (track(&mut rng), any_frame(&mut rng, 10)))
                        .collect();
                    (members, rng.below(31) as u16)
                })
                .collect();
            let mut messages = Vec::new();
            let mut kept = Vec::new();
            let due = Stamp(1_000_000);
            for (members, m) in &blocks {
                let coded = enc
                    .assemble(members.iter().map(|(t, b)| (*t, b.as_slice())))
                    .unwrap()
                    .code(*m, due)
                    .unwrap();
                let n = coded.head().n();
                let mut arrived = vec![false; n];
                for (i, a) in arrived.iter_mut().enumerate() {
                    if rng.below(3) == 0 {
                        continue;
                    }
                    *a = true;
                    let len = coded.message(i, Stamp(i as u32)).encode(&mut buf);
                    messages.push(buf[..len].to_vec());
                    if rng.below(5) == 0 {
                        messages.push(buf[..len].to_vec());
                    }
                }
                kept.push((*coded.head(), members.clone(), arrived));
            }
            for i in (1..messages.len()).rev() {
                messages.swap(i, rng.below(i as u64 + 1) as usize);
            }
            let mut out: Vec<Option<Vec<u8>>> = vec![None; kept.len()];
            for msg in &messages {
                let lane = Lane::decode(msg).unwrap();
                let which = kept
                    .iter()
                    .position(|(h, _, _)| h.block == lane.symbol().head.block)
                    .unwrap();
                if let Took::Whole(head, bytes) = dec.take(lane, Stamp(500)).unwrap() {
                    assert_eq!(head, kept[which].0);
                    assert!(out[which].is_none(), "whole twice");
                    out[which] = Some(bytes.to_vec());
                }
            }
            for (i, (head, members, arrived)) in kept.iter().enumerate() {
                let count = arrived.iter().filter(|&&a| a).count();
                if count >= head.k() {
                    had_k += 1;
                    whole_with_k += out[i].is_some() as usize;
                }
                if let Some(got) = &out[i] {
                    assert!(count >= head.k(), "whole from {count} of k {}", head.k());
                    let got: Vec<(Seq, Track, Vec<u8>)> = head
                        .members(got)
                        .unwrap()
                        .map(|m| (m.frame, m.track, m.bytes.to_vec()))
                        .collect();
                    let want: Vec<(Seq, Track, Vec<u8>)> = members
                        .iter()
                        .enumerate()
                        .map(|(j, (t, b))| (Seq(head.first.get() + j as u64), *t, b.clone()))
                        .collect();
                    assert_eq!(got, want);
                }
                let Some(Decided { report, last }) = dec.decide(due) else {
                    panic!("block {i} not decided")
                };
                assert_eq!((report.block, last), (head.block, head.last()));
                let Fate::Seen { outcome, arrivals } = report.fate else {
                    panic!()
                };
                assert_eq!(outcome == Outcome::Decoded, out[i].is_some());
                let got: Vec<Option<OneWay>> = arrivals.iter().collect();
                let want: Vec<Option<OneWay>> = arrived
                    .iter()
                    .enumerate()
                    .map(|(j, &a)| a.then(|| OneWay::of(Stamp(j as u32), Stamp(500))))
                    .collect();
                assert_eq!(got, want);
            }
            assert!(dec.decide(Stamp(u32::MAX / 2)).is_none());
        }
        assert!(
            whole_with_k as f64 >= had_k as f64 * (1.0 - 3.0 / 255.0),
            "{whole_with_k} of {had_k}"
        );
    }

    /// Under independent erasure, for k, m and p drawn at random, a frame's decode failures
    /// sit on the binomial bound: it
    /// fails whenever fewer than k of its n arrive (no code does better), and otherwise only
    /// when the received coefficients are singular, which a random square system over
    /// GF(2^8) is with chance under 1/255 + 1/255^2 + ... = 1/254.
    #[test]
    fn decoding_sits_on_the_binomial_bound() {
        let mut rng = Xorshift::new(0x1319_8A2E_0370_7344);
        let mut buf = [0u8; DATAGRAM];
        for _ in 0..8 {
            let k = rng.range(1, 33) as usize;
            let m = rng.range(1, 9) as u16;
            let p = rng.range(50, 301) as f64 / 1000.0;
            let mut enc = Encoder::new();
            let mut dec = Decoder::new();
            let (mut failed, mut short, mut exact) = (0, 0, 0);
            let trials = 1500;
            for t in 0..trials {
                // An Opus packet whose block (a 5-byte table and the packet) is k symbols.
                let bytes: Vec<u8> = (0..k * SYMBOL - 5).map(|_| rng.next() as u8).collect();
                let coded = one(&mut enc, &bytes, m, Stamp(t));
                let mut got = 0;
                let mut whole = false;
                let mut messages = Vec::new();
                for i in 0..k + m as usize {
                    if (rng.below(1 << 20) as f64) < p * (1 << 20) as f64 {
                        continue;
                    }
                    got += 1;
                    let len = coded.message(i, Stamp(t)).encode(&mut buf);
                    messages.push(buf[..len].to_vec());
                }
                for msg in &messages {
                    if let Took::Whole(h, b) =
                        dec.take(Lane::decode(msg).unwrap(), Stamp(t)).unwrap()
                    {
                        let member = h.members(b).unwrap().next().unwrap();
                        assert_eq!(member.bytes, &bytes[..]);
                        whole = true;
                    }
                }
                while dec.decide(Stamp(t)).is_some() {}
                short += (got < k) as usize;
                exact += (got == k) as usize;
                failed += !whole as usize;
            }
            let degenerate = failed - short;
            let bound = exact as f64 / 254.0 + 3.0 * (exact as f64 / 254.0).sqrt() + 1.0;
            assert!(
                degenerate as f64 <= bound,
                "k {k} m {m} p {p}: {degenerate} singular of {exact} with exactly k, bound {bound}"
            );
        }
    }

    /// The m chosen for a miss target meets it, and three quarters of it miss it: frames sent
    /// with each through the real encoder and decoder, their symbols erased by the runs in the
    /// order they are sent; runs from memoryless to bursts with a long tail.
    #[test]
    fn the_chosen_m_meets_its_target_and_three_quarters_miss_it() {
        let mut rng = Xorshift::new(0xA409_3822_299F_31D0);
        let mut buf = [0u8; DATAGRAM];
        // Arrivals in runs of 3 to 40, erasures mostly alone but one run in ten of 8.
        let tail = Runs::counted([
            [vec![0; 2], vec![1; 38]].concat(),
            [vec![9; 1], vec![0; 6], vec![1]].concat(),
        ]);
        let cases = [
            (Runs::geometric(0.09, 0.09), 2, 0.003),
            (Runs::geometric(0.033, 0.667), 6, 0.01),
            (Runs::geometric(0.033, 0.667), 10, 0.01),
            (Runs::geometric(0.05, 0.5), 20, 0.003),
            (Runs::geometric(0.02, 0.8), 1, 0.01),
            (tail.clone(), 4, 0.01),
            (tail, 12, 0.003),
        ];
        for (runs, k, wanted) in cases {
            let choice =
                Table::new(runs.clone()).repairs(NonZeroUsize::new(k).unwrap(), wanted, WINDOW - k);
            let less = (choice.m * 3) / 4;
            assert!(
                less < choice.m && choice.miss <= wanted,
                "loss {} k {k}: m {} misses {}, wanted {wanted}",
                runs.loss(),
                choice.m,
                choice.miss
            );
            let target = choice.miss;
            let trials = ((30.0 / target) as usize).clamp(3000, 20_000);
            let mut run = |m: u16| -> f64 {
                let mut enc = Encoder::new();
                let mut dec = Decoder::new();
                let mut misses = 0;
                let bytes: Vec<u8> = (0..k * SYMBOL - 7).map(|i| i as u8).collect();
                for t in 0..trials {
                    let coded = one(&mut enc, &bytes, m, Stamp(t as u32));
                    let n = coded.head().n();
                    let mut whole = false;
                    for (i, now) in (0..n).zip(runs.draw(&mut rng)) {
                        if now == Arrival::Erased {
                            continue;
                        }
                        let len = coded.message(i, Stamp(t as u32)).encode(&mut buf);
                        if let Took::Whole(..) = dec
                            .take(Lane::decode(&buf[..len]).unwrap(), Stamp(t as u32))
                            .unwrap()
                        {
                            whole = true;
                        }
                    }
                    while dec.decide(Stamp(t as u32)).is_some() {}
                    misses += !whole as usize;
                }
                misses as f64 / trials as f64
            };
            let se = (target * (1.0 - target) / trials as f64).sqrt();
            let at_m = run(choice.m as u16);
            let at_less = run(less as u16);
            assert!(
                at_m <= target * (1.0 + 1.0 / 254.0) + 3.0 * se,
                "loss {} k {k}: m {} missed {at_m}, target {target}",
                runs.loss(),
                choice.m
            );
            assert!(
                at_less > target + 3.0 * se,
                "loss {} k {k}: 3/4 of m ({less}) missed {at_less}, target {target}",
                runs.loss()
            );
        }
    }
}
