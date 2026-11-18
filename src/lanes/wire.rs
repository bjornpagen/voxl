//! Every message the lanes and the control stream carry, and the one way each is written and
//! read.
//!
//! On a lane: [`Attach`] (relay to daemon, the lane's first message and its only one that
//! way), then [`Lane::Piece`] and [`Lane::Repair`] (daemon to relay). On the control stream
//! (the stdio of `voxl attach` over ssh): [`Down`] (daemon to relay) and [`Up`] (relay to
//! daemon). A datagram is one message; a stream lane and the control stream carry each
//! message after its length ([`Framer`]). Numbers are big-endian, as the page's video header
//! is.
//!
//! Reading never panics and never allocates by a length it read: every reader checks a
//! length against the bytes in hand and against the message's fixed maximum first, and
//! returns a [`Malformed`] for anything else.

use std::num::{NonZeroU32, NonZeroU8, NonZeroUsize};
use std::time::{Duration, Instant};

/// Where every lane goes, as the proxy is asked for it: the relay's port on the server's own
/// loopback, which the server's proxy maps to the daemon's lane sockets. The relay dials it,
/// and its length is part of every lane datagram's overhead ([`DATAGRAM`]).
pub const DESTINATION: &str = "localhost:6901";

/// The least datagram every QUIC path carries: a QUIC endpoint "MUST NOT" be used on a path
/// that cannot carry a UDP payload of 1200 bytes (RFC 9000 §14).
const QUIC_LEAST_DATAGRAM: usize = 1200;

/// What QUIC itself takes of a datagram at most, around a DATAGRAM frame in a short-header
/// packet: the first byte, the longest destination connection ID (20, RFC 9000 §17.2), the
/// longest packet number (4), the AEAD tag of every TLS 1.3 cipher QUIC uses (16, RFC 9001
/// §5.3), and the DATAGRAM frame's type and length (1 and 2, RFC 9221 §4).
const QUIC_OVERHEAD: usize = 1 + 20 + 4 + 16 + 1 + 2;

/// What a Hysteria 2 UDP message puts before its payload (its protocol's "UDP" section):
/// session ID (4), packet ID (2), fragment ID (1), fragment count (1), the address's length as
/// a varint (1, the address being shorter than 64 bytes) and the address, [`DESTINATION`].
const UDP_MESSAGE_OVERHEAD: usize = 4 + 2 + 1 + 1 + 1 + DESTINATION.len();

/// The longest lane message, so that a lane datagram crosses as one QUIC datagram on any
/// path, never in fragments that would turn the loss of one into the loss of several.
///
/// Derived: `QUIC_LEAST_DATAGRAM - QUIC_OVERHEAD - UDP_MESSAGE_OVERHEAD`, 1133 bytes. A stream
/// lane carries the same messages, so one symbol size serves every kind of lane.
pub const DATAGRAM: usize = QUIC_LEAST_DATAGRAM - QUIC_OVERHEAD - UDP_MESSAGE_OVERHEAD;

/// The header a piece or repair carries before its symbol: tag (1), block (8), first frame
/// (8), members (1), bytes (4), m (2), due (4), sent (4) and index (2).
pub const HEADER: usize = 1 + 8 + 8 + 1 + 4 + 2 + 4 + 4 + 2;

/// A source symbol's bytes: what a lane message holds after the longest header. A frame is
/// cut into symbols of this length, the last shorter.
pub const SYMBOL: usize = DATAGRAM - HEADER;

/// The most symbols a frame may have (k + m), and the most the relay holds at once of all its
/// live frames together: what a symbol's 16-bit index and a report's 16-bit count can number.
/// It bounds the relay's memory for symbols at `WINDOW · SYMBOL`, 71 MB, which it takes only
/// as symbols arrive. The relay's memory is a deadline of its own: a block that would hold
/// more decides the oldest live blocks first, missed unless whole, and reports them as any
/// block ([`super::code::Decoder::take`]). Nothing on the daemon's side bounds what it sends
/// unreported: the transport's rate does.
pub const WINDOW: usize = u16::MAX as usize;

/// The longest control message: the most the page's WebSocket took of one message before the
/// control stream replaced it (`ws.max_message_size(64 << 20)`, src/server.rs:334 at
/// 8f460d3), which a clipboard image or a cursor crosses whole.
pub const MAX_CONTROL: usize = 64 << 20;

/// The longest payload a [`Down::Binary`] carries: the longest control message less its kind
/// byte.
pub const MAX_DOWN_BINARY: usize = MAX_CONTROL - std::mem::size_of::<u8>();

/// A late arrival's bytes on the wire: its block (8), its position among the block's n symbols
/// (2: sources, then repairs) and its delay ([`DELAY`]).
pub const LATE_ENTRY: usize = 8 + 2 + DELAY;

/// The longest report of a block, without its late arrivals: block (8), sent (4), fate (1),
/// n (2), a bitmap of [`WINDOW`] bits and every one of them arrived, with its delay.
const REPORT_MOST: usize = 8 + 4 + 1 + 2 + WINDOW.div_ceil(8) + DELAY * WINDOW;

/// The most late arrivals one report carries: as many as a control message holds after its
/// kind (1), the longest report of a block and the late arrivals' 4-byte count.
///
/// Derived: `(MAX_CONTROL - 1 - REPORT_MOST - 4) / LATE_ENTRY`, 4,774,330. The decoder holds
/// the rest for the next report: none is dropped.
pub const LATE: usize = (MAX_CONTROL - 1 - REPORT_MOST - 4) / LATE_ENTRY;

/// How many symbols a frame of `bytes` is cut into: one, and one more for each whole
/// `SYMBOL` after its first byte.
pub const fn symbols(bytes: NonZeroUsize) -> NonZeroUsize {
    NonZeroUsize::MIN.saturating_add((bytes.get() - 1) / SYMBOL)
}

/// A frame's number in its session, from 0, counting every frame of every track. 64 bits:
/// it never wraps. Only the encoder numbers frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Seq(pub(super) u64);

impl Seq {
    pub const fn first() -> Seq {
        Seq(0)
    }

