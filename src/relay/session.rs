//! One page's session: its WebSocket, its `voxl attach` over ssh (the control stream), and
//! its lanes. They start together and end together, for one cause the page is told; nothing
//! reconnects by itself.
//!
//! The control stream carries the page's messages up as they are, the daemon's down as they
//! are, the lane's confirmation, the size the lane's socket buffers, and on a datagram lane
//! a report of every block. The session's one lane carries its frames: on a datagram lane
//! the pieces and repairs of coded blocks, which the decoder makes whole; on a stream lane
//! the frames as they are, in order. [`Release`] decides when the page is given them.

use super::description::{Description, Malformed as Undescribed};
use super::proxy::{self, ProxyError};
use super::release::{Give, Release, When};
use super::{files, ssh};
use crate::lanes::code::{self, Decided, Decoder, Took};
use crate::lanes::wire::{
    self, Attach, Capability, Down, Framer, Generation, Lane, Member, Stamp, Up,
};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use std::fmt;
use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::process::{Child, ChildStdout};
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinHandle;

/// What every session is started with.
pub struct Config {
    /// The lanes description, read at every session start.
    pub lanes: PathBuf,
    pub ssh: ssh::Destination,
    /// Where the files routes learn the shared folder from a session's hello.
    pub files: Arc<files::Files>,
}

/// The longest message the page may send: one that fits a control message with its tag.
/// Derived: `wire::MAX_CONTROL` less the one byte of `Up`'s tag.
const PAGE_MESSAGE_MOST: usize = wire::MAX_CONTROL - 1;

/// The bytes read from ssh's stdout, or from a stream lane, at a time: a pipe's capacity,
/// 65,536 bytes on Linux (pipe(7): "the pipe capacity is 16 pages") and on macOS, the most
/// one read of ssh's stdout gives; a stream lane's frame larger than it is gathered by its
/// framer over several reads. Published.
const CONTROL_READ: usize = 64 << 10;

pub async fn upgrade(ws: WebSocketUpgrade, State(cfg): State<Arc<Config>>) -> Response {
    ws.max_message_size(PAGE_MESSAGE_MOST)
        .on_upgrade(move |socket| run(socket, cfg))
}

/// Why a session ended: the page shows it.
#[derive(Debug)]
pub enum End {
    /// The page closed its socket.
    PageClosed,
    /// The lanes description could not be read.
    Lanes(Undescribed),
    /// ssh could not be started.
    Ssh(std::io::Error),
    /// ssh failed before the session began: no door to the desktop is open (the relay's
    /// ssh rides the connection `ssh <destination>` holds, and never logs in on its own).
    NoDoor(ssh::Destination),
    /// The control stream ended: ssh exited (its own words are in the relay's log).
    ControlEnded(String),
    /// The control stream carried something that is not a control message.
    Control(wire::Malformed),
    /// The control stream's first message was not the capability, or a second one came.
    Capability,
    /// The lane could not be opened through the proxy.
    Proxy(ProxyError),
    /// The lane ended before it was attached: its transport refuses lanes.
    LaneEnded(String),
    /// The lane carried something that is not a lane message.
    Lane(wire::Malformed),
    /// The decoder refused a piece or repair.
    Refused(code::Refused),
}

impl fmt::Display for End {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            End::PageClosed => write!(f, "the page closed"),
            End::Lanes(m) => write!(f, "{m}"),
            End::Ssh(e) => write!(f, "ssh could not start: {e}"),
            End::NoDoor(to) => write!(
                f,
                "no door to the desktop is open: run `ssh {to}` in a terminal first, then reload"
            ),
            End::ControlEnded(how) => write!(f, "the ssh session ended ({how})"),
            End::Control(e) => write!(f, "the control stream is broken: {e}"),
            End::Capability => write!(f, "the server's voxl did not open the session as expected"),
            End::Proxy(error) => write!(f, "the lane: {error}"),
            End::LaneEnded(why) => write!(f, "the lane ended before it was attached: {why}"),
            End::Lane(error) => write!(f, "the lane is broken: {error}"),
            End::Refused(why) => write!(f, "the lane's decoder refused {why:?}"),
        }
    }
}

/// The page's socket, as the session writes to it: a message that cannot go is the page
/// gone.
#[derive(Clone)]
struct Page(mpsc::UnboundedSender<Message>);

impl Page {
    fn send(&self, m: Message) -> Result<(), End> {
        self.0.send(m).map_err(|_| End::PageClosed)
    }

