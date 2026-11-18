//! One page's session: its WebSocket, its `voxl attach` over ssh (the control stream), and
//! its lanes. They start together and end together, for one cause the page is told; nothing
//! reconnects by itself.
//!
//! The control stream carries the page's messages up as they are, the daemon's down as they
//! are, each lane's confirmation, and a report of every frame. The lanes carry the frames'
//! pieces and repairs to the decoder, which makes frames whole; [`Release`] decides what of
//! them the page is given.

use super::description::{Description, Malformed as Undescribed};
use super::proxy::{self, ProxyError};
use super::release::{Give, Release};
use super::set::{self, Attaching, Ended, Wanted};
use super::{files, ssh};
use crate::lanes::code::{self, Decided, Decoder, Took};
use crate::lanes::wire::{
    self, Attach, Capability, Down, Framer, Generation, Lane, LaneIndex, Stamp, Up,
};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
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

/// The bytes read from ssh's stdout at a time: a pipe's capacity, 65,536 bytes on Linux
/// (pipe(7): "the pipe capacity is 16 pages") and on macOS, the most one read gives.
/// Published.
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
    /// A lane could not be opened through the proxy.
    Proxy { lane: usize, error: ProxyError },
    /// A lane ended before its set was attached: its transport refuses lanes.
    LaneEnded { lane: usize, why: String },
    /// A lane carried something that is not a lane message.
    Lane { lane: usize, error: wire::Malformed },
    /// The decoder refused a piece or repair.
    Refused { lane: usize, why: code::Refused },
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
            End::Proxy { lane, error } => write!(f, "lane {lane}: {error}"),
            End::LaneEnded { lane, why } => {
                write!(f, "lane {lane} ended before the lanes were attached: {why}")
            }
            End::Lane { lane, error } => write!(f, "lane {lane} is broken: {error}"),
            End::Refused { lane, why } => write!(f, "lane {lane}: the decoder refused {why:?}"),
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