    pub fn next(self) -> Seq {
        Seq(self.0 + 1)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

/// A video update's number among the session's updates, from 0: the daemon numbers them with
/// [`Number::first`] and [`Number::next`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Number(pub(super) u64);

impl Number {
    pub const fn first() -> Number {
        Number(0)
    }

    pub fn next(self) -> Number {
        Number(self.0 + 1)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

/// A time on one side's own clock, in tenths of a millisecond, wrapping every 119 hours. Two
/// stamps of one clock are compared by their difference, which is right while they are less
/// than 59 hours apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp(pub(super) u32);

impl Stamp {
    /// `now` on a clock that read 0 at `origin`.
    pub fn since(origin: Instant, now: Instant) -> Stamp {
        Stamp(tenths(now.saturating_duration_since(origin)) as u32)
    }

    /// From this stamp to `later`, in tenths of a millisecond; negative if `later` is earlier.
    pub fn to(self, later: Stamp) -> i64 {
        later.0.wrapping_sub(self.0) as i32 as i64
    }

    /// This stamp, `tenths` later (or earlier, if negative).
    pub fn plus(self, tenths: i64) -> Stamp {
        Stamp(self.0.wrapping_add(tenths as u32))
    }

    /// From this stamp until `later`; none if `later` has already passed.
    pub fn until(self, later: Stamp) -> Duration {
        from_tenths(self.to(later))
    }
}

/// The unit every stamp and delay counts in: a tenth of a millisecond.
pub const TENTH: Duration = Duration::from_micros(100);

/// `d` in whole tenths of a millisecond.
pub fn tenths(d: Duration) -> i64 {
    (d.as_micros() / TENTH.as_micros()).min(i64::MAX as u128) as i64
}

/// A count of tenths as a duration; a negative count, none.
pub fn from_tenths(t: i64) -> Duration {
    TENTH * t.max(0).min(u32::MAX as i64) as u32
}

/// A delay measured across the two clocks: the arrival's stamp on the relay's clock less the
/// sending stamp on the daemon's (or the other way for the control stream's direction). It
/// holds the clocks' offset, so it means something only against another sample taken the same
/// way, where the offset cancels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OneWay(pub(super) u32);

impl OneWay {
    /// The sample of a message stamped `sent` on one clock and `arrived` on the other.
    pub fn of(sent: Stamp, arrived: Stamp) -> OneWay {
        OneWay(arrived.0.wrapping_sub(sent.0))
    }

    /// By how much this sample is over `other`, in tenths of a millisecond.
    pub fn over(self, other: OneWay) -> i64 {
        self.0.wrapping_sub(other.0) as i32 as i64
    }
}

/// Which opening of the session's lane a lane is: a lane reopened after it attached is a new
/// generation, and the daemon drops the old one. The relay counts them with
/// [`Generation::first`] and [`Generation::next`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Generation(u32);

impl Generation {
    pub const fn first() -> Generation {
        Generation(0)
    }

    pub fn next(self) -> Generation {
        Generation(self.0.wrapping_add(1))
    }
}

/// A capability's bytes: 256 bits, the contract's capability (§2).
pub const CAPABILITY: usize = 32;

/// A delay's bytes on the wire, as a report carries it: a [`OneWay`]'s `u32`.
pub const DELAY: usize = std::mem::size_of::<u32>();

/// The capability that lets a lane attach to a session: 32 random bytes the daemon mints
/// per control connection and forgets when it closes.
///
/// It has no `Debug`, `Display` or `Clone`: it never reaches a log line, a panic message or a
/// second copy. Two are compared in constant time, and its bytes are zeroed when it drops.
pub struct Capability([u8; CAPABILITY]);

/// A capability where it lies: the daemon's own, lent to a control message, or the bytes of
/// the control frame that brought it to the relay. The same guarantees as [`Capability`].
#[derive(Clone, Copy)]
pub struct CapabilityRef<'a>(&'a [u8; CAPABILITY]);

impl CapabilityRef<'_> {
    /// The relay's one copy of the capability the control stream brought, taken from the
    /// frame it arrived in, so that it outlives that frame's buffer.
    pub fn keep(self) -> Capability {
        Capability(*self.0)
    }

    /// Whether `other` is this capability, in time that does not depend on where they differ.
    pub fn matches(self, other: &Capability) -> bool {
        same(self.0, &other.0)
    }
}

/// Whether two capabilities' bytes are the same, looking at every byte whatever they hold.
fn same(a: &[u8; CAPABILITY], b: &[u8; CAPABILITY]) -> bool {
    let mut d = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        d |= x ^ y;
    }
    std::hint::black_box(d) == 0
}

impl Capability {
    /// This capability, lent.
    pub fn lend(&self) -> CapabilityRef<'_> {
        CapabilityRef(&self.0)
    }

    /// A fresh capability from the system's random source.
    pub fn mint() -> Result<Capability, getrandom::Error> {
        let mut c = Capability([0; CAPABILITY]);
        getrandom::fill(&mut c.0)?;
        Ok(c)
    }

    /// Whether `other` is this capability, in time that does not depend on where they differ.
    pub fn matches(&self, other: &Capability) -> bool {
        same(&self.0, &other.0)
    }

    fn read(r: &mut Reader<'_>) -> Result<Capability, Malformed> {
        let mut c = Capability([0; CAPABILITY]);
        c.0.copy_from_slice(r.bytes(CAPABILITY)?);
        Ok(c)
    }
}

impl Drop for Capability {
    fn drop(&mut self) {
        for b in self.0.iter_mut() {
            // SAFETY: `b` is a valid, aligned, exclusive reference into this capability.
            unsafe { std::ptr::write_volatile(b, 0) };
        }
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }
}

/// What a frame is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Track {
    /// One Opus packet.
    Audio,
    /// One video update: its `number` among the updates, from 0.
    Video { number: Number },
}

/// What every piece and repair of a block says about it, the same in each.
///
/// A block is what leaves together: the video update of one capture, with the Opus packet
/// that waits for it, or an Opus packet alone. Its members are frames, numbered on
/// from `first`; its bytes are a table of its members ([`Head::members`]) and then the
/// members' bytes, one after another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Head {
    /// Its number among the session's blocks, from 0.
    pub block: Seq,
    /// Its first member's number among the session's frames; the others follow it.
    pub first: Seq,
    pub members: NonZeroU8,
    /// Its length, table and members: it is cut into `symbols(bytes)` symbols.
    pub bytes: NonZeroU32,
    /// The repairs it is sent with.
    pub m: u16,
    /// Its deadline, on the relay's clock: a symbol that has not arrived by then is erased.
    pub due: Stamp,
}

impl Head {
    /// Its source symbols.
    pub fn k(&self) -> usize {
        1 + (self.bytes.get() as usize - 1) / SYMBOL
    }

    /// All its symbols, sources first.
    pub fn n(&self) -> usize {
        self.k() + self.m as usize
    }

    /// Source symbol `i`'s length.
    pub fn source_len(&self, i: usize) -> usize {
        (self.bytes.get() as usize - i * SYMBOL).min(SYMBOL)
    }

    /// Every repair's length: the longest source's, the first's.
    pub fn repair_len(&self) -> usize {
        (self.bytes.get() as usize).min(SYMBOL)
    }

    /// The length of its message `i` on the wire (sources first, then repairs).
    pub fn message_len(&self, i: usize) -> usize {
        HEADER
            + match i < self.k() {
                true => self.source_len(i),
                false => self.repair_len(),
            }
    }

    /// Its newest member's frame number.
    pub fn last(&self) -> Seq {
        Seq(self.first.0 + self.members.get() as u64 - 1)
    }

    /// Its members, from the block's bytes (once whole): each frame's number, track and
    /// bytes. A table that does not account for the block's bytes exactly is malformed.
    pub fn members<'a>(&self, block: &'a [u8]) -> Result<Members<'a>, Malformed> {
        let mut r = Reader(block);
        let mut lens = 0usize;
        for _ in 0..self.members.get() {
            read_track(&mut r)?;
            lens += r.u32()? as usize;
        }
        let table = block.len() - r.0.len();
        match lens == r.0.len() {
            true => Ok(Members {
                table: Reader(&block[..table]),
                body: r.0,
                next: self.first,
            }),
            false => Err(Malformed::Length),
        }
    }

    fn write(&self, w: &mut Writer<'_>) {
        w.u64(self.block.0);
        w.u64(self.first.0);
        w.u8(self.members.get());
        w.u32(self.bytes.get());
        w.u16(self.m);
        w.u32(self.due.0);
    }

    fn read(r: &mut Reader<'_>) -> Result<Head, Malformed> {
        let head = Head {
            block: Seq(r.u64()?),
            first: Seq(r.u64()?),
            members: NonZeroU8::new(r.u8()?).ok_or(Malformed::Empty)?,
            bytes: NonZeroU32::new(r.u32()?).ok_or(Malformed::Empty)?,
            m: r.u16()?,
            due: Stamp(r.u32()?),
        };
        match head.n() > WINDOW
            || head
                .first
                .0
                .checked_add(head.members.get() as u64)
                .is_none()
        {
            true => Err(Malformed::TooLarge),
            false => Ok(head),
        }
    }
}