    fn text(&self, t: String) -> Result<(), End> {
        self.send(Message::Text(t.into()))
    }

    /// A released frame, after the type byte its track has on the page (`bin` in
    /// web/src/wire.json): the lanes carry no type byte, the track says it.
    fn give(&self, g: Give<'_>) -> Result<(), End> {
        let (tag, bytes) = match g {
            Give::Video(b) => (VIDEO, b),
            Give::Audio(b) => (AUDIO, b),
        };
        let mut m = Vec::with_capacity(1 + bytes.len());
        m.push(tag);
        m.extend_from_slice(bytes);
        self.send(Message::Binary(Bytes::from(m)))
    }
}

/// The page's type bytes of a video update and an Opus packet: `bin.video` and `bin.audio`
/// in web/src/wire.json, which the page reads and a test here checks.
const VIDEO: u8 = 0x01;
const AUDIO: u8 = 0x02;

async fn run(socket: WebSocket, cfg: Arc<Config>) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut to_page) = mpsc::unbounded_channel::<Message>();
    let page = Page(tx);
    let writer = tokio::spawn(async move {
        while let Some(m) = to_page.recv().await {
            if sink.send(m).await.is_err() {
                return;
            }
        }
    });
    let end = session(&cfg, &page, &mut stream).await;
    log::info!("session ended: {end}");
    let close = serde_json::json!({ "t": "close", "why": end.to_string() }).to_string();
    // The page is told the cause before its socket closes, unless the page is what ended.
    if page
        .text(close)
        .and(page.send(Message::Close(None)))
        .is_ok()
    {
        drop(page);
        if let Err(e) = writer.await {
            log::error!("the page's writer: {e}");
        }
    }
}

/// What the lane tasks and the session share: the decoder and the release, the page, and
/// the session's clock.
struct Receiver {
    decoder: Decoder,
    release: Release,
    page: Page,
    /// The relay's clock reads 0 here.
    origin: Instant,
}

impl Receiver {
    fn now(&self) -> Stamp {
        Stamp::since(self.origin, Instant::now())
    }

    /// One message from the lane, arrived now: a piece or repair of a datagram lane's block,
    /// or a stream lane's whole frame.
    fn take(&mut self, b: &[u8]) -> Result<(), End> {
        let msg = Lane::decode(b).map_err(End::Lane)?;
        let at = self.now();
        let Receiver {
            decoder,
            release,
            page,
            ..
        } = self;
        let mut given = Ok(());
        let mut give = |g: Give<'_>| {
            if given.is_ok() {
                given = page.give(g);
            }
        };
        match msg {
            // On a stream lane a frame arrives whole and in order: every frame before it has
            // arrived, so it is decided as it comes.
            Lane::Frame {
                frame,
                track,
                bytes,
            } => {
                let member = Member {
                    frame,
                    track,
                    bytes,
                };
                release.whole(&member, When::Now, &mut give);
                release.decided(frame, &mut give);
            }
            Lane::Piece(_) | Lane::Repair(_) => match decoder.take(msg, at) {
                Ok(Took::Whole(head, bytes)) => {
                    let due = head.due;
                    for member in head.members(bytes).map_err(End::Lane)? {
                        release.whole(&member, When::At(due), &mut give);
                    }
                }
                Ok(Took::Held | Took::Spare | Took::Late) => {}
                Err(why) => return Err(End::Refused(why)),
            },
        }
        given
    }

    /// Decides every frame due by now, writing each report into `out` as a control message,
    /// and gives the page the Opus packets due by now.
    fn decide(&mut self, out: &mut Vec<u8>) -> Result<(), End> {
        let now = self.now();
        let Receiver {
            decoder,
            release,
            page,
            ..
        } = self;
        let mut given = Ok(());
        let mut give = |g: Give<'_>| {
            if given.is_ok() {
                given = page.give(g);
            }
        };
        while let Some(Decided { report, last }) = decoder.decide(now) {
            Up::Report(report).encode(out);
            release.decided(last, &mut give);
        }
        release.due(now, &mut give);
        given
    }

    /// When the decoder next has a frame to decide, or an Opus packet falls due, whichever
    /// is first.
    fn wake(&self) -> Option<Instant> {
        let now = Instant::now();
        let stamp = Stamp::since(self.origin, now);
        [self.decoder.wake(), self.release.next_due()]
            .into_iter()
            .flatten()
            .map(|at| now + stamp.until(at))
            .min()
    }
}

