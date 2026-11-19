//! The lane socket: where a proxy's connections to the lane port arrive, and the writer that
//! sends each lane's queue.
//!
//! A lane's first bytes are a session's capability, and they are the only bytes the daemon
//! ever reads from a lane. A lane that opens with a capability no open session has is closed
//! unanswered; a lane attached is never read again.

use super::sender::{Next, Writer};
use crate::hub::{Client, Hub};
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
/// then, attached, its writer. When the lane ends the capture loop is woken, to look again
/// at who watches with a lane.
pub(super) async fn lane(stream: tokio::net::UnixStream, hub: Arc<Hub>) {
    let (mut read, write) = stream.into_split();
    let mut first = [0u8; CAPABILITY];
    if read.read_exact(&mut first).await.is_err() {
        return;
    }
    let presented = Capability::presented(&first);
    // The capability is read; the buffer that held it is zeroed.
    zero(&mut first);
    let Some((session, writer)) = hub.attach(&presented) else {
        return;
    };
    drop(read);
    // The hub is not held for as long as the lane lasts: a lane may outlast it.
    let hub = Arc::downgrade(&hub);
    write_lane(&session, &writer, &write).await;
    if let Some(hub) = hub.upgrade() {
        hub.wake();
    }
}

/// Zeroes bytes that held a capability.
fn zero(b: &mut [u8]) {
    for x in b.iter_mut() {
        // SAFETY: a write to a byte of a buffer this function borrows mutably; volatile,
        // so the write is not elided as dead.
        unsafe { std::ptr::write_volatile(x, 0) };
    }
}

/// Writes `session`'s lane, each framed frame after the one before, as fast as the socket
/// takes them, until the lane is the session's no longer. Each write is one step under the
/// session's lock (`Sender::write`), so once the session has closed, or taken another
/// lane, not one byte more is written; a write that fails ends the lane.
async fn write_lane(session: &Client, writer: &Writer, socket: &OwnedWriteHalf) {
    loop {
        match session.lane(|s| s.write(writer, |bytes| socket.try_write(bytes))) {
            Next::Write => {}
            // The lane's end wakes it too, so a writer waiting on a full socket ends with it.
            Next::Writable => tokio::select! {
                ready = socket.writable() => if ready.is_err() {
                    session.lane(|s| s.lost(writer));
                    return;
                },
                () = writer.woken() => {}
            },
            Next::Idle => writer.woken().await,
            Next::End => return,
        }
    }
}
