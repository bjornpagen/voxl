//! The lane sockets: where a proxy's connections and datagrams to the lane port arrive,
//! and the writers that send each lane's queue.
//!
//! A lane's first message is its `Attach`, and it is the only message the daemon ever
//! reads from a lane. A stream lane that opens with anything else, or with a capability no
//! open session has, is closed unanswered; a stream lane attached is never read again. A
//! datagram lane is its peer's address: the datagram socket reads each datagram's sender
//! first, drops anything from a peer already attached unread, and takes an `Attach` from
//! any other peer or drops it, again with no reply.

use super::sender::{Kind, Phase, Queue, Waiting};
use crate::hub::Hub;
use crate::lanes::wire::Attach;
use std::os::fd::AsFd;
use std::os::unix::net::UnixDatagram;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::AsyncReadExt;
use tokio::io::Interest;
use tokio::net::unix::OwnedWriteHalf;
#[cfg(target_os = "linux")]
use tokio::net::UnixListener;

#[cfg(target_os = "linux")]
/// Accepts stream lanes on `listener` for `hub`'s sessions, until the daemon ends.
pub async fn streams(listener: UnixListener, hub: Arc<Hub>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(stream_lane(stream, hub.clone()));
            }
            Err(e) => log::warn!("lane stream accept: {e}"),
        }
    }
}

/// A stream lane: its first message, which must be an attach after its length
/// ([`Attach::decode_stream`]), read into a buffer of that size and nothing more; then,
/// attached, its writer.
pub(super) async fn stream_lane(stream: tokio::net::UnixStream, hub: Arc<Hub>) {
    let (mut read, write) = stream.into_split();
    let mut first = [0u8; Attach::STREAM_LEN];
    if read.read_exact(&mut first).await.is_err() {
        return;
    }
    let attach = Attach::decode_stream(&first);
    // The capability is read; the buffer that held it is zeroed.
    zero(&mut first);
    let Ok(attach) = attach else {
        return;
    };
    let queue = Queue::new(Kind::Stream, Instant::now(), waker(&hub));
    if hub.attach(attach, queue.clone(), Instant::now()) {
        drop(read);
        stream_writer(queue, write).await;
    }
}

/// What a lane's queue wakes when room opens on it: the hub's capture loop.
fn waker(hub: &Arc<Hub>) -> super::sender::Wake {
    let hub = Arc::downgrade(hub);
    Arc::new(move || {
        if let Some(hub) = hub.upgrade() {
            hub.wake();
        }
    })
}

/// Zeroes bytes that held a capability.
fn zero(b: &mut [u8]) {
    for x in b.iter_mut() {
        // SAFETY: a write to a byte of a buffer this function borrows mutably; volatile,
        // so the write is not elided as dead.
        unsafe { std::ptr::write_volatile(x, 0) };
    }
}

/// What a writer does next.
enum Next {
    /// Write again: more waits, and the socket may take it.
    Write,
    /// Wait for the socket to take more bytes.
    Writable,
    /// Wait for a message.
    Idle,
    /// Its lane closed: end.
    End,
}

/// Gives `socket` the send buffer the sender asked for, if it has not been given it. The
/// kernel doubles what it is given, for its bookkeeping (socket(7)); the size asked for is the
/// size of the messages it may hold.
fn give_buffer(w: &mut Waiting, socket: impl AsFd) {
    if let Some(bytes) = w.buffer.filter(|&b| w.given != Some(b)) {
        let set = nix::sys::socket::setsockopt(
            &socket,
            nix::sys::socket::sockopt::SndBuf,
            &(bytes as usize),
        );
        match set {
            Ok(()) => w.given = Some(bytes),
            Err(e) => log::warn!("lane socket's send buffer not set to {bytes}: {e}"),
        }
    }
}

/// Writes a stream lane's queue, each framed message after the one before, as fast as the
/// socket takes them, until the lane closes. Bytes are written only under the queue's lock and
/// only while the lane is open, so after `Queue::close` returns no byte more is written; a
/// write that fails loses the lane, which tells the relay ([`Waiting::lost`]) and keeps what it
/// holds for the next. A message the socket took only part of stays at the front, and is
/// written on before any other (on the next lane, whole).
async fn stream_writer(queue: Arc<Queue>, socket: OwnedWriteHalf) {
    loop {
        let next = {
            let mut w = queue.lock();
            give_buffer(&mut w, socket.as_ref());
            match (w.phase, w.messages.front()) {
                (Phase::Closed | Phase::Lost, _) => Next::End,
                (Phase::Open, None) => Next::Idle,
                (Phase::Open, Some(m)) => match socket.try_write(m.from(w.written)) {
                    Ok(n) if w.written + n == m.len() => {
                        w.sent(Instant::now());
                        Next::Write
                    }
                    Ok(n) => {
                        w.written += n;
                        Next::Write
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        w.refused();
                        Next::Writable
                    }
                    Err(_) => {
                        w.lost();
                        Next::End
                    }
                },
            }
        };
        match next {
            Next::Write => {}
            Next::Writable => {
                if socket.writable().await.is_err() {
                    queue.lock().lost();
                    return;
                }
            }
            Next::Idle => queue.woken().await,
            Next::End => return,
        }
    }
}