/// What a lane task says when it ends.
struct LaneEnd {
    generation: Generation,
    end: End,
}

/// Where the lane is in attaching.
#[derive(Clone, Copy, PartialEq)]
enum Attaching {
    /// Its attach went at this moment, unconfirmed.
    Sent(Instant),
    Attached,
}

/// The session's lane: its opening, its attach, its reading task (stopped when the lane is
/// dropped), its socket (for the size the daemon names), and for a datagram lane its
/// association (to send the attach again).
struct Open {
    generation: Generation,
    attaching: Attaching,
    task: JoinHandle<()>,
    socket: Socket,
    /// What its socket holds, as the daemon last named it.
    buffer: std::cell::Cell<proxy::Buffer>,
    association: Option<Arc<proxy::Association>>,
}

/// The lane's socket, kept open by its reader.
enum Socket {
    Datagram(Arc<proxy::Association>),
    Stream(Arc<TcpStream>),
}

impl Socket {
    fn fd(&self) -> RawFd {
        match self {
            Socket::Datagram(a) => a.socket.as_raw_fd(),
            Socket::Stream(s) => s.as_raw_fd(),
        }
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The session's lane: closed until the daemon names the size its socket holds (the first
/// `Down::Buffer`, which the daemon sends before anything else), then open.
enum Lanes {
    Closed,
    Open(Open),
}

/// When the datagram lane's attach, if unconfirmed, goes again: a round trip after it went,
/// when no confirmation could still be on its way.
fn resend(lane: &Lanes, round_trip: Duration) -> Option<Instant> {
    match lane {
        Lanes::Open(Open {
            association: Some(_),
            attaching: Attaching::Sent(at),
            ..
        }) => Some(*at + round_trip),
        _ => None,
    }
}

/// Everything a session holds while it runs.
struct Session<'a> {
    cfg: &'a Config,
    description: Description,
    capability: Capability,
    receiver: Arc<Mutex<Receiver>>,
    /// Rung by the lane task after a message arrived, so the next decision is looked at
    /// again.
    arrived: Arc<Notify>,
    ends: mpsc::UnboundedSender<LaneEnd>,
    /// From starting `ssh … voxl attach` to its capability: at least one round trip of the
    /// path (see `round_trip`), which each attach tells the daemon, its first sample of the
    /// path before any report.
    door: Duration,
    /// The least round trip seen, after which an unconfirmed datagram attach goes again.
    /// Before any attach is confirmed it is the time from starting `ssh … voxl attach` to its
    /// capability: ssh's request to run `voxl attach` went to the server and the daemon's
    /// first message came back, so it holds at least one round trip of the path, and an
    /// attach is never sent again while its confirmation could still be on its way. Each
    /// confirmation then brings it down to the attach's own round trip, if less.
    round_trip: Duration,
}

async fn session(
    cfg: &Config,
    page: &Page,
    from_page: &mut futures_util::stream::SplitStream<WebSocket>,
) -> End {
    let description = match Description::read(&cfg.lanes) {
        Ok(d) => d,
        Err(m) => return End::Lanes(m),
    };
    let opened = Instant::now();
    let mut child: Child = match ssh::attach(&cfg.ssh) {
        Ok(c) => c,
        Err(e) => return End::Ssh(e),
    };
    let (Some(mut to_daemon), Some(mut from_daemon)) = (child.stdin.take(), child.stdout.take())
    else {
        return End::Ssh(std::io::Error::other("ssh gave no stdin or stdout"));
    };
    let mut framer = Framer::control();
    let mut buf = vec![0u8; CONTROL_READ];
    // The capability is the first message, and is read before anything else is done.
    let capability = loop {
        match framer.frame() {
            Err(e) => return End::Control(e),
            Ok(Some(m)) => match Down::decode(m) {
                Ok(Down::Capability(c)) => break c.keep(),
                Ok(_) => return End::Capability,
                Err(e) => return End::Control(e),
            },
            // An end before the capability: ssh's own failure is the door's absence.
            Ok(None) => match from_daemon.read(&mut buf).await {
                Ok(0) => {
                    return match child.wait().await {
                        Ok(status) => match ssh::Unopened::of(status) {
                            ssh::Unopened::NoDoor => End::NoDoor(cfg.ssh.clone()),
                            ssh::Unopened::Exited(s) => End::ControlEnded(s.to_string()),
                        },
                        Err(e) => End::ControlEnded(e.to_string()),
                    }
                }
                Ok(n) => framer.push(&buf[..n]),
                Err(e) => return End::ControlEnded(e.to_string()),
            },
        }
    };
    // The page learns the lane's kind before the stream's hello: on a datagram lane a keyframe
    // may miss, and the page waits through the deltas after it; on a stream lane every frame
    // comes.
    let kind = match &description {
        Description::Datagram { .. } => "datagram",
        Description::Stream { .. } => "stream",
    };
    if let Err(end) = page.text(serde_json::json!({ "t": "lane", "kind": kind }).to_string()) {
        return end;
    }
    let (ends, mut ended) = mpsc::unbounded_channel();
    let door = opened.elapsed();
    let mut s = Session {
        cfg,
        description,
        capability,
        receiver: Arc::new(Mutex::new(Receiver {
            decoder: Decoder::new(),
            release: Release::new(),
            page: page.clone(),
            origin: opened,
        })),
        arrived: Arc::new(Notify::new()),
        ends,
        door,
        round_trip: door,
    };
    let mut lane = Lanes::Closed;
    let mut out = Vec::new();
    loop {
        let decide = s.receiver.lock().wake();
        let resend = resend(&lane, s.round_trip);
        tokio::select! {
            m = from_page.next() => {
                let up = match &m {
                    Some(Ok(Message::Text(t))) => Up::Text(t.as_str()),
                    Some(Ok(Message::Binary(b))) => Up::Binary(b),
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    Some(Ok(Message::Close(_)) | Err(_)) | None => return End::PageClosed,
                };
                up.encode(&mut out);
            }
            r = read(&mut from_daemon, &mut buf, &mut child) => {
                match r {
                    Ok(n) => framer.push(&buf[..n]),
                    Err(end) => return end,
                }
                while let Some(m) = match framer.frame() {
                    Ok(m) => m,
                    Err(e) => return End::Control(e),
                } {
                    match Down::decode(m) {
                        Err(e) => return End::Control(e),
                        Ok(Down::Capability(_)) => return End::Capability,
                        Ok(Down::Attached { generation }) => {
                            if let Lanes::Open(open) = &mut lane {
                                s.attached(open, generation);
                            }
                        }
                        Ok(Down::Buffer { bytes }) => {
                            let buffer = proxy::Buffer::named(bytes);
                            match &lane {
                                // The lane opens once the daemon has named its size.
                                Lanes::Closed => match s.open(Generation::first(), buffer).await {
                                    Ok(open) => lane = Lanes::Open(open),
                                    Err(end) => return end,
                                },
                                Lanes::Open(open) => {
                                    if let Err(e) = buffer.hold(open.socket.fd()) {
                                        return End::Proxy(e);
                                    }
                                    open.buffer.set(buffer);
                                }
                            }
                        }
                        Ok(Down::Text(t)) => {
                            if let Ok(Said::Hello { files }) = serde_json::from_str::<Said>(t) {
                                *s.cfg.files.grant.lock() =
                                    Some(files::Grant { root: files.root });
                            }
                            if let Err(end) = page.text(t.to_string()) {
                                return end;
                            }
                        }
                        Ok(Down::Binary(b)) => {
                            if let Err(end) = page.send(Message::Binary(Bytes::copy_from_slice(b))) {
                                return end;
                            }
                        }
                    }
                }
            }
            Some(e) = ended.recv() => {
                let Lanes::Open(open) = &lane else { continue };
                // The end of a lane opened before this one changes nothing.
                if e.generation != open.generation {
                    continue;
                }
                // A lane that attached and then ended was closed by its transport: it opens
                // again, holding what the daemon last named. One that never attached is a
                // transport refusing lanes.
                if open.attaching != Attaching::Attached {
                    return e.end;
                }
                let (generation, buffer) = (open.generation.next(), open.buffer.get());
                log::info!("the lane ended ({}): it opens again as {generation:?}", e.end);
                // The old lane's reader stops before the new lane opens.
                drop(std::mem::replace(&mut lane, Lanes::Closed));
                lane = match s.open(generation, buffer).await {
                    Ok(open) => Lanes::Open(open),
                    Err(end) => return end,
                };
            }
            () = sleep_until(decide) => {
                if let Err(end) = s.receiver.lock().decide(&mut out) {
                    return end;
                }
            }
            () = sleep_until(resend) => {
                let Lanes::Open(open) = &mut lane else { continue };
                if let Err(end) = s.resend(open).await {
                    return end;
                }
            }
            () = s.arrived.notified() => {}
        }
        if !out.is_empty() {
            if let Err(e) = to_daemon.write_all(&out).await {
                return End::ControlEnded(format!("writing to ssh: {e}"));
            }
            out.clear();
        }
    }
}

/// Reads what ssh's stdout has, or why the control stream ended.
async fn read(from: &mut ChildStdout, buf: &mut [u8], child: &mut Child) -> Result<usize, End> {
    match from.read(buf).await {
        Ok(0) => Err(End::ControlEnded(match child.wait().await {
            Ok(s) => s.to_string(),
            Err(e) => e.to_string(),
        })),
        Ok(n) => Ok(n),
        Err(e) => Err(End::ControlEnded(e.to_string())),
    }
}

/// Sleeps until `at`, or forever.
async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at.into()).await,
        None => std::future::pending().await,
    }
}

