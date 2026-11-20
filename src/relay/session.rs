//! One page's session: its WebSocket, its `voxl attach` over ssh (the control stream), and
//! its lane. They start together and end together, for one cause the page is told; nothing
//! reconnects by itself.
//!
//! The control stream carries the page's messages up as they are, the daemon's down as they
//! are, the lane's confirmation, and the relay's own word of each frame the page took. The
//! lane is one stream through an HTTP CONNECT proxy, which carries every frame whole and in
//! order: the screen's updates and the sound.
//!
//! **The start.** Nothing waits for what it does not need. The proxy is asked for the lane's
//! stream as the session begins, beside ssh: the stream needs nothing of the session. The
//! session's capability, the control stream's first message, is written on the stream as
//! soon as both are there, and the hello is given to the page as soon as it comes, whether
//! the lane is open yet or not. What the page says goes up the control stream from the
//! first, and what the daemon says goes to the page as it comes, whatever the lane is doing.
//!
//! **The hand-off.** The page is given one of the lane's frames at a time, its message as it
//! is (a frame on the lane is its name, then the page's message), and the next only once the
//! page has said it took the one before ([`TOOK`]); while the page holds one, the lane is not
//! read. The page's socket cannot make the relay wait: what a browser lets arrive unread it
//! queues where the daemon cannot see it (about a megabyte, measured), so the page says when
//! it is done, and a page that falls behind backs up the lane. Each frame the page takes, a
//! video update or an Opus packet, is told to the daemon by its name (`Up::Took`), which
//! knows no delivery but that.
//!
//! **The hello comes first.** The hello crosses ssh and the first keyframe the lane, another
//! transport, and either may arrive first; the page ends its session on an update before the
//! hello. So a frame is given only with the page's [`Turn`](turn::Turn), and the one place a
//! turn comes from nothing is the page being given the hello ([`turn::hello`]): a state of
//! the lane that can give a frame holds a turn, which is the proof. Giving the page a frame
//! spends the turn, and only the page's word makes the next, so no frame is given while one
//! waits to be taken.

use super::proxy::{self, ProxyError};
use super::{files, ssh};
use crate::wire::{self, Capability, Down, Frame, Framer, Header, Holds, Sequence, Track, Up};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use bytes::Bytes;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, Stream, StreamExt};
use std::convert::Infallible;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::process::{Child, ChildStdout};
use tokio::sync::Notify;
use turn::{Turn, Untaken};

/// What every session is started with.
pub struct Config {
    /// The HTTP CONNECT proxy the lane goes through.
    pub proxy: proxy::Endpoint,
    pub ssh: ssh::Destination,
    /// Which daemon on the server `voxl attach` reaches.
    pub daemon: ssh::Daemon,
    /// Where the lane goes, as the server's proxy is asked for it: that daemon's lane.
    pub place: proxy::Place,
    /// Where the files routes learn the shared folder from a session's hello.
    pub files: Arc<files::Files>,
}

/// The longest message the page may send: one that fits a control message with its tag.
/// Derived: `wire::MAX_MESSAGE` less the one byte of `Up`'s tag.
const PAGE_MESSAGE_MOST: usize = wire::MAX_MESSAGE - 1;

/// The bytes read at a time from ssh's stdout and from the lane: the most a pipe holds, which
/// is the most one read of ssh's stdout can give. A buffer larger is never filled; one
/// smaller takes more reads for the same bytes. The lane is a socket, with no such capacity:
/// it is read by the same amount, and a frame larger than a read is gathered by its framer
/// over several.
///
/// @limitKind published
/// @owner the kernel's pipe, which ssh's stdout is
/// @unit bytes
/// @scope one read of the control stream, and one of the lane
/// @constraint kernel capacity: what a pipe holds before a write to it waits
/// @source https://man7.org/linux/man-pages/man7/pipe.7.html "Since Linux 2.6.11, the pipe
/// capacity is 16 pages (i.e., 65,536 bytes in a system with a page size of 4096 bytes)."
/// The relay also runs on macOS, where it was measured: a pipe took 65,536 bytes before a
/// write waited, and one read of it gave them all (macOS 27.2.1, 2026-11-18).
/// @verified 2026-11-18
const READ: usize = 64 << 10;

/// The page's word that it took the lane's frame it was given last: a binary message of
/// this one byte, `bin.took` in web/src/wire.json (a test holds the two equal). It is the
/// relay's alone: the daemon reads no message of this type, and is told what the word means
/// for it (`Up::Took`).
const TOOK: u8 = 19;

pub async fn upgrade(ws: WebSocketUpgrade, State(cfg): State<Arc<Config>>) -> Response {
    ws.max_message_size(PAGE_MESSAGE_MOST)
        .on_upgrade(move |socket| run(socket, cfg))
}

/// Why a session ended: the page shows it.
#[derive(Debug)]
pub enum End {
    /// The page closed its socket.
    PageClosed,
    /// The page's socket failed: the page is gone, for this.
    PageLost(axum::Error),
    /// The page said it took a frame when it held none.
    Took,
    /// ssh could not be started.
    Ssh(std::io::Error),
    /// ssh failed before the session began: no door to the desktop is open (the relay's
    /// ssh rides the connection `ssh <destination>` holds, and never logs in on its own).
    NoDoor(ssh::Destination),
    /// The control stream ended: ssh exited (its own words are in the relay's log).
    ControlEnded(String),
    /// The control stream carried something that is not a control message.
    Control(wire::Malformed),
    /// The control stream carried text for the page that is not one of the daemon's
    /// messages to it.
    Text(serde_json::Error),
    /// The control stream's first message was not the capability, a second one came, or the
    /// lane was confirmed when it was not waiting to be.
    Capability,
    /// The lane could not be opened through the proxy.
    Proxy(ProxyError),
    /// The lane ended before it was attached: the daemon or the transport refuses it.
    LaneEnded(String),
    /// The lane carried something that is not a frame.
    Lane(wire::Malformed),
}

impl fmt::Display for End {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            End::PageClosed => write!(f, "the page closed"),
            End::PageLost(e) => write!(f, "the page's socket failed: {e}"),
            End::Took => write!(f, "the page took a frame it was not given"),
            End::Ssh(e) => write!(f, "ssh could not start: {e}"),
            End::NoDoor(to) => write!(
                f,
                "no door to the desktop is open: run `ssh {to}` in a terminal first, then reload"
            ),
            End::ControlEnded(how) => write!(f, "the ssh session ended ({how})"),
            End::Control(e) => write!(f, "the control stream is broken: {e}"),
            End::Text(e) => write!(f, "the server's voxl sent text that is no message: {e}"),
            End::Capability => write!(f, "the server's voxl did not open the session as expected"),
            End::Proxy(error) => write!(f, "the lane: {error}"),
            End::LaneEnded(why) => write!(f, "the lane ended before it was attached: {why}"),
            End::Lane(error) => write!(f, "the lane is broken: {error}"),
        }
    }
}

/// The page's socket, as the session writes to it: each message sent once the socket has
/// taken the one before; one that cannot go is the page gone.
trait Page {
    async fn send(&mut self, m: Message) -> Result<(), End>;
}

impl Page for SplitSink<WebSocket, Message> {
    async fn send(&mut self, m: Message) -> Result<(), End> {
        SinkExt::send(self, m).await.map_err(End::PageLost)
    }
}

async fn run(socket: WebSocket, cfg: Arc<Config>) {
    let (mut page, from_page) = socket.split();
    let end = session(&cfg, &mut page, from_page).await;
    log::info!("session ended: {end}");
    // The page is told the cause before its socket closes, unless the page is what ended.
    match end {
        End::PageClosed | End::PageLost(_) => {}
        End::Took
        | End::Ssh(_)
        | End::NoDoor(_)
        | End::ControlEnded(_)
        | End::Control(_)
        | End::Text(_)
        | End::Capability
        | End::Proxy(_)
        | End::LaneEnded(_)
        | End::Lane(_) => {
            let close = serde_json::json!({ "t": "close", "why": end.to_string() }).to_string();
            let told = Page::send(&mut page, Message::Text(close.into())).await;
            if told.is_ok() && Page::send(&mut page, Message::Close(None)).await.is_err() {
                log::debug!("the page went before its socket closed");
            }
        }
    }
}

/// The page's turn to be given a frame, sealed in its own module so that nothing else makes
/// one: the page being given the session's hello makes it ([`hello`]), giving the page a
/// frame spends it ([`Turn::give`]), and the page's word that it took the frame is the only
/// thing that makes another ([`Untaken::taken`]). A turn is not copied, so a session holds
/// at most one: while the session does not hold it, it is in the [`Hand`](super::Hand).
mod turn {
    use super::{End, Message, Page, Sequence};

    /// The page's turn, and the proof that the page has the session's hello.
    pub(super) struct Turn(());

    /// A frame the page was given and has not said it took: its name.
    pub(super) struct Untaken(Sequence);

    /// Gives the page the session's hello, `text`: the one place a turn comes from nothing,
    /// and it is the session's only once the page's socket has taken the hello.
    pub(super) async fn hello(page: &mut impl Page, text: &str) -> Result<Turn, End> {
        page.send(Message::Text(text.into())).await?;
        Ok(Turn(()))
    }