/// A datagram peer: the address its datagrams come from and its lane's go to. On Linux
/// each proxy session sends from an abstract address of its own (autobind); a peer with
/// no address cannot be answered, and is no peer.
#[derive(Clone)]
enum Peer {
    Path(std::path::PathBuf),
    #[cfg(any(target_os = "linux", target_os = "android"))]
    Abstract(Vec<u8>),
}

impl Peer {
    fn of(addr: &std::os::unix::net::SocketAddr) -> Option<Peer> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            use std::os::linux::net::SocketAddrExt;
            if let Some(name) = addr.as_abstract_name() {
                return Some(Peer::Abstract(name.to_vec()));
            }
        }
        addr.as_pathname().map(|p| Peer::Path(p.to_path_buf()))
    }

    fn same(&self, other: &Peer) -> bool {
        match (self, other) {
            (Peer::Path(a), Peer::Path(b)) => a == b,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            (Peer::Abstract(a), Peer::Abstract(b)) => a == b,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            (Peer::Path(_), Peer::Abstract(_)) | (Peer::Abstract(_), Peer::Path(_)) => false,
        }
    }

    /// Sends `b` to it from `socket`, without waiting.
    fn send(&self, socket: &UnixDatagram, b: &[u8]) -> std::io::Result<usize> {
        match self {
            Peer::Path(p) => socket.send_to(b, p),
            #[cfg(any(target_os = "linux", target_os = "android"))]
            Peer::Abstract(name) => {
                use std::os::linux::net::SocketAddrExt;
                let addr = std::os::unix::net::SocketAddr::from_abstract_name(name)?;
                socket.send_to_addr(b, &addr)
            }
        }
    }
}

/// Reads the datagram socket for `hub`'s sessions, until the daemon ends.
pub async fn datagrams(socket: UnixDatagram, hub: Arc<Hub>) {
    // Read, and waited on for room, through tokio; sent from the std socket (`Peer::send`),
    // which can address an abstract peer: one socket, two handles.
    let reader = match socket
        .try_clone()
        .and_then(tokio::net::UnixDatagram::from_std)
    {
        Ok(r) => Arc::new(r),
        Err(e) => {
            log::error!("lane datagram socket: {e}");
            return;
        }
    };
    let socket = Arc::new(socket);
    // The peers attached, each with its lane's queue: one whose queue has closed (its
    // session gone) is a peer like any other again.
    let mut attached: Vec<(Peer, Arc<Queue>)> = Vec::new();
    // One datagram at a time, of at most a lane message: an attach is shorter, and a
    // longer datagram is cut, which an attach never is.
    let mut buf = [0u8; crate::lanes::wire::DATAGRAM];
    loop {
        let (len, from) = match reader.recv_from(&mut buf).await {
            Ok(got) => got,
            Err(e) => {
                log::warn!("lane datagram: {e}");
                continue;
            }
        };
        let Some(peer) = Peer::of(&from.into()) else {
            continue;
        };
        attached.retain(|(_, q)| q.lock().phase == Phase::Open);
        if attached.iter().any(|(p, _)| p.same(&peer)) {
            continue;
        }
        if len != Attach::LEN {
            continue;
        }
        let attach = Attach::decode(&buf[..len]);
        zero(&mut buf[..len]);
        let Ok(attach) = attach else {
            continue;
        };
        let queue = Queue::new(Kind::Datagram, Instant::now(), waker(&hub));
        if hub.attach(attach, queue.clone(), Instant::now()) {
            attached.push((peer.clone(), queue.clone()));
            tokio::spawn(datagram_writer(queue, socket.clone(), reader.clone(), peer));
        }
    }
}