/// One frame of a block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Member<'a> {
    pub frame: Seq,
    pub track: Track,
    pub bytes: &'a [u8],
}

/// A whole block's members, in order, read from a table [`Head::members`] has checked.
pub struct Members<'a> {
    table: Reader<'a>,
    body: &'a [u8],
    next: Seq,
}

impl<'a> Iterator for Members<'a> {
    type Item = Member<'a>;

    fn next(&mut self) -> Option<Member<'a>> {
        let track = read_track(&mut self.table).ok()?;
        let len = self.table.u32().ok()? as usize;
        let (bytes, rest) = self.body.split_at(len);
        self.body = rest;
        let frame = self.next;
        self.next = Seq(frame.0 + 1);
        Some(Member {
            frame,
            track,
            bytes,
        })
    }
}

/// Appends a block's table entry for a member of `track` and `len` bytes.
pub(super) fn write_entry(out: &mut Vec<u8>, track: Track, len: u32) {
    match track {
        Track::Audio => out.push(0),
        Track::Video { number } => {
            out.push(1);
            out.extend_from_slice(&number.0.to_be_bytes());
        }
    }
    out.extend_from_slice(&len.to_be_bytes());
}

fn read_track(r: &mut Reader<'_>) -> Result<Track, Malformed> {
    match r.u8()? {
        0 => Ok(Track::Audio),
        1 => Ok(Track::Video {
            number: Number(r.u64()?),
        }),
        t => Err(Malformed::Track(t)),
    }
}

/// One symbol of a frame, as a lane carries it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Symbol<'a> {
    pub head: Head,
    /// When it left, on the daemon's clock.
    pub sent: Stamp,
    /// Its index among the frame's sources (a piece, below k) or repairs (a repair, below m).
    pub index: u16,
    /// Its bytes: as long as its index makes it ([`Head::source_len`], [`Head::repair_len`]).
    pub data: &'a [u8],
}