    impl Turn {
        /// The page is given the frame named `sequence`: the turn is spent on it.
        pub(super) fn give(self, sequence: Sequence) -> Untaken {
            Untaken(sequence)
        }
    }

    impl Untaken {
        /// The page said it took the frame: its name, and the turn for the next.
        pub(super) fn taken(self) -> (Sequence, Turn) {
            (self.0, Turn(()))
        }
    }
}

/// Where the page's turn is while the session does not hold it: with the frame the page was
/// given, or come back. The session and the reader of the page's words are two halves of
/// one session, each waiting on its own stream, and this is all they share.
enum Slot {
    /// The session holds the turn, or none was made yet.
    Empty,
    /// The page was given a frame, and has not said it took it.
    Given(Untaken),
    /// The page said it took the frame: the turn waits for the session.
    Back(Turn),
}

struct Hand {
    slot: parking_lot::Mutex<Slot>,
    /// Told when the turn comes back.
    back: Notify,
}

impl Hand {
    fn new() -> Hand {
        Hand {
            slot: parking_lot::Mutex::new(Slot::Empty),
            back: Notify::new(),
        }
    }

    /// The session gives the page a frame: said before the frame is sent, so that the
    /// page's word cannot come first.
    fn give(&self, untaken: Untaken) {
        *self.slot.lock() = Slot::Given(untaken);
    }

    /// The page said it took the frame it holds: the frame's name, and its turn comes back.
    /// With no frame held, the page took nothing.
    fn took(&self) -> Result<Sequence, End> {
        let mut slot = self.slot.lock();
        match std::mem::replace(&mut *slot, Slot::Empty) {
            Slot::Given(untaken) => {
                let (sequence, turn) = untaken.taken();
                *slot = Slot::Back(turn);
                self.back.notify_one();
                Ok(sequence)
            }
            other @ (Slot::Empty | Slot::Back(_)) => {
                *slot = other;
                Err(End::Took)
            }
        }
    }

    /// The turn, once the page's word brought it back. Dropped before then, it has taken
    /// nothing.
    async fn turn(&self) -> Turn {
        loop {
            let back = self.back.notified();
            {
                let mut slot = self.slot.lock();
                match std::mem::replace(&mut *slot, Slot::Empty) {
                    Slot::Back(turn) => return turn,
                    other @ (Slot::Empty | Slot::Given(_)) => *slot = other,
                }
            }
            back.await;
        }
    }
}

/// The page's messages, carried up the control stream as they come, from the session's
/// start, with the relay's own word of each update the page took, until the page closes or
/// the control stream does: the half of a session that hears the page. The session waits
/// on it beside everything else it waits on (`converse`), so input never waits behind a
/// frame the page has yet to take.
async fn upward(
    mut from_page: impl Stream<Item = Result<Message, axum::Error>> + Unpin,
    mut to_daemon: impl AsyncWrite + Unpin,
    hand: &Hand,
) -> End {
    let mut out = Vec::new();
    loop {
        let m = from_page.next().await;
        out.clear();
        match m {
            Some(Ok(Message::Text(t))) => Up::Text(t.as_str()).encode(&mut out),
            // The page's word: of the frame it holds.
            Some(Ok(Message::Binary(b))) if b.as_ref() == [TOOK] => match hand.took() {
                Ok(sequence) => Up::Took { sequence }.encode(&mut out),
                Err(end) => return end,
            },
            Some(Ok(Message::Binary(b))) => Up::Binary(&b).encode(&mut out),
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
            Some(Ok(Message::Close(_))) | None => return End::PageClosed,
            Some(Err(e)) => return End::PageLost(e),
        }
        if let Err(e) = to_daemon.write_all(&out).await {
            return End::ControlEnded(format!("writing to ssh: {e}"));
        }
    }
}

/// How the lane's stream is got: from the proxy, in a session; in the tests, in memory.
trait Door: Clone + Send {
    type Stream: AsyncRead + AsyncWrite + Unpin + Send;
    /// Asks for the lane's stream. It needs nothing of the session, so it is asked for as
    /// the session begins.
    fn connect(self) -> impl Future<Output = Result<Connected<Self::Stream>, End>> + Send;
}

/// The lane's stream as the proxy gave it: nothing has been sent on it.
struct Connected<S> {
    stream: S,
    /// What came with the proxy's answer, past its head: the lane's first bytes.
    framer: Framer,
}

impl<S: AsyncWrite + Unpin> Connected<S> {
    /// Attaches the lane to the session: its capability and what the page holds of the
    /// stream are the lane's first bytes, and the only ones the relay ever sends on it.
    async fn attach(mut self, key: &Capability, holds: Holds) -> Result<Open<S>, End> {
        let mut said = Vec::new();
        holds.encode(&mut said);
        // The capability goes from where it lies: it is copied into no buffer.
        for bytes in [&key.bytes()[..], &said] {
            self.stream
                .write_all(bytes)
                .await
                .map_err(|e| End::Proxy(ProxyError::Io(e)))?;
        }
        Ok(Open {
            stream: self.stream,
            framer: self.framer,
            read: vec![0; READ],
            standing: Standing::Unconfirmed,
        })
    }
}

/// Whether the daemon has confirmed an open lane (`Down::Attached`). One that ends attached
/// was closed by its transport, and is opened again, saying what the page holds: the daemon
/// goes on from that if it is the stream's newest frame, and starts its viewer over on a
/// keyframe if not. One that ends unconfirmed is the daemon or the transport refusing it, and
/// opening it again would loop.
///
/// It is the lane's stream's own, and an axis of the session of its own: the confirmation
/// crosses ssh, while the hello's turn comes from the page and the frames cross the lane, so
/// a stream is confirmed or not in each state that holds one (before the hello, with the
/// page free, with the page holding a frame: the session's test reaches all six).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Standing {
    Unconfirmed,
    Attached,
}

/// The lane's stream, open: the capability was sent on it.
struct Open<S> {
    stream: S,
    /// What has come of its frames.
    framer: Framer,
    /// Where its bytes are read.
    read: Vec<u8>,
    standing: Standing,
}

impl<S: AsyncRead + Unpin> Open<S> {
    /// The daemon confirmed the lane, which it does once.
    fn attached(self) -> Result<Open<S>, End> {
        match self.standing {
            Standing::Unconfirmed => Ok(Open {
                standing: Standing::Attached,
                ..self
            }),
            Standing::Attached => Err(End::Capability),
        }
    }

    /// The next whole frame, if one is in: its name, the page's message, which is the frame
    /// as it is without its name, and what the page holds of the stream once it took it, if
    /// the frame is an update. An update is its header and a picture: one cut short of
    /// either, or with flags no reader knows, is no frame.
    fn frame(&mut self) -> Result<Option<(Sequence, Bytes, Option<Holds>)>, End> {
        let Some(m) = self.framer.frame().map_err(End::Lane)? else {
            return Ok(None);
        };
        let frame = Frame::decode(m).map_err(End::Lane)?;
        let holds = match frame.track {
            Track::Video => {
                let update = Header::read(frame.bytes).map_err(End::Lane)?;
                Some(Holds::Through(update.header.version))
            }
            Track::Audio => None,
        };
        let message = Bytes::copy_from_slice(frame.page());
        Ok(Some((frame.sequence, message, holds)))
    }

    /// Reads what the lane has into its framer; or, at its end, says how it ended. Dropped
    /// before it is done, it has read nothing.
    async fn read(&mut self) -> Result<(), String> {
        match self.stream.read(&mut self.read).await {
            Ok(0) => Err("the proxy closed it".to_string()),
            Ok(n) => {
                self.framer.push(&self.read[..n]);
                Ok(())
            }
            Err(e) => Err(e.to_string()),
        }
    }
}

/// The proxy being asked for the lane's stream, before the session's capability came.
type Connecting<'d, S> = Pin<Box<dyn Future<Output = Result<Connected<S>, End>> + Send + 'd>>;

/// The lane being opened, once the capability is known: the proxy asked for its stream (or
/// already answered), then the capability written on it.
type Attaching<'k, S> = Pin<Box<dyn Future<Output = Result<Open<S>, End>> + Send + 'k>>;

/// The lane opened through `door` and attached with `key`, its page holding `holds`.
fn attaching<'k, D: Door + 'k>(
    door: D,
    key: &'k Capability,
    holds: Holds,
) -> Attaching<'k, D::Stream> {
    Box::pin(async move { door.connect().await?.attach(key, holds).await })
}

/// The lane before the session's capability came: nothing can be written on it.
enum Unkeyed<'d, S> {
    /// The proxy is being asked.
    Asking(Connecting<'d, S>),
    /// The proxy answered: the stream waits for the capability.
    Connected(Connected<S>),
}

/// A session's lane, once its capability came, and the page's turn, which are one state. The
/// lane's stream needs the proxy's answer and the capability written; a turn needs the hello
/// given; giving a frame needs an open lane and a turn. Each arm holds what has come, and
/// waits for exactly what is still missing (`serve` says what each waits on).
enum Lane<'k, S> {
    /// The lane is being opened, and the page has not been given the hello.
    Attaching { attaching: Attaching<'k, S> },
    /// The lane is open and the page has not been given the hello: there is no turn, and
    /// the lane is not read, whatever it brings.
    Unmet { open: Open<S> },
    /// The page has the hello, and the lane is being opened: for the first time still, or
    /// again after a confirmed lane ended. The page holds no frame.
    Opening {
        attaching: Attaching<'k, S>,
        turn: Turn,
    },
    /// The lane is open and the page holds no frame: the lane's next frame is the page's.
    Free { open: Open<S>, turn: Turn },
    /// The page holds a frame of the lane, which is not read until the page took it.
    Held { open: Open<S> },
}

