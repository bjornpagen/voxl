//! Lanes: how video and audio cross a lossy path without waiting for a resend.
//!
//! A transport is a set of in-order lanes from the daemon to the relay: one datagram lane
//! (each message alone, lost or not), or several streams (each in order on its own, a loss
//! stalling it for a round trip). A piece of a frame that has not arrived by the frame's
//! deadline is erased, whether it was lost or is stuck behind a resend. One code, one budget
//! and one report serve every kind of lane; only the lanes differ.
//!
//! Everything here is pure and synchronous: no I/O, no clock reads, no tokio. Times come in
//! as [`wire::Stamp`]s, the tenths of a millisecond a side's own clock reads; the two sides'
//! clocks are never compared with each other, only samples taken across them with samples
//! taken the same way, so their offset cancels.
//!
//! - [`gf`]: the field, GF(2^8) over x^8 + x^4 + x^3 + x^2 + 1 (0x11d), as tables.
//! - [`wire`]: every message: on a lane, `Attach` (relay to daemon, once, first) and `Piece`
//!   and `Repair` (daemon to relay); on the control stream, [`wire::Down`] and [`wire::Up`].
//!   Fixed maximum sizes, length-prefixed framing for streams ([`wire::Framer`]), typed
//!   errors for every malformed input, the capability as a newtype that never prints, and a
//!   borrowed view of it ([`wire::CapabilityRef`]) for a message to carry.
//! - [`code`]: the code. What leaves together is one block: the video update of one
//!   capture, with the Opus packet waiting for it, or an Opus packet alone; its frames are
//!   its members, listed in a table at the head of its bytes. A block is cut into k source
//!   symbols of [`wire::SYMBOL`] bytes and sent with m repairs, each a combination of the
//!   block's k symbols over GF(2^8) whose coefficients are a pure function of the block, the
//!   repair and the position, never sent. [`code::Encoder`] assembles a block and turns it
//!   into its n = k + m messages; [`code::Decoder`] takes pieces and repairs as they arrive,
//!   holds each live block's linear system in reduced row echelon form, hands a block over
//!   the moment it is whole (its members then go on, video in its order), and at
//!   each block's deadline decides it and writes its report.
//!
//!   A repair covers its own block's k sources and nothing else. A window of repairs over
//!   several live blocks would pool their repairs, but a block's chance of a miss would then
//!   hang on the erasures of every block its repairs share, and m could not be sized by the
//!   exact chance over the block's own n symbols. Kept to its block, that chance is exact,
//!   and the decoder's system is the block's. A block of one symbol (an Opus packet alone)
//!   is then sent with copies, each repair a multiple of the one source: repetition is what a
//!   code of one symbol is.
//! - [`model`]: the two-state (Gilbert-Elliott) chain of erasures over the symbols in the
//!   order they were sent, fitted to the reported outcomes ([`model::Erasures`]), and the
//!   exact chance that a block of n symbols in a row loses more than m of them, by forward
//!   recursion; m is the least that meets a miss target ([`link::FREEZE_EVERY`]). Read in send order, not lane by lane: a burst on the path and the stalls it
//!   starts on stream lanes both show there as runs.
//! - [`path`]: the one model of the path: BBR's state (BtlBw, RTprop, the gain cycle) and
//!   the erasure channel, the spread and the video update rate, fed by reports; every
//!   control (pacing, the in-flight cap, each block's deadline, how long a miss leaves the
//!   picture stale) is a function of it.
//! - [`link`]: the daemon's whole view, the path fed by reports: [`link::Link::plan`] gives
//!   each block its m, its deadline and its sending time, [`link::Link::reported`] takes a
//!   report back, [`link::Link::data`] is what the encoders aim at, and
//!   [`link::Link::wanted`] is how many stream lanes the session wants.
//!
//! The deadline is computed by the daemon, which holds every sample it rests on (the delays
//! both ways, the pacing), and carried on the relay's clock in every message's head; the
//! relay applies it.
//!
//! **The daemon** keeps one [`link::Link`] and one [`code::Encoder`] per session. For each
//! block it assembles the members, asks the link for a [`link::Plan`] by its length and its
//! members' tracks, codes the block with it, sends message `i` on lane [`link::Plan::lane`]
//! (evenly over the plan's sending time, a lane holding no more than
//! [`link::Link::lane_bound`] unreported bytes), keeps the [`link::Flight`] the plan gives,
//! and hands it back with the block's report when the report comes up the control stream.
//! A missed block is the daemon's cue to invalidate the reference of the video update it
//! held. When [`link::Link::wanted`] changes it sends [`wire::Down::Lanes`].
//!
//! **The relay** keeps one [`code::Decoder`] per session. Every piece and repair from any
//! lane goes to [`code::Decoder::take`] with the relay's stamp of its arrival; a block that
//! comes out whole gives its members ([`wire::Head::members`]) at once (each update in its
//! order, by its epoch). When [`code::Decoder::wake`] says, [`code::Decoder::decide`]
//! decides the next blocks in order and writes each one's report, which goes up the control
//! stream; every frame up to the [`code::Decided`]'s `last` is then decided.

pub mod code;
pub mod gf;
pub mod link;
pub mod model;
pub mod path;
#[cfg(test)]
pub(crate) mod sim;
pub mod wire;
