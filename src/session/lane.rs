//! The lane socket: where a proxy's connections to the lane port arrive, and the writer that
//! sends each lane's queue.
//!
//! A lane's first bytes are a session's capability, and they are the only bytes the daemon
//! ever reads from a lane. A lane that opens with a capability no open session has is closed
//! unanswered; a lane attached is never read again.

use super::sender::{Phase, Queue};
use crate::hub::Hub;
use crate::wire::{Capability, CAPABILITY};
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::net::unix::OwnedWriteHalf;
#[cfg(target_os = "linux")]
use tokio::net::UnixListener;

#[cfg(target_os = "linux")]
/// Accepts lanes on `listener` for `hub`'s sessions, until the daemon ends.
pub async fn lanes(listener: UnixListener, hub: Arc<Hub>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(lane(stream, hub.clone()));
            }
            Err(e) => log::warn!("lane accept: {e}"),
        }
    }
}

/// A lane: the capability it presents, read into a buffer of that size and nothing more;
/// then, attached, its writer.
pub(super) async fn lane(stream: tokio::net::UnixStream, hub: Arc<Hub>) {
    let (mut read, write) = stream.into_split();
    let mut first = [0u8; CAPABILITY];
    if read.read_exact(&mut first).await.is_err() {
        return;
    }
    let presented = Capability::presented(&first);
    // The capability is read; the buffer that held it is zeroed.
    zero(&mut first);
    let queue = Queue::new(waker(&hub));
    if hub.attach(&presented, queue.clone()) {
        drop(read);
        writer(queue, write).await;
    }
}

/// What a lane's queue wakes when its gate opens: the hub's capture loop.
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
    /// Wait for a frame.
    Idle,
    /// Its lane ended: end.
    End,
}

/// Writes a lane's queue, each framed frame after the one before, as fast as the socket takes
/// them, until the lane ends. Bytes are written only under the queue's lock and only while the
/// lane is open, so after `Queue::end` returns no byte more is written; a write that fails
/// ends the lane. On Linux, where the daemon runs, the socket is given the least send buffer
/// first ([`super::sender::SEND_BUFFER`]).
pub(crate) async fn writer(queue: Arc<Queue>, socket: OwnedWriteHalf) {
    #[cfg(target_os = "linux")]
    if let Err(e) = nix::sys::socket::setsockopt(
        &socket.as_ref(),
        nix::sys::socket::sockopt::SndBuf,
        &super::sender::SEND_BUFFER,
    ) {
        log::warn!("the lane socket's send buffer not set to the least: {e}");
    }
    loop {
        let next = {
            let mut w = queue.lock();
            match (w.phase, w.messages.front()) {
                (Phase::Ended, _) => Next::End,
                (Phase::Open, None) => Next::Idle,
                (Phase::Open, Some(m)) => {
                    let len = m.len();
                    match socket.try_write(&m[w.written..]) {
                        Ok(n) if w.written + n == len => {
                            w.sent();
                            Next::Write
                        }
                        Ok(n) => {
                            w.written += n;
                            Next::Write
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Next::Writable,
                        Err(_) => {
                            w.end();
                            Next::End
                        }
                    }
                }
            }
        };
        match next {
            Next::Write => {}
            // The lane's end wakes it too, so a writer waiting on a full socket ends with it.
            Next::Writable => tokio::select! {
                r = socket.writable() => if r.is_err() {
                    queue.lock().end();
                    return;
                },
                () = queue.woken() => {}
            },
            Next::Idle => queue.woken().await,
            Next::End => return,
        }
    }
}
