//! HTTP + WebSocket front end. Serves the embedded web client and the media/input socket.

use crate::event::{InputEvent, KeyAct};
use crate::hub::{Client, Hub, Out, ServerMessage};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use bytes::Bytes;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;

#[derive(Clone)]
pub struct AppState {
    pub hub: Arc<Hub>,
    pub audio: bool,
    pub files: Arc<crate::files::Files>,
    pub mic: Option<std::sync::mpsc::Sender<crate::mic::MicMsg>>,
    /// Resizing was enabled; hub.resize_max tracks the current display's capability.
    pub resize: bool,
    /// Viewers only watch: input, clipboard writes and the microphone are refused (--read-only).
    pub read_only: bool,
    pub recorder: Arc<crate::record::Recorder>,
    /// The web client's page, with its title filled in (`index_html`).
    pub index: Bytes,
    /// The page's icon and its content type (--favicon).
    pub favicon: Bytes,
    pub favicon_type: &'static str,
    /// The debugging variables (`VOXL_*`), read when voxl started.
    pub debug: crate::env::Debugging,
}

/// The web client's page, titled `title` (--title): the browser tab, and the name of
/// the app if a viewer installs it.
pub fn index_html(title: &str) -> Bytes {
    let mut escaped = String::with_capacity(title.len());
    for c in title.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            c => escaped.push(c),
        }
    }
    Bytes::from(include_str!("../web/index.html").replace("{{title}}", &escaped))
}

/// The page's icon (--favicon): that file's bytes and content type, or voxl's own icon.
pub fn favicon(path: Option<&str>) -> Result<(Bytes, &'static str), String> {
    let Some(path) = path else {
        return Ok((
            Bytes::from_static(include_bytes!("../web/favicon.ico")),
            "image/x-icon",
        ));
    };
    let ctype = match Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("ico") => "image/x-icon",
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        _ => {
            return Err(format!(
                "--favicon {path}: expected an .ico, .png, .gif, .webp or .svg file"
            ))
        }
    };
    let bytes = std::fs::read(path).map_err(|e| format!("--favicon {path}: {e}"))?;
    Ok((Bytes::from(bytes), ctype))
}

async fn favicon_route(State(s): State<AppState>) -> Response {
    (
        [
            (header::CONTENT_TYPE, s.favicon_type),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        s.favicon,
    )
        .into_response()
}

async fn index(State(s): State<AppState>) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        s.index,
    )
        .into_response()
}