impl<'k, S: AsyncRead + AsyncWrite + Unpin> Lane<'k, S> {
    /// The daemon confirmed the lane: one that is open, and not yet confirmed.
    fn attached(self) -> Result<Lane<'k, S>, End> {
        match self {
            Lane::Attaching { .. } | Lane::Opening { .. } => Err(End::Capability),
            Lane::Unmet { open } => Ok(Lane::Unmet {
                open: open.attached()?,
            }),
            Lane::Free { open, turn } => Ok(Lane::Free {
                open: open.attached()?,
                turn,
            }),
            Lane::Held { open } => Ok(Lane::Held {
                open: open.attached()?,
            }),
        }
    }

    /// Gives the page a hello, `text`. The session's first makes the page's turn
    /// ([`turn::hello`]); a hello after it is given as any text is, makes none, and is the
    /// page's to refuse.
    async fn hello(self, page: &mut impl Page, text: &str) -> Result<Lane<'k, S>, End> {
        Ok(match self {
            Lane::Attaching { attaching } => Lane::Opening {
                attaching,
                turn: turn::hello(page, text).await?,
            },
            Lane::Unmet { open } => Lane::Free {
                open,
                turn: turn::hello(page, text).await?,
            },
            met @ (Lane::Opening { .. } | Lane::Free { .. } | Lane::Held { .. }) => {
                page.send(Message::Text(text.into())).await?;
                met
            }
        })
    }

    /// Gives the page the lane's next frame, if the page holds none and one is in: the turn
    /// is spent on it, and `hand` holds what the page was given before the page does. An
    /// update given is what the page `holds` of the stream once it took it; and a lane is
    /// opened again only with the page's turn in hand, so then it took every frame given.
    async fn give(
        self,
        hand: &Hand,
        page: &mut impl Page,
        holds: &mut Holds,
    ) -> Result<Lane<'k, S>, End> {
        match self {
            Lane::Free { mut open, turn } => match open.frame()? {
                None => Ok(Lane::Free { open, turn }),
                Some((sequence, message, shows)) => {
                    if let Some(shows) = shows {
                        *holds = shows;
                    }
                    hand.give(turn.give(sequence));
                    page.send(Message::Binary(message)).await?;
                    Ok(Lane::Held { open })
                }
            },
            waiting @ (Lane::Attaching { .. }
            | Lane::Unmet { .. }
            | Lane::Opening { .. }
            | Lane::Held { .. }) => Ok(waiting),
        }
    }
}

/// The control stream from the daemon, as a session reads it. Dropped before it is done, a
/// read has read nothing.
trait Control {
    /// Reads what it has into `buf`, before the session's capability came: an end now is
    /// the session never having begun.
    fn first(&mut self, buf: &mut [u8]) -> impl Future<Output = Result<usize, End>> + Send;
    /// Reads what it has into `buf`, once the capability came.
    fn read(&mut self, buf: &mut [u8]) -> impl Future<Output = Result<usize, End>> + Send;
}

/// `voxl attach` over ssh: its stdout, and the process, whose exit says how it ended.
struct Ssh<'a> {
    to: &'a ssh::Destination,
    from_daemon: ChildStdout,
    child: Child,
}

impl Control for Ssh<'_> {
    /// An end before the capability: ssh's own failure is the door's absence.
    async fn first(&mut self, buf: &mut [u8]) -> Result<usize, End> {
        match self.from_daemon.read(buf).await {
            Ok(0) => Err(match self.child.wait().await {
                Ok(status) => match ssh::Unopened::of(status) {
                    ssh::Unopened::NoDoor => End::NoDoor(self.to.clone()),
                    ssh::Unopened::Exited(s) => End::ControlEnded(s.to_string()),
                },
                Err(e) => End::ControlEnded(e.to_string()),
            }),
            Ok(n) => Ok(n),
            Err(e) => Err(End::ControlEnded(e.to_string())),
        }
    }

    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, End> {
        match self.from_daemon.read(buf).await {
            Ok(0) => Err(End::ControlEnded(match self.child.wait().await {
                Ok(s) => s.to_string(),
                Err(e) => e.to_string(),
            })),
            Ok(n) => Ok(n),
            Err(e) => Err(End::ControlEnded(e.to_string())),
        }
    }
}

/// The way to the daemon's lane in a session: an HTTP CONNECT through the proxy.
#[derive(Clone, Copy)]
struct Proxied<'a>(&'a Config);

impl Door for Proxied<'_> {
    type Stream = TcpStream;

    async fn connect(self) -> Result<Connected<TcpStream>, End> {
        let reader = proxy::connect(&self.0.proxy, &self.0.place)
            .await
            .map_err(End::Proxy)?;
        let mut framer = Framer::new();
        framer.push(reader.buffer());
        Ok(Connected {
            stream: reader.into_inner(),
            framer,
        })
    }
}

/// A session, from ssh's start to its end.
async fn session(
    cfg: &Config,
    page: &mut impl Page,
    from_page: impl Stream<Item = Result<Message, axum::Error>> + Unpin,
) -> End {
    let mut child: Child = match ssh::attach(&cfg.ssh, &cfg.daemon) {
        Ok(c) => c,
        Err(e) => return End::Ssh(e),
    };
    let (Some(to_daemon), Some(from_daemon)) = (child.stdin.take(), child.stdout.take()) else {
        return End::Ssh(std::io::Error::other("ssh gave no stdin or stdout"));
    };
    let control = Ssh {
        to: &cfg.ssh,
        from_daemon,
        child,
    };
    converse(
        &cfg.files,
        Proxied(cfg),
        control,
        to_daemon,
        from_page,
        page,
    )
    .await
}

/// A session over its streams, until it ends: its two halves, each waiting on its own. One
/// hears the page and writes the control stream ([`upward`]); the other reads the control
/// stream and the lane and writes the page ([`serve`]). Both are futures of this one, so
/// neither can end but by returning its cause. Neither waits for the other but for the
/// page's turn, and the first to end is the session's end.
async fn converse(
    files: &files::Files,
    door: impl Door,
    control: impl Control,
    to_daemon: impl AsyncWrite + Unpin,
    from_page: impl Stream<Item = Result<Message, axum::Error>> + Unpin,
    page: &mut impl Page,
) -> End {
    let hand = Hand::new();
    tokio::select! {
        end = upward(from_page, to_daemon, &hand) => end,
        Err(end) = serve(files, door, control, &hand, page) => end,
    }
}

