//! `voxl attach`: the relay's door to the daemon, run over the ssh master. It says it opens
//! a session, and is then a pipe between its stdio and the daemon's control socket: what the daemon
//! says (the capability first) goes out on stdout, ssh's channel, and what the relay says
//! comes in on stdin. It ends when either side does, and the daemon's session with it.

#[cfg(target_os = "linux")]
use std::path::Path;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

#[cfg(target_os = "linux")]
/// Runs `voxl attach` on `control`, the daemon's control socket, and exits. It exits
/// rather than return: a read of stdin may still be waiting on its thread, and a runtime
/// dropped waits for it.
pub fn run(control: &Path) -> ! {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("voxl attach: no runtime: {e}");
            std::process::exit(1)
        }
    };
    let result = rt.block_on(async {
        let mut socket = tokio::net::UnixStream::connect(control).await?;
        socket
            .write_all(&super::query::Opening::Session.bytes())
            .await?;
        pipe(tokio::io::stdin(), tokio::io::stdout(), socket).await
    });
    match result {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("voxl attach: {}: {e}", control.display());
            std::process::exit(1)
        }
    }
}

/// Copies `input` into `socket` and `socket` into `output` until the socket ends. The end
/// of `input` (ssh's channel closed) closes the socket's writing side, which ends the
/// daemon's session; the daemon then closes the socket, which ends the pipe.
async fn pipe(
    mut input: impl AsyncRead + Unpin,
    mut output: impl AsyncWrite + Unpin,
    socket: tokio::net::UnixStream,
) -> std::io::Result<()> {
    let (mut from_daemon, mut to_daemon) = socket.into_split();
    let up = async {
        tokio::io::copy(&mut input, &mut to_daemon).await?;
        to_daemon.shutdown().await
    };
    let down = async {
        tokio::io::copy(&mut from_daemon, &mut output).await?;
        output.flush().await
    };
    tokio::pin!(down);
    // The daemon's side ending is the session's end, whatever the relay still says.
    tokio::select! {
        r = &mut down => return r,
        r = up => r?,
    }
    down.await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    /// Bytes go through both ways unchanged, in any sizes and order, and the end of the
    /// relay's side reaches the daemon as the end of its stream.
    #[tokio::test]
    async fn the_pipe_carries_both_ways_and_ends_with_its_input() {
        let (daemon, theirs) = tokio::net::UnixStream::pair().unwrap();
        let (relay_in, mut relay_writes) = tokio::io::duplex(1 << 16);
        let (mut relay_reads, relay_out) = tokio::io::duplex(1 << 16);
        let piped = tokio::spawn(pipe(relay_in, relay_out, theirs));
        let up: Vec<u8> = (0..70_000u32).map(|i| (i * 7 + i / 300) as u8).collect();
        let down: Vec<u8> = (0..50_000u32).map(|i| (i * 13 + i / 77) as u8).collect();
        let (mut d_read, mut d_write) = daemon.into_split();
        let to_relay = down.clone();
        let daemon_side = tokio::spawn(async move {
            d_write.write_all(&to_relay).await.unwrap();
            let mut got = Vec::new();
            d_read.read_to_end(&mut got).await.unwrap();
            // The relay's end arrived: the daemon ends the session.
            drop(d_write);
            got
        });
        relay_writes.write_all(&up).await.unwrap();
        drop(relay_writes);
        let mut got = Vec::new();
        relay_reads.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, down);
        assert_eq!(daemon_side.await.unwrap(), up);
        piped.await.unwrap().unwrap();
    }
}