/// The kind the page is told, as `LanesKind` in web/src/protocol.ts names it.
fn kind(d: &Description) -> &'static str {
    match d {
        Description::Datagram { .. } => "datagram",
        Description::Streams { .. } => "streams",
        Description::Connections { .. } => "connections",
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

    /// One message from lane `lane`, arrived now.
    fn take(&mut self, lane: usize, b: &[u8]) -> Result<(), End> {
        let msg = Lane::decode(b).map_err(|error| End::Lane { lane, error })?;
        let at = self.now();
        let Receiver {
            decoder,
            release,
            page,
            ..
        } = self;
        match decoder.take(msg, at) {
            Ok(Took::Whole(head, bytes)) => {
                let members = head
                    .members(bytes)
                    .map_err(|error| End::Lane { lane, error })?;
                let mut given = Ok(());
                for member in members {
                    release.whole(&member, head.due, &mut |g| {
                        if given.is_ok() {
                            given = page.give(g);
                        }
                    });
                }
                given
            }
            Ok(Took::Held | Took::Spare | Took::Late) => Ok(()),
            Err(why) => Err(End::Refused { lane, why }),
        }
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
    lane: LaneIndex,
    generation: Generation,
    end: End,
}

/// What carries a lane: its reading task, stopped when the lane is closed, and for the
/// datagram lane its association, to send the attach again.
struct Carrier {
    task: JoinHandle<()>,
    association: Option<Arc<proxy::Association>>,
}

impl Drop for Carrier {
    fn drop(&mut self) {
        self.task.abort();
    }
}

type Set = set::Set<Instant, Carrier>;

/// When the datagram lane's attach, if unconfirmed, goes again: a round trip after it went,
/// when no confirmation could still be on its way.
fn resend(set: &Set, round_trip: Duration) -> Option<Instant> {
    match set.slots().first() {
        Some(slot) if slot.carrier.association.is_some() => match slot.attaching {
            Attaching::Sent(at) => Some(at + round_trip),
            Attaching::Attached => None,
        },
        _ => None,
    }
}

/// Everything a session holds while it runs.
struct Session<'a> {
    cfg: &'a Config,
    description: Description,
    capability: Capability,
    receiver: Arc<Mutex<Receiver>>,
    /// Rung by a lane task after a frame arrived, so the next decision is looked at again.
    arrived: Arc<Notify>,
    ends: mpsc::UnboundedSender<LaneEnd>,
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
    let text = serde_json::json!({ "t": "lanes", "kind": kind(&description) }).to_string();
    if let Err(end) = page.text(text) {
        return end;
    }
    let (ends, mut ended) = mpsc::unbounded_channel();
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
        round_trip: opened.elapsed(),
    };
    let mut set = Set::new(match &s.description {
        Description::Datagram { .. } => Wanted::One,
        Description::Connections { lanes, .. } => Wanted::All(*lanes),
        Description::Streams { cap, .. } => Wanted::UpTo(*cap),
    });
    if let Err(end) = s.fill(&mut set).await {
        return end;
    }
    let mut out = Vec::new();
    loop {
        let decide = s.receiver.lock().wake();
        let resend = resend(&set, s.round_trip);
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
                        Ok(Down::Attached { lane, generation }) => s.attached(&mut set, lane, generation),
                        Ok(Down::Lanes(lanes)) => {
                            set.asked(lanes);
                            if let Err(end) = s.fill(&mut set).await {
                                return end;
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
            Some(e) = ended.recv() => match set.ended(e.lane, e.generation) {
                Ended::Stale => continue,
                Ended::End => return e.end,
                Ended::Reopen(generation) => {
                    log::info!(
                        "lane {} ended ({}): the lanes open again as generation {generation:?}",
                        e.lane.get(),
                        e.end
                    );
                    if let Err(end) = s.fill(&mut set).await {
                        return end;
                    }
                }
            },
            () = sleep_until(decide) => {
                if let Err(end) = s.receiver.lock().decide(&mut out) {
                    return end;
                }
            }
            () = sleep_until(resend) => {
                if let Err(end) = s.resend(&mut set).await {
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
    /// Opens the lanes the set lacks, in order, each attached as it opens.
    async fn fill(&self, set: &mut Set) -> Result<(), End> {
        let missing: Vec<LaneIndex> = set.missing().collect();
        let generation = set.generation();
        for lane in missing {
            let (sent, carrier) = match &self.description {
                Description::Datagram { proxy } => {
                    let a = proxy::Association::open(proxy)
                        .await
                        .map_err(|error| End::Proxy {
                            lane: lane.get(),
                            error,
                        })?;
                    let a = Arc::new(a);
                    let sent = self.attach_datagram(&a, lane, generation).await?;
                    let task = tokio::spawn(datagram_lane(
                        a.clone(),
                        lane,
                        generation,
                        self.receiver.clone(),
                        self.arrived.clone(),
                        self.ends.clone(),
                    ));
                    (
                        sent,
                        Carrier {
                            task,
                            association: Some(a),
                        },
                    )
                }
                Description::Streams { proxy, .. } | Description::Connections { proxy, .. } => {
                    let failed = |e: std::io::Error| End::Proxy {
                        lane: lane.get(),
                        error: ProxyError::Io(e),
                    };
                    let mut stream = proxy::connect(proxy).await.map_err(|error| End::Proxy {
                        lane: lane.get(),
                        error,
                    })?;
                    let attach = Attach::encode(&self.capability, lane, generation, self.stamp());
                    let tcp = stream.get_mut();
                    tcp.write_all(&wire::lane_prefix(Attach::LEN))
                        .await
                        .map_err(failed)?;
                    tcp.write_all(&attach).await.map_err(failed)?;
                    let sent = Instant::now();
                    let task = tokio::spawn(stream_lane(
                        stream,
                        lane,
                        generation,
                        self.receiver.clone(),
                        self.arrived.clone(),
                        self.ends.clone(),
                    ));
                    (
                        sent,
                        Carrier {
                            task,
                            association: None,
                        },
                    )
                }
            };
            set.opened(lane, sent, carrier);
        }
        Ok(())
    }

    /// Now on the relay's clock.
    fn stamp(&self) -> Stamp {
        self.receiver.lock().now()
    }

    /// Sends the datagram lane's attach: the moment it went.
    async fn attach_datagram(
        &self,
        a: &proxy::Association,
        lane: LaneIndex,
        generation: Generation,
    ) -> Result<Instant, End> {
        let attach = Attach::encode(&self.capability, lane, generation, self.stamp());
        a.send(&attach).await.map_err(|e| End::Proxy {
            lane: lane.get(),
            error: ProxyError::Io(e),
        })?;
        Ok(Instant::now())
    }

    /// The datagram lane's attach went a round trip ago unconfirmed: it goes again.
    async fn resend(&self, set: &mut Set) -> Result<(), End> {
        let generation = set.generation();
        let Some(slot) = set.slots_mut().first_mut() else {
            return Ok(());
        };
        let Some(a) = slot.carrier.association.clone() else {
            return Ok(());
        };
        slot.attaching = Attaching::Sent(self.attach_datagram(&a, slot.lane, generation).await?);
        Ok(())
    }

    /// The daemon confirmed lane `lane` of `generation`.
    fn attached(&mut self, set: &mut Set, lane: LaneIndex, generation: Generation) {
        if let Some(sent) = set.attached(lane, generation) {
            self.round_trip = self.round_trip.min(sent.elapsed());
        }
    }
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

/// Reads a stream lane's messages into the decoder until it ends.
async fn stream_lane(
    mut stream: BufReader<TcpStream>,
    lane: LaneIndex,
    generation: Generation,
    receiver: Arc<Mutex<Receiver>>,
    arrived: Arc<Notify>,
    ends: mpsc::UnboundedSender<LaneEnd>,
) {
    let mut framer = Framer::lane();
    let mut buf = [0u8; wire::LANE_PREFIX + wire::DATAGRAM];
    let end = 'read: loop {
        match stream.read(&mut buf).await {
            Ok(0) => {
                break End::LaneEnded {
                    lane: lane.get(),
                    why: "the proxy closed it".into(),
                }
            }
            Ok(n) => framer.push(&buf[..n]),
            Err(e) => {
                break End::LaneEnded {
                    lane: lane.get(),
                    why: e.to_string(),
                }
            }
        }
        loop {
            match framer.frame() {
                Ok(Some(m)) => {
                    if let Err(end) = receiver.lock().take(lane.get(), m) {
                        break 'read end;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    break 'read End::Lane {
                        lane: lane.get(),
                        error,
                    }
                }
            }
        }
        arrived.notify_one();
    };
    // The session holds the other end while it runs; once it is gone, so is the lane.
    let _ = ends.send(LaneEnd {
        lane,
        generation,
        end,
    });
}

/// Reads the datagram lane's messages into the decoder until its association ends.
async fn datagram_lane(
    a: Arc<proxy::Association>,
    lane: LaneIndex,
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
                        if let Err(end) = receiver.lock().take(lane.get(), m) {
                            break end;
                        }
                        arrived.notify_one();
                    }
                }
                Err(e) => break End::LaneEnded { lane: lane.get(), why: e.to_string() },
            },
            // The association lasts as long as its TCP connection, on which the proxy says
            // nothing.
            r = a.control_read(&mut control) => break End::LaneEnded {
                lane: lane.get(),
                why: match r {
                    Ok(_) => "the proxy ended the UDP association".into(),
                    Err(e) => e.to_string(),
                },
            },
        }
    };
    // The session holds the other end while it runs; once it is gone, so is the lane.
    let _ = ends.send(LaneEnd {
        lane,
        generation,
        end,
    });
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