/// A message on a lane from the daemon to the relay: on a datagram lane a piece or a repair
/// of a coded block, one a datagram; on a stream lane a frame as it is, one a framed message
/// ([`Framer::stream`]), in order and whole, since the stream delivers every byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lane<'a> {
    /// A source symbol, as it is in the frame.
    Piece(Symbol<'a>),
    /// A repair: the sum over the frame's sources `i` of `c(frame, index, i)` times source `i`
    /// ([`super::code::coefficient`]).
    Repair(Symbol<'a>),
    /// A whole frame: its number among the session's frames, what it is, its bytes.
    Frame {
        frame: Seq,
        track: Track,
        bytes: &'a [u8],
    },
}

const ATTACH: u8 = 1;
const PIECE: u8 = 2;
const REPAIR: u8 = 3;
const FRAME: u8 = 4;

/// The most bytes a stream lane's frame carries: as many as a datagram lane's block may
/// ([`WINDOW`] symbols), so that either lane takes every frame the encoder makes.
pub const MAX_FRAME: usize = WINDOW * SYMBOL;

/// A frame message's bytes before the frame's own: its tag, its number, and its track at the
/// longest (a video update's tag and number).
pub const FRAME_HEADER: usize = 1 + 8 + 1 + 8;

impl<'a> Lane<'a> {
    /// Writes a piece or repair into `out`, returning its length.
    pub fn encode_symbol(tag: Symbolic, s: &Symbol<'_>, out: &mut [u8; DATAGRAM]) -> usize {
        let mut w = Writer::new(out);
        w.u8(match tag {
            Symbolic::Piece => PIECE,
            Symbolic::Repair => REPAIR,
        });
        s.head.write(&mut w);
        w.u32(s.sent.0);
        w.u16(s.index);
        w.bytes(s.data);
        w.at
    }

    /// Appends a frame message to `out`, after its stream length ([`STREAM_PREFIX`]).
    /// `bytes` is at most [`MAX_FRAME`].
    pub fn encode_frame(frame: Seq, track: Track, bytes: &[u8], out: &mut Vec<u8>) {
        debug_assert!(bytes.len() <= MAX_FRAME);
        let start = out.len();
        out.extend_from_slice(&[0; STREAM_PREFIX]);
        out.push(FRAME);
        out.extend_from_slice(&frame.0.to_be_bytes());
        match track {
            Track::Audio => out.push(0),
            Track::Video { number } => {
                out.push(1);
                out.extend_from_slice(&number.0.to_be_bytes());
            }
        }
        out.extend_from_slice(bytes);
        let len = (out.len() - start - STREAM_PREFIX) as u32;
        out[start..start + STREAM_PREFIX].copy_from_slice(&len.to_be_bytes());
    }

    /// Reads one message from a lane: a datagram (a piece or a repair), or one framed message
    /// of a stream lane (a frame).
    pub fn decode(b: &'a [u8]) -> Result<Lane<'a>, Malformed> {
        let mut r = Reader(b);
        let tag = r.u8()?;
        if tag == FRAME {
            if b.len() > FRAME_HEADER + MAX_FRAME {
                return Err(Malformed::Long(b.len()));
            }
            let frame = Seq(r.u64()?);
            let track = read_track(&mut r)?;
            return Ok(Lane::Frame {
                frame,
                track,
                bytes: r.rest(),
            });
        }
        if b.len() > DATAGRAM {
            return Err(Malformed::Long(b.len()));
        }
        if tag != PIECE && tag != REPAIR {
            return Err(Malformed::Tag(tag));
        }
        let head = Head::read(&mut r)?;
        let sent = Stamp(r.u32()?);
        let index = r.u16()?;
        let len = match tag {
            PIECE if (index as usize) < head.k() => head.source_len(index as usize),
            REPAIR if index < head.m => head.repair_len(),
            _ => return Err(Malformed::Index),
        };
        let data = r.rest();
        if data.len() != len {
            return Err(Malformed::Length);
        }
        let s = Symbol {
            head,
            sent,
            index,
            data,
        };
        Ok(if tag == PIECE {
            Lane::Piece(s)
        } else {
            Lane::Repair(s)
        })
    }
}

/// Where a datagram's head holds its block's deadline, and where the datagram holds when it
/// left: after the tag, the block (8), the first frame (8), the members (1), the bytes (4) and
/// m (2); then the deadline (4) and the time it left (4). The daemon's lane writer stamps both
/// as the socket accepts the datagram ([`stamp_datagram`]).
const DUE_AT: usize = 1 + 8 + 8 + 1 + 4 + 2;
const SENT_AT: usize = DUE_AT + 4;

/// Stamps a piece's or repair's datagram, as [`Lane::encode_symbol`] wrote it, with its block's
/// deadline `due` and the time it left, `sent`.
pub fn stamp_datagram(datagram: &mut [u8], due: Stamp, sent: Stamp) {
    datagram[DUE_AT..DUE_AT + 4].copy_from_slice(&due.0.to_be_bytes());
    datagram[SENT_AT..SENT_AT + 4].copy_from_slice(&sent.0.to_be_bytes());
}

/// Which of a block's symbols a datagram carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Symbolic {
    Piece,
    Repair,
}

/// A lane's first message, from the relay: the session's capability, the lane's generation
/// (a lane reopened after it attached is the next), when it left on the relay's clock (the
/// daemon's first sample of the delay toward it, before any report), and the relay's measured
/// round trip (from starting its control connection to receiving the capability: at least one
/// round trip), which a deadline waits before any report gives the real figures.
pub struct Attach {
    pub capability: Capability,
    pub generation: Generation,
    pub sent: Stamp,
    pub round_trip: Duration,
}

impl Attach {
    /// Its length, always.
    pub const LEN: usize = 1 + CAPABILITY + 4 + 4 + 4;

    /// Its length as a stream lane's first message: after its own length ([`STREAM_PREFIX`]).
    pub const STREAM_LEN: usize = STREAM_PREFIX + Attach::LEN;

    /// Writes an attach of a lane with `capability`, as a datagram lane's first datagram. A
    /// round trip past what a `u32` of tenths holds (119 hours) is written as that much.
    pub fn encode(
        capability: &Capability,
        generation: Generation,
        sent: Stamp,
        round_trip: Duration,
    ) -> [u8; Attach::LEN] {
        let mut out = [0; Attach::LEN];
        Attach::write(
            &mut Writer::new(&mut out),
            capability,
            generation,
            sent,
            round_trip,
        );
        out
    }

    /// Writes an attach as a stream lane's first message: its length, then the attach.
    pub fn encode_stream(
        capability: &Capability,
        generation: Generation,
        sent: Stamp,
        round_trip: Duration,
    ) -> [u8; Attach::STREAM_LEN] {
        let mut out = [0; Attach::STREAM_LEN];
        let mut w = Writer::new(&mut out);
        w.u32(Attach::LEN as u32);
        Attach::write(&mut w, capability, generation, sent, round_trip);
        out
    }

    fn write(
        w: &mut Writer,
        capability: &Capability,
        generation: Generation,
        sent: Stamp,
        round_trip: Duration,
    ) {
        w.u8(ATTACH);
        w.bytes(&capability.0);
        w.u32(generation.0);
        w.u32(sent.0);
        w.u32(tenths(round_trip).clamp(0, u32::MAX as i64) as u32);
    }

    /// Reads a stream lane's first message, read whole as [`Attach::STREAM_LEN`] bytes: a
    /// length that is not an attach's is an error, as is anything but an attach after it.
    pub fn decode_stream(b: &[u8; Attach::STREAM_LEN]) -> Result<Attach, Malformed> {
        let (len, attach) = b.split_at(STREAM_PREFIX);
        match Reader(len).u32()? as usize {
            Attach::LEN => Attach::decode(attach),
            _ => Err(Malformed::Length),
        }
    }

    /// Reads a lane's first message; anything but an attach is an error.
    pub fn decode(b: &[u8]) -> Result<Attach, Malformed> {
        let mut r = Reader(b);
        match r.u8()? {
            ATTACH => {}
            t => return Err(Malformed::Tag(t)),
        }
        let a = Attach {
            capability: Capability::read(&mut r)?,
            generation: Generation(r.u32()?),
            sent: Stamp(r.u32()?),
            round_trip: from_tenths(r.u32()? as i64),
        };
        r.end()?;
        Ok(a)
    }
}

/// A control message from the daemon to the relay.
pub enum Down<'a> {
    /// The session's capability: the first message, and only then.
    Capability(CapabilityRef<'a>),
    /// The lane of this generation has attached with it.
    Attached { generation: Generation },
    /// The bytes the daemon gave its lane socket's send buffer, whenever it sizes it: what one
    /// deadline holds at the drain rate. The relay gives its lane socket's receive buffer the
    /// same.
    Buffer { bytes: u32 },
    /// A text message for the page (its hello, a clipboard, ...), as the page reads it.
    Text(&'a str),
    /// A binary message for the page (a cursor's image, ...).
    Binary(&'a [u8]),
}

const D_CAPABILITY: u8 = 1;
const D_ATTACHED: u8 = 2;
const D_TEXT: u8 = 3;
const D_BINARY: u8 = 4;
const D_BUFFER: u8 = 5;

impl<'a> Down<'a> {
    /// Appends the message to `out`, after its length.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let start = Control::open(out);
        match self {
            Down::Capability(c) => {
                out.push(D_CAPABILITY);
                out.extend_from_slice(c.0);
            }
            Down::Attached { generation } => {
                out.push(D_ATTACHED);
                out.extend_from_slice(&generation.0.to_be_bytes());
            }
            Down::Buffer { bytes } => {
                out.push(D_BUFFER);
                out.extend_from_slice(&bytes.to_be_bytes());
            }
            Down::Text(t) => {
                out.push(D_TEXT);
                out.extend_from_slice(t.as_bytes());
            }
            Down::Binary(b) => {
                out.push(D_BINARY);
                out.extend_from_slice(b);
            }
        }
        Control::close(out, start);
    }

    /// Reads one message, as [`Framer::frame`] gives it (without its length).
    pub fn decode(b: &'a [u8]) -> Result<Down<'a>, Malformed> {
        let mut r = Reader(b);
        let d = match r.u8()? {
            D_CAPABILITY => {
                let bytes = <&[u8; CAPABILITY]>::try_from(r.bytes(CAPABILITY)?)
                    .map_err(|_| Malformed::Truncated)?;
                Down::Capability(CapabilityRef(bytes))
            }
            D_ATTACHED => Down::Attached {
                generation: Generation(r.u32()?),
            },
            D_BUFFER => Down::Buffer { bytes: r.u32()? },
            D_TEXT => return text(r.rest()).map(Down::Text),
            D_BINARY => return Ok(Down::Binary(r.rest())),
            t => return Err(Malformed::Tag(t)),
        };
        r.end()?;
        Ok(d)
    }
}

/// A control message from the relay to the daemon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Up<'a> {
    /// A text message from the page (input, a clipboard, ...), as the page wrote it.
    Text(&'a str),
    /// A binary message from the page (the microphone, a clipboard image).
    Binary(&'a [u8]),
    /// A datagram lane's block decided. A stream lane has no reports: it delivers every frame,
    /// in order.
    Report(Report<'a>),
}

const U_TEXT: u8 = 1;
const U_BINARY: u8 = 2;
const U_REPORT: u8 = 3;

impl<'a> Up<'a> {
    /// Appends the message to `out`, after its length.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let start = Control::open(out);
        match self {
            Up::Text(t) => {
                out.push(U_TEXT);
                out.extend_from_slice(t.as_bytes());
            }
            Up::Binary(b) => {
                out.push(U_BINARY);
                out.extend_from_slice(b);
            }
            Up::Report(report) => {
                out.push(U_REPORT);
                report.write(out);
            }
        }
        Control::close(out, start);
    }

    /// Reads one message, as [`Framer::frame`] gives it (without its length).
    pub fn decode(b: &'a [u8]) -> Result<Up<'a>, Malformed> {
        let mut r = Reader(b);
        match r.u8()? {
            U_TEXT => text(r.rest()).map(Up::Text),
            U_BINARY => Ok(Up::Binary(r.rest())),
            U_REPORT => {
                let report = Report::read(&mut r)?;
                r.end()?;
                Ok(Up::Report(report))
            }
            t => Err(Malformed::Tag(t)),
        }
    }
}

/// What the relay says of one block once it is decided: every block is reported, in the
/// order of their numbers, and the report is the only answer the daemon gets for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Report<'a> {
    pub block: Seq,
    /// When the report left, on the relay's clock: with its arrival on the daemon's, the
    /// daemon's sample of the delay toward it.
    pub sent: Stamp,
    pub fate: Fate<'a>,
    /// The pieces and repairs that arrived after their block's deadline, or after it was
    /// decided, since the last report, each of this block or an earlier one: which symbol it
    /// was, and its delay. The arrivals the deadline left out: a late symbol is delay, never
    /// loss, and without its delay the spread of arrivals could only be read below the
    /// deadline it sets.
    pub late: Late<'a>,
}

/// What became of a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fate<'a> {
    /// Nothing of it arrived by its deadline; the relay knows it only by its number.
    Unseen,
    /// Something of it arrived: whether it was decoded by its deadline, and when each of its
    /// n symbols arrived by it.
    Seen {
        outcome: Outcome,
        arrivals: Arrivals<'a>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Decoded,
    Missed,
}

/// Each of a frame's n symbols (sources then repairs): erased, or its delay
/// ([`OneWay::of`] its `sent` and its arrival) if it arrived by the deadline. As it is on the
/// wire: a bitmap of the arrived, then their delays in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Arrivals<'a> {
    n: u16,
    bits: &'a [u8],
    delays: &'a [u8],
}

impl<'a> Arrivals<'a> {
    /// Arrivals as the relay wrote them into `bits` (`n.div_ceil(8)` bytes, bit `i % 8` of
    /// byte `i / 8` for symbol `i`) and `delays` (4 bytes for each set bit, in order).
    pub fn new(n: u16, bits: &'a [u8], delays: &'a [u8]) -> Result<Arrivals<'a>, Malformed> {
        let ones: usize = bits.iter().map(|b| b.count_ones() as usize).sum();
        match () {
            _ if n as usize > WINDOW => Err(Malformed::TooLarge),
            _ if bits.len() != (n as usize).div_ceil(8) => Err(Malformed::Length),
            _ if n % 8 != 0 && bits[bits.len() - 1] >> (n % 8) != 0 => Err(Malformed::Length),
            _ if delays.len() != DELAY * ones => Err(Malformed::Length),
            _ => Ok(Arrivals { n, bits, delays }),
        }
    }

    pub fn n(&self) -> usize {
        self.n as usize
    }

    /// Arrivals the decoder wrote, which hold by how it wrote them.
    pub(super) fn written(n: u16, bits: &'a [u8], delays: &'a [u8]) -> Arrivals<'a> {
        debug_assert!(Arrivals::new(n, bits, delays).is_ok());
        Arrivals { n, bits, delays }
    }

    /// Each symbol's delay, or `None` if it was erased.
    pub fn iter(&self) -> impl Iterator<Item = Option<OneWay>> + 'a {
        let (bits, delays) = (self.bits, self.delays);
        let mut next = 0;
        (0..self.n as usize).map(move |i| {
            (bits[i / 8] >> (i % 8) & 1 == 1).then(|| {
                let d = &delays[next..next + DELAY];
                next += DELAY;
                OneWay(u32::from_be_bytes([d[0], d[1], d[2], d[3]]))
            })
        })
    }
}

