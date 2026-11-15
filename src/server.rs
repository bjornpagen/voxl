//! HTTP + WebSocket front end. Serves the embedded web client and the media/input socket.

use crate::hub::{Client, Hub, Out};
use crate::input::{InputEvent, KeyAct};
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
    pub env: crate::env::Env,
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
        .route("/ws", get(ws_handler))
        .with_state(state)
        .merge(file_routes)
        .merge(record_routes)
        .merge(wait_routes)
}

async fn stats(State(s): State<AppState>) -> Response {
    let st = &s.hub.stats;
    let clients: Vec<_> = s
        .hub
        .clients
        .lock()
        .iter()
        .map(|c| {
            let (n, age) = c.backlog();
            serde_json::json!({"id": c.id, "video": c.video.load(Ordering::Relaxed), "inflight": n, "oldest_ms": age,
                "min_rtt_ms": c.min_rtt_ms.load(Ordering::Relaxed), "queue_ms": c.queue_ms() as u64, "capacity_kbps": c.capacity().map(|k| k.get() as u64),
                "client_ms": (*c.client_ms.lock() * 10.0).round() / 10.0})
        })
        .collect();
    let body = serde_json::json!({
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
        "target_kbps": s.hub.target().map(|k| k.get() as u64),
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

async fn ws_handler(ws: WebSocketUpgrade, State(s): State<AppState>) -> Response {
    ws.max_message_size(64 << 20)
        .on_upgrade(move |socket| handle_socket(socket, s))
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
        s.hub.add_client(tx)
    };
    log::info!("client {} connected", client.id);
    client.send(Out::Text(s.hub.hello()));
    if let Some(c) = s.hub.cursor.lock().clone() {
        client.send(Out::Bin(c));
    }
    let dbg = s.env.marker.is_some();
    let writer = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            let t = Instant::now();
            let big = matches!(&m, Out::Bin(b) if b.len() > 2000);
            let msg = match m {
                Out::Bin(b) => Message::Binary(b),
                Out::Text(t) => Message::Text(t.into()),
            };
            if sink.send(msg).await.is_err() {
                break;
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
    let pinger = {
        let c = client.clone();
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(crate::hub::PING_EVERY);
            loop {
                iv.tick().await;
                let token = crate::hub::Token::random();
                c.pings.lock().send(Instant::now(), token);
                c.send(Out::Text(format!("{{\"t\":\"ping\",\"s\":\"{token}\"}}")));
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
    s.hub.remove_client(client.id);
    log::info!("client {} disconnected", client.id);
}

fn num<T: std::str::FromStr>(it: &mut std::str::Split<'_, char>) -> Option<T> {
    it.next()?.trim().parse().ok()
}

/// A button the viewer's pointer has, which a command names by X's number for it.
/// pointer.ts sends the browser's five (main, middle, secondary, back, forward) as 1, 2,
/// 3, 8 and 9, and those five are all macOS and Windows press. X has more (4 to 7 are its
/// wheel, which `s` and `sp` turn) and no button 0.
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

/// The most notches one wheel message turns, either way.
///
/// Who needs it: X turns its wheel by a press and a release of a button per notch, so
/// this bounds what one message makes the input thread send X at 40 events, here and in
/// its pixel-scroll fallback on a server without smooth scrolling. macOS and Windows
/// negate a count, and Windows scales it by its 120 units a notch, which a count this
/// size cannot overflow.
///
/// What sets it: 93ad2d4 clamps every wheel message to 20 notches on X, macOS and
/// Windows, so each count that turned a wheel before turns it as it did. The web client
/// sends no `s` (pointer.ts sends pixels), and bench/macos/input.js sends 3; half or
/// double this changes nothing either sends.
pub const MAX_NOTCHES: i8 = 20;

/// A wheel turned by whole notches, at most `MAX_NOTCHES` either way: positive turns it
/// down, or right.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Notches(i8);

impl Notches {
    fn parse(field: &str) -> Option<Notches> {
        let n: i8 = field.trim().parse().ok()?;
        (-MAX_NOTCHES..=MAX_NOTCHES)
            .contains(&n)
            .then_some(Notches(n))
    }

    pub fn get(self) -> i8 {
        self.0
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
struct Pixels(f64);

impl Pixels {
    fn parse(field: &str) -> Option<Pixels> {
        let px: f64 = field.trim().parse().ok()?;
        (px.abs() <= MAX_SCROLL_PX).then_some(Pixels(px))
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
    /// `s,<dx>,<dy>`: wheel notches.
    Scroll { dx: Notches, dy: Notches },
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
    /// The command `t` is, if it is one this server knows, well formed.
    fn parse(t: &str) -> Option<Command> {
        let (name, rest) = t.split_once(',').unwrap_or((t, ""));
        let mut it = rest.split(',');
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
        Some(match name {
            "a" => Command::Ack {
                frame: num(&mut it)?,
                hold: crate::hub::Hold::parse(it.next()),
            },
            "m" => Command::Move {
                x: num(&mut it)?,
                y: num(&mut it)?,
            },
            "b" => Command::Button {
                button: Button::parse(it.next()?)?,
                down: flag(it.next()?)?,
            },
            "s" => Command::Scroll {
                dx: Notches::parse(it.next()?)?,
                dy: Notches::parse(it.next()?)?,
            },
            "sp" => {
                let (dx, dy) = rest.split_once(',')?;
                Command::ScrollPx {
                    dx: Pixels::parse(dx)?,
                    dy: Pixels::parse(dy)?,
                }
            }
            "k" => Command::Key {
                keysym: num(&mut it)?,
                act: key(it.next()?)?,
            },
            "r" => Command::ReleaseAll,
            "c" => Command::Clipboard(rest.to_string()),
            "v" => Command::Video(flag(rest)?),
            "au" => Command::Audio(flag(rest)?),
            "mi" => Command::Mic(flag(rest)?),
            "sz" => Command::Resize {
                w: num(&mut it)?,
                h: num(&mut it)?,
            },
            "kf" => Command::Keyframe,
            "po" => Command::Pong {
                token: crate::hub::Token::parse(it.next()?)?,
            },
            #[cfg(target_os = "macos")]
            "ds" => Command::Display(rest.parse().ok()?),
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
            | Command::Scroll { .. }
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
                c.needs_keyframe.store(true, Ordering::Relaxed);
                c.send(Out::Text(s.hub.hello()));
                s.hub.wake();
            }
        },
        Command::Ack { frame, hold } => {
            c.on_ack(Instant::now(), frame, hold);
            s.hub.wake();
        }
        Command::Move { x, y } => input(InputEvent::Move { x, y }),
        Command::Button { button, down } => input(InputEvent::Button { button, down }),
        Command::Scroll { dx, dy } => input(InputEvent::Scroll { dx, dy }),
        Command::ScrollPx { dx, dy } => input(InputEvent::ScrollPx { dx: dx.0, dy: dy.0 }),
        Command::Key { keysym, act } => input(InputEvent::Key { keysym, act }),
        Command::ReleaseAll => input(InputEvent::ReleaseAll),
        Command::Clipboard(text) => input(InputEvent::Clipboard(
            crate::desktop::ClipContent::Text(text),
        )),
        Command::Video(on) => {
            c.video.store(on, Ordering::Relaxed);
            if on {
                c.inflight.lock().clear();
                c.started.store(false, Ordering::Relaxed);
                c.needs_keyframe.store(true, Ordering::Relaxed);
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
        // The desktop size this client's window wants (dynamic resolution). The latest
        // request wins; the video thread clamps it and applies it.
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
            if let Some(max) = *s.hub.resize_max.lock() {
                let want = crate::screen::fit(w, h, max);
                let current = (
                    s.hub.width.load(Ordering::Relaxed),
                    s.hub.height.load(Ordering::Relaxed),
                );
                *s.hub.resize_req.lock() = (want != current).then_some(want);
                s.hub.wake();
            }
        }
        Command::Keyframe => {
            c.started.store(false, Ordering::Relaxed);
            c.needs_keyframe.store(true, Ordering::Relaxed);
            s.hub.wake();
        }
        Command::Pong { token } => {
            let Some(rtt) = c.pings.lock().answer(Instant::now(), token) else {
                return;
            };
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
    use std::time::Duration;

    /// Every command the web client sends (web/src/protocol.ts), with arguments.
    const ALL: &[&str] = &[
        "a,1",
        "a,2,3.5",
        "m,10,20",
        "b,1,1",
        "b,1,0",
        "s,0,1",
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

    /// Sends every command to a server that has input, a microphone and resizing wired
    /// up, and returns (input events, microphone messages, a resize was asked for).
    fn drive(read_only: bool) -> (usize, usize, bool) {
        let hub = Hub::new();
        hub.width.store(1280, Ordering::Relaxed);
        hub.height.store(800, Ordering::Relaxed);
        *hub.resize_max.lock() = Some((2560, 1600));
        let (input_tx, input_rx) = std::sync::mpsc::channel();
        *hub.input.lock() = Some(input_tx);
        let (mic_tx, mic_rx) = std::sync::mpsc::channel();
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
            mic: Some(mic_tx),
            resize: true,
            read_only,
            recorder: crate::record::Recorder::new(hub.clone(), Duration::from_secs(1)),
            index: Bytes::new(),
            favicon: Bytes::new(),
            favicon_type: "image/x-icon",
            env: crate::env::Env::read(|_| None),
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        let client = hub.add_client(tx);
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
        let hub = Hub::new();
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
            env: crate::env::Env::read(|_| None),
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        let client = hub.add_client(tx);
        let mut rtts = Default::default();
        // No ping was sent to this viewer. It names a moment just past, as replies did when
        // pings were named by the microsecond they went out, then a token.
        for reply in ["po,9999000", "po,0123456789abcdef"] {
            handle_text(&s, &client, reply, &mut rtts);
        }
        assert_eq!(
            client.rtt_ms(),
            None,
            "a reply to no ping read as a round trip"
        );
    }

    /// The other half of the one above, so it checks the record rather than a server that
    /// ignores every reply: the reply to a ping the pinger recorded is a round trip.
    #[test]
    fn a_reply_to_the_ping_sent_is_a_round_trip() {
        let hub = Hub::new();
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
            env: crate::env::Env::read(|_| None),
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        let client = hub.add_client(tx);
        let mut rtts = Default::default();
        let token = crate::hub::Token::random();
        client.pings.lock().send(Instant::now(), token);
        handle_text(&s, &client, &format!("po,{token}"), &mut rtts);
        assert!(
            client.rtt_ms().is_some(),
            "the reply to a ping was no round trip"
        );
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
    /// where it is used. X takes a pointer position as 16 bits, a wheel count is negated
    /// to scroll up, and X has no button 0. These are no command: a position that only
    /// fits by wrapping, a wheel count that cannot be negated, and a button that isn't.
    #[test]
    fn what_parses_is_in_range_where_it_is_used() {
        let mut out = vec![];
        for line in ["m,65636,100", "m,-32769,0", "s,0,-2147483648", "b,0,1"] {
            match Command::parse(line) {
                // A position is X's 16 bits, so these would have wrapped to fit.
                Some(Command::Move { x, y }) => {
                    out.push(format!("{line}: the pointer goes to ({x}, {y})"))
                }
                Some(Command::Scroll { dx, dy })
                    if dx.get().checked_neg().is_none() || dy.get().checked_neg().is_none() =>
                {
                    out.push(format!("{line}: a wheel count that cannot be negated"))
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

    /// What the input threads do with a command's numbers, every step checked: X clicks a
    /// wheel button once a notch, macOS negates a count and Windows negates and scales it;
    /// a pixel scroll is scaled to wheel units (on macOS to points, which are fewer),
    /// added to a remainder of up to a notch, and its whole units are sent as an i32. A
    /// position and a button go through as they are.
    fn through_the_input_threads(c: &Command) -> Result<(), String> {
        match c {
            Command::Scroll { dx, dy } => {
                for n in [dx.get(), dy.get()] {
                    let clicks = n.unsigned_abs() <= MAX_NOTCHES.unsigned_abs();
                    let scaled = i32::from(n)
                        .checked_neg()
                        .and_then(|n| n.checked_mul(UNITS_PER_NOTCH as i32));
                    if !clicks || scaled.is_none() {
                        return Err(format!("{n} notches"));
                    }
                }
            }
            Command::ScrollPx { dx, dy } => {
                for px in [dx.0, dy.0] {
                    let units = UNITS_PER_NOTCH + px.abs() * UNITS_PER_NOTCH / LEAST_PX_PER_NOTCH;
                    if !units.is_finite() || units.trunc() > f64::from(i32::MAX) {
                        return Err(format!("{px} px: {units} wheel units"));
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Each number a command carries parses into the range the input threads take, as it
    /// is, or the line is no command, as a malformed one is: nothing is clamped or wrapped.
    #[test]
    fn a_number_out_of_range_makes_no_command() {
        assert!(matches!(
            Command::parse("m,32767,-32768"),
            Some(Command::Move {
                x: 32767,
                y: -32768
            })
        ));
        for (line, said) in [
            ("b,1,1", Button::Left),
            ("b,2,1", Button::Middle),
            ("b,3,1", Button::Right),
            ("b,8,1", Button::Back),
            ("b,9,1", Button::Forward),
        ] {
            assert!(
                matches!(Command::parse(line), Some(Command::Button { button, down: true }) if button == said),
                "{line}"
            );
        }
        let edges = [
            ("s,-20,20", (-20.0, 20.0)),
            ("sp,-1048576,1048576", (-MAX_SCROLL_PX, MAX_SCROLL_PX)),
        ];
        for (line, (x, y)) in edges {
            let c = Command::parse(line).unwrap_or_else(|| panic!("{line} is no command"));
            let got = match &c {
                Command::Scroll { dx, dy } => (f64::from(dx.get()), f64::from(dy.get())),
                Command::ScrollPx { dx, dy } => (dx.0, dy.0),
                _ => panic!("{line} is another command"),
            };
            assert_eq!(got, (x, y), "{line}");
            assert_eq!(through_the_input_threads(&c), Ok(()), "{line}");
        }
        for line in [
            "m,32768,0",
            "m,0,-32769",
            "m,65636,100",
            "b,0,1",
            "b,4,1",
            "b,7,0",
            "b,10,1",
            "b,256,1",
            "b,-1,1",
            "s,21,0",
            "s,0,-21",
            "s,0,-2147483648",
            "s,128,0",
            "sp,1048576.5,0",
            "sp,0,-1048577",
            "sp,0,1.7e308",
            "sp,-1e308,0",
        ] {
            assert!(Command::parse(line).is_none(), "{line} parsed");
        }
    }

    /// 200,000 random lines: every command name, and some that are none, with no fields up
    /// to four, each empty, at an edge of a type or a range, signed, huge, a float, `NaN`,
    /// `inf`, not a number, or random. Nothing panics; and each that parses into a position,
    /// a button, a wheel or a pixel scroll holds exactly the numbers the line says, and
    /// they go through the input threads' arithmetic.
    #[test]
    fn random_lines_parse_in_range_or_not_at_all() {
        const NAMES: [&str; 17] = [
            "a", "m", "b", "s", "sp", "k", "r", "c", "v", "au", "mi", "sz", "kf", "po", "ds", "zz",
            "",
        ];
        const EDGES: [&str; 40] = [
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
        let mut seed = 0x9E3779B97F4A7C15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        // Lines that parsed into a position, a button, a wheel and a pixel scroll.
        let mut parsed = [0; 4];
        let mut out = vec![];
        for _ in 0..200_000 {
            let mut line = NAMES[next() as usize % NAMES.len()].to_string();
            for _ in 0..next() % 5 {
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
            let Some(c) = Command::parse(&line) else {
                continue;
            };
            let said: Vec<&str> = line.split(',').skip(1).collect();
            let int = |i: usize| said[i].trim().parse::<i128>().ok();
            let float = |i: usize| said[i].trim().parse::<f64>().ok();
            let (kind, as_said) = match &c {
                Command::Move { x, y } => (
                    0,
                    int(0) == Some((*x).into()) && int(1) == Some((*y).into()),
                ),
                Command::Button { button, .. } => (1, int(0) == Some(button.number().into())),
                Command::Scroll { dx, dy } => (
                    2,
                    int(0) == Some(dx.get().into()) && int(1) == Some(dy.get().into()),
                ),
                Command::ScrollPx { dx, dy } => {
                    (3, float(0) == Some(dx.0) && float(1) == Some(dy.0))
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
            "too few reached (positions, buttons, wheels, pixel scrolls): {parsed:?}"
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
        let hub = Hub::new();
        let (sender, events) = std::sync::mpsc::channel();
        *hub.input.lock() = Some(sender);
        let (out, _messages) = mpsc::unbounded_channel();
        let old = hub.add_client(out);
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
        let fresh = hub.add_client(out);
        send_input(&hub, &fresh, InputEvent::Move { x: 3, y: 4 });
        assert!(matches!(
            events.try_recv(),
            Ok(InputEvent::Move { x: 3, y: 4 })
        ));
    }
}
