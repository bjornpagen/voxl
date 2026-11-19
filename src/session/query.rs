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
use crate::wire::Byte;
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

/// An opening's first byte: what the connection is.
#[derive(Clone, Copy)]
enum Kind {
    Session,
    Stats,
    Screenshot,
    Record,
}

impl Byte for Kind {
    const ALL: &'static [Kind] = &[Kind::Session, Kind::Stats, Kind::Screenshot, Kind::Record];

    fn byte(self) -> u8 {
        match self {
            Kind::Session => 1,
            Kind::Stats => 2,
            Kind::Screenshot => 3,
            Kind::Record => 4,
        }
    }
}

/// A recording's second byte: what ends it.
#[derive(Clone, Copy)]
enum Ends {
    UntilClosed,
    Seconds,
}

impl Byte for Ends {
    const ALL: &'static [Ends] = &[Ends::UntilClosed, Ends::Seconds];

    fn byte(self) -> u8 {
        match self {
            Ends::UntilClosed => 0,
            Ends::Seconds => 1,
        }
    }
}

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
            Opening::Session => vec![Kind::Session.byte()],
            Opening::Query(Query::Stats) => vec![Kind::Stats.byte()],
            Opening::Query(Query::Screenshot) => vec![Kind::Screenshot.byte()],
            Opening::Query(Query::Record(Recording::UntilClosed)) => {
                vec![Kind::Record.byte(), Ends::UntilClosed.byte()]
            }
            Opening::Query(Query::Record(Recording::Seconds(s))) => {
                let mut b = vec![Kind::Record.byte(), Ends::Seconds.byte()];
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
        let kind = byte().await?;
        match Kind::of(kind).ok_or(NoOpening::Tag(kind))? {
            Kind::Session => Ok(Opening::Session),
            Kind::Stats => Ok(Opening::Query(Query::Stats)),
            Kind::Screenshot => Ok(Opening::Query(Query::Screenshot)),
            Kind::Record => {
                let ends = byte().await?;
                match Ends::of(ends).ok_or(NoOpening::Recording(ends))? {
                    Ends::UntilClosed => Ok(Opening::Query(Query::Record(Recording::UntilClosed))),
                    Ends::Seconds => {
                        let mut s = [0u8; 4];
                        for b in s.iter_mut() {
                            *b = byte().await?;
                        }
                        NonZeroU32::new(u32::from_be_bytes(s))
                            .map(|s| Opening::Query(Query::Record(Recording::Seconds(s))))
                            .ok_or(NoOpening::NoSeconds)
                    }
                }
            }
        }
    }
}

/// An answer's first byte: whether the query was answered, or why not follows.
#[derive(Clone, Copy)]
enum Answer {
    Answered,
    Refused,
}

impl Byte for Answer {
    const ALL: &'static [Answer] = &[Answer::Answered, Answer::Refused];

    fn byte(self) -> u8 {
        match self {
            Answer::Answered => 0,
            Answer::Refused => 1,
        }
    }
}

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
        Ok(body) => (Answer::Answered.byte(), body),
        Err(why) => (Answer::Refused.byte(), why.into_bytes()),
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
    let [tag] = tag;
    let answer = Answer::of(tag).ok_or_else(|| {
        let what = format!("an answer that begins with {tag}: no answer does");
        std::io::Error::new(std::io::ErrorKind::InvalidData, what)
    })?;
    let asked = match answer {
        Answer::Answered => {
            tokio::io::copy(&mut from, &mut output).await?;
            output.flush().await?;
            Asked::Answered
        }
        Answer::Refused => {
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
