//! A lane's path, as its transport states it: the one source of `link::Path`.
//!
//! The transport that carries a lane measures its path to pace itself: its least round trip
//! and the rate it delivers. The proxy that hands the lane's stream to the lane socket
//! states them: with the stream's first bytes it passes one descriptor (`SCM_RIGHTS`), the
//! reading end of a `SOCK_SEQPACKET` pair, and on it writes a record whenever the path
//! differs from the last it wrote, at most once in each of the transport's rounds; the
//! statement ends, at its end of file, when the stream does. A record is one message of
//! [`RECORD`] bytes: the least round trip in nanoseconds, then the delivery rate in bytes a
//! second, each a big-endian `u64` and neither zero (a transport that has yet to measure
//! one writes no record).
//!
//! A transport that states no path, and a lane socket reached by something that passes no
//! descriptor, bring the bytes alone: such a lane's path is `Path::Unstated` for good, and
//! it is sent one frame at a time. Nothing here estimates a path: the daemon knows of one
//! exactly what its transport said.

use super::link::{BytesPerSecond, Known};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::Duration;

/// A record's length: its two fields.
///
/// @limitKind published
/// @owner the proxy that states a stream's path to the place it relays the stream to
/// @unit bytes
/// @scope every record of every statement
/// @constraint a record is exactly a least round trip in nanoseconds and a delivery rate in
/// bytes a second, each an unsigned 64-bit integer, big-endian, the round trip first: every
/// message of a statement is one whole record
/// @source the proxy's contract with its places: "16-byte big-endian records (least round
/// trip in nanoseconds u64, delivery rate in bytes/second u64, both nonzero)"
/// @verified 2026-11-19
pub const RECORD: usize = 2 * FIELD;
const FIELD: usize = std::mem::size_of::<u64>();

/// Why a statement's message is no record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoRecord {
    /// It is not [`RECORD`] bytes long: its length.
    Length(usize),
    /// A field is zero: no record says a path its transport has not measured.
    Unmeasured,
}

impl std::fmt::Display for NoRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NoRecord::Length(n) => write!(f, "a message of {n} bytes, not a record's {RECORD}"),
            NoRecord::Unmeasured => write!(f, "a record with a zero field"),
        }
    }
}

/// The path one message of a statement says.
pub fn record(message: &[u8]) -> Result<Known, NoRecord> {
    let Ok(fields) = <&[u8; RECORD]>::try_from(message) else {
        return Err(NoRecord::Length(message.len()));
    };
    let (least, rate) = fields.split_at(FIELD);
    let field = |b: &[u8]| b.iter().fold(0u64, |n, byte| n << 8 | u64::from(*byte));
    match (field(least), field(rate)) {
        (0, _) | (_, 0) => Err(NoRecord::Unmeasured),
        (least, rate) => Ok(Known {
            round_trip: Duration::from_nanos(least),
            rate: BytesPerSecond(rate),
        }),
    }
}

/// The most descriptors one message of a unix socket carries: room is made for as many, so
/// no receive is ever cut short of descriptors, each of which this process then owns and
/// closes.
///
/// @limitKind published
/// @owner Linux, include/net/scm.h
/// @unit descriptors a message
/// @scope the control buffer of every receive on a lane's socket
/// @constraint a receive with less room installs the descriptors that fit and marks the
/// rest cut (`MSG_CTRUNC`), which the reader of the control messages then refuses to walk:
/// what was installed would be nobody's to close
/// @source include/net/scm.h "#define SCM_MAX_FD 253"; net/core/scm.c, scm_fp_copy: "if
/// (fpl->count + num > fpl->max) return -EINVAL;", with `max` set to SCM_MAX_FD
/// @verified 2026-11-19
const MOST_DESCRIPTORS: usize = 253;