/// The half of a session that reads the control stream and the lane, and writes the page.
///
/// It begins by waiting for the session's capability, the control stream's first message,
/// beside the proxy's answer ([`Unkeyed`]). From then on each round acts on every control
/// message already read (one read often holds several: the capability, the hello, the
/// cursor, and a message left in the framer would wait on a read that may never come),
/// gives the page the lane's next frame if it may, and then waits for one thing, by what the
/// lane is ([`Lane`]). The control stream is read in every state, and beside it:
/// - asking, before the capability: the proxy's answer;
/// - connected, before the capability: nothing else (the stream waits for the capability);
/// - attaching and opening: the proxy's answer and then the capability's write, in one
///   future;
/// - unmet: nothing else (the lane is not read before the page has the hello);
/// - free: the lane's bytes;
/// - held: the page's word that it took its frame.
///
/// So the hello goes to the page as soon as it is read, though the lane is not open yet; the
/// capability is written as soon as both it and the stream are there; and the page's words
/// go up beside all of it ([`upward`]).
async fn serve<D: Door>(
    files: &files::Files,
    door: D,
    mut control: impl Control,
    hand: &Hand,
    page: &mut impl Page,
) -> Result<Infallible, End> {
    let mut framer = Framer::new();
    let mut buf = vec![0u8; READ];
    let mut unkeyed = Unkeyed::Asking(Box::pin(door.clone().connect()));
    let key = loop {
        if let Some(m) = framer.frame().map_err(End::Control)? {
            match Down::decode(m).map_err(End::Control)? {
                Down::Capability(key) => break key.keep(),
                Down::Attached | Down::Text(_) | Down::Binary(_) => return Err(End::Capability),
            }
        }
        unkeyed = match unkeyed {
            Unkeyed::Asking(mut connecting) => tokio::select! {
                n = control.first(&mut buf) => {
                    framer.push(&buf[..n?]);
                    Unkeyed::Asking(connecting)
                }
                connected = &mut connecting => Unkeyed::Connected(connected?),
            },
            Unkeyed::Connected(connected) => {
                let n = control.first(&mut buf).await?;
                framer.push(&buf[..n]);
                Unkeyed::Connected(connected)
            }
        };
    };
    let key = &key;
    // What the page holds of the stream: nothing before its first update, then the last
    // update it was given, which it took before any lane is opened again.
    let mut holds = Holds::Nothing;
    let mut lane: Lane<'_, D::Stream> = Lane::Attaching {
        attaching: match unkeyed {
            Unkeyed::Asking(connecting) => {
                Box::pin(async move { connecting.await?.attach(key, holds).await })
            }
            Unkeyed::Connected(connected) => Box::pin(connected.attach(key, holds)),
        },
    };
    loop {
        while let Some(m) = framer.frame().map_err(End::Control)? {
            lane = match Down::decode(m).map_err(End::Control)? {
                Down::Capability(_) => return Err(End::Capability),
                Down::Attached => lane.attached()?,
                Down::Text(text) => match serde_json::from_str(text).map_err(End::Text)? {
                    Greeting::Hello { files: shared } => {
                        *files.grant.lock() = Some(files::Grant { root: shared.root });
                        lane.hello(page, text).await?
                    }
                    Greeting::Other => {
                        page.send(Message::Text(text.into())).await?;
                        lane
                    }
                },
                Down::Binary(b) => {
                    page.send(Message::Binary(Bytes::copy_from_slice(b)))
                        .await?;
                    lane
                }
            };
        }
        lane = lane.give(hand, page, &mut holds).await?;
        lane = match lane {
            Lane::Attaching { mut attaching } => tokio::select! {
                n = control.read(&mut buf) => {
                    framer.push(&buf[..n?]);
                    Lane::Attaching { attaching }
                }
                open = &mut attaching => Lane::Unmet { open: open? },
            },
            Lane::Unmet { open } => {
                let n = control.read(&mut buf).await?;
                framer.push(&buf[..n]);
                Lane::Unmet { open }
            }
            Lane::Opening {
                mut attaching,
                turn,
            } => tokio::select! {
                n = control.read(&mut buf) => {
                    framer.push(&buf[..n?]);
                    Lane::Opening { attaching, turn }
                }
                open = &mut attaching => Lane::Free { open: open?, turn },
            },
            Lane::Free { mut open, turn } => tokio::select! {
                // What the control stream has is taken before the lane's end is: a lane the
                // daemon confirmed is not taken for one it refused because its end was read
                // first.
                biased;
                n = control.read(&mut buf) => {
                    framer.push(&buf[..n?]);
                    Lane::Free { open, turn }
                }
                read = open.read() => match (read, open.standing) {
                    (Ok(()), Standing::Unconfirmed | Standing::Attached) => {
                        Lane::Free { open, turn }
                    }
                    (Err(why), Standing::Unconfirmed) => return Err(End::LaneEnded(why)),
                    (Err(why), Standing::Attached) => {
                        log::info!("the lane ended ({why}): it opens again");
                        Lane::Opening {
                            attaching: attaching(door.clone(), key, holds),
                            turn,
                        }
                    }
                },
            },
            Lane::Held { open } => tokio::select! {
                n = control.read(&mut buf) => {
                    framer.push(&buf[..n?]);
                    Lane::Held { open }
                }
                turn = hand.turn() => Lane::Free { open, turn },
            },
        };
    }
}

/// What the relay reads of the daemon's text for the page: a hello names the shared folder
/// the files routes reach; any other kind of message is the page's alone. Every text, the
/// hello too, goes to the page as it is.
#[derive(serde::Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
enum Greeting {
    Hello {
        files: Shared,
    },
    #[serde(other)]
    Other,
}