/// A symbol that arrived after its block's deadline: which it was, and its delay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LateArrival {
    pub block: Seq,
    /// Its position among the block's n symbols: a source's index, or k and a repair's.
    pub position: u16,
    pub delay: OneWay,
}

impl LateArrival {
    pub(super) fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.block.0.to_be_bytes());
        out.extend_from_slice(&self.position.to_be_bytes());
        out.extend_from_slice(&self.delay.0.to_be_bytes());
    }
}

/// Late arrivals, as on the wire: [`LATE_ENTRY`] bytes each, at most [`LATE`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Late<'a>(&'a [u8]);

impl<'a> Late<'a> {
    /// Late arrivals as the relay wrote them for the report of `block`: each of it or an
    /// earlier block.
    pub fn new(b: &'a [u8], block: Seq) -> Result<Late<'a>, Malformed> {
        match () {
            _ if !b.len().is_multiple_of(LATE_ENTRY) => Err(Malformed::Length),
            _ if b.len() / LATE_ENTRY > LATE => Err(Malformed::TooLarge),
            _ if Late(b).iter().any(|l| l.block > block) => Err(Malformed::Ahead),
            _ => Ok(Late(b)),
        }
    }

    /// Late arrivals the decoder wrote, which hold by how it wrote them.
    pub(super) fn written(b: &'a [u8], block: Seq) -> Late<'a> {
        debug_assert!(Late::new(b, block).is_ok());
        Late(b)
    }

    pub fn len(&self) -> usize {
        self.0.len() / LATE_ENTRY
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = LateArrival> + 'a {
        self.0
            .as_chunks::<LATE_ENTRY>()
            .0
            .iter()
            .map(|e| LateArrival {
                block: Seq(u64::from_be_bytes([
                    e[0], e[1], e[2], e[3], e[4], e[5], e[6], e[7],
                ])),
                position: u16::from_be_bytes([e[8], e[9]]),
                delay: OneWay(u32::from_be_bytes([e[10], e[11], e[12], e[13]])),
            })
    }
}

const UNSEEN: u8 = 0;
const DECODED: u8 = 1;
const MISSED: u8 = 2;

impl<'a> Report<'a> {
    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.block.0.to_be_bytes());
        out.extend_from_slice(&self.sent.0.to_be_bytes());
        match self.fate {
            Fate::Unseen => out.push(UNSEEN),
            Fate::Seen { outcome, arrivals } => {
                out.push(match outcome {
                    Outcome::Decoded => DECODED,
                    Outcome::Missed => MISSED,
                });
                out.extend_from_slice(&arrivals.n.to_be_bytes());
                out.extend_from_slice(arrivals.bits);
                out.extend_from_slice(arrivals.delays);
            }
        }
        out.extend_from_slice(&(self.late.len() as u32).to_be_bytes());
        out.extend_from_slice(self.late.0);
    }

    fn read(r: &mut Reader<'a>) -> Result<Report<'a>, Malformed> {
        let block = Seq(r.u64()?);
        let sent = Stamp(r.u32()?);
        let fate = match r.u8()? {
            UNSEEN => Fate::Unseen,
            o @ (DECODED | MISSED) => {
                let n = r.u16()?;
                if n as usize > WINDOW {
                    return Err(Malformed::TooLarge);
                }
                let bits = r.bytes((n as usize).div_ceil(8))?;
                let ones: usize = bits.iter().map(|b| b.count_ones() as usize).sum();
                let delays = r.bytes(DELAY * ones)?;
                Fate::Seen {
                    outcome: if o == DECODED {
                        Outcome::Decoded
                    } else {
                        Outcome::Missed
                    },
                    arrivals: Arrivals::new(n, bits, delays)?,
                }
            }
            o => return Err(Malformed::Outcome(o)),
        };
        let late = r.u32()? as usize;
        if late > LATE {
            return Err(Malformed::TooLarge);
        }
        let late = Late::new(r.bytes(LATE_ENTRY * late)?, block)?;
        Ok(Report {
            block,
            sent,
            fate,
            late,
        })
    }
}

/// Why bytes are not a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Malformed {
    /// It ends before a field it must hold.
    Truncated,
    /// Bytes follow its end.
    Trailing,
    /// No message has this tag (here).
    Tag(u8),
    /// No track has this tag.
    Track(u8),
    /// No outcome has this tag.
    Outcome(u8),
    /// A frame of no bytes.
    Empty,
    /// A frame, or a report of one, of more than [`WINDOW`] symbols; or more than [`LATE`]
    /// late arrivals.
    TooLarge,
    /// A late arrival of a block after the one its report is of.
    Ahead,
    /// A piece's index at or past k, or a repair's at or past m.
    Index,
    /// A part whose length is not the one the message's other fields make it.
    Length,
    /// Text that is not UTF-8.
    Utf8,
    /// A framed length, or a datagram, over the longest message of its kind.
    Long(usize),
}

