//! The lane socket: where a proxy's connections to the lane port arrive, and the writer that
//! sends each lane's queue.
//!
//! A lane's first bytes are a session's capability and what its page holds of the stream
//! (`wire::Holds`), and they are the only bytes the daemon ever reads from a lane. A lane
//! that opens with a capability no open session has, or with anything else after it, is
//! closed unanswered; a lane attached is never read again.
//!
//! With those bytes the lane's transport hands over its statement of the path, a
//! descriptor (`statement`): read for as long as the lane is its session's, each path it
//! says is the lane's path from then (`link::Path`), and the capture loop is woken to look
//! at the gate again. A lane that brings no descriptor has no statement, and its path is
//! not stated; one that brings a descriptor that is no statement, or more than one, is
//! closed as a lane with no capability is.

use super::link::Path;
use super::sender::{Next, Writer};
use super::statement::{self, Handed, Said, Statement};
use crate::hub::{Client, Hub};
use crate::wire::{Capability, Holds, CAPABILITY};
use std::sync::Arc;
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

/// A lane: the capability it presents and what its page holds, read into buffers of their
/// sizes and nothing more, with the statement of its path, if one was handed over with
/// them; then, attached, its writer, and beside it the reader of its statement. When the lane ends the capture loop
/// is woken, to look again at who watches with a lane.
pub(super) async fn lane(stream: tokio::net::UnixStream, hub: Arc<Hub>) {
    let mut first = [0u8; CAPABILITY];
    let Ok(handed) = statement::first(&stream, &mut first).await else {
        return;
    };
    let presented = Capability::presented(&first);
    // The capability is read; the buffer that held it is zeroed.
    zero(&mut first);
    // What the page holds: its tag, which says how many bytes follow it, and those.
    let mut said = [0u8; 1 + std::mem::size_of::<u32>()];
    let Ok(with_tag) = statement::first(&stream, &mut said[..1]).await else {
        return;
    };
    let Ok(after) = Holds::after(said[0]) else {
        return;
    };
    let said = &mut said[..1 + after];
    let Ok(with_rest) = statement::first(&stream, &mut said[1..]).await else {
        return;
    };
    let Ok(holds) = Holds::decode(said) else {
        return;
    };
    let handed = handed.and(with_tag).and(with_rest);
    let statement = match handed {
        Handed::Nothing => None,
        Handed::One(descriptor) => match Statement::of(descriptor) {
            Ok(statement) => Some(statement),
            Err(why) => {
                log::warn!("a lane handed over a descriptor that states no path: {why}");
                return;
            }
        },
        Handed::Many => {
            log::warn!("a lane handed over more than one descriptor: none is its statement");
            return;
        }
    };
    let path = match statement {
        None => Path::Unstated,
        Some(_) => Path::Unknown,
    };
    let Some((session, writer)) = hub.attach(&presented, path, holds) else {
        return;
    };
    let (read, write) = stream.into_split();
    drop(read);
    // The hub is not held for as long as the lane lasts: a lane may outlast it.
    let hub = Arc::downgrade(&hub);
    let stated = async {
        if let Some(statement) = statement {
            follow(&statement, &session, &writer, &hub).await;
        }
        // The statement said its last, or there is none: the lane goes on as it is.
        std::future::pending::<()>().await
    };
    tokio::select! {
        () = write_lane(&session, &writer, &write) => {}
        () = stated => {}
    }
    if let Some(hub) = hub.upgrade() {
        hub.wake();
    }
}

/// Reads `statement`, the statement of the lane `writer` writes for `session`, to its end:
/// each path it says is the lane's from then, and at its end nothing is known of the path
/// any more. Each wakes the capture loop, which waits on a gate with no time: a path that
/// widens the window opens it.
async fn follow(
    statement: &Statement,
    session: &Client,
    writer: &Writer,
    hub: &std::sync::Weak<Hub>,
) {
    loop {
        let (path, ended) = match statement.next().await {
            Said::Path(known) => (Path::Known(known), false),
            Said::End(end) => {
                log::info!(
                    "session {}: its lane's path is stated no longer: {end}",
                    session.id
                );
                (Path::Unknown, true)
            }
        };
        session.stated(writer, path);
        if let Some(hub) = hub.upgrade() {
            hub.wake();
        }
        if ended {
            return;
        }
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