impl Session<'_> {
    /// Opens the lane as `generation`, its socket holding `buffer`, attached as it opens.
    async fn open(&self, generation: Generation, buffer: proxy::Buffer) -> Result<Open, End> {
        let lane = match &self.description {
            Description::Datagram { proxy } => {
                let a = Arc::new(
                    proxy::Association::open(proxy, buffer)
                        .await
                        .map_err(End::Proxy)?,
                );
                let sent = self.attach_datagram(&a, generation).await?;
                let task = tokio::spawn(datagram_lane(
                    a.clone(),
                    generation,
                    self.receiver.clone(),
                    self.arrived.clone(),
                    self.ends.clone(),
                ));
                Open {
                    generation,
                    attaching: Attaching::Sent(sent),
                    task,
                    socket: Socket::Datagram(a.clone()),
                    buffer: std::cell::Cell::new(buffer),
                    association: Some(a),
                }
            }
            Description::Stream { proxy } => {
                let reader = proxy::connect(proxy, buffer).await.map_err(End::Proxy)?;
                // What the proxy sent past its answer's head: the lane's first bytes.
                let first = reader.buffer().to_vec();
                let tcp = Arc::new(reader.into_inner());
                let framed =
                    Attach::encode_stream(&self.capability, generation, self.stamp(), self.door);
                write_all(&tcp, &framed)
                    .await
                    .map_err(|e| End::Proxy(ProxyError::Io(e)))?;
                let sent = Instant::now();
                let task = tokio::spawn(stream_lane(
                    tcp.clone(),
                    first,
                    generation,
                    self.receiver.clone(),
                    self.arrived.clone(),
                    self.ends.clone(),
                ));
                Open {
                    generation,
                    attaching: Attaching::Sent(sent),
                    task,
                    socket: Socket::Stream(tcp),
                    buffer: std::cell::Cell::new(buffer),
                    association: None,
                }
            }
        };
        Ok(lane)
    }

    /// Now on the relay's clock.
    fn stamp(&self) -> Stamp {
        self.receiver.lock().now()
    }

    /// Sends the datagram lane's attach: the moment it went.
    async fn attach_datagram(
        &self,
        a: &proxy::Association,
        generation: Generation,
    ) -> Result<Instant, End> {
        let attach = Attach::encode(&self.capability, generation, self.stamp(), self.door);
        a.send(&attach)
            .await
            .map_err(|e| End::Proxy(ProxyError::Io(e)))?;
        Ok(Instant::now())
    }

    /// The datagram lane's attach went a round trip ago unconfirmed: it goes again.
    async fn resend(&self, lane: &mut Open) -> Result<(), End> {
        let Some(a) = lane.association.clone() else {
            return Ok(());
        };
        lane.attaching = Attaching::Sent(self.attach_datagram(&a, lane.generation).await?);
        Ok(())
    }

    /// The daemon confirmed the lane of `generation`.
    fn attached(&mut self, lane: &mut Open, generation: Generation) {
        if generation != lane.generation {
            return;
        }
        if let Attaching::Sent(at) = lane.attaching {
            self.round_trip = self.round_trip.min(at.elapsed());
        }
        lane.attaching = Attaching::Attached;
    }
}

