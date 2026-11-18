//! What a control connection is: its first message says, before the daemon says anything.
//! `voxl attach` opens a session; `voxl stats`, `voxl screenshot` and `voxl record` ask a
//! query, which is answered once, on the same connection, and closed. A query never gets a
//! capability and never reaches a lane: it is not a session.
//!
//! An opening is one tag byte, and for a recording what ends it: `0` its asker's stream
//! closing, or `1` and a big-endian u32 of seconds over 0. An answer is one byte, `0` and
//! what was asked (JSON, a PNG, an MP4), or `1` and the reason it was refused, in UTF-8;
//! then the connection closes.

use super::control::Daemon;
use std::num::NonZeroU32;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
#[cfg(target_os = "linux")]
use tokio::io::AsyncWrite;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

/// What a control connection is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Opening {
    /// A session: `voxl attach`.
    Session,
    Query(Query),
}

/// A query: asked once, answered once.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Query {
    /// The pipeline's counts and each client's state, as JSON: `voxl stats`.
    Stats,
    /// The screen as a PNG: `voxl screenshot`.
    Screenshot,
    /// An MP4 of what the stream sends until this ends it: `voxl record`.
    Record(Recording),
}

/// What ends a recording asked for, before `--max-record-seconds` does.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Recording {
    /// Its asker's stream closing (`voxl record` reading its stdin to its end).
    UntilClosed,
    /// This many seconds (`voxl record --seconds N`).
    Seconds(NonZeroU32),
}

const SESSION: u8 = 1;
const STATS: u8 = 2;
const SCREENSHOT: u8 = 3;
const RECORD: u8 = 4;
const UNTIL_CLOSED: u8 = 0;
const SECONDS: u8 = 1;

/// Why bytes are no opening.
#[derive(Debug, PartialEq)]
pub enum NoOpening {
    /// The connection ended first.
    Ended,
    /// No opening has this tag.
    Tag(u8),
    /// No recording ends this way.
    Recording(u8),
    /// A recording of 0 seconds.
    NoSeconds,
}

impl Opening {
    /// Its bytes, as its asker writes them.
    pub fn bytes(self) -> Vec<u8> {
        match self {
            Opening::Session => vec![SESSION],
            Opening::Query(Query::Stats) => vec![STATS],
            Opening::Query(Query::Screenshot) => vec![SCREENSHOT],
            Opening::Query(Query::Record(Recording::UntilClosed)) => vec![RECORD, UNTIL_CLOSED],
            Opening::Query(Query::Record(Recording::Seconds(s))) => {
                let mut b = vec![RECORD, SECONDS];
                b.extend_from_slice(&s.get().to_be_bytes());
                b
            }
        }
    }

    /// Reads one from the front of `r`, taking exactly its bytes and nothing after them.
    pub async fn read(r: &mut (impl AsyncRead + Unpin)) -> Result<Opening, NoOpening> {
        let mut byte = async || {
            let mut b = [0u8; 1];
            r.read_exact(&mut b).await.map_err(|_| NoOpening::Ended)?;
            Ok(b[0])
        };
        match byte().await? {
            SESSION => Ok(Opening::Session),
            STATS => Ok(Opening::Query(Query::Stats)),
            SCREENSHOT => Ok(Opening::Query(Query::Screenshot)),
            RECORD => match byte().await? {
                UNTIL_CLOSED => Ok(Opening::Query(Query::Record(Recording::UntilClosed))),
                SECONDS => {
                    let mut s = [0u8; 4];
                    for b in s.iter_mut() {
                        *b = byte().await?;
                    }
                    NonZeroU32::new(u32::from_be_bytes(s))
                        .map(|s| Opening::Query(Query::Record(Recording::Seconds(s))))
                        .ok_or(NoOpening::NoSeconds)
                }
                t => Err(NoOpening::Recording(t)),
            },
            t => Err(NoOpening::Tag(t)),
        }
    }
}

const ANSWERED: u8 = 0;
const REFUSED: u8 = 1;