/// The hello's `files`, as far as the files routes need it.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Shared {
    root: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::{FilesConfig, Hello, Resize, ServerMessage, Startup};
    use crate::session::control::Audio;
    use crate::wire::Byte;
    use crate::xorshift::Xorshift;
    use tokio::io::DuplexStream;
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
    use tokio::task::JoinHandle;

    /// What the daemon sends the page reads, as the relay reads it: every hello, as the
    /// daemon writes it, names the shared folder it was given; a clipboard or a microphone
    /// message is the page's alone.
    #[test]
    fn the_relay_reads_the_hello_and_passes_the_rest() {
        let mut rng = Xorshift::new(0xE7037ED1A0B428DB);
        for _ in 0..1_000 {
            let root: String = (0..rng.range(1, 30))
                .map(|_| ['/', 'a', 'é', '"', '\\', ' ', '7'][rng.below(7) as usize])
                .collect();
            let hello = ServerMessage::Hello(Hello {
                codec: format!("av01.0.{:02}M.08", rng.below(20)),
                startup: Startup {
                    audio: [Audio::Sent, Audio::Off][rng.below(2) as usize],
                    files: FilesConfig {
                        root: root.clone(),
                        dir: "Downloads".into(),
                    },
                    resize: Resize {
                        scale: 0.5 + rng.below(8) as f64 / 4.0,
                    },
                },
            });
            match serde_json::from_str::<Greeting>(&hello.json()) {
                Ok(Greeting::Hello { files }) => {
                    assert_eq!(files.root, root)
                }
                Ok(Greeting::Other) | Err(_) => {
                    panic!("a hello not read as one: {}", hello.json())
                }
            }
            let clip = ServerMessage::Clip { d: root.clone() };
            let mic = ServerMessage::Mic {
                on: rng.below(2) == 0,
            };
            for other in [clip, mic] {
                assert!(matches!(
                    serde_json::from_str::<Greeting>(&other.json()),
                    Ok(Greeting::Other)
                ));
            }
        }
    }

    /// The page's word is the page's byte: `bin.took` in web/src/wire.json, and no type
    /// the daemon reads from the page, so the daemon could not take one for its own.
    #[test]
    fn the_pages_word_is_its_own_byte() {
        let wire: serde_json::Value =
            serde_json::from_str(include_str!("../../web/src/wire.json")).unwrap();
        assert_eq!(wire["bin"]["took"], u64::from(TOOK));
        assert!(<crate::event::BinUp as wire::Byte>::of(TOOK).is_none());
    }

    /// What a pipe between the fake daemon and the relay holds: a quarter of the read the
    /// relay makes at a time, and less than the larger updates the tests send, so a lane's
    /// writer waits on the relay's reading.
    const PIPE: usize = READ / 4;

    /// The largest update the tests send: several pipes' worth.
    const UPDATE_MOST: u64 = 5 * PIPE as u64;

    /// A page that takes a message only when the test lets it: its socket, one message
    /// deep.
    struct Slow(tokio::sync::mpsc::Sender<Message>);

    impl Page for Slow {
        async fn send(&mut self, m: Message) -> Result<(), End> {
            self.0.send(m).await.map_err(|_| End::PageClosed)
        }
    }

    /// The proxy's answer to one asking for the lane's stream: the fake daemon gives the
    /// relay's end of a pipe when it will, or drops this, which is the proxy refusing.
    type Answer = tokio::sync::oneshot::Sender<DuplexStream>;

    /// The fake proxy: each asking for the lane's stream arrives at the fake daemon.
    #[derive(Clone)]
    struct Fake(UnboundedSender<Answer>);

    impl Door for Fake {
        type Stream = DuplexStream;

        async fn connect(self) -> Result<Connected<DuplexStream>, End> {
            let (answer, stream) = tokio::sync::oneshot::channel();
            self.0
                .send(answer)
                .map_err(|_| End::Proxy(ProxyError::Answer))?;
            Ok(Connected {
                stream: stream.await.map_err(|_| End::Proxy(ProxyError::Answer))?,
                framer: Framer::new(),
            })
        }
    }

    /// The control stream from a fake daemon: a pipe in memory, whose end is the stream's.
    struct Pipe(DuplexStream);

    impl Pipe {
        async fn read(&mut self, buf: &mut [u8], ended: &str) -> Result<usize, End> {
            match self.0.read(buf).await {
                Ok(0) | Err(_) => Err(End::ControlEnded(ended.into())),
                Ok(n) => Ok(n),
            }
        }
    }

    impl Control for Pipe {
        async fn first(&mut self, buf: &mut [u8]) -> Result<usize, End> {
            self.read(buf, "before the capability").await
        }

        async fn read(&mut self, buf: &mut [u8]) -> Result<usize, End> {
            self.read(buf, "after the capability").await
        }
    }

    /// A session (`converse`: both its halves) between a fake daemon and a fake page, all in
    /// memory.
    struct Rig {
        session: JoinHandle<End>,
        /// Each asking of the proxy for the lane's stream.
        asks: UnboundedReceiver<Answer>,
        /// The control stream down: the daemon's end.
        control: DuplexStream,
        /// The control stream up: the daemon's end.
        up: DuplexStream,
        /// What the page is given.
        page: tokio::sync::mpsc::Receiver<Message>,
        /// What the page says.
        says: UnboundedSender<Result<Message, axum::Error>>,
    }

    fn rig() -> Rig {
        let (door, asks) = unbounded_channel();
        let (control, from_daemon) = tokio::io::duplex(PIPE);
        let (to_daemon, up) = tokio::io::duplex(1 << 22);
        let (to_page, page) = tokio::sync::mpsc::channel(1);
        let (says, said) = unbounded_channel();
        let from_page = Box::pin(futures_util::stream::unfold(said, |mut said| async {
            said.recv().await.map(|m| (m, said))
        }));
        let session = tokio::spawn(async move {
            let files = files::Files {
                to: ssh::Destination::parse("desktop").unwrap(),
                grant: parking_lot::Mutex::new(None),
            };
            let mut page = Slow(to_page);
            converse(
                &files,
                Fake(door),
                Pipe(from_daemon),
                to_daemon,
                from_page,
                &mut page,
            )
            .await
        });
        Rig {
            session,
            asks,
            control,
            up,
            page,
            says,
        }
    }

    /// Longer than any session in a test takes on the tests' clock, which runs only while
    /// every task waits: a wait this long that ends is a session that stopped.
    const HOUR: std::time::Duration = std::time::Duration::from_secs(3600);

    /// The turns `settle` gives the other tasks: several times what the largest update
    /// takes to cross a pipe and reach the page (a turn of its writer and one of the relay
    /// for each pipe's worth, `UPDATE_MOST / PIPE` of them, and the hand-off's few).
    const SETTLE: u64 = 8 * (UPDATE_MOST / PIPE as u64 + 3);

    /// Lets every task run until none can: the pipes are in memory, so nothing waits on
    /// anything but another task, and a frame crosses a pipe in a few turns.
    async fn settle() {
        for _ in 0..SETTLE {
            tokio::task::yield_now().await;
        }
    }

    /// A message with its length before it, as a lane or the control stream carries it.
    fn framed(message: &[u8]) -> Vec<u8> {
        let mut out = (message.len() as u32).to_be_bytes().to_vec();
        out.extend_from_slice(message);
        out
    }

    /// A lane frame as the lane carries it, without its length.
    fn frame(sequence: Sequence, track: Track, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        Frame::encode(sequence, track, bytes, &mut out);
        out.split_off(wire::PREFIX)
    }

    /// A frame the fake daemon writes on the lane: as the lane carries it, as the page is
    /// to be given it, its name, which the daemon is to be told when the page took it, and,
    /// of an update, the version its header says.
    #[derive(Clone)]
    struct Written {
        lane: Vec<u8>,
        page: Vec<u8>,
        sequence: Sequence,
        shows: Option<wire::Version>,
    }

    impl Written {
        fn of(sequence: Sequence, track: Track, bytes: &[u8]) -> Written {
            Written {
                lane: frame(sequence, track, bytes),
                page: [&[track.byte()][..], bytes].concat(),
                sequence,
                shows: match track {
                    Track::Video => Some(wire::Version(u32::from_be_bytes(
                        bytes[1..5].try_into().unwrap(),
                    ))),
                    Track::Audio => None,
                },
            }
        }
    }

    /// A control message as the control stream carries it, without its length.
    fn up(up: Up<'_>) -> Vec<u8> {
        let mut out = Vec::new();
        up.encode(&mut out);
        out.split_off(wire::PREFIX)
    }

    fn down(down: Down<'_>) -> Vec<u8> {
        let mut out = Vec::new();
        down.encode(&mut out);
        out
    }

    /// A video update of a picture of `len` bytes, its header and its picture.
    fn update(rng: &mut Xorshift, coding: wire::Coding, len: usize) -> Vec<u8> {
        let header = Header {
            coding,
            version: wire::Version(rng.next() as u32),
            pipe: wire::Pipe::of(std::time::Duration::from_micros(rng.next() >> 40)),
        };
        let mut bytes = header.bytes().to_vec();
        bytes.resize(Header::LEN + len, rng.next() as u8);
        bytes
    }

    /// How a lane's frame of this place is coded: its first a keyframe, the rest deltas.
    fn first_or_later(place: u64) -> wire::Coding {
        match place {
            0 => wire::Coding::Key,
            1.. => wire::Coding::Delta,
        }
    }

    /// A session's hello, as the daemon writes it.
    fn hello(rng: &mut Xorshift) -> String {
        format!(
            "{{\"t\":\"hello\",\"codec\":\"av01.0.{:02}M.08\",\"audio\":true,\"files\":{{\"root\":\"/r\",\"dir\":\"d\"}}}}",
            rng.below(20)
        )
    }

    /// What a fake daemon waits for of the relay, or a panic if the relay never does it: a
    /// test stops for a relay that stopped, and does not hang with it.
    async fn within<T>(what: &str, relay_does: impl Future<Output = T>) -> T {
        match tokio::time::timeout(HOUR, relay_does).await {
            Ok(done) => done,
            Err(_) => panic!("the relay never did this: {what}"),
        }
    }

    /// The fake proxy answers one asking: the daemon's end of the lane's stream.
    async fn answer(asks: &mut UnboundedReceiver<Answer>) -> DuplexStream {
        let (relay, daemon) = tokio::io::duplex(PIPE);
        let ask = within("ask the proxy for the lane", asks.recv())
            .await
            .expect("the relay's door is there");
        ask.send(relay).expect("the relay waits for the answer");
        daemon
    }

    /// Waits until `met` holds, which whoever makes it hold says on `told`.
    async fn wait(told: &tokio::sync::Notify, met: impl Fn() -> bool) {
        loop {
            let said = told.notified();
            if met() {
                return;
            }
            said.await;
        }
    }

    /// The lane attached: its first bytes are the session's capability, and then what the
    /// page holds of the stream, which is returned.
    async fn attached(lane: &mut DuplexStream, key: &Capability) -> Holds {
        let mut first = [0u8; wire::CAPABILITY];
        within("attach the lane", lane.read_exact(&mut first))
            .await
            .expect("the lane is open");
        assert!(
            Capability::presented(&first).matches(key),
            "the lane's first bytes are not the session's capability"
        );
        let mut said = vec![0u8; 1];
        within("say what the page holds", lane.read_exact(&mut said))
            .await
            .expect("the lane is open");
        let after = Holds::after(said[0]).expect("a lane says what its page holds");
        said.resize(1 + after, 0);
        within("say what the page holds", lane.read_exact(&mut said[1..]))
            .await
            .expect("the lane is open");
        Holds::decode(&said).expect("a lane says what its page holds")
    }

    /// How a fake session starts: what the relay has when.
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Start {
        /// The proxy answers before the capability comes.
        AnswerFirst,
        /// The capability comes before the proxy answers.
        CapabilityFirst,
        /// The capability and the hello come, and the proxy answers only once the page has
        /// the hello.
        HelloBeforeTheAnswer,
    }

    /// What the fake daemon does once the lane is attached, in its order.
    enum Does {
        /// Writes a frame on the lane.
        Frame(Written),
        /// Confirms the lane.
        Attach,
        /// Writes the session's hello on the control stream, once the relay has had every
        /// chance to give the page what the lane brought before it.
        Hello(String),
        /// Sends the page a text or a binary message on the control stream.
        Tell(Message),
        /// Ends the lane, once the page has every frame written on it, and takes the next
        /// the relay asks for.
        EndLane,
    }

    /// Everything the page is given and the daemon is told, over generated sessions. The
    /// proxy answers before the capability comes or after it, or only after the page has
    /// the hello. The daemon writes updates of generated sizes and Opus packets on the lane,
    /// each named the next of a sequence that starts anywhere and wraps, confirms each lane before its frames or among them, writes the hello on
    /// the control stream before the lane's first frames or after some of them, sends the
    /// page texts and binary messages, and ends the lane at drawn places, after which the
    /// relay asks for it again. The page takes each frame when the test lets it, and says
    /// other things (input, the microphone) before and after its word. In every session:
    /// - the proxy is asked for the lane at once, before anything has come from the daemon;
    ///   the lane's first bytes are the session's capability, on every lane; and the hello
    ///   reaches the page though the proxy has not answered;
    /// - the page is given no frame before the session's hello, whichever of the two
    ///   streams brought its own first;
    /// - the page is given one frame at a time: while it holds one, nothing of the lane
    ///   comes, and while it holds its first, a lane with more to carry than its pipe, a
    ///   read and a frame hold has not been read through (the lane is not read);
    /// - every frame reaches the page whole, its message as it is without its name, in the
    ///   lane's order, each once, and every control message as it is, in the control
    ///   stream's order;
    /// - the daemon is told what the page said, as it said it, in order, and for the page's
    ///   word after a frame, update or packet alike, that frame's name;
    /// - a confirmed lane that ends is asked for again, and loses nothing it carried whole;
    ///   a session's first lane says its page holds nothing, and each lane opened again
    ///   says the last update written before it, on any lane, which the page took;
    /// - the session goes on until the page closes, and then ends for that.
    #[tokio::test(start_paused = true)]
    async fn the_page_is_given_one_frame_at_a_time_and_the_daemon_is_told() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let mut rng = Xorshift::new(0x3c6e_f372_fe94_f82b);
        let (mut reopened, mut held_back, mut told_in_hand, mut tooks) = (0, 0, 0, 0);
        let (mut lane_first, mut unread, mut again, mut starts) = (0, 0, 0, [0; 3]);
        // Lanes opened again that said their page held an update.
        let held_through = Arc::new(AtomicUsize::new(0));
        // Where each lane's standing is met, by the page's turn (no hello yet, free,
        // holding a frame) and the standing (unconfirmed, attached), as the plan orders the
        // two streams: the standing is an axis of its own, and every pair comes.
        const UNMET: usize = 0;
        const FREE: usize = 1;
        const HELD: usize = 2;
        let mut axes = [[0u32; 2]; 3];
        for case in 0..60 {
            let start = [
                Start::AnswerFirst,
                Start::CapabilityFirst,
                Start::HelloBeforeTheAnswer,
            ][case % 3];
            starts[case % 3] += 1;
            let greeting = hello(&mut rng);
            let mut sequence = Sequence((rng.next() >> rng.below(64)) as u32);
            let mut plan = vec![];
            let (mut attached_said, mut frames_on_lane) = (false, 0u64);
            // Where the hello goes among the daemon's doings: before the lane's frames, or
            // after some of them.
            let steps = rng.range(4, 50);
            let hello_at = match start {
                Start::HelloBeforeTheAnswer => None,
                Start::AnswerFirst | Start::CapabilityFirst => Some(rng.below(steps.min(5))),
            };
            let mut met = hello_at.is_none();
            for i in 0..steps {
                if hello_at == Some(i) {
                    if frames_on_lane > 0 {
                        lane_first += 1;
                    }
                    plan.push(Does::Hello(greeting.clone()));
                    met = true;
                }
                if !attached_said && (frames_on_lane > 0 || rng.below(2) == 0) {
                    plan.push(Does::Attach);
                    attached_said = true;
                    if !met {
                        axes[UNMET][1] += 1;
                    }
                }
                let standing = usize::from(attached_said);
                match met {
                    true => {
                        axes[FREE][standing] += 1;
                        axes[HELD][standing] += 1;
                    }
                    false => axes[UNMET][standing] += 1,
                }
                if rng.below(5) == 0 {
                    plan.push(Does::Tell(match rng.below(2) {
                        0 => Message::Text(format!("{{\"t\":\"clip\",\"d\":\"{i}\"}}").into()),
                        1.. => Message::Binary(vec![3, i as u8, 0].into()),
                    }));
                }
                sequence = sequence.next();
                plan.push(Does::Frame(match rng.below(3) {
                    0 => {
                        let packet = vec![i as u8; rng.range(1, 300) as usize];
                        Written::of(sequence, Track::Audio, &packet)
                    }
                    1.. => {
                        let len = match rng.below(4) {
                            0 => rng.range(1, UPDATE_MOST),
                            1.. => rng.range(1, 3_000),
                        } as usize;
                        let coding = first_or_later(frames_on_lane);
                        Written::of(sequence, Track::Video, &update(&mut rng, coding, len))
                    }
                }));
                frames_on_lane += 1;
                // A hello again, which only the page refuses: the relay makes no turn of it.
                if met && rng.below(12) == 0 {
                    plan.push(Does::Hello(greeting.clone()));
                    again += 1;
                }
                // A lane ends only once the page has all it carried, so only after the hello.
                if attached_said && met && rng.below(8) == 0 {
                    plan.push(Does::EndLane);
                    (attached_said, frames_on_lane) = (false, 0);
                    reopened += 1;
                }
            }
            let frames: Vec<Written> = plan
                .iter()
                .filter_map(|d| match d {
                    Does::Frame(written) => Some(written.clone()),
                    Does::Attach | Does::Hello(_) | Does::Tell(_) | Does::EndLane => None,
                })
                .collect();
            let hello_first = match start {
                Start::HelloBeforeTheAnswer => Some(Message::Text(greeting.clone().into())),
                Start::AnswerFirst | Start::CapabilityFirst => None,
            };
            let tells: Vec<Message> = hello_first
                .into_iter()
                .chain(plan.iter().filter_map(|d| match d {
                    Does::Tell(m) => Some(m.clone()),
                    Does::Hello(text) => Some(Message::Text(text.clone().into())),
                    Does::Frame(..) | Does::Attach | Does::EndLane => None,
                }))
                .collect();
            let size: usize = frames.iter().map(|f| f.lane.len()).sum();

            let Rig {
                session,
                mut asks,
                mut control,
                up: mut went_up,
                mut page,
                says,
            } = rig();
            // The proxy is asked before anything has come from the daemon.
            settle().await;
            assert!(
                !asks.is_empty(),
                "case {case}: the proxy was not asked for the lane at once"
            );
            // The fake daemon. `arrived` is the frames the page has been given, `greeted`
            // whether it has the hello, and `done` whether the daemon wrote everything.
            let arrived = Arc::new((tokio::sync::Notify::new(), AtomicUsize::new(0)));
            let greeted = Arc::new((tokio::sync::Notify::new(), AtomicBool::new(false)));
            let done = Arc::new(AtomicBool::new(false));
            // Its lane and its control stream are written by a task each, as the daemon's
            // are: one stream waiting on the relay does not hold the other. What ties them is
            // what a daemon's own order ties: a lane ends only after what the control stream
            // said before it (its confirmation, the hello), and the control stream speaks of
            // a lane only once it is attached. `went` counts, in the plan's order, the frames
            // the lane has begun to write, the lanes ended and attached again, and what the
            // control stream has said.
            let plan = Arc::new(plan);
            let went = Arc::new((
                tokio::sync::Notify::new(),
                [const { AtomicUsize::new(0) }; 3],
            ));
            const BEGUN: usize = 0;
            const ENDED: usize = 1;
            const SAID: usize = 2;
            // For each place in the plan, how many of each come before it.
            let before: Arc<Vec<[usize; 3]>> = Arc::new(
                plan.iter()
                    .scan([0; 3], |seen, does| {
                        let here = *seen;
                        match does {
                            Does::Frame(..) => seen[BEGUN] += 1,
                            Does::EndLane => seen[ENDED] += 1,
                            Does::Attach | Does::Hello(_) | Does::Tell(_) => seen[SAID] += 1,
                        }
                        Some(here)
                    })
                    .collect(),
            );
            let (hand_over, handed) = tokio::sync::oneshot::channel::<DuplexStream>();
            let held_through = held_through.clone();
            let on_lane = {
                let (arrived, done, greeted) = (arrived.clone(), done.clone(), greeted.clone());
                let (plan, went, before) = (plan.clone(), went.clone(), before.clone());
                let greeting = greeting.clone();
                tokio::spawn(async move {
                    let key = Capability::mint().unwrap();
                    let capability = down(Down::Capability(key.lend()));
                    let mut lane = match start {
                        Start::AnswerFirst => {
                            let lane = answer(&mut asks).await;
                            settle().await;
                            control.write_all(&capability).await.unwrap();
                            lane
                        }
                        Start::CapabilityFirst => {
                            control.write_all(&capability).await.unwrap();
                            settle().await;
                            answer(&mut asks).await
                        }
                        Start::HelloBeforeTheAnswer => {
                            control.write_all(&capability).await.unwrap();
                            let hello = down(Down::Text(&greeting));
                            control.write_all(&hello).await.unwrap();
                            wait(&greeted.0, || greeted.1.load(Ordering::SeqCst)).await;
                            answer(&mut asks).await
                        }
                    };
                    // A session's first lane says its page holds nothing.
                    assert_eq!(attached(&mut lane, &key).await, Holds::Nothing);
                    hand_over
                        .send(control)
                        .expect("the control stream's task waits");
                    let mut written = 0;
                    // The last update written, on any lane: the page took it before the lane
                    // it came on was ended.
                    let mut shown = Holds::Nothing;
                    for (does, before) in plan.iter().zip(before.iter()) {
                        match does {
                            Does::Frame(frame) => {
                                went.1[BEGUN].fetch_add(1, Ordering::SeqCst);
                                went.0.notify_waiters();
                                lane.write_all(&framed(&frame.lane)).await.unwrap();
                                written += 1;
                                if let Some(version) = frame.shows {
                                    shown = Holds::Through(version);
                                }
                            }
                            Does::EndLane => {
                                wait(&went.0, || {
                                    went.1[SAID].load(Ordering::SeqCst) >= before[SAID]
                                })
                                .await;
                                wait(&arrived.0, || arrived.1.load(Ordering::SeqCst) == written)
                                    .await;
                                settle().await;
                                drop(lane);
                                lane = answer(&mut asks).await;
                                let says = attached(&mut lane, &key).await;
                                assert_eq!(says, shown, "what a lane opened again says");
                                held_through.fetch_add(
                                    usize::from(shown != Holds::Nothing),
                                    Ordering::SeqCst,
                                );
                                went.1[ENDED].fetch_add(1, Ordering::SeqCst);
                                went.0.notify_waiters();
                            }
                            Does::Attach | Does::Hello(_) | Does::Tell(_) => {}
                        }
                    }
                    done.store(true, Ordering::SeqCst);
                    // The lane stays open: its end would be asked for again.
                    std::future::pending::<()>().await;
                    drop(lane);
                })
            };
            let on_control = {
                let (plan, went, before) = (plan.clone(), went.clone(), before.clone());
                tokio::spawn(async move {
                    let mut control = handed.await.expect("the lane's task hands it over");
                    for (does, before) in plan.iter().zip(before.iter()) {
                        let said = match does {
                            Does::Frame(..) | Does::EndLane => continue,
                            Does::Attach => down(Down::Attached),
                            Does::Hello(text) => down(Down::Text(text)),
                            Does::Tell(Message::Text(t)) => down(Down::Text(t.as_str())),
                            Does::Tell(Message::Binary(b)) => down(Down::Binary(b)),
                            Does::Tell(_) => unreachable!("the plan tells texts and binaries"),
                        };
                        wait(&went.0, || {
                            went.1[ENDED].load(Ordering::SeqCst) >= before[ENDED]
                        })
                        .await;
                        // The lane writes what the plan has before this, as far as the
                        // relay lets it, and the relay has every chance to give it to the
                        // page: only then does the control stream speak.
                        loop {
                            let begun = went.1[BEGUN].load(Ordering::SeqCst);
                            settle().await;
                            if begun >= before[BEGUN]
                                || begun == went.1[BEGUN].load(Ordering::SeqCst)
                            {
                                break;
                            }
                        }
                        control.write_all(&said).await.unwrap();
                        went.1[SAID].fetch_add(1, Ordering::SeqCst);
                        went.0.notify_waiters();
                    }
                    // The control stream stays open: its end is the session's.
                    std::future::pending::<()>().await;
                    drop(control);
                })
            };

            // The page.
            let mut expected_up: Vec<Vec<u8>> = Vec::new();
            let say = |rng: &mut Xorshift, expected_up: &mut Vec<Vec<u8>>| {
                for _ in 0..rng.below(3) {
                    match rng.below(2) {
                        0 => {
                            let text = format!("k,{},1", rng.below(1000));
                            expected_up.push(up(Up::Text(&text)));
                            says.send(Ok(Message::Text(text.into()))).unwrap();
                        }
                        1.. => {
                            // The microphone, or the page's word with more after it,
                            // which is not the page's word.
                            let mut b = [vec![18], vec![TOOK, 0]][rng.below(2) as usize].clone();
                            b.extend((0..rng.below(20)).map(|_| rng.next() as u8));
                            expected_up.push(up(Up::Binary(&b)));
                            says.send(Ok(Message::Binary(b.into()))).unwrap();
                        }
                    }
                }
            };
            // What the page says goes up from the session's start, before any capability.
            say(&mut rng, &mut expected_up);
            let (mut got_frames, mut got_tells) = (0, 0);
            let lanes_own =
                |m: &Message| matches!(m, Message::Binary(b) if matches!(b.first(), Some(1 | 2)));
            let mut told = |m: &Message, got_tells: &mut usize| {
                assert!(
                    tells.get(*got_tells) == Some(m),
                    "case {case}: control message {got_tells} is not the stream's next"
                );
                *got_tells += 1;
                if *m != Message::Text(greeting.clone().into())
                    || greeted.1.swap(true, Ordering::SeqCst)
                {
                    return;
                }
                greeted.0.notify_waiters();
                // The lane is not read before the hello is given: one that brought its own
                // first, with more to carry than its pipe holds, has not been read through.
                if hello_at.is_some() && size > 2 * PIPE {
                    assert!(
                        !done.load(Ordering::SeqCst),
                        "case {case}: the lane was read through before the hello"
                    );
                    unread += 1;
                }
            };
            while got_frames < frames.len() || got_tells < tells.len() {
                let next = tokio::time::timeout(HOUR, page.recv());
                let Ok(Some(m)) = next.await else {
                    panic!(
                        "case {case} ({start:?}): the session stopped at frame {got_frames} \
                         of {} and control message {got_tells} of {}: {:?}",
                        frames.len(),
                        tells.len(),
                        session.is_finished().then_some("it ended")
                    );
                };
                if !lanes_own(&m) {
                    told(&m, &mut got_tells);
                    continue;
                }
                assert!(
                    greeted.1.load(Ordering::SeqCst),
                    "case {case} ({start:?}): a frame before the session's hello"
                );
                let Message::Binary(m) = m else {
                    unreachable!("a lane's frame is binary")
                };
                let written = &frames[got_frames];
                assert!(
                    m[..] == written.page[..],
                    "case {case}: frame {got_frames} is not the lane's"
                );
                got_frames += 1;
                arrived.1.store(got_frames, Ordering::SeqCst);
                arrived.0.notify_waiters();
                // While the page holds the frame, only the control stream's come.
                say(&mut rng, &mut expected_up);
                settle().await;
                while let Ok(m) = page.try_recv() {
                    assert!(
                        !lanes_own(&m),
                        "case {case}: a second frame while the page holds one"
                    );
                    told(&m, &mut got_tells);
                    told_in_hand += 1;
                    settle().await;
                }
                // The lane is not read while the page holds a frame: one with more to carry
                // than its pipe, a read and its largest frame hold is not read through.
                if got_frames == 1 && size > 2 * PIPE + READ + UPDATE_MOST as usize {
                    assert!(
                        !done.load(Ordering::SeqCst),
                        "case {case}: the lane was read through while the page held"
                    );
                    held_back += 1;
                }
                says.send(Ok(Message::Binary(vec![TOOK].into()))).unwrap();
                expected_up.push(up(Up::Took {
                    sequence: written.sequence,
                }));
                tooks += 1;
                say(&mut rng, &mut expected_up);
            }
            settle().await;
            assert!(page.try_recv().is_err(), "case {case}: a message too many");
            assert!(!session.is_finished(), "case {case}: the session ended");
            // The page closes: the session ends for that, and the daemon was told all.
            drop(says);
            let end = within("end when the page closed", session).await.unwrap();
            assert!(matches!(end, End::PageClosed), "case {case}: {end}");
            let mut bytes = Vec::new();
            went_up.read_to_end(&mut bytes).await.unwrap();
            let mut framer = Framer::new();
            framer.push(&bytes);
            let mut got_up = Vec::new();
            while let Some(m) = framer.frame().unwrap() {
                got_up.push(m.to_vec());
            }
            assert!(
                got_up == expected_up,
                "case {case}: {} went up, {} expected",
                got_up.len(),
                expected_up.len()
            );
            on_lane.abort();
            on_control.abort();
        }
        assert!(
            reopened > 20 && held_back > 5 && told_in_hand > 20 && tooks > 500,
            "{reopened} {held_back} {told_in_hand} {tooks}"
        );
        assert!(
            lane_first > 5 && unread > 5 && again > 5 && starts == [20; 3],
            "{lane_first} {unread} {again} {starts:?}"
        );
        let held_through = held_through.load(Ordering::SeqCst);
        assert!(held_through > 10, "{held_through} lanes said an update");
        assert!(
            axes.iter().flatten().all(|&n| n > 5),
            "the turn and the standing: {axes:?}"
        );
    }

    /// What ends a session, each for its cause: over generated sessions that go well for a
    /// drawn number of frames and then meet one fault. The proxy refusing the lane; a first
    /// message that is not the capability, or a second capability; a lane that ends before
    /// the daemon confirmed it; a confirmation of a lane already confirmed; the page's word
    /// with nothing held; a frame shorter than its name, a frame of no track, an update
    /// shorter than its header, one with no picture after it, one whose flags set a bit no
    /// reader knows, and a control message of no tag, which are malformed; text for the page that is none of the daemon's
    /// messages; the control stream's end, before the capability and after it; and the
    /// page's closing, and its socket's failure.
    #[tokio::test(start_paused = true)]
    async fn a_fault_ends_the_session_for_its_cause() {
        #[derive(Clone, Copy, Debug)]
        enum Fault {
            ProxyRefused,
            NotTheCapability,
            EndedUnopened,
            Refused,
            ConfirmedTwice,
            SecondCapability,
            Unheld,
            Unnamed,
            NoTrack,
            ShortUpdate,
            NoPicture,
            OtherFlags,
            NoTag,
            NotAMessage,
            ControlEnded,
            PageClosed,
            PageLost,
        }
        const FAULTS: [Fault; 17] = [
            Fault::ProxyRefused,
            Fault::NotTheCapability,
            Fault::EndedUnopened,
            Fault::Refused,
            Fault::ConfirmedTwice,
            Fault::SecondCapability,
            Fault::Unheld,
            Fault::Unnamed,
            Fault::NoTrack,
            Fault::ShortUpdate,
            Fault::NoPicture,
            Fault::OtherFlags,
            Fault::NoTag,
            Fault::NotAMessage,
            Fault::ControlEnded,
            Fault::PageClosed,
            Fault::PageLost,
        ];
        let mut rng = Xorshift::new(0x1F83_D9AB_FB41_BD6B);
        for case in 0..340 {
            let fault = FAULTS[case % FAULTS.len()];
            let Rig {
                session,
                mut asks,
                mut control,
                up: _up,
                mut page,
                says,
            } = rig();
            let key = Capability::mint().unwrap();
            let flags = 2 << rng.below(7) | rng.below(2) as u8;
            let ended = match fault {
                // Before the session's capability: nothing else has happened.
                Fault::ProxyRefused => {
                    drop(within("ask the proxy for the lane", asks.recv()).await);
                    session
                }
                Fault::NotTheCapability => {
                    let first = [
                        down(Down::Attached),
                        down(Down::Text(&hello(&mut rng))),
                        down(Down::Text("{\"t\":\"clip\",\"d\":\"x\"}")),
                        down(Down::Binary(&[3, 1])),
                    ][rng.below(4) as usize]
                        .clone();
                    control.write_all(&first).await.unwrap();
                    session
                }
                Fault::EndedUnopened => {
                    drop(control);
                    session
                }
                Fault::Refused
                | Fault::ConfirmedTwice
                | Fault::SecondCapability
                | Fault::Unheld
                | Fault::Unnamed
                | Fault::NoTrack
                | Fault::ShortUpdate
                | Fault::NoPicture
                | Fault::OtherFlags
                | Fault::NoTag
                | Fault::NotAMessage
                | Fault::ControlEnded
                | Fault::PageClosed
                | Fault::PageLost => {
                    let mut lane = answer(&mut asks).await;
                    control
                        .write_all(&down(Down::Capability(key.lend())))
                        .await
                        .unwrap();
                    attached(&mut lane, &key).await;
                    let greeting = hello(&mut rng);
                    control
                        .write_all(&down(Down::Text(&greeting)))
                        .await
                        .unwrap();
                    let got = tokio::time::timeout(HOUR, page.recv()).await;
                    assert!(
                        got == Ok(Some(Message::Text(greeting.into()))),
                        "case {case} ({fault:?}): the hello did not come"
                    );
                    if !matches!(fault, Fault::Refused) {
                        control.write_all(&down(Down::Attached)).await.unwrap();
                    }
                    // The session goes well for some frames, each taken.
                    for i in 0..rng.below(4) {
                        let len = rng.range(1, 2000) as usize;
                        let bytes = update(&mut rng, first_or_later(i), len);
                        let sent = Written::of(Sequence(i as u32), Track::Video, &bytes);
                        lane.write_all(&framed(&sent.lane)).await.unwrap();
                        let got = tokio::time::timeout(HOUR, page.recv());
                        let Ok(Some(Message::Binary(m))) = got.await else {
                            panic!("case {case} ({fault:?}): frame {i} did not come");
                        };
                        assert!(m[..] == sent.page[..], "case {case} ({fault:?}): frame {i}");
                        says.send(Ok(Message::Binary(vec![TOOK].into()))).unwrap();
                    }
                    settle().await;
                    assert!(
                        !session.is_finished(),
                        "case {case} ({fault:?}): it ended early"
                    );
                    let mut kept = (Some(lane), Some(control), Some(says));
                    let (lane, control) = (kept.0.as_mut().unwrap(), kept.1.as_mut().unwrap());
                    let mut header = Header {
                        coding: wire::Coding::Delta,
                        version: wire::Version(9),
                        pipe: wire::Pipe::of(std::time::Duration::ZERO),
                    }
                    .bytes()
                    .to_vec();
                    let named = Sequence(9);
                    match fault {
                        Fault::ProxyRefused | Fault::NotTheCapability | Fault::EndedUnopened => {
                            unreachable!("met before the capability")
                        }
                        Fault::Refused => drop(kept.0.take()),
                        Fault::ConfirmedTwice => {
                            control.write_all(&down(Down::Attached)).await.unwrap()
                        }
                        Fault::SecondCapability => control
                            .write_all(&down(Down::Capability(key.lend())))
                            .await
                            .unwrap(),
                        Fault::Unheld => {
                            let says = kept.2.as_ref().unwrap();
                            says.send(Ok(Message::Binary(vec![TOOK].into()))).unwrap()
                        }
                        Fault::Unnamed => {
                            let short = vec![0; rng.below(wire::PREFIX as u64) as usize];
                            lane.write_all(&framed(&short)).await.unwrap()
                        }
                        Fault::NoTrack => {
                            let none = [&named.0.to_be_bytes()[..], &[9, 0, 0]].concat();
                            lane.write_all(&framed(&none)).await.unwrap()
                        }
                        Fault::ShortUpdate => {
                            header.truncate(rng.below(Header::LEN as u64) as usize);
                            let cut = frame(named, Track::Video, &header);
                            lane.write_all(&framed(&cut)).await.unwrap()
                        }
                        Fault::NoPicture => {
                            let bare = frame(named, Track::Video, &header);
                            lane.write_all(&framed(&bare)).await.unwrap()
                        }
                        Fault::OtherFlags => {
                            header[0] = flags;
                            header.extend([7, 7, 7]);
                            let odd = frame(named, Track::Video, &header);
                            lane.write_all(&framed(&odd)).await.unwrap()
                        }
                        Fault::NoTag => control.write_all(&framed(&[99, 1])).await.unwrap(),
                        Fault::NotAMessage => {
                            let text = [
                                "not JSON",
                                "{\"d\":\"no kind\"}",
                                "{\"t\":\"hello\",\"codec\":\"no files\"}",
                            ][rng.below(3) as usize];
                            control.write_all(&down(Down::Text(text))).await.unwrap()
                        }
                        Fault::ControlEnded => drop(kept.1.take()),
                        Fault::PageClosed => drop(kept.2.take()),
                        Fault::PageLost => {
                            let says = kept.2.as_ref().unwrap();
                            let reset = axum::Error::new(std::io::Error::other("reset"));
                            says.send(Err(reset)).unwrap()
                        }
                    }
                    let end = tokio::time::timeout(HOUR, session).await;
                    drop(kept);
                    match end {
                        Ok(end) => {
                            let end = end.unwrap();
                            let right = match fault {
                                Fault::ProxyRefused
                                | Fault::NotTheCapability
                                | Fault::EndedUnopened => false,
                                Fault::Refused => matches!(end, End::LaneEnded(_)),
                                Fault::ConfirmedTwice | Fault::SecondCapability => {
                                    matches!(end, End::Capability)
                                }
                                Fault::Unheld => matches!(end, End::Took),
                                Fault::NoTrack => {
                                    matches!(end, End::Lane(wire::Malformed::Track(9)))
                                }
                                Fault::Unnamed | Fault::ShortUpdate => {
                                    matches!(end, End::Lane(wire::Malformed::Truncated))
                                }
                                Fault::NoPicture => {
                                    matches!(end, End::Lane(wire::Malformed::NoPicture))
                                }
                                Fault::OtherFlags => {
                                    matches!(end, End::Lane(wire::Malformed::Flags(f)) if f == flags)
                                }
                                Fault::NoTag => {
                                    matches!(end, End::Control(wire::Malformed::Tag(99)))
                                }
                                Fault::NotAMessage => matches!(end, End::Text(_)),
                                Fault::ControlEnded => {
                                    matches!(&end, End::ControlEnded(s) if s == "after the capability")
                                }
                                Fault::PageClosed => matches!(end, End::PageClosed),
                                Fault::PageLost => matches!(end, End::PageLost(_)),
                            };
                            assert!(right, "case {case} ({fault:?}): ended for {end:?}");
                            continue;
                        }
                        Err(_) => panic!("case {case} ({fault:?}): the session went on"),
                    }
                }
            };
            let end = tokio::time::timeout(HOUR, ended)
                .await
                .unwrap_or_else(|_| panic!("case {case} ({fault:?}): the session went on"))
                .unwrap();
            let right = match fault {
                Fault::ProxyRefused => matches!(end, End::Proxy(ProxyError::Answer)),
                Fault::NotTheCapability => matches!(end, End::Capability),
                Fault::EndedUnopened => {
                    matches!(&end, End::ControlEnded(s) if s == "before the capability")
                }
                Fault::Refused
                | Fault::ConfirmedTwice
                | Fault::SecondCapability
                | Fault::Unheld
                | Fault::Unnamed
                | Fault::NoTrack
                | Fault::ShortUpdate
                | Fault::NoPicture
                | Fault::OtherFlags
                | Fault::NoTag
                | Fault::NotAMessage
                | Fault::ControlEnded
                | Fault::PageClosed
                | Fault::PageLost => unreachable!("met after the capability"),
            };
            assert!(right, "case {case} ({fault:?}): ended for {end:?}");
        }
    }

    /// A lane's stream comes from the proxy by a CONNECT, and attaching it sends the
    /// session's capability, what its page holds, and nothing else. Against a proxy on 127.0.0.1 that answers the
    /// CONNECT and reads what follows until the relay's end closes.
    #[tokio::test]
    async fn a_lane_attaches_with_the_capability() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let ssh = ssh::Destination::parse("desktop").unwrap();
        let cfg = Config {
            proxy: proxy::Endpoint::parse(&format!("127.0.0.1:{port}")).unwrap(),
            ssh: ssh.clone(),
            daemon: ssh::Daemon::Default,
            place: proxy::Place::protocol(),
            files: Arc::new(files::Files {
                to: ssh,
                grant: parking_lot::Mutex::new(None),
            }),
        };
        let capability = Capability::mint().unwrap();
        let proxy = tokio::spawn(async move {
            let (mut tcp, _) = listener.accept().await.unwrap();
            let mut asked = Vec::new();
            while !asked.ends_with(b"\r\n\r\n") {
                let mut b = [0u8; 1];
                tcp.read_exact(&mut b).await.unwrap();
                asked.push(b[0]);
            }
            tcp.write_all(b"HTTP/1.1 200 OK\r\n\r\n").await.unwrap();
            let mut sent = Vec::new();
            tcp.read_to_end(&mut sent).await.unwrap();
            sent
        });
        let connected = Proxied(&cfg)
            .connect()
            .await
            .unwrap_or_else(|end| panic!("{end}"));
        let mut rng = Xorshift::new(0xa54f_f53a_5f1d_36f1);
        let holds = Holds::Through(wire::Version(rng.next() as u32));
        let open = connected
            .attach(&capability, holds)
            .await
            .unwrap_or_else(|end| panic!("{end}"));
        assert_eq!(open.standing, Standing::Unconfirmed);
        // The relay's end closes: the proxy has read all the lane sent.
        drop(open);
        let sent = proxy.await.unwrap();
        let Some((presented, said)) = sent.split_first_chunk::<{ wire::CAPABILITY }>() else {
            panic!("the lane sent {} bytes", sent.len());
        };
        assert!(Capability::presented(presented).matches(&capability));
        assert_eq!(Holds::decode(said), Ok(holds));
    }
}