/// Writes all of `b` to a stream shared with its reader.
async fn write_all(tcp: &TcpStream, mut b: &[u8]) -> std::io::Result<()> {
    while !b.is_empty() {
        tcp.writable().await?;
        match tcp.try_write(b) {
            Ok(n) => b = &b[n..],
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// What the relay reads of the daemon's text for the page: a hello names the shared folder
/// the files routes reach. Every text, the hello too, goes to the page as it is.
#[derive(serde::Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
enum Said {
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

/// Reads a stream lane's frames until it ends; `first` are its bytes that came with the
/// proxy's answer.
async fn stream_lane(
    tcp: Arc<TcpStream>,
    first: Vec<u8>,
    generation: Generation,
    receiver: Arc<Mutex<Receiver>>,
    arrived: Arc<Notify>,
    ends: mpsc::UnboundedSender<LaneEnd>,
) {
    let mut framer = Framer::stream();
    framer.push(&first);
    let mut buf = vec![0u8; CONTROL_READ];
    let end = 'read: loop {
        loop {
            match framer.frame() {
                Ok(Some(m)) => {
                    if let Err(end) = receiver.lock().take(m) {
                        break 'read end;
                    }
                }
                Ok(None) => break,
                Err(error) => break 'read End::Lane(error),
            }
        }
        arrived.notify_one();
        if let Err(e) = tcp.readable().await {
            break End::LaneEnded(e.to_string());
        }
        match tcp.try_read(&mut buf) {
            Ok(0) => break End::LaneEnded("the proxy closed it".into()),
            Ok(n) => framer.push(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => break End::LaneEnded(e.to_string()),
        }
    };
    // The session holds the other end while it runs; once it is gone, so is the lane.
    let _ = ends.send(LaneEnd { generation, end });
}

/// Reads the datagram lane's messages into the decoder until its association ends.
async fn datagram_lane(
    a: Arc<proxy::Association>,
    generation: Generation,
    receiver: Arc<Mutex<Receiver>>,
    arrived: Arc<Notify>,
    ends: mpsc::UnboundedSender<LaneEnd>,
) {
    let mut buf = [0u8; proxy::UDP_HEADER_MOST + wire::DATAGRAM];
    let mut control = [0u8; 1];
    let end = loop {
        tokio::select! {
            r = a.socket.recv(&mut buf) => match r {
                // A datagram that is not one whole SOCKS5 datagram is no lane message.
                Ok(n) => {
                    if let Some(m) = proxy::payload(&buf[..n]) {
                        if let Err(end) = receiver.lock().take(m) {
                            break end;
                        }
                        arrived.notify_one();
                    }
                }
                Err(e) => break End::LaneEnded(e.to_string()),
            },
            // The association lasts as long as its TCP connection, on which the proxy says
            // nothing.
            r = a.control_read(&mut control) => break End::LaneEnded(match r {
                Ok(_) => "the proxy ended the UDP association".into(),
                Err(e) => e.to_string(),
            }),
        }
    };
    // The session holds the other end while it runs; once it is gone, so is the lane.
    let _ = ends.send(LaneEnd { generation, end });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::{FilesConfig, Hello, ServerMessage, Settings, Startup};
    use crate::xorshift::Xorshift;

    /// The type bytes the relay gives frames on the page are web/src/wire.json's, the
    /// page's one source of them.
    #[test]
    fn the_page_reads_the_type_bytes_the_relay_writes() {
        let wire: serde_json::Value =
            serde_json::from_str(include_str!("../../web/src/wire.json")).unwrap();
        assert_eq!(wire["bin"]["video"], u64::from(VIDEO));
        assert_eq!(wire["bin"]["audio"], u64::from(AUDIO));
    }

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
                w: rng.below(8000) as u32,
                h: rng.below(8000) as u32,
                codec: Some(format!("vp09.00.{}.08", 10 + rng.below(52))),
                settings: Settings {
                    startup: Some(Startup {
                        audio: rng.below(2) == 0,
                        files: FilesConfig {
                            root: root.clone(),
                            dir: "Downloads".into(),
                        },
                    }),
                    ..Settings::default()
                },
            });
            match serde_json::from_str::<Said>(&hello.json()) {
                Ok(Said::Hello { files }) => {
                    assert_eq!(files.root, root)
                }
                _ => panic!("a hello not read as one: {}", hello.json()),
            }
            let clip = ServerMessage::Clip { d: root.clone() };
            let mic = ServerMessage::Mic {
                on: rng.below(2) == 0,
            };
            for other in [clip, mic] {
                assert!(matches!(
                    serde_json::from_str::<Said>(&other.json()),
                    Ok(Said::Other)
                ));
            }
        }
    }
}