impl std::fmt::Display for Malformed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Malformed::Truncated => write!(f, "the message ends early"),
            Malformed::Trailing => write!(f, "bytes follow the message"),
            Malformed::Tag(t) => write!(f, "no message has tag {t}"),
            Malformed::Track(t) => write!(f, "no track has tag {t}"),
            Malformed::Outcome(t) => write!(f, "no outcome has tag {t}"),
            Malformed::Empty => write!(f, "a frame of no bytes"),
            Malformed::TooLarge => write!(f, "a frame or report too large"),
            Malformed::Ahead => write!(f, "a late arrival of a block not yet reported"),
            Malformed::Index => write!(f, "a symbol index out of its frame"),
            Malformed::Length => write!(f, "a part of the wrong length"),
            Malformed::Utf8 => write!(f, "text that is not UTF-8"),
            Malformed::Long(n) => write!(f, "a message of {n} bytes, over the longest"),
        }
    }
}

impl std::error::Error for Malformed {}

fn text(b: &[u8]) -> Result<&str, Malformed> {
    std::str::from_utf8(b).map_err(|_| Malformed::Utf8)
}

/// The control stream's framing: a 4-byte length before each message.
struct Control;

impl Control {
    fn open(out: &mut Vec<u8>) -> usize {
        out.extend_from_slice(&[0; CONTROL_PREFIX]);
        out.len()
    }

    fn close(out: &mut [u8], start: usize) {
        let len = out.len() - start;
        debug_assert!(len <= MAX_CONTROL, "a control message of {len} bytes");
        out[start - CONTROL_PREFIX..start].copy_from_slice(&(len as u32).to_be_bytes());
    }
}

/// The length a stream lane writes before each frame message: a `u32`, which holds
/// [`FRAME_HEADER`] + [`MAX_FRAME`].
pub const STREAM_PREFIX: usize = std::mem::size_of::<u32>();

/// The control stream's length before each message: a `u32`, which holds [`MAX_CONTROL`].
pub const CONTROL_PREFIX: usize = std::mem::size_of::<u32>();

/// Cuts a stream into its messages: a stream lane's (its attach and its frames, 4-byte lengths,
/// at most [`FRAME_HEADER`] + [`MAX_FRAME`]) or the control stream's (4-byte lengths, at most
/// [`MAX_CONTROL`]).
///
/// It holds only bytes that arrived: a length is checked against the maximum as soon as it
/// is whole, and room grows only as bytes come, never by a length read.
pub struct Framer {
    buf: Vec<u8>,
    /// Bytes at the front already handed out.
    used: usize,
    prefix: usize,
    max: usize,
}

impl Framer {
    pub fn stream() -> Framer {
        Framer {
            buf: Vec::new(),
            used: 0,
            prefix: STREAM_PREFIX,
            max: FRAME_HEADER + MAX_FRAME,
        }
    }

    pub fn control() -> Framer {
        Framer {
            buf: Vec::new(),
            used: 0,
            prefix: CONTROL_PREFIX,
            max: MAX_CONTROL,
        }
    }

    /// Bytes as they came from the stream.
    pub fn push(&mut self, bytes: &[u8]) {
        self.compact();
        self.buf.extend_from_slice(bytes);
    }

    /// The next whole message, if one is in; an error is final (the stream is not framed).
    pub fn frame(&mut self) -> Result<Option<&[u8]>, Malformed> {
        self.compact();
        let Some(prefix) = self.buf.get(..self.prefix) else {
            return Ok(None);
        };
        let len = prefix.iter().fold(0usize, |len, &b| len << 8 | b as usize);
        if len > self.max {
            return Err(Malformed::Long(len));
        }
        let end = self.prefix + len;
        if self.buf.len() < end {
            return Ok(None);
        }
        self.used = end;
        Ok(Some(&self.buf[self.prefix..end]))
    }

    fn compact(&mut self) {
        if self.used > 0 {
            self.buf.drain(..self.used);
            self.used = 0;
        }
    }
}