/// Sends a datagram lane's queue to its peer as fast as the socket takes it, until the lane
/// closes or is lost (its peer gone: the relay is told, and the queue kept for the next lane).
/// A datagram the socket cannot take now waits for the socket: none is ever dropped.
/// Each is stamped as it goes, with when it left and its block's deadline
/// (`Waiting::stamp`). Sent only under the queue's lock and only while the lane is open.
///
/// The socket is shared and unconnected, so its writability is its own send buffer's, which the
/// kernel charges with every datagram until the peer reads it and frees as it does; the buffer
/// ([`super::sender`]) is far smaller than the peer's queue of datagrams, so the buffer is what
/// fills first, and a wait on it ends when the peer reads.
async fn datagram_writer(
    queue: Arc<Queue>,
    socket: Arc<UnixDatagram>,
    ready: Arc<tokio::net::UnixDatagram>,
    peer: Peer,
) {
    loop {
        let next = {
            let mut w = queue.lock();
            give_buffer(&mut w, socket.as_ref());
            match w.phase {
                Phase::Closed | Phase::Lost => Next::End,
                Phase::Open if w.messages.is_empty() => Next::Idle,
                Phase::Open => {
                    let now = Instant::now();
                    w.stamp(now);
                    let sent = match w.messages.front() {
                        Some(m) => {
                            ready.try_io(Interest::WRITABLE, || peer.send(&socket, m.from(0)))
                        }
                        None => Ok(0),
                    };
                    match sent {
                        Ok(_) => {
                            w.sent(now);
                            Next::Write
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            w.refused();
                            Next::Writable
                        }
                        // The peer is gone (its proxy session ended): the lane is lost, and the
                        // relay told.
                        Err(_) => {
                            w.lost();
                            Next::End
                        }
                    }
                }
            }
        };
        match next {
            Next::Write => {}
            Next::Writable => {
                tokio::select! {
                    r = ready.writable() => if r.is_err() {
                        queue.lock().lost();
                        return;
                    },
                    _ = queue.woken() => {}
                }
            }
            Next::Idle => queue.woken().await,
            Next::End => return,
        }
    }
}

/// On Linux, where the daemon's lane sockets are.
#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::lanes::wire::{Lane, Number};
    use crate::session::sender::Sender;
    use std::time::Duration;

    /// A datagram lane never drops a datagram: a peer that reads nothing while the writer sends
    /// a queue far larger than the socket's buffer (so the socket refuses, and the writer waits
    /// on it) then reads it all, and gets every byte queued, every datagram whole, in the order
    /// queued. A writer that dropped what the socket refused fails this.
    #[tokio::test]
    async fn a_datagram_lane_never_drops_one() {
        let dir = std::env::temp_dir().join(format!("voxl-writer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (ours, theirs) = (dir.join("ours"), dir.join("theirs"));
        let _ = std::fs::remove_file(&ours);
        let _ = std::fs::remove_file(&theirs);
        let socket = UnixDatagram::bind(&ours).unwrap();
        socket.set_nonblocking(true).unwrap();
        let ready =
            Arc::new(tokio::net::UnixDatagram::from_std(socket.try_clone().unwrap()).unwrap());
        let peer = tokio::net::UnixDatagram::bind(&theirs).unwrap();
        let now = Instant::now();
        let (mut s, queue, _control) = Sender::with_lane(now);
        let mut number = Number::first();
        for _ in 0..60 {
            s.video(number, &bytes::Bytes::from(vec![9u8; 20_000]), now);
            number = number.next();
        }
        let total = queue.backlog_bytes();
        tokio::spawn(datagram_writer(
            queue.clone(),
            Arc::new(socket),
            ready,
            Peer::Path(theirs.clone()),
        ));
        // The writer sends until the socket refuses, and waits on it.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            queue.backlog_bytes() > 0,
            "the socket took {total} bytes at once"
        );
        let (mut got, mut last) = (0usize, None);
        let mut buf = vec![0u8; crate::lanes::wire::DATAGRAM + 1];
        while got < total {
            let n = tokio::time::timeout(Duration::from_secs(5), peer.recv(&mut buf))
                .await
                .expect("the writer goes on as the peer reads")
                .unwrap();
            let Ok(Lane::Piece(m) | Lane::Repair(m)) = Lane::decode(&buf[..n]) else {
                panic!("a datagram, whole");
            };
            let at = (
                m.head.block.get(),
                Lane::decode(&buf[..n])
                    .map(|l| match l {
                        Lane::Piece(p) => p.index as usize,
                        Lane::Repair(r) => r.head.k() + r.index as usize,
                        Lane::Frame { .. } => usize::MAX,
                    })
                    .unwrap(),
            );
            assert!(last.is_none_or(|l| l < at), "{at:?} after {last:?}");
            last = Some(at);
            got += n;
        }
        assert_eq!(got, total, "every byte queued");
        assert_eq!(queue.backlog_bytes(), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