/// What a lane's first bytes brought beside themselves.
#[derive(Debug)]
pub(super) enum Handed {
    /// No descriptor: nothing states the lane's path.
    Nothing,
    /// One: the lane's statement, if it is one (`Statement::of`).
    One(OwnedFd),
    /// More than one, which no statement is: each is closed.
    Many,
}

impl Handed {
    /// What was handed, with `more` handed after it.
    fn and(self, more: Handed) -> Handed {
        match (self, more) {
            (Handed::Nothing, handed) | (handed, Handed::Nothing) => handed,
            (Handed::One(_) | Handed::Many, Handed::One(_) | Handed::Many) => Handed::Many,
        }
    }
}

/// One receive on `socket` into `buf`: the bytes read, and the descriptors that came with
/// them, each this process's own from here (closed with what holds it), close-on-exec.
fn receive(socket: RawFd, buf: &mut [u8]) -> io::Result<(usize, Handed)> {
    use nix::sys::socket::{recvmsg, ControlMessageOwned, MsgFlags};
    // Linux marks them close-on-exec as it installs them; elsewhere they are marked below.
    #[cfg(target_os = "linux")]
    let flags = MsgFlags::MSG_CMSG_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let flags = MsgFlags::empty();
    let mut control = nix::cmsg_space!([RawFd; MOST_DESCRIPTORS]);
    let mut into = [io::IoSliceMut::new(buf)];
    let got = recvmsg::<()>(socket, &mut into, Some(&mut control), flags)?;
    let mut handed = Handed::Nothing;
    for message in got.cmsgs()? {
        if let ControlMessageOwned::ScmRights(descriptors) = message {
            for raw in descriptors {
                // SAFETY: the kernel installed `raw` for this receive, and nothing else
                // holds it: it is owned from here.
                let descriptor = unsafe { OwnedFd::from_raw_fd(raw) };
                #[cfg(not(target_os = "linux"))]
                nix::fcntl::fcntl(
                    &descriptor,
                    nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
                )?;
                handed = handed.and(Handed::One(descriptor));
            }
        }
    }
    Ok((got.bytes, handed))
}

/// Reads exactly `first.len()` bytes off `lane`, its first, and what was handed over with
/// them. A lane that ends before them, or fails, is the error.
pub(super) async fn first(lane: &tokio::net::UnixStream, first: &mut [u8]) -> io::Result<Handed> {
    let (mut read, mut handed) = (0, Handed::Nothing);
    while read < first.len() {
        lane.readable().await?;
        let rest = &mut first[read..];
        match lane.try_io(tokio::io::Interest::READABLE, || {
            receive(lane.as_raw_fd(), rest)
        }) {
            Ok((0, _)) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok((n, more)) => {
                read += n;
                handed = handed.and(more);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e),
        }
    }
    Ok(handed)
}

/// A lane's statement: the reading end its transport's proxy handed over. tokio has no type
/// for a `SOCK_SEQPACKET` socket; its datagram socket reads one message a receive, which is
/// all that is asked of this one.
pub(super) struct Statement(tokio::net::UnixDatagram);

/// Why a descriptor handed over with a lane is no statement.
#[derive(Debug)]
pub(super) enum NoStatement {
    /// It is not a `SOCK_SEQPACKET` socket, so what is read of it is not one whole record
    /// at a time.
    NotRecords,
    /// It could not be asked or set up: why.
    Failed(io::Error),
}

impl std::fmt::Display for NoStatement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NoStatement::NotRecords => write!(f, "not a SOCK_SEQPACKET socket"),
            NoStatement::Failed(e) => write!(f, "{e}"),
        }
    }
}

/// What a statement says next.
#[derive(Debug)]
pub(super) enum Said {
    /// The path, as it is now.
    Path(Known),
    /// Nothing more: it ended.
    End(End),
}

/// How a statement ended.
#[derive(Debug)]
pub(super) enum End {
    /// Its end of file: the stream ended, or its proxy stopped stating.
    Closed,
    /// A message that is no record.
    NoRecord(NoRecord),
    /// A read that failed.
    Failed(io::Error),
}

