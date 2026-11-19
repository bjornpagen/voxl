//! One page's session: its WebSocket, its `voxl attach` over ssh (the control stream), and
//! its lane. They start together and end together, for one cause the page is told; nothing
//! reconnects by itself.
//!
//! The control stream carries the page's messages up as they are, the daemon's down as they
//! are, and the lane's confirmation. The lane is one stream through the proxy, which carries
//! every frame whole and in order. Each frame goes to the page as it is (a frame on the lane
//! is the page's message), and the next is read only once the page's socket has taken it: no
//! queue of the relay's holds frames, so a page that falls behind backs up the stream, the
//! transport's window and the daemon's socket, and shuts the daemon's capture gate.

use super::proxy::{self, ProxyError};
use super::{files, ssh};
use crate::wire::{self, Capability, Down, Frame, Framer, Up};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use bytes::Bytes;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use std::fmt;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::task::JoinHandle;

/// What every session is started with.
pub struct Config {
    /// The HTTP proxy the lane goes through.
    pub proxy: proxy::Endpoint,
    pub ssh: ssh::Destination,
    /// The folder of the daemon `voxl attach` reaches; none for the daemon's default.
    pub runtime: Option<ssh::Runtime>,
    /// Where the lane goes, as the server's proxy is asked for it: that daemon's lane.
    pub place: proxy::Place,
    /// Where the files routes learn the shared folder from a session's hello.
    pub files: Arc<files::Files>,
}

/// The longest message the page may send: one that fits a control message with its tag.
/// Derived: `wire::MAX_MESSAGE` less the one byte of `Up`'s tag.
const PAGE_MESSAGE_MOST: usize = wire::MAX_MESSAGE - 1;

/// The bytes read from ssh's stdout, or from the lane, at a time: a pipe's capacity, 65,536
/// bytes on Linux (pipe(7): "the pipe capacity is 16 pages") and on macOS, the most one read
/// of ssh's stdout gives; a frame larger than it is gathered by its framer over several
/// reads. Published.
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
    /// The lane ended before it was attached: the daemon or the transport refuses it.
    LaneEnded(String),
    /// The lane carried something that is not a frame.
    Lane(wire::Malformed),
}

impl fmt::Display for End {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            End::PageClosed => write!(f, "the page closed"),
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
        SinkExt::send(self, m).await.map_err(|_| End::PageClosed)
    }
}

async fn run(socket: WebSocket, cfg: Arc<Config>) {
    let (mut page, from_page) = socket.split();
    let end = session(&cfg, &mut page, from_page).await;
    log::info!("session ended: {end}");
    // The page is told the cause before its socket closes, unless the page is what ended.
    if !matches!(end, End::PageClosed) {
        let close = serde_json::json!({ "t": "close", "why": end.to_string() }).to_string();
        let told = Page::send(&mut page, Message::Text(close.into())).await;
        if told.is_ok() && Page::send(&mut page, Message::Close(None)).await.is_err() {
            log::debug!("the page went before its socket closed");
        }
    }
}

/// The page's messages, carried up the control stream as they come, until the page closes
/// or the control stream does. A task of its own, so that input never waits behind a frame
/// the page has yet to take.
struct Upward(JoinHandle<End>);