macro_rules! asset {
    ($name:literal, $ctype:literal) => {
        get(|| async {
            (
                [
                    (header::CONTENT_TYPE, $ctype),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                include_bytes!(concat!("../web/", $name)).as_slice(),
            )
        })
    };
}

/// Scripts bundled from web/src by build.rs.
macro_rules! bundled {
    ($name:literal) => {
        get(|| async {
            (
                [
                    (header::CONTENT_TYPE, "text/javascript"),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                include_bytes!(concat!(env!("OUT_DIR"), "/web/", $name)).as_slice(),
            )
        })
    };
}

pub fn router(state: AppState) -> Router {
    use crate::files;
    let file_routes = Router::new()
        .route("/files/list", get(files::list))
        .route("/files/download", get(files::download))
        .route("/files/upload", axum::routing::put(files::upload))
        .with_state(state.files.clone());
    use crate::record;
    let record_routes = Router::new()
        .route("/screenshot", get(record::screenshot))
        .route("/record", get(record::record))
        .route("/record/start", axum::routing::post(record::start))
        .route("/record/stop", axum::routing::post(record::stop))
        .with_state(state.recorder.clone());
    let wait_routes = Router::new()
        .route("/wait", get(crate::wait::wait))
        .with_state(state.hub.clone());
    Router::new()
        .route("/", get(index))
        .route("/index.html", get(index))
        .route("/client.js", bundled!("client.js"))
        .route("/worker.js", bundled!("worker.js"))
        .route("/audio.js", bundled!("audio.js"))
        .route("/mic.js", bundled!("mic.js"))
        .route("/favicon.ico", get(favicon_route))
        .route("/inter.woff2", asset!("inter.woff2", "font/woff2"))
        .route("/healthz", get(|| async { "ok" }))
        .route("/stats", get(stats))
        .route("/stats/trace", get(trace))
        .route("/ws", get(ws_handler))
        .with_state(state)
        .merge(file_routes)
        .merge(record_routes)
        .merge(wait_routes)
}

async fn stats(State(s): State<AppState>) -> Response {
    let st = &s.hub.stats;
    let now = Instant::now();
    let viewers = s.hub.clients.lock().clone();
    let clients: Vec<_> = viewers
        .iter()
        .map(|c| {
            let backlog = c.backlog(now);
            let (n, age) = backlog.map_or((0, 0), |b| (b.frames.get(), b.oldest.as_millis()));
            let (carries, lowest, flying, fastest) =
                c.window(|w| (w.carries(), w.lowest(), w.flying(), w.fastest()));
            serde_json::json!({"id": c.id, "video": c.viewer() != crate::hub::Viewer::NotWatching, "inflight": n, "oldest_ms": age,
                "min_rtt_ms": c.min_rtt_ms.load(Ordering::Relaxed), "carries_bytes": carries,
                "lowest_carries_bytes": lowest, "flying_bytes": flying,
                "fastest_ms": fastest.map(|t| t.as_secs_f64() * 1000.0),
                "client_ms": (*c.client_ms.lock() * 10.0).round() / 10.0})
        })
        .collect();
    // What the slowest watching viewer's path carries, as a rate.
    let target = viewers
        .iter()
        .filter(|c| c.viewer() != crate::hub::Viewer::NotWatching)
        .map(|c| c.window(|w| w.carries()))
        .min()
        .map(|bytes| bytes as f64 * 8.0 / crate::window::LATENESS.as_secs_f64() / 1000.0);
    let body = serde_json::json!({
        "now_us": s.hub.timeline.us(Instant::now()),
        "captured": st.captured.load(Ordering::Relaxed),
        "encoded": st.encoded.load(Ordering::Relaxed),
        "bytes": st.bytes.load(Ordering::Relaxed),
        "encode_us_total": st.encode_us.load(Ordering::Relaxed),
        "grab_us_total": st.grab_us.load(Ordering::Relaxed),
        "convert_us_total": st.convert_us.load(Ordering::Relaxed),
        "flow_blocked": st.flow_blocked.load(Ordering::Relaxed),
        "paintovers": st.paintovers.load(Ordering::Relaxed),
        "keyframes": st.keyframes.load(Ordering::Relaxed),
        "refines": st.refines.load(Ordering::Relaxed),
        "target_kbps": target.map(|kbps| kbps as u64),
        "damage_events": crate::capture::DAMAGE_EVENTS.load(Ordering::Relaxed),
        "damage_wait_us_total": st.damage_wait_us.load(Ordering::Relaxed),
        "send_lat_us_total": st.send_lat_us.load(Ordering::Relaxed),
        "clients": clients,
    });
    (
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

/// What the server remembers of the last frames, every time in microseconds on the
/// timeline's clock (`Timeline::us`), as `/stats`'s `now_us` is: each frame's way from
/// the screen to the sockets, and each viewer's frames as they were sent and acked, those
/// of the last viewers to leave with when they left (crate::timeline).
async fn trace(State(s): State<AppState>) -> Response {
    use crate::timeline::{Recorded, SentRow, MARKS};
    /// Written by serde straight, not through a `serde_json::Value`, whose numbers stop
    /// at 64 bits: a viewer may report a hold of more microseconds than that.
    #[derive(serde::Serialize)]
    struct Body {
        now_us: u64,
        marks: [&'static str; MARKS.len()],
        frames: Vec<Recorded>,
        viewers: Vec<Viewer>,
        left: Vec<Gone>,
    }
    #[derive(serde::Serialize)]
    struct Viewer {
        id: u64,
        frames: Vec<SentRow>,
    }
    #[derive(serde::Serialize)]
    struct Gone {
        id: u64,
        left_us: u64,
        frames: Vec<SentRow>,
    }
    let timeline = &s.hub.timeline;
    let frames = timeline.newest(crate::timeline::FRAMES_KEPT);
    let viewers = s.hub.traces();
    let left = s.hub.left_traces();
    axum::Json(Body {
        now_us: timeline.us(Instant::now()),
        marks: MARKS,
        frames,
        viewers: viewers
            .iter()
            .map(|(id, trace)| Viewer {
                id: *id,
                frames: timeline.rows(trace),
            })
            .collect(),
        left: left
            .iter()
            .map(|(id, at, trace)| Gone {
                id: *id,
                left_us: timeline.us(*at),
                frames: timeline.rows(trace),
            })
            .collect(),
    })
    .into_response()
}

async fn ws_handler(ws: WebSocketUpgrade, State(s): State<AppState>) -> Response {
    ws.max_message_size(64 << 20)
        .on_upgrade(move |socket| handle_socket(socket, s))
}

/// What a viewer is sent as it connects, in this order: a ping, its hello, and the cursor's
/// image. The ping goes first so that nothing is on the path ahead of it: the window takes
/// the path's fastest trip from pings alone, and under motion from the first frame no later
/// ping may have the path to itself. Behind the hello and the cursor's image (4 bytes a
/// pixel: 16.4 KB at 64 px) its trip would hold their crossing for as long as the motion
/// lasts.
pub(crate) fn greet(hub: &Hub, c: &Client) {
    ping(c);
    c.send(Out::Text(hub.hello()));
    if let Some(cursor) = hub.cursor.lock().clone() {
        c.send(Out::Bin(cursor));
    }
}

/// Pings `c`: the ping goes into its record, then to it.
fn ping(c: &Client) {
    let token = crate::hub::Token::random();
    c.ping(Instant::now(), token);
    c.send(Out::Text(ServerMessage::Ping { s: token }));
}

async fn handle_socket(socket: WebSocket, s: AppState) {
    use futures_util::{SinkExt, StreamExt};
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Out>();
    let client = {
        // New connections must not receive the old geometry during a display change.
        #[cfg(target_os = "macos")]
        let input = s.hub.input.lock();
        #[cfg(target_os = "macos")]
        if input.is_none() && s.hub.input_floor.load(Ordering::Relaxed) != 0 {
            return;
        }
        s.hub.add_client(tx, Instant::now())
    };
    log::info!("client {} connected", client.id);
    greet(&s.hub, &client);
    let dbg = s.debug.marker.is_some();
    let hub = s.hub.clone();
    let writer = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            let t = Instant::now();
            let big = matches!(&m, Out::Bin(b) if b.len() > 2000);
            let frame = match &m {
                Out::Bin(b) => crate::video::frame_of(b),
                Out::Text(_) => None,
            };
            let msg = match m {
                Out::Bin(b) => Message::Binary(b),
                Out::Text(m) => Message::Text(m.json().into()),
            };
            if sink.send(msg).await.is_err() {
                break;
            }
            if let Some(frame) = frame {
                hub.timeline.written(frame, Instant::now());
            }
            if dbg && big {
                log::info!(
                    "ws send took {} us (queued {})",
                    t.elapsed().as_micros(),
                    rx.len()
                );
            }
        }
    });
    // The first ping went with the greeting; the next goes a `PING_EVERY` after it.
    let pinger = {
        let c = client.clone();
        tokio::spawn(async move {
            let every = crate::hub::PING_EVERY;
            let mut iv = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
            loop {
                iv.tick().await;
                ping(&c);
            }
        })
    };
    let mut rtts: std::collections::VecDeque<u32> = Default::default();
    while let Some(Ok(msg)) = stream.next().await {
        match msg {
            Message::Text(t) => handle_text(&s, &client, t.as_str(), &mut rtts),
            // Every binary message from a viewer acts on the desktop (clipboard images,
            // microphone audio), so a read-only viewer's are all dropped.
            Message::Binary(_) if s.read_only => {}
            // 0x11: clipboard image (PNG) pasted on the client.
            Message::Binary(b) if b.first() == Some(&0x11) && b.len() > 1 => {
                send_input(
                    &s.hub,
                    &client,
                    InputEvent::Clipboard(crate::desktop::ClipContent::Image(b[1..].to_vec())),
                );
            }
            // 0x12: microphone Opus packet, 0x13: microphone PCM (s16le mono 48 kHz).
            Message::Binary(b) if matches!(b.first(), Some(0x12 | 0x13)) && b.len() > 1 => {
                if let Some(tx) = &s.mic {
                    let data = b.slice(1..);
                    let _ = tx.send(if b[0] == 0x12 {
                        crate::mic::MicMsg::Opus(client.id, data)
                    } else {
                        crate::mic::MicMsg::Pcm(client.id, data)
                    });
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    pinger.abort();
    writer.abort();
    send_input(&s.hub, &client, InputEvent::ReleaseAll);
    if let Some(tx) = &s.mic {
        let _ = tx.send(crate::mic::MicMsg::Stop(client.id));
    }
    s.hub.remove_client(client.id, Instant::now());
    log::info!("client {} disconnected", client.id);
}

fn num<T: std::str::FromStr>(field: &str) -> Option<T> {
    field.trim().parse().ok()
}

/// A button the viewer's pointer has, which a command names by X's number for it.
/// pointer.ts sends the browser's five (main, middle, secondary, back, forward) as 1, 2,
/// 3, 8 and 9, and those five are all macOS and Windows press. X has more (4 to 7 are its
/// wheel, which `sp` turns) and no button 0.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Button {
    Left,
    Middle,
    Right,
    Back,
    Forward,
}

impl Button {
    pub const ALL: [Button; 5] = [
        Button::Left,
        Button::Middle,
        Button::Right,
        Button::Back,
        Button::Forward,
    ];

    /// Its number on X, and in a command.
    pub fn number(self) -> u8 {
        match self {
            Button::Left => 1,
            Button::Middle => 2,
            Button::Right => 3,
            Button::Back => 8,
            Button::Forward => 9,
        }
    }

    fn parse(field: &str) -> Option<Button> {
        let number: u8 = field.trim().parse().ok()?;
        Button::ALL.into_iter().find(|b| b.number() == number)
    }
}

/// The longest pixel scroll one message carries, either way on either axis: 2^20 desktop
/// pixels, about a million.
///
/// Who needs it: an input thread adds each pixel scroll to the remainder it keeps between
/// messages, scaled to wheel units (on X and Windows 120 a notch over
/// `--scroll-px-per-notch`, which may be as small as 1), and sends the whole units on as
/// an i32. A finite distance near f64::MAX scales to infinity there, the remainder is NaN
/// from then on, and no pixel scroll moves anything until voxl restarts. At this bound
/// one message makes at most 120 x 2^20 units, and with the remainder under a notch the
/// sum is under 2^27: exact, and in an i32 sixteen times over.
///
/// What sets it: a message is what pointer.ts gathered in one animation frame, in desktop
/// pixels: the wheel's deltas (pixels, lines times 40 or pages times 800) and two fingers'
/// moves, times desktop pixels per CSS pixel. A page is the most one notch scrolls; on a
/// desktop drawn at an eighth of its size that is 6,400 px, and five notches in one frame
/// 32,000. This is 32 times that, so half or double it changes nothing either side.
const MAX_SCROLL_PX: f64 = 1_048_576.0;

/// A distance in desktop pixels, finite and at most `MAX_SCROLL_PX` either way. Rust's
/// `f64` parser reads `NaN` and `inf` too (JavaScript's `toFixed` writes them as `NaN`
/// and `Infinity`); added to the input thread's scroll remainder, either one stays in it,
/// as a finite distance past the bound does once scaled, and every pixel scroll after it
/// moves nothing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pixels(f64);

impl Pixels {
    fn parse(field: &str) -> Option<Pixels> {
        let px: f64 = field.trim().parse().ok()?;
        (px.abs() <= MAX_SCROLL_PX).then_some(Pixels(px))
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

fn send_input(hub: &Hub, client: &Client, event: InputEvent) {
    let input = hub.input.lock();
    #[cfg(target_os = "macos")]
    if client.id < hub.input_floor.load(Ordering::Relaxed) {
        return;
    }
    #[cfg(not(target_os = "macos"))]
    let _ = client;
    if let Some(tx) = input.as_ref() {
        let _ = tx.send(event);
    }
}

/// A text command from a viewer (web/src/protocol.ts), parsed once where it arrives.
enum Command {
    /// `a,<frame>[,<hold ms>]`
    Ack { frame: u32, hold: crate::hub::Hold },
    /// `m,<x>,<y>`: a desktop pixel, from the top left. X's coordinates are 16-bit (XTest
    /// takes them as they are); macOS's CGFloat and Windows's LONG hold every one.
    Move { x: i16, y: i16 },
    /// `b,<button>,<1|0>`
    Button { button: Button, down: bool },
    /// `sp,<dx>,<dy>`: desktop pixels.
    ScrollPx { dx: Pixels, dy: Pixels },
    /// `k,<keysym>,<1|2|0>`: down, repeat, up.
    Key { keysym: u32, act: KeyAct },
    /// `r`
    ReleaseAll,
    /// `c,<text>`: the text is everything after the first comma.
    Clipboard(String),
    /// `v,<1|0>`
    Video(bool),
    /// `au,<1|0>`
    Audio(bool),
    /// `mi,<1|0>`
    Mic(bool),
    /// `sz,<w>,<h>`: the desktop size the viewer's window wants.
    Resize { w: u32, h: u32 },
    /// `kf`
    Keyframe,
    /// `po,<token>`: a ping's reply, echoing the token the ping carried.
    Pong { token: crate::hub::Token },
    /// `ds,<id>`: switch to another display.
    #[cfg(target_os = "macos")]
    Display(u32),
}

impl Command {
    /// The command `t` is, if it is one this server knows, well formed. One rule for all
    /// of them: a command is its name and exactly the fields its pattern names, the line
    /// split at every comma, so a field more or a field fewer is no command, as a
    /// malformed one is. A clipboard's one field is its text, the rest of the line, commas
    /// and all.
    fn parse(t: &str) -> Option<Command> {
        let (name, fields): (&str, Vec<&str>) = match t.split_once(',') {
            Some(("c", text)) => return Some(Command::Clipboard(text.to_string())),
            Some((name, rest)) => (name, rest.split(',').collect()),
            None => (t, vec![]),
        };
        let flag = |v: &str| match v {
            "1" => Some(true),
            "0" => Some(false),
            _ => None,
        };
        let key = |v: &str| match v {
            "1" => Some(KeyAct::Down),
            "2" => Some(KeyAct::Repeat),
            "0" => Some(KeyAct::Up),
            _ => None,
        };
        Some(match (name, fields.as_slice()) {
            ("a", [frame]) => Command::Ack {
                frame: num(frame)?,
                hold: crate::hub::Hold::Unsaid,
            },
            ("a", [frame, hold]) => Command::Ack {
                frame: num(frame)?,
                hold: crate::hub::Hold::parse(hold),
            },
            ("m", [x, y]) => Command::Move {
                x: num(x)?,
                y: num(y)?,
            },
            ("b", [button, down]) => Command::Button {
                button: Button::parse(button)?,
                down: flag(down)?,
            },
            ("sp", [dx, dy]) => Command::ScrollPx {
                dx: Pixels::parse(dx)?,
                dy: Pixels::parse(dy)?,
            },
            ("k", [keysym, act]) => Command::Key {
                keysym: num(keysym)?,
                act: key(act)?,
            },
            ("r", []) => Command::ReleaseAll,
            ("v", [on]) => Command::Video(flag(on)?),
            ("au", [on]) => Command::Audio(flag(on)?),
            ("mi", [on]) => Command::Mic(flag(on)?),
            ("sz", [w, h]) => Command::Resize {
                w: num(w)?,
                h: num(h)?,
            },
            ("kf", []) => Command::Keyframe,
            ("po", [token]) => Command::Pong {
                token: crate::hub::Token::parse(token)?,
            },
            #[cfg(target_os = "macos")]
            ("ds", [id]) => Command::Display(id.parse().ok()?),
            _ => return None,
        })
    }

    /// Whether a --read-only viewer may send it: what watching needs (acks, video,
    /// audio, keyframe requests, ping replies). Everything else acts on the desktop
    /// (input, clipboard, microphone, resizing, displays). Checked right where commands
    /// are applied, so a client that ignores the hello flag cannot drive the desktop;
    /// web/src/client/main.ts keeps the same list.
    fn watches(&self) -> bool {
        match self {
            Command::Ack { .. }
            | Command::Video(_)
            | Command::Audio(_)
            | Command::Keyframe
            | Command::Pong { .. } => true,
            Command::Move { .. }
            | Command::Button { .. }
            | Command::ScrollPx { .. }
            | Command::Key { .. }
            | Command::ReleaseAll
            | Command::Clipboard(_)
            | Command::Mic(_)
            | Command::Resize { .. } => false,
            #[cfg(target_os = "macos")]
            Command::Display(_) => false,
        }
    }
}

fn handle_text(s: &AppState, c: &Arc<Client>, t: &str, rtts: &mut std::collections::VecDeque<u32>) {
    let Some(command) = Command::parse(t) else {
        return;
    };
    if s.read_only && !command.watches() {
        return;
    }
    let input = |ev| send_input(&s.hub, c, ev);
    match command {
        #[cfg(target_os = "macos")]
        Command::Display(id) => match crate::mac::request_display(&s.hub, id, c.id) {
            Ok(true) => {}
            result => {
                if let Err(e) = result {
                    log::warn!("display switch: {e}");
                }
                // The page is told the stream again, which resets its decoders: for this
                // viewer the stream starts over. A keyframe that reached the page ahead of
                // the hello would be lost there, with the viewer counted as started.
                c.start_over(Instant::now(), s.hub.hello());
                s.hub.wake();
            }
        },
        Command::Ack { frame, hold } => {
            c.on_ack(Instant::now(), frame, hold);
            s.hub.wake();
        }
        Command::Move { x, y } => input(InputEvent::Move { x, y }),
        Command::Button { button, down } => input(InputEvent::Button { button, down }),
        Command::ScrollPx { dx, dy } => input(InputEvent::ScrollPx { dx, dy }),
        Command::Key { keysym, act } => input(InputEvent::Key { keysym, act }),
        Command::ReleaseAll => input(InputEvent::ReleaseAll),
        Command::Clipboard(text) => input(InputEvent::Clipboard(
            crate::desktop::ClipContent::Text(text),
        )),
        Command::Video(on) => {
            if on {
                c.watch(Instant::now());
            } else {
                c.stop_watching();
            }
            s.hub.wake();
        }
        Command::Audio(on) => c.audio.store(on && s.audio, Ordering::Relaxed),
        Command::Mic(on) => {
            if let Some(tx) = &s.mic {
                let _ = tx.send(if on {
                    crate::mic::MicMsg::Start(c.id)
                } else {
                    crate::mic::MicMsg::Stop(c.id)
                });
            }
        }
        // The desktop size this client's window wants (dynamic resolution). The newest new
        // ask wins: it is the request until a newer one, and no ask clears another, so the
        // screen ends at the size asked last whatever the capture loop was doing. The
        // capture loop gives it to the screen, which fits it to a size it can take (each
        // platform has its own rule) and changes nothing if it has that size already.
        Command::Resize { w, h } => {
            if !s.resize {
                return;
            }
            #[cfg(target_os = "macos")]
            let input = s.hub.input.lock();
            #[cfg(target_os = "macos")]
            if input.is_none() || c.id < s.hub.input_floor.load(Ordering::Relaxed) {
                return;
            }
            if s.hub.resize_max.lock().is_some() && c.asks_for((w, h)) {
                *s.hub.resize_req.lock() = Some((w, h));
                s.hub.wake();
            }
        }
        Command::Keyframe => {
            c.ask_keyframe();
            s.hub.wake();
        }
        Command::Pong { token } => {
            let now = Instant::now();
            let Some((rtt, ahead)) = c.pings.lock().answer(now, token) else {
                return;
            };
            c.pinged(now, rtt, ahead);
            // Whole ms, and at least 1: a client's min_rtt_ms is 0 until it has one. A
            // ping in the record went out at most a few seconds ago, so a u32 holds it.
            rtts.push_back((rtt.as_millis() as u32).max(1));
            if rtts.len() > 20 {
                rtts.pop_front();
            }
            c.min_rtt_ms
                .store(*rtts.iter().min().unwrap(), Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::Hold;
    use std::time::Duration;

    /// Every command the web client sends (web/src/protocol.ts), with arguments.
    const ALL: &[&str] = &[
        "a,1",
        "a,2,3.5",
        "m,10,20",
        "b,1,1",
        "b,1,0",
        "sp,0.0,12.0",
        "k,65,1",
        "k,65,2",
        "k,65,0",
        "r",
        "c,text",
        "v,1",
        "au,1",
        "mi,1",
        "sz,640,480",
        "kf",
        "po,0123456789abcdef",
        "ds,4294967295",
    ];

    /// A server for `hub` with resizing wired up, and a microphone if `mic`.
    fn app(
        hub: &Arc<Hub>,
        read_only: bool,
        mic: Option<std::sync::mpsc::Sender<crate::mic::MicMsg>>,
    ) -> AppState {
        AppState {
            hub: hub.clone(),
            audio: true,
            files: Arc::new(crate::files::Files {
                root: std::env::temp_dir(),
                upload: false,
                download: false,
                default_dir: String::new(),
                max_upload_bytes: 0,
            }),
            mic,
            resize: true,
            read_only,
            recorder: crate::record::Recorder::new(hub.clone(), Duration::from_secs(1)),
            index: Bytes::new(),
            favicon: Bytes::new(),
            favicon_type: "image/x-icon",
            debug: crate::env::Debugging::read(|_| None).0,
        }
    }

    /// Sends every command to a server that has input, a microphone and resizing wired
    /// up, and returns (input events, microphone messages, a resize was asked for).
    fn drive(read_only: bool) -> (usize, usize, bool) {
        let hub = Hub::new(crate::hub::Paths::ANY);
        hub.width.store(1280, Ordering::Relaxed);
        hub.height.store(800, Ordering::Relaxed);
        *hub.resize_max.lock() = Some((2560, 1600));
        let (input_tx, input_rx) = std::sync::mpsc::channel();
        *hub.input.lock() = Some(input_tx);
        let (mic_tx, mic_rx) = std::sync::mpsc::channel();
        let s = app(&hub, read_only, Some(mic_tx));
        let (tx, _rx) = mpsc::unbounded_channel();
        let client = hub.add_client(tx, Instant::now());
        let mut rtts = Default::default();
        for c in ALL {
            handle_text(&s, &client, c, &mut rtts);
        }
        let resized = hub.resize_req.lock().is_some();
        (
            input_rx.try_iter().count(),
            mic_rx.try_iter().count(),
            resized,
        )
    }

    /// A screen as dynamic resolution sees it: its size, the sizes it refuses (a CRTC that
    /// will not take them), how many times its size really changed, and how many asks it
    /// refused and found already met.
    struct Screen {
        size: (u32, u32),
        refuses: Vec<(u32, u32)>,
        changes: usize,
        refused: usize,
        met: usize,
    }

    impl crate::video::Resize for Screen {
        fn resize(&mut self, w: u32, h: u32) -> Result<(u32, u32), String> {
            if self.refuses.contains(&(w, h)) {
                self.refused += 1;
                return Err("refused".into());
            }
            if (w, h) == self.size {
                self.met += 1;
            } else {
                self.size = (w, h);
                self.changes += 1;
            }
            Ok(self.size)
        }
    }

    /// A page: as the web client is now (`resize.ts` asks on connect and when its window
    /// changes), or a tab still running the client from before (it also asks on every
    /// hello); its connection, if it has one, what it asked on it, and its window.
    struct Page {
        asks_on_hello: bool,
        client: Option<Arc<crate::hub::Client>>,
        asked: Option<(u32, u32)>,
        window: (u32, u32),
    }

    /// The pages and the screen, driven through the server's own `sz` handling and the
    /// capture loop's own decision (`video::rebuilds_for`).
    struct Desk {
        s: AppState,
        screen: Screen,
        pages: Vec<Page>,
        rtts: std::collections::VecDeque<u32>,
        rebuilds: usize,
        /// The newest ask that changed what its connection asks for: the oracle's rule.
        newest: Option<(u32, u32)>,
        /// Asks that repeated what their connection last asked for.
        repeats: usize,
    }

    impl Desk {
        /// Page `i`, connected, asks for its window.
        fn ask(&mut self, i: usize) {
            let p = &mut self.pages[i];
            let c = p.client.clone().expect("only a connected page asks");
            match p.asked == Some(p.window) {
                true => self.repeats += 1,
                false => self.newest = Some(p.window),
            }
            p.asked = Some(p.window);
            let sz = format!("sz,{},{}", p.window.0, p.window.1);
            handle_text(&self.s, &c, &sz, &mut self.rtts);
        }

        /// The capture loop's turn: it takes the request, gives it to the screen, and if
        /// the stream is rebuilt, every page is said hello.
        fn capture(&mut self) {
            let hub = self.s.hub.clone();
            let Some(want) = hub.resize_req.lock().take() else {
                return;
            };
            let streamed = (
                hub.width.load(Ordering::Relaxed),
                hub.height.load(Ordering::Relaxed),
            );
            if crate::video::rebuilds_for(&mut self.screen, want, streamed) {
                self.rebuilds += 1;
                hub.width.store(self.screen.size.0, Ordering::Relaxed);
                hub.height.store(self.screen.size.1, Ordering::Relaxed);
                for i in 0..self.pages.len() {
                    if self.pages[i].asks_on_hello && self.pages[i].client.is_some() {
                        self.ask(i);
                    }
                }
            }
        }
    }

    /// Dynamic resolution takes the screen to the size asked last and rests there. Over 500
    /// generated runs of 1 to 4 pages (each new or old, as `Page` says) connecting, leaving
    /// and changing their windows among 5 sizes, 40 events each, with the screen refusing
    /// each size in one run of four and the capture loop taking the request at generated
    /// moments between the events (each rebuild saying hello to every page):
    /// - the stream is rebuilt exactly as often as the screen's size really changed: a
    ///   refused ask, or one for the size it has, rebuilds nothing;
    /// - after the last connect or window change the capture loop rebuilds at most once,
    ///   and then never again: a hello brings no new ask, an old page's included;
    /// - the screen then has the size of the newest ask that changed what its connection
    ///   asks for, unless it refuses that size: no page's ask clears another's.
    ///
    /// Each clause is counted over the runs (refused asks, asks already met, repeated asks,
    /// rests reached with one rebuild and with none, newest asks met and refused, and real
    /// changes of the screen's size), so none holds unreached. The screen is the test's own,
    /// fit and all: the server gives it the size as asked, so the property is the same on
    /// every platform, whatever its screen's rule for fitting a size.
    #[test]
    fn resizing_rebuilds_once_per_size_granted_and_rests_after_the_last_ask() {
        let sizes = [
            (1280, 800),
            (1456, 868),
            (1920, 1080),
            (2560, 1600),
            (2912, 1736),
        ];
        let max = (3024, 1898);
        let mut rng = crate::xorshift::Xorshift::new(0x9E37_79B9_7F4A_7C15);
        // refused, met, repeated, rested after one rebuild, after none, newest met, newest
        // refused, real changes of the screen's size
        let mut reach = [0usize; 8];
        for run in 0..500 {
            let hub = Hub::new(crate::hub::Paths::ANY);
            hub.width.store(max.0, Ordering::Relaxed);
            hub.height.store(max.1, Ordering::Relaxed);
            *hub.resize_max.lock() = Some(max);
            // An input thread, as `run` gives the hub: on macOS a client may resize only
            // while the hub has input, and every client here joins at or over its floor.
            let (input, _input_thread) = std::sync::mpsc::channel();
            *hub.input.lock() = Some(input);
            let mut d = Desk {
                s: app(&hub, false, None),
                screen: Screen {
                    size: max,
                    refuses: sizes
                        .iter()
                        .copied()
                        .filter(|_| rng.below(4) == 0)
                        .collect(),
                    changes: 0,
                    refused: 0,
                    met: 0,
                },
                pages: (0..rng.range(1, 5))
                    .map(|_| Page {
                        asks_on_hello: rng.below(2) == 0,
                        client: None,
                        asked: None,
                        window: sizes[rng.below(5) as usize],
                    })
                    .collect(),
                rtts: Default::default(),
                rebuilds: 0,
                newest: None,
                repeats: 0,
            };
            for _ in 0..40 {
                let i = rng.below(d.pages.len() as u64) as usize;
                match (rng.below(4), d.pages[i].client.clone()) {
                    // It connects: it is said hello, and asks once, new or old.
                    (0, None) => {
                        let (tx, _rx) = mpsc::unbounded_channel();
                        d.pages[i].client = Some(hub.add_client(tx, Instant::now()));
                        d.pages[i].asked = None;
                        d.ask(i);
                    }
                    // It leaves.
                    (0, Some(c)) => {
                        hub.remove_client(c.id, Instant::now());
                        d.pages[i].client = None;
                    }
                    // Its window changes, and it asks if it is connected.
                    (1, client) => {
                        d.pages[i].window = sizes[rng.below(5) as usize];
                        if client.is_some() {
                            d.ask(i);
                        }
                    }
                    // The capture loop takes its turn.
                    (_, _) => d.capture(),
                }
            }
            let before = d.rebuilds;
            for _ in 0..10 {
                d.capture();
            }
            let after = d.rebuilds - before;
            assert!(after <= 1, "run {run}: {after} rebuilds after the last ask");
            assert!(
                hub.resize_req.lock().is_none(),
                "run {run}: still asking after rest"
            );
            assert_eq!(
                d.rebuilds, d.screen.changes,
                "run {run}: rebuilds against the screen's real changes"
            );
            match d.newest {
                Some(n) if d.screen.refuses.contains(&n) => reach[6] += 1,
                Some(n) => {
                    assert_eq!(d.screen.size, n, "run {run}: the newest ask was lost");
                    reach[5] += 1;
                }
                None => {}
            }
            reach[0] += d.screen.refused;
            reach[1] += d.screen.met;
            reach[2] += d.repeats;
            reach[7] += d.screen.changes;
            reach[3 + usize::from(after == 0)] += 1;
        }
        assert!(reach.iter().all(|&n| n > 0), "unreached: {reach:?}");
    }

    #[test]
    fn read_only_viewers_cannot_act_on_the_desktop() {
        let (input, mic, resized) = drive(true);
        assert_eq!(input, 0, "input events from a read-only viewer");
        assert_eq!(mic, 0, "microphone messages from a read-only viewer");
        assert!(!resized, "a read-only viewer resized the desktop");
    }

    #[test]
    fn other_viewers_still_can() {
        // The same commands do reach the desktop without --read-only, so the test above
        // checks the guard rather than an unwired server.
        let (input, mic, resized) = drive(false);
        assert!(input >= 9, "{input} input events");
        assert_eq!(mic, 1);
        assert!(resized);
    }

    /// A reply to no ping is no round trip (wip/red-ping-unsent 25e4049). With no record of
    /// the pings sent, a reply naming a moment just past read as a 1 ms round trip on a
    /// viewer that was never pinged: its frames were then overdue at 81 ms, and the flow
    /// check held every viewer's frames once one of its own was 121 ms old. A read-only
    /// viewer may send it.
    #[test]
    fn a_reply_to_no_ping_is_no_round_trip() {
        let hub = Hub::new(crate::hub::Paths::ANY);
        let s = AppState {
            hub: hub.clone(),
            audio: true,
            files: Arc::new(crate::files::Files {
                root: std::env::temp_dir(),
                upload: false,
                download: false,
                default_dir: String::new(),
                max_upload_bytes: 0,
            }),
            mic: None,
            resize: false,
            read_only: true,
            recorder: crate::record::Recorder::new(hub.clone(), Duration::from_secs(1)),
            index: Bytes::new(),
            favicon: Bytes::new(),
            favicon_type: "image/x-icon",
            debug: crate::env::Debugging::read(|_| None).0,
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        let client = hub.add_client(tx, Instant::now());
        let mut rtts = Default::default();
        // No ping was sent to this viewer. It names a moment just past, as replies did when
        // pings were named by the microsecond they went out, then a token.
        for reply in ["po,9999000", "po,0123456789abcdef"] {
            handle_text(&s, &client, reply, &mut rtts);
        }
        assert_eq!(
            (
                client.min_rtt_ms.load(Ordering::Relaxed),
                client.window(|w| w.fastest())
            ),
            (0, None),
            "a reply to no ping read as a round trip"
        );
    }

    /// The other half of the one above, so it checks the record rather than a server that
    /// ignores every reply: the reply to a ping the pinger recorded is a round trip.
    #[test]
    fn a_reply_to_the_ping_sent_is_a_round_trip() {
        let hub = Hub::new(crate::hub::Paths::ANY);
        let s = AppState {
            hub: hub.clone(),
            audio: true,
            files: Arc::new(crate::files::Files {
                root: std::env::temp_dir(),
                upload: false,
                download: false,
                default_dir: String::new(),
                max_upload_bytes: 0,
            }),
            mic: None,
            resize: false,
            read_only: true,
            recorder: crate::record::Recorder::new(hub.clone(), Duration::from_secs(1)),
            index: Bytes::new(),
            favicon: Bytes::new(),
            favicon_type: "image/x-icon",
            debug: crate::env::Debugging::read(|_| None).0,
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        let client = hub.add_client(tx, Instant::now());
        let mut rtts = Default::default();
        let token = crate::hub::Token::random();
        client.ping(Instant::now(), token);
        handle_text(&s, &client, &format!("po,{token}"), &mut rtts);
        assert!(
            client.min_rtt_ms.load(Ordering::Relaxed) > 0
                && client.window(|w| w.fastest()).is_some(),
            "the reply to a ping was no round trip"
        );
    }

    /// A connecting viewer is sent a ping before anything else, so that nothing is on the
    /// path ahead of it, then its hello and the cursor's image if there is one; and the
    /// ping is in its record, with nothing ahead of it, so its reply is a round trip. With
    /// no cursor and with cursors of 24 to 64 px.
    #[test]
    fn a_viewer_is_pinged_before_anything_else_is_sent() {
        for cursor in [None, Some(24), Some(32), Some(48), Some(64)] {
            let hub = Hub::new(crate::hub::Paths::ANY);
            *hub.cursor.lock() = cursor.map(|px: usize| Bytes::from(vec![3u8; px * px * 4 + 14]));
            let (tx, mut rx) = mpsc::unbounded_channel();
            let client = hub.add_client(tx, Instant::now());
            greet(&hub, &client);
            let sent: Vec<Out> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
            let Some(Out::Text(ServerMessage::Ping { s: token })) = sent.first() else {
                panic!("cursor {cursor:?}: the first message is no ping");
            };
            assert!(matches!(
                sent.get(1),
                Some(Out::Text(ServerMessage::Hello(_)))
            ));
            assert_eq!(
                sent.len(),
                2 + usize::from(cursor.is_some()),
                "cursor {cursor:?}"
            );
            let answer = client.pings.lock().answer(Instant::now(), *token);
            assert!(
                answer.is_some_and(|(_, ahead)| ahead == crate::window::Ahead::Nothing),
                "cursor {cursor:?}: the ping is not in the record, or not alone: {answer:?}"
            );
        }
    }

    /// Every command the web client sends parses here (a display switch only on
    /// macOS), and one that is malformed is no command at all.
    #[test]
    fn commands_are_parsed_once_at_the_door() {
        for c in ALL {
            let known = Command::parse(c).is_some();
            assert!(
                known || (c.starts_with("ds,") && !cfg!(target_os = "macos")),
                "{c}"
            );
        }
        for c in [
            "v,2", "v", "a", "a,x", "k,65", "k,65,3", "b,1,7", "sp,1", "po,-1", "zz,1",
        ] {
            assert!(Command::parse(c).is_none(), "{c} parsed");
        }
    }

    /// How many fields each command has, written apart from `parse`: an ack's hold is the
    /// one field that may be left out. A clipboard is not here: its one field is the rest
    /// of the line, commas and all.
    fn fields_of(name: &str) -> &'static [usize] {
        match name {
            "a" => &[1, 2],
            "m" | "b" | "sp" | "k" | "sz" => &[2],
            "v" | "au" | "mi" | "po" | "ds" => &[1],
            "r" | "kf" => &[0],
            _ => &[],
        }
    }

    /// The buttons pointer.ts sends, by the number a command names each with: the browser's
    /// main, middle, secondary, back and forward.
    const BUTTONS: [(i128, Button); 5] = [
        (1, Button::Left),
        (2, Button::Middle),
        (3, Button::Right),
        (8, Button::Back),
        (9, Button::Forward),
    ];

    /// Whether a position, a button, a pixel scroll or a clipboard with its own count of
    /// fields is a command, written apart from `parse` from what each number must be where
    /// it is used: a position in X's 16 bits, a button one of `BUTTONS` and a flag, a pixel
    /// scroll finite and within `MAX_SCROLL_PX`, and a clipboard any text. A line naming
    /// the wheel (`s`) never is. `None` for any other line.
    fn takes(name: &str, fields: &[&str]) -> Option<bool> {
        let int = |f: &str| f.trim().parse::<i128>().ok();
        let position = |f: &str| int(f).is_some_and(|n| i16::try_from(n).is_ok());
        let pixels = |f: &str| {
            f.trim()
                .parse::<f64>()
                .is_ok_and(|px| px.abs() <= MAX_SCROLL_PX)
        };
        Some(match (name, fields) {
            ("m", [x, y]) => position(x) && position(y),
            ("b", [button, down]) => {
                BUTTONS.iter().any(|&(n, _)| int(button) == Some(n)) && matches!(*down, "0" | "1")
            }
            ("s", _) => false,
            ("sp", [dx, dy]) => pixels(dx) && pixels(dy),
            ("c", [_, ..]) => true,
            _ => return None,
        })
    }

    /// One rule for every command: its name and exactly its fields. Each command the web
    /// client sends, with none to four well-formed fields (its own, then copies of its
    /// last), parses exactly when that is a count `fields_of` gives it; an ack without its
    /// hold is `Unsaid`, and with it, the hold as the field says. A clipboard takes one
    /// field or more, and its text is all of them, commas and all.
    #[test]
    fn a_command_is_exactly_its_fields() {
        for c in ALL {
            let (name, own) = match c.split_once(',') {
                Some((name, rest)) => (name, rest.split(',').collect()),
                None => (*c, vec![]),
            };
            if name == "ds" && !cfg!(target_os = "macos") {
                continue;
            }
            let last = own.last().copied().unwrap_or("1");
            for count in 0..=4 {
                let fields: Vec<&str> = own
                    .iter()
                    .copied()
                    .chain(std::iter::repeat(last))
                    .take(count)
                    .collect();
                let line = [name]
                    .iter()
                    .chain(&fields)
                    .copied()
                    .collect::<Vec<_>>()
                    .join(",");
                let parsed = Command::parse(&line);
                let has = match name {
                    "c" => count >= 1,
                    name => fields_of(name).contains(&count),
                };
                assert_eq!(parsed.is_some(), has, "{line}");
                match parsed {
                    Some(Command::Ack { hold, .. }) => {
                        let said = fields.get(1).map_or(Hold::Unsaid, |f| Hold::parse(f));
                        assert_eq!(hold, said, "{line}");
                    }
                    Some(Command::Clipboard(text)) => assert_eq!(text, fields.join(","), "{line}"),
                    _ => {}
                }
            }
        }
    }

    /// A pixel scroll is two finite numbers. `NaN` and `inf` parse as `f64` but are no
    /// distance, and one of them in the input thread's scroll remainder never leaves it.
    #[test]
    fn a_pixel_scroll_is_finite() {
        let Some(Command::ScrollPx { dx, dy }) = Command::parse("sp,-0.5,1e3") else {
            panic!("sp,-0.5,1e3 is no pixel scroll");
        };
        assert_eq!((dx, dy), (Pixels(-0.5), Pixels(1000.0)));
        for c in [
            "sp,NaN,0",
            "sp,0,Infinity",
            "sp,-Infinity,1",
            "sp,inf,0",
            "sp,0,nan",
        ] {
            assert!(Command::parse(c).is_none(), "{c} parsed");
        }
    }

    /// A key's last field says whether it went down, repeated or came up.
    #[test]
    fn a_key_says_what_it_did() {
        for (line, said) in [
            ("k,65,1", KeyAct::Down),
            ("k,65,2", KeyAct::Repeat),
            ("k,65,0", KeyAct::Up),
        ] {
            assert!(
                matches!(Command::parse(line), Some(Command::Key { keysym: 65, act }) if act == said),
                "{line}"
            );
        }
    }

    /// A command is parsed once, into what it is (02b9dd0), so what parses is in range
    /// where it is used. X takes a pointer position as 16 bits, and X has no button 0.
    /// These are no command: a position that only fits by wrapping, and a button that
    /// isn't.
    #[test]
    fn what_parses_is_in_range_where_it_is_used() {
        let mut out = vec![];
        for line in ["m,65636,100", "m,-32769,0", "b,0,1"] {
            match Command::parse(line) {
                // A position is X's 16 bits, so these would have wrapped to fit.
                Some(Command::Move { x, y }) => {
                    out.push(format!("{line}: the pointer goes to ({x}, {y})"))
                }
                // No button is 0, so any button this is was not the one asked for.
                Some(Command::Button { button, .. }) => {
                    out.push(format!("{line}: button {}", button.number()))
                }
                _ => {}
            }
        }
        assert!(out.is_empty(), "{out:#?}");
    }

    /// `Pixels` holds a finite number, but the input thread adds it to its remainder
    /// scaled by `NOTCH / px_per_notch` (input.rs: 120 over `--scroll-px-per-notch`, which
    /// may be anything from 1). Below 120 px a notch the scale is over one, and a finite
    /// pixel count near f64::MAX would become infinite in the remainder, then NaN once the
    /// whole units are taken off: every pixel scroll after it would move nothing.
    #[test]
    fn a_pixel_scroll_stays_finite_in_the_input_threads_remainder() {
        // input.rs's NOTCH, and --scroll-px-per-notch at its floor, at a value below
        // the 120 px a notch where the scale passes one, and at its default.
        const NOTCH: f64 = 120.0;
        let mut out = vec![];
        for line in ["sp,0,1.7e308", "sp,-1e308,0"] {
            let Some(Command::ScrollPx { dx, dy }) = Command::parse(line) else {
                continue;
            };
            for px_per_notch in [1.0, 100.0, 180.0] {
                let k = NOTCH / px_per_notch;
                if !(dx.0 * k).is_finite() || !(dy.0 * k).is_finite() {
                    out.push(format!("{line} at {px_per_notch} px a notch"));
                }
            }
        }
        assert!(out.is_empty(), "infinite in the remainder: {out:?}");
    }

    /// Wheel units a notch: input.rs's NOTCH, and Windows's WHEEL_DELTA.
    const UNITS_PER_NOTCH: f64 = 120.0;
    /// `--scroll-px-per-notch` at its floor (X's and Windows's input threads raise
    /// anything smaller to it), where a pixel is the most wheel units.
    const LEAST_PX_PER_NOTCH: f64 = 1.0;

    /// What the input threads do with a command's numbers, every step checked: a pixel
    /// scroll is scaled to wheel units (on macOS to points, which are fewer), added to a
    /// remainder of up to a notch, and its whole units are sent as an i32. A position and a
    /// button go through as they are.
    fn through_the_input_threads(c: &Command) -> Result<(), String> {
        if let Command::ScrollPx { dx, dy } = c {
            for px in [dx.0, dy.0] {
                let units = UNITS_PER_NOTCH + px.abs() * UNITS_PER_NOTCH / LEAST_PX_PER_NOTCH;
                if !units.is_finite() || units.trunc() > f64::from(i32::MAX) {
                    return Err(format!("{px} px: {units} wheel units"));
                }
            }
        }
        Ok(())
    }

    /// 200,000 random lines: every command name, and some that are none, with no fields up
    /// to four, each empty, at an edge of a type or a range, signed, huge, a float, `NaN`,
    /// `inf`, not a number, or random. Nothing panics; a position, a button, a pixel
    /// scroll, a clipboard or a wheel is a command exactly when `takes` says it is; and
    /// each that parses into a position, a button or a pixel scroll holds exactly the
    /// numbers the line says (a button the one `BUTTONS` names), and they go through the
    /// input threads' arithmetic.
    #[test]
    fn random_lines_parse_in_range_or_not_at_all() {
        const NAMES: [&str; 17] = [
            "a", "m", "b", "s", "sp", "k", "r", "c", "v", "au", "mi", "sz", "kf", "po", "ds", "zz",
            "",
        ];
        const EDGES: [&str; 42] = [
            "",
            "0",
            "-0",
            "+1",
            " 2 ",
            "-1",
            "3",
            "8",
            "9",
            "20",
            "-20",
            "21",
            "-21",
            "127",
            "-128",
            "128",
            "255",
            "256",
            "32767",
            "-32768",
            "32768",
            "-32769",
            "65636",
            "2147483647",
            "-2147483648",
            "2147483648",
            "1048576",
            "-1048576",
            "1048576.5",
            "-1048577",
            "10",
            "1.7e308",
            "-1e308",
            "1e309",
            "0.5",
            "NaN",
            "inf",
            "-Infinity",
            "x",
            "é",
            "1e3",
            "18446744073709551616",
        ];
        let mut rng = crate::xorshift::Xorshift::new(0x9E3779B97F4A7C15);
        let mut next = move || rng.next();
        // Lines that parsed into a position, a button and a pixel scroll.
        let mut parsed = [0; 3];
        let mut out = vec![];
        for _ in 0..200_000 {
            let mut line = NAMES[next() as usize % NAMES.len()].to_string();
            // Two fields most often, the most any command has; none to four.
            for _ in 0..[0, 1, 1, 2, 2, 2, 3, 4][next() as usize % 8] {
                let r = next();
                // Half a digit (a flag, a button, a small count), a quarter an edge, and
                // the rest an integer or a float of any size, or any f64 at all.
                let field = match r % 8 {
                    0..=3 => ((r >> 3) % 10).to_string(),
                    4 | 5 => EDGES[(r >> 3) as usize % EDGES.len()].to_string(),
                    6 => ((next() as i64) >> ((r >> 3) % 64)).to_string(),
                    _ if r & 8 == 0 => {
                        (((next() as i64) >> ((r >> 4) % 64)) as f64 / 10.0).to_string()
                    }
                    _ => f64::from_bits(next()).to_string(),
                };
                line = format!("{line},{field}");
            }
            let said: Vec<&str> = line.split(',').skip(1).collect();
            let name = line.split(',').next().expect("a line has a name");
            let command = Command::parse(&line);
            match takes(name, &said) {
                Some(true) if command.is_none() => out.push(format!("{line}: no command")),
                Some(false) if command.is_some() => out.push(format!("{line}: a command")),
                _ => {}
            }
            let Some(c) = command else {
                continue;
            };
            if name != "c" && !fields_of(name).contains(&said.len()) {
                out.push(format!("{line}: {} fields", said.len()));
            }
            let int = |i: usize| said[i].trim().parse::<i128>().ok();
            let float = |i: usize| said[i].trim().parse::<f64>().ok();
            let (kind, as_said) = match &c {
                Command::Move { x, y } => (
                    0,
                    int(0) == Some((*x).into()) && int(1) == Some((*y).into()),
                ),
                Command::Button { button, .. } => (
                    1,
                    BUTTONS.contains(&(int(0).expect("a button is a number"), *button)),
                ),
                Command::ScrollPx { dx, dy } => {
                    (2, float(0) == Some(dx.0) && float(1) == Some(dy.0))
                }
                _ => continue,
            };
            parsed[kind] += 1;
            if !as_said {
                out.push(format!("{line}: not what it says"));
            }
            if let Err(e) = through_the_input_threads(&c) {
                out.push(format!("{line}: {e}"));
            }
        }
        assert!(out.is_empty(), "{out:#?}");
        assert!(
            parsed.iter().all(|&n| n >= 100),
            "too few reached (positions, buttons, pixel scrolls): {parsed:?}"
        );
    }

    #[test]
    fn favicon_uses_file_extension_for_content_type() {
        let (bytes, content_type) = favicon(None).unwrap();
        assert_eq!(content_type, "image/x-icon");
        assert!(!bytes.is_empty());

        let dir = std::env::temp_dir().join(format!("voxl-favicon-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("icon.PNG");
        std::fs::write(&png, b"\\x89PNG").unwrap();
        let (bytes, content_type) = favicon(Some(png.to_str().unwrap())).unwrap();
        assert_eq!(content_type, "image/png");
        assert_eq!(&bytes[..], b"\\x89PNG");
        assert!(favicon(Some(dir.join("icon.txt").to_str().unwrap())).is_err());
        assert!(favicon(Some(dir.join("missing.ico").to_str().unwrap())).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn display_switch_drains_input_and_retires_old_connections() {
        let hub = Hub::new(crate::hub::Paths::ANY);
        let (sender, events) = std::sync::mpsc::channel();
        *hub.input.lock() = Some(sender);
        let (out, _messages) = mpsc::unbounded_channel();
        let old = hub.add_client(out, Instant::now());
        send_input(&hub, &old, InputEvent::Move { x: 1, y: 2 });
        let request = hub.pause_display_input(20, old.id).unwrap();
        assert!(
            hub.clients.lock().is_empty(),
            "old subscriptions were not retired"
        );
        assert!(matches!(
            events.try_recv(),
            Ok(InputEvent::Move { x: 1, y: 2 })
        ));
        let InputEvent::DisplayBarrier(done) = events.try_recv().unwrap() else {
            panic!("missing input barrier")
        };
        send_input(
            &hub,
            &old,
            InputEvent::Key {
                keysym: 65,
                act: KeyAct::Down,
            },
        );
        assert!(
            events.try_recv().is_err(),
            "input entered while capture was changing"
        );
        done.send(()).unwrap();
        request.ready.recv().unwrap();
        *hub.input.lock() = Some(request.sender);
        send_input(&hub, &old, InputEvent::Move { x: 100, y: 200 });
        assert!(
            events.try_recv().is_err(),
            "an old connection used the new geometry"
        );
        assert!(hub.pause_display_input(30, old.id).is_none());
        let (out, _messages) = mpsc::unbounded_channel();
        let fresh = hub.add_client(out, Instant::now());
        send_input(&hub, &fresh, InputEvent::Move { x: 3, y: 4 });
        assert!(matches!(
            events.try_recv(),
            Ok(InputEvent::Move { x: 3, y: 4 })
        ));
    }
}