/// Answers `query` on its connection, then closes it.
pub async fn answer(
    query: Query,
    mut read: tokio::net::unix::OwnedReadHalf,
    mut write: tokio::net::unix::OwnedWriteHalf,
    d: Arc<Daemon>,
) {
    let answer = match query {
        Query::Stats => Ok(d.hub.stats().to_string().into_bytes()),
        Query::Screenshot => crate::record::screenshot(&d.hub).await,
        Query::Record(Recording::Seconds(s)) => {
            let until = crate::record::Until::Elapsed(Duration::from_secs(s.get().into()));
            crate::record::record(&d.hub, until, d.max_record).await
        }
        Query::Record(Recording::UntilClosed) => {
            let (stop, stopped) = tokio::sync::oneshot::channel();
            // The asker's stream closing ends it; anything it sends before is not read
            // for meaning.
            tokio::spawn(async move {
                let mut sink = [0u8; 256];
                while matches!(read.read(&mut sink).await, Ok(n) if n > 0) {}
                if stop.send(()).is_err() {
                    log::debug!("a recording ended before its asker's stream did");
                }
            });
            let until = crate::record::Until::Stopped(stopped);
            crate::record::record(&d.hub, until, d.max_record).await
        }
    };
    let (tag, body) = match answer {
        Ok(body) => (ANSWERED, body),
        Err(why) => (REFUSED, why.into_bytes()),
    };
    let written = async {
        write.write_all(&[tag]).await?;
        write.write_all(&body).await?;
        write.shutdown().await
    };
    if let Err(e) = written.await {
        log::info!("a query's asker left before its answer: {e}");
    }
}

#[cfg(target_os = "linux")]
/// Runs a query subcommand (`voxl stats|screenshot|record`) against the daemon's control
/// socket `control`: its answer on stdout, or the refusal on stderr and exit status 1.
pub fn run(query: Query, control: &Path) -> ! {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("voxl: no runtime: {e}");
            std::process::exit(1)
        }
    };
    let status = rt.block_on(async {
        let socket = match tokio::net::UnixStream::connect(control).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("voxl: {}: {e}", control.display());
                return 1;
            }
        };
        match ask(query, socket, tokio::io::stdin(), tokio::io::stdout()).await {
            Ok(Asked::Answered) => 0,
            Ok(Asked::Refused(why)) => {
                eprintln!("voxl: {why}");
                1
            }
            Err(e) => {
                eprintln!("voxl: {e}");
                1
            }
        }
    });
    std::process::exit(status)
}

#[cfg(target_os = "linux")]
/// What came of a query.
#[derive(Debug, PartialEq)]
pub enum Asked {
    /// Its answer was written out.
    Answered,
    Refused(String),
}

#[cfg(target_os = "linux")]
/// Asks `query` on `socket`; for a recording until closed, `input` ending closes the
/// asking side. The answer is copied to `output`.
pub async fn ask(
    query: Query,
    socket: tokio::net::UnixStream,
    mut input: impl AsyncRead + Unpin + Send + 'static,
    mut output: impl AsyncWrite + Unpin,
) -> std::io::Result<Asked> {
    let (mut from, mut to) = socket.into_split();
    to.write_all(&Opening::Query(query).bytes()).await?;
    let ends = match query {
        Query::Record(Recording::UntilClosed) => Some(tokio::spawn(async move {
            let mut sink = [0u8; 256];
            while matches!(input.read(&mut sink).await, Ok(n) if n > 0) {}
            // A daemon that has already answered has closed its end.
            if let Err(e) = to.shutdown().await {
                eprintln!("voxl: the end of the recording not told: {e}");
            }
        })),
        Query::Stats | Query::Screenshot | Query::Record(Recording::Seconds(_)) => None,
    };
    let mut tag = [0u8; 1];
    from.read_exact(&mut tag).await?;
    let asked = match tag[0] {
        ANSWERED => {
            tokio::io::copy(&mut from, &mut output).await?;
            output.flush().await?;
            Asked::Answered
        }
        _ => {
            let mut why = Vec::new();
            from.read_to_end(&mut why).await?;
            Asked::Refused(String::from_utf8_lossy(&why).into_owned())
        }
    };
    if let Some(ends) = ends {
        ends.abort();
    }
    Ok(asked)
}