impl Drop for Upward {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn upward(mut from_page: SplitStream<WebSocket>, mut to_daemon: ChildStdin) -> End {
    let mut out = Vec::new();
    loop {
        let m = from_page.next().await;
        let up = match &m {
            Some(Ok(Message::Text(t))) => Up::Text(t.as_str()),
            Some(Ok(Message::Binary(b))) => Up::Binary(b),
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
            Some(Ok(Message::Close(_)) | Err(_)) | None => return End::PageClosed,
        };
        out.clear();
        up.encode(&mut out);
        if let Err(e) = to_daemon.write_all(&out).await {
            return End::ControlEnded(format!("writing to ssh: {e}"));
        }
    }
}

/// The session's lane: its stream, what has come of its frames, and whether the daemon has
/// confirmed it.
struct Lane {
    tcp: TcpStream,
    framer: Framer,
    attached: bool,
}

async fn session(cfg: &Config, page: &mut impl Page, from_page: SplitStream<WebSocket>) -> End {
    let mut child: Child = match ssh::attach(&cfg.ssh, cfg.runtime.as_ref()) {
        Ok(c) => c,
        Err(e) => return End::Ssh(e),
    };
    let (Some(to_daemon), Some(mut from_daemon)) = (child.stdin.take(), child.stdout.take()) else {
        return End::Ssh(std::io::Error::other("ssh gave no stdin or stdout"));
    };
    let mut framer = Framer::new();
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
    let mut up = Upward(tokio::spawn(upward(from_page, to_daemon)));
    let mut lane = match open(cfg, &capability).await {
        Ok(lane) => lane,
        Err(end) => return end,
    };
    let mut lane_buf = vec![0u8; CONTROL_READ];
    loop {
        // Every message already read is handled before the session waits for more: one read
        // often holds several (the capability, the hello, the cursor), and a message left in
        // the framer would wait on a read that may never come.
        loop {
            let said = match framer.frame() {
                Ok(Some(m)) => Down::decode(m).map(Said::of),
                Ok(None) => break,
                Err(e) => Err(e),
            };
            let said = match said {
                Ok(said) => said,
                Err(e) => return End::Control(e),
            };
            if let Err(end) = down(cfg, &mut lane, page, said).await {
                return end;
            }
        }
        tokio::select! {
            end = &mut up.0 => return end.unwrap_or(End::PageClosed),
            r = read(&mut from_daemon, &mut buf, &mut child) => match r {
                Ok(n) => framer.push(&buf[..n]),
                Err(end) => return end,
            },
            r = lane.tcp.read(&mut lane_buf) => {
                let why = match r {
                    Ok(0) => "the proxy closed it".to_string(),
                    Ok(n) => {
                        lane.framer.push(&lane_buf[..n]);
                        if let Err(end) = give(&mut lane.framer, page).await {
                            return end;
                        }
                        continue;
                    }
                    Err(e) => e.to_string(),
                };
                // A lane that attached and then ended was closed by its transport: it opens
                // again, and the daemon starts its viewer over on a keyframe. One that never
                // attached is refused, and opening it again would loop.
                if !lane.attached {
                    return End::LaneEnded(why);
                }
                log::info!("the lane ended ({why}): it opens again");
                lane = match open(cfg, &capability).await {
                    Ok(lane) => lane,
                    Err(end) => return end,
                };
            }
        }
    }
}

/// A control message from the daemon, owned, as the session acts on it.
enum Said {
    Capability,
    Attached,
    Text(String),
    Binary(Bytes),
}

impl Said {
    fn of(d: Down<'_>) -> Said {
        match d {
            Down::Capability(_) => Said::Capability,
            Down::Attached => Said::Attached,
            Down::Text(t) => Said::Text(t.to_string()),
            Down::Binary(b) => Said::Binary(Bytes::copy_from_slice(b)),
        }
    }
}

/// Acts on one message from the daemon's control stream.
async fn down(cfg: &Config, lane: &mut Lane, page: &mut impl Page, said: Said) -> Result<(), End> {
    match said {
        Said::Capability => Err(End::Capability),
        Said::Attached => {
            lane.attached = true;
            Ok(())
        }
        Said::Text(t) => {
            if let Ok(Greeting::Hello { files }) = serde_json::from_str::<Greeting>(&t) {
                *cfg.files.grant.lock() = Some(files::Grant { root: files.root });
            }
            page.send(Message::Text(t.into())).await
        }
        Said::Binary(b) => page.send(Message::Binary(b)).await,
    }
}

/// Gives the page every whole frame `framer` holds, in order, each once the page's socket has
/// taken the one before: a frame is the page's message as it is, once its track is one the
/// page reads.
async fn give(framer: &mut Framer, page: &mut impl Page) -> Result<(), End> {
    loop {
        let frame = match framer.frame() {
            Ok(Some(m)) => Frame::decode(m).map(|_| Bytes::copy_from_slice(m)),
            Ok(None) => return Ok(()),
            Err(e) => Err(e),
        };
        page.send(Message::Binary(frame.map_err(End::Lane)?))
            .await?;
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

/// Opens a lane through the proxy, attached as it opens: the capability its first bytes.
async fn open(cfg: &Config, capability: &Capability) -> Result<Lane, End> {
    let reader = proxy::connect(&cfg.proxy, &cfg.place)
        .await
        .map_err(End::Proxy)?;
    // What the proxy sent past its answer's head: the lane's first bytes.
    let mut framer = Framer::new();
    framer.push(reader.buffer());
    let mut tcp = reader.into_inner();
    tcp.write_all(capability.bytes())
        .await
        .map_err(|e| End::Proxy(ProxyError::Io(e)))?;
    Ok(Lane {
        tcp,
        framer,
        attached: false,
    })
}

/// What the relay reads of the daemon's text for the page: a hello names the shared folder
/// the files routes reach. Every text, the hello too, goes to the page as it is.
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
    use crate::hub::{FilesConfig, Hello, ServerMessage, Settings, Startup};
    use crate::xorshift::Xorshift;

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
            match serde_json::from_str::<Greeting>(&hello.json()) {
                Ok(Greeting::Hello { files }) => {
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
                    serde_json::from_str::<Greeting>(&other.json()),
                    Ok(Greeting::Other)
                ));
            }
        }
    }

    /// A page that takes a message only when the test lets it: its socket, one message
    /// deep.
    struct Slow(tokio::sync::mpsc::Sender<Message>);

    impl Page for Slow {
        async fn send(&mut self, m: Message) -> Result<(), End> {
            self.0.send(m).await.map_err(|_| End::PageClosed)
        }
    }

    /// A slow page shuts the daemon's capture gate, and only the page's pace opens it: the
    /// daemon's sender and lane writer and the relay's reading of the lane together, over a
    /// socket between them. Over generated runs of updates of generated sizes: while the page
    /// takes nothing, the relay takes at most one frame past what the page holds, and once
    /// the updates sent outgrow the socket's buffers the gate stays shut; once the page takes
    /// what comes, the gate opens, and the page has every update, whole, in order.
    #[tokio::test]
    async fn a_slow_page_shuts_the_gate() {
        use crate::session::sender::{Queue, Sender};
        let mut rng = Xorshift::new(0x3c6e_f372_fe94_f82b);
        for case in 0..20 {
            let (daemon_end, relay_end) = tokio::net::UnixStream::pair().unwrap();
            let (_, write) = daemon_end.into_split();
            let mut s = Sender::new(Capability::mint().unwrap());
            let queue = Queue::new(std::sync::Arc::new(|| {}));
            assert!(s.attach(queue.clone()));
            tokio::spawn(crate::session::lane::writer(queue, write));
            let (tx, mut page) = tokio::sync::mpsc::channel(1);
            // The relay: the lane read only once the page took what came before.
            let relay = tokio::spawn(async move {
                let (mut lane, mut framer) = (relay_end, Framer::new());
                let mut buf = vec![0u8; CONTROL_READ];
                let mut slow = Slow(tx);
                loop {
                    match lane.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => framer.push(&buf[..n]),
                    }
                    if give(&mut framer, &mut slow).await.is_err() {
                        return;
                    }
                }
            });
            let mut sent = Vec::new();
            // Far past what the socket buffers hold, in updates of up to 200 KB.
            while sent.iter().sum::<usize>() < 4 << 20 {
                let len = rng.range(1, 200_000) as usize;
                let update = bytes::Bytes::from(vec![sent.len() as u8; len]);
                s.video(&update);
                sent.push(len);
            }
            for _ in 0..1000 {
                tokio::task::yield_now().await;
            }
            assert!(
                s.waits(),
                "case {case}: the gate is open with the page stopped"
            );
            // One message in the page's socket, and one the relay holds: no more.
            let mut got = Vec::new();
            while let Ok(m) = page.try_recv() {
                got.push(m);
            }
            assert!(
                got.len() <= 1,
                "case {case}: the page was given {}",
                got.len()
            );
            // The page takes what comes: the gate opens, and every update arrived.
            let all = tokio::time::timeout(std::time::Duration::from_secs(30), async {
                while got.len() < sent.len() {
                    match page.recv().await {
                        Some(m) => got.push(m),
                        None => return,
                    }
                }
            });
            assert!(
                all.await.is_ok(),
                "case {case}: the page was not given every update"
            );
            assert_eq!(got.len(), sent.len(), "case {case}: the relay ended");
            for _ in 0..1000 {
                tokio::task::yield_now().await;
            }
            assert!(
                !s.waits(),
                "case {case}: the gate is shut with the page caught up"
            );
            for (i, (m, len)) in got.iter().zip(&sent).enumerate() {
                let Message::Binary(b) = m else {
                    panic!("case {case}: a frame not binary");
                };
                let f = Frame::decode(b).unwrap();
                assert_eq!(f.track, crate::wire::Track::Video);
                assert_eq!(f.bytes.len(), *len, "case {case}: update {i}");
                assert!(
                    f.bytes.iter().all(|&b| b == i as u8),
                    "case {case}: update {i}"
                );
            }
            relay.abort();
        }
    }
}