impl std::fmt::Display for End {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            End::Closed => write!(f, "its end"),
            End::NoRecord(why) => write!(f, "{why}"),
            End::Failed(e) => write!(f, "{e}"),
        }
    }
}

impl Statement {
    /// The statement `descriptor` is, if it is the reading end of a `SOCK_SEQPACKET` pair:
    /// read without blocking from here, on the runtime this is called in.
    pub(super) fn of(descriptor: OwnedFd) -> Result<Statement, NoStatement> {
        use nix::sys::socket::{getsockopt, sockopt, SockType};
        // A descriptor that is no socket at all fails here (ENOTSOCK).
        let kind = getsockopt(&descriptor, sockopt::SockType)
            .map_err(|e| NoStatement::Failed(e.into()))?;
        if kind != SockType::SeqPacket {
            return Err(NoStatement::NotRecords);
        }
        let socket = std::os::unix::net::UnixDatagram::from(descriptor);
        socket.set_nonblocking(true).map_err(NoStatement::Failed)?;
        tokio::net::UnixDatagram::from_std(socket)
            .map(Statement)
            .map_err(NoStatement::Failed)
    }

    /// What it says next, once it says it. A message is read into room for one byte more
    /// than a record, so one longer than a record is read as longer, and is none.
    pub(super) async fn next(&self) -> Said {
        let mut message = [0u8; RECORD + 1];
        match self.0.recv(&mut message).await {
            Ok(0) => Said::End(End::Closed),
            Ok(n) => match record(&message[..n]) {
                Ok(known) => Said::Path(known),
                Err(why) => Said::End(End::NoRecord(why)),
            },
            Err(e) => Said::End(End::Failed(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xorshift::Xorshift;

    /// A statement's message is a record exactly when it is 16 bytes whose two fields are
    /// not zero, and then it says those fields: over generated messages of every length
    /// from none to 40 bytes, and of a record's length with generated fields (some zero,
    /// some of a few bits, some of all 64), each written here as the proxy writes one
    /// (big-endian, the round trip first): the path read is the round trip in nanoseconds
    /// and the rate in bytes a second; a zero field is refused as unmeasured; any other
    /// length is refused by its length.
    #[test]
    fn a_record_is_its_two_fields() {
        let mut rng = Xorshift::new(0x6a09_e667_f3bc_c908);
        let (mut read, mut unmeasured, mut other) = (0, 0, 0);
        let field = |rng: &mut Xorshift| match rng.below(6) {
            0 => 0,
            1.. => rng.next() >> rng.below(64),
        };
        for _ in 0..50_000 {
            let (least, rate) = (field(&mut rng), field(&mut rng));
            let mut message = [least.to_be_bytes(), rate.to_be_bytes()].concat();
            let want = match rng.below(4) {
                0 => {
                    let len = rng.below(41) as usize;
                    message.resize(len, rng.below(256) as u8);
                    match len {
                        RECORD => None,
                        _ => Some(Err(NoRecord::Length(len))),
                    }
                }
                1.. => None,
            };
            let want = want.unwrap_or(match (least, rate) {
                (0, _) | (_, 0) => Err(NoRecord::Unmeasured),
                _ => Ok(Known {
                    round_trip: Duration::new(
                        least / 1_000_000_000,
                        (least % 1_000_000_000) as u32,
                    ),
                    rate: BytesPerSecond(rate),
                }),
            });
            assert_eq!(record(&message), want, "{message:02x?}");
            match want {
                Ok(_) => read += 1,
                Err(NoRecord::Unmeasured) => unmeasured += 1,
                Err(NoRecord::Length(_)) => other += 1,
            }
        }
        assert!(
            read > 20_000 && unmeasured > 5_000 && other > 5_000,
            "{read} {unmeasured} {other}"
        );
    }
}
