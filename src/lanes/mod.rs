//! Lanes: how video and audio cross the path from the daemon to the relay, through a transport
//! that owns the rate.
//!
//! A session has one lane, of one of two kinds:
//!
//! - **Datagram** (Hysteria2's UDP relay): each message alone, lost or not. Frames go coded:
//!   a block (one capture's update with the Opus packet waiting for it, or an Opus packet
//!   alone) is cut into k source symbols and sent with m repairs, and a block not whole by its
//!   deadline is missed. The relay reports every block; the reports size the repairs and the
//!   deadline.
//! - **Stream** (one HTTP CONNECT stream through naive): every byte arrives, in order. Frames
//!   go as they are, one framed message each ([`wire::Lane::Frame`]); there is no code, no
//!   deadline and no report.
//!
//! The rate is the transport's. The daemon writes a message only when the lane's socket
//! accepts it, keeps what waits in one queue, and never discards one
//! ([`crate::session::sender`]); its capture and its encoder follow what the socket drains.
//! voxl runs no congestion control of its own.
//!
//! Everything here is pure and synchronous: no I/O, no clock reads, no tokio. Times come in
//! as [`wire::Stamp`]s, the tenths of a millisecond a side's own clock reads; the two sides'
//! clocks are never compared with each other, only samples taken across them with samples
//! taken the same way, so their offset cancels.
//!
//! - [`gf`]: the field, GF(2^8) over x^8 + x^4 + x^3 + x^2 + 1 (0x11d).
//! - [`wire`]: every message, with fixed maximum sizes, length-prefixed framing for streams
//!   ([`wire::Framer`]), typed errors for every malformed input, and the capability as a
//!   newtype that never prints.
//! - [`code`]: the datagram lane's code. [`code::Encoder`] assembles a block and turns it into
//!   its n = k + m messages, each repair a combination of the block's own k symbols whose
//!   coefficients are a pure function of the block, the repair and the position, never sent;
//!   [`code::Decoder`] holds each live block's linear system in reduced row echelon form, hands
//!   a block over the moment it is whole, and at each block's deadline decides it and writes
//!   its report.
//! - [`model`]: the erasures as runs in send order, fitted to the reports
//!   ([`model::Erasures`]), and the exact chance that a block of n symbols in a row loses more
//!   than m of them ([`model::Table`]).
//! - [`path`]: what the reports tell of the path: the base delay toward the relay, the spread
//!   of arrivals over it, the erasures and each track's frame rate.
//! - [`link`]: the datagram lane's view: [`link::Link::plan`] gives a block its m,
//!   [`link::Link::after`] what its deadline waits past the socket's acceptance,
//!   [`link::Link::share`] the frames' share of the wire, [`link::Link::reported`] takes a
//!   report back.
//!
//! **The relay** keeps one [`code::Decoder`] per datagram session: every piece and repair goes
//! to [`code::Decoder::take`] with the relay's stamp of its arrival; a block that comes out
//! whole gives its members at once. When [`code::Decoder::wake`] says, [`code::Decoder::decide`]
//! decides the next blocks in order and writes each one's report.

pub mod code;
pub mod gf;
pub mod link;
pub mod model;
pub mod path;
#[cfg(test)]
pub(crate) mod sim;
pub mod wire;