/// Reads fields from the front of a message.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], Malformed> {
        if self.0.len() < n {
            return Err(Malformed::Truncated);
        }
        let (b, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(b)
    }

    fn u8(&mut self) -> Result<u8, Malformed> {
        Ok(self.bytes(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, Malformed> {
        let b = self.bytes(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, Malformed> {
        let b = self.bytes(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64, Malformed> {
        let b = self.bytes(8)?;
        let mut a = [0; 8];
        a.copy_from_slice(b);
        Ok(u64::from_be_bytes(a))
    }

    fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.0)
    }

    fn end(&self) -> Result<(), Malformed> {
        match self.0 {
            [] => Ok(()),
            _ => Err(Malformed::Trailing),
        }
    }
}

/// Writes fields into a buffer that holds them (a lane message's, sized [`DATAGRAM`]).
struct Writer<'a> {
    out: &'a mut [u8],
    at: usize,
}

impl<'a> Writer<'a> {
    fn new(out: &'a mut [u8]) -> Writer<'a> {
        Writer { out, at: 0 }
    }

    fn bytes(&mut self, b: &[u8]) {
        self.out[self.at..self.at + b.len()].copy_from_slice(b);
        self.at += b.len();
    }

    fn u8(&mut self, v: u8) {
        self.bytes(&[v]);
    }

    fn u16(&mut self, v: u16) {
        self.bytes(&v.to_be_bytes());
    }

    fn u32(&mut self, v: u32) {
        self.bytes(&v.to_be_bytes());
    }

    fn u64(&mut self, v: u64) {
        self.bytes(&v.to_be_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xorshift::Xorshift;

    fn bytes(rng: &mut Xorshift, n: usize) -> Vec<u8> {
        (0..n).map(|_| rng.next() as u8).collect()
    }

    fn track(rng: &mut Xorshift) -> Track {
        match rng.below(2) {
            0 => Track::Audio,
            _ => Track::Video {
                number: Number(rng.below(1 << 40)),
            },
        }
    }

    fn head(rng: &mut Xorshift) -> Head {
        let k = rng.range(1, 60) as usize;
        Head {
            block: Seq(rng.below(1 << 60)),
            first: Seq(rng.below(1 << 60)),
            members: NonZeroU8::new(rng.range(1, 256) as u8).unwrap(),
            bytes: NonZeroU32::new(
                ((k - 1) * SYMBOL) as u32 + rng.range(1, SYMBOL as u64 + 1) as u32,
            )
            .unwrap(),
            m: rng.below(40) as u16,
            due: Stamp(rng.next() as u32),
        }
    }

    /// A block's members written as an encoder writes them, read back exactly; and any table
    /// cut, extended or with a byte changed is either refused or read as a table that accounts
    /// for the block's bytes exactly, never a panic.
    #[test]
    fn a_block_is_its_members() {
        let mut rng = Xorshift::new(0x7137_4491_B5C0_FBCF);
        for _ in 0..2000 {
            let members: Vec<(Track, Vec<u8>)> = (0..rng.range(1, 30))
                .map(|_| {
                    let t = track(&mut rng);
                    let n = rng.range(1, 3000) as usize;
                    (t, bytes(&mut rng, n))
                })
                .collect();
            let mut block = Vec::new();
            for (t, b) in &members {
                write_entry(&mut block, *t, b.len() as u32);
            }
            for (_, b) in &members {
                block.extend_from_slice(b);
            }
            let first = Seq(rng.below(1 << 60));
            let head = Head {
                block: Seq(0),
                first,
                members: NonZeroU8::new(members.len() as u8).unwrap(),
                bytes: NonZeroU32::new(block.len() as u32).unwrap(),
                m: 0,
                due: Stamp(0),
            };
            let got: Vec<(Seq, Track, Vec<u8>)> = head
                .members(&block)
                .unwrap()
                .map(|m| (m.frame, m.track, m.bytes.to_vec()))
                .collect();
            let want: Vec<(Seq, Track, Vec<u8>)> = members
                .iter()
                .enumerate()
                .map(|(i, (t, b))| (Seq(first.0 + i as u64), *t, b.clone()))
                .collect();
            assert_eq!(got, want);
            let mut bad = block.clone();
            match rng.below(3) {
                0 => bad.truncate(rng.below(bad.len() as u64) as usize),
                1 => bad.push(rng.next() as u8),
                _ => {
                    let i = rng.below(bad.len() as u64) as usize;
                    bad[i] ^= 1 << rng.below(8);
                }
            }
            if let Ok(ms) = head.members(&bad) {
                let total: usize = ms.map(|m| m.bytes.len()).sum();
                assert!(total <= bad.len());
            }
        }
    }

    /// A valid lane message, its bytes.
    fn lane(rng: &mut Xorshift) -> Vec<u8> {
        let h = head(rng);
        let piece = h.m == 0 || rng.below(2) == 0;
        let index = match piece {
            true => rng.below(h.k() as u64) as u16,
            false => rng.below(h.m as u64) as u16,
        };
        let len = match piece {
            true => h.source_len(index as usize),
            false => h.repair_len(),
        };
        let data = bytes(rng, len);
        let s = Symbol {
            head: h,
            sent: Stamp(rng.next() as u32),
            index,
            data: &data,
        };
        let mut out = [0; DATAGRAM];
        let tag = match piece {
            true => Symbolic::Piece,
            false => Symbolic::Repair,
        };
        let n = Lane::encode_symbol(tag, &s, &mut out);
        out[..n].to_vec()
    }

    /// A valid stream lane frame message, its bytes without its length.
    fn frame(rng: &mut Xorshift) -> Vec<u8> {
        let t = track(rng);
        let n = rng.below(3000) as usize;
        let data = bytes(rng, n);
        let mut out = Vec::new();
        Lane::encode_frame(Seq(rng.below(1 << 60)), t, &data, &mut out);
        out[STREAM_PREFIX..].to_vec()
    }

    /// A lane message: a datagram's or a stream's.
    fn any_lane(rng: &mut Xorshift) -> Vec<u8> {
        match rng.below(2) {
            0 => lane(rng),
            _ => frame(rng),
        }
    }

    fn report(rng: &mut Xorshift) -> Vec<u8> {
        let n = rng.range(1, 300) as u16;
        let mut bits = vec![0u8; (n as usize).div_ceil(8)];
        let mut delays = Vec::new();
        for i in 0..n as usize {
            if rng.below(3) > 0 {
                bits[i / 8] |= 1 << (i % 8);
                delays.extend_from_slice(&(rng.next() as u32).to_be_bytes());
            }
        }
        let block = Seq(rng.next());
        let mut late = Vec::new();
        for _ in 0..rng.below(300) {
            LateArrival {
                block: Seq(rng.below(block.0.saturating_add(1))),
                position: rng.next() as u16,
                delay: OneWay(rng.next() as u32),
            }
            .write(&mut late);
        }
        let fate = match rng.below(3) {
            0 => Fate::Unseen,
            o => Fate::Seen {
                outcome: if o == 1 {
                    Outcome::Decoded
                } else {
                    Outcome::Missed
                },
                arrivals: Arrivals::new(n, &bits, &delays).unwrap(),
            },
        };
        let mut out = Vec::new();
        Up::Report(Report {
            block,
            sent: Stamp(rng.next() as u32),
            fate,
            late: Late::new(&late, block).unwrap(),
        })
        .encode(&mut out);
        out[CONTROL_PREFIX..].to_vec()
    }

    fn down(rng: &mut Xorshift) -> Vec<u8> {
        let mut out = Vec::new();
        let text: String = (0..rng.below(50))
            .map(|_| char::from_u32(rng.range(32, 0x2FFF) as u32).unwrap_or('x'))
            .collect();
        let bin = {
            let n = rng.below(100) as usize;
            bytes(rng, n)
        };
        let cap = Capability(bytes(rng, CAPABILITY).try_into().unwrap());
        match rng.below(5) {
            0 => Down::Capability(cap.lend()).encode(&mut out),
            1 => Down::Attached {
                generation: Generation(rng.next() as u32),
            }
            .encode(&mut out),
            2 => Down::Buffer {
                bytes: rng.next() as u32,
            }
            .encode(&mut out),
            3 => Down::Text(&text).encode(&mut out),
            _ => Down::Binary(&bin).encode(&mut out),
        }
        out[CONTROL_PREFIX..].to_vec()
    }

    fn up(rng: &mut Xorshift) -> Vec<u8> {
        let mut out = Vec::new();
        match rng.below(3) {
            0 => return report(rng),
            1 => Up::Text("k,65,1").encode(&mut out),
            _ => Up::Binary(&{
                let n = rng.below(100) as usize;
                bytes(rng, n)
            })
            .encode(&mut out),
        }
        out[CONTROL_PREFIX..].to_vec()
    }

    fn attach(rng: &mut Xorshift) -> Vec<u8> {
        let cap = Capability(bytes(rng, CAPABILITY).try_into().unwrap());
        Attach::encode(
            &cap,
            Generation(rng.next() as u32),
            Stamp(rng.next() as u32),
            from_tenths(rng.next() as u32 as i64),
        )
        .to_vec()
    }

    fn stream_attach(rng: &mut Xorshift) -> Vec<u8> {
        let cap = Capability(bytes(rng, CAPABILITY).try_into().unwrap());
        Attach::encode_stream(
            &cap,
            Generation(rng.next() as u32),
            Stamp(rng.next() as u32),
            from_tenths(rng.next() as u32 as i64),
        )
        .to_vec()
    }

    /// Each kind's decoder re-encoded: `None` if it refuses the bytes, else the bytes its
    /// decoding encodes to.
    fn again(kind: usize, b: &[u8]) -> Option<Vec<u8>> {
        match kind {
            0 => Lane::decode(b).ok().map(|l| {
                let mut out = [0; DATAGRAM];
                match l {
                    Lane::Piece(s) => {
                        let n = Lane::encode_symbol(Symbolic::Piece, &s, &mut out);
                        out[..n].to_vec()
                    }
                    Lane::Repair(s) => {
                        let n = Lane::encode_symbol(Symbolic::Repair, &s, &mut out);
                        out[..n].to_vec()
                    }
                    Lane::Frame {
                        frame,
                        track,
                        bytes,
                    } => {
                        let mut out = Vec::new();
                        Lane::encode_frame(frame, track, bytes, &mut out);
                        out[STREAM_PREFIX..].to_vec()
                    }
                }
            }),
            1 => Attach::decode(b).ok().map(|a| {
                Attach::encode(&a.capability, a.generation, a.sent, a.round_trip).to_vec()
            }),
            2 => Down::decode(b).ok().map(|d| {
                let mut out = Vec::new();
                d.encode(&mut out);
                out[CONTROL_PREFIX..].to_vec()
            }),
            3 => <&[u8; Attach::STREAM_LEN]>::try_from(b)
                .ok()
                .and_then(|b| Attach::decode_stream(b).ok())
                .map(|a| {
                    Attach::encode_stream(&a.capability, a.generation, a.sent, a.round_trip)
                        .to_vec()
                }),
            _ => Up::decode(b).ok().map(|u| {
                let mut out = Vec::new();
                u.encode(&mut out);
                out[CONTROL_PREFIX..].to_vec()
            }),
        }
    }

    /// Every message of every kind decodes to what encodes it again, byte for byte; and
    /// every input at all (random bytes, valid messages with bytes flipped, cut or added)
    /// either is refused with a typed error or is such a message: nothing panics, and nothing
    /// decodes to a message that is not exactly its bytes.
    #[test]
    fn every_input_is_a_message_or_a_typed_error() {
        let mut rng = Xorshift::new(0x082E_FA98_EC4E_6C89);
        let makers: [fn(&mut Xorshift) -> Vec<u8>; 5] = [any_lane, attach, down, stream_attach, up];
        for _ in 0..20_000 {
            let kind = rng.below(5) as usize;
            let valid = makers[kind](&mut rng);
            assert_eq!(
                again(kind, &valid).as_deref(),
                Some(&valid[..]),
                "kind {kind}"
            );
            let mut b = match rng.below(4) {
                0 => {
                    let n = rng.below(1500) as usize;
                    bytes(&mut rng, n)
                }
                _ => valid.clone(),
            };
            match rng.below(4) {
                0 if !b.is_empty() => {
                    let i = rng.below(b.len() as u64) as usize;
                    b[i] ^= 1 << rng.below(8);
                }
                1 => b.truncate(rng.below(b.len() as u64 + 1) as usize),
                2 => b.extend({
                    let n = rng.range(1, 9) as usize;
                    bytes(&mut rng, n)
                }),
                _ => {
                    if !b.is_empty() {
                        b[0] = rng.below(8) as u8;
                    }
                }
            }
            for k in 0..5 {
                if let Some(out) = again(k, &b) {
                    assert_eq!(out, b, "kind {k} decoded bytes it does not encode to");
                }
            }
        }
    }

    /// Every strict prefix of a message whose fields fix its length (a piece or repair, an
    /// attach, a report, and the daemon's capability, attached and buffer) is refused. (A
    /// frame's length is its stream's framing's, not its own fields'.)
    #[test]
    fn every_cut_message_is_refused() {
        let mut rng = Xorshift::new(0x4528_21E6_38D0_1377);
        for _ in 0..2000 {
            let (kind, valid) = match rng.below(4) {
                0 => (0, lane(&mut rng)),
                1 => (1, attach(&mut rng)),
                2 => (3, report(&mut rng)),
                _ => loop {
                    let d = down(&mut rng);
                    if !matches!(d[0], D_TEXT | D_BINARY) {
                        break (2, d);
                    }
                },
            };
            for cut in 0..valid.len() {
                assert!(
                    again(kind, &valid[..cut]).is_none(),
                    "kind {kind} cut at {cut}"
                );
            }
        }
    }

    /// Messages framed one after another and pushed in pieces of any size come out whole, in
    /// order; a length over the kind's longest is an error as soon as it is whole.
    #[test]
    fn framing_cuts_a_stream_back_into_its_messages() {
        let mut rng = Xorshift::new(0xBE54_66CF_34E9_0C6C);
        for case in 0..300 {
            let control = case % 2 == 0;
            let messages: Vec<Vec<u8>> = (0..rng.range(1, 20))
                .map(|_| match control {
                    true => up(&mut rng),
                    false => frame(&mut rng),
                })
                .collect();
            let mut stream = Vec::new();
            for m in &messages {
                match control {
                    true => stream.extend_from_slice(&(m.len() as u32).to_be_bytes()),
                    false => stream.extend_from_slice(&(m.len() as u32).to_be_bytes()),
                }
                stream.extend_from_slice(m);
            }
            let mut f = if control {
                Framer::control()
            } else {
                Framer::stream()
            };
            let mut got = Vec::new();
            let mut at = 0;
            while at < stream.len() {
                let step = rng.range(1, 700) as usize;
                let end = (at + step).min(stream.len());
                f.push(&stream[at..end]);
                at = end;
                while let Some(m) = f.frame().unwrap() {
                    got.push(m.to_vec());
                }
            }
            assert_eq!(got, messages);
        }
        let mut f = Framer::stream();
        let long = FRAME_HEADER + MAX_FRAME + 1;
        f.push(&(long as u32).to_be_bytes());
        assert_eq!(f.frame(), Err(Malformed::Long(long)));
        let mut f = Framer::control();
        f.push(&((MAX_CONTROL + 1) as u32).to_be_bytes());
        assert_eq!(f.frame(), Err(Malformed::Long(MAX_CONTROL + 1)));
    }

    /// A datagram stamped as its socket takes it reads back with the deadline and the time it
    /// left it was stamped with, and with everything else as it was written.
    #[test]
    fn a_stamped_datagram_says_its_stamps() {
        let mut rng = Xorshift::new(0x9B05_688C_2B3E_6C1F);
        for _ in 0..2000 {
            let mut b = lane(&mut rng);
            let (due, sent) = (Stamp(rng.next() as u32), Stamp(rng.next() as u32));
            let before = match Lane::decode(&b) {
                Ok(Lane::Piece(s) | Lane::Repair(s)) => (s.head, s.index, s.data.to_vec()),
                other => panic!("a symbol: {other:?}"),
            };
            stamp_datagram(&mut b, due, sent);
            match Lane::decode(&b) {
                Ok(Lane::Piece(s) | Lane::Repair(s)) => {
                    assert_eq!(s.head, Head { due, ..before.0 });
                    assert_eq!((s.sent, s.index, s.data), (sent, before.1, &before.2[..]));
                }
                other => panic!("a symbol: {other:?}"),
            }
        }
    }

    /// Two capabilities match exactly when their bytes are equal, wherever they differ.
    #[test]
    fn capabilities_match_only_themselves() {
        let mut rng = Xorshift::new(0xC0AC_29B7_C97C_50DD);
        for _ in 0..1000 {
            let b = bytes(&mut rng, CAPABILITY);
            let mut c = b.clone();
            if rng.below(2) == 0 {
                c[rng.below(CAPABILITY as u64) as usize] ^= 1 << rng.below(8);
            }
            let x = Capability(b.clone().try_into().unwrap());
            let y = Capability(c.clone().try_into().unwrap());
            assert_eq!(x.matches(&y), b == c);
        }
    }
}
