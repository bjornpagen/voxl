//! A session's control connection: `voxl attach`'s, over ssh. The daemon mints the
//! session's capability and sends it first, then the hello and the cursor; after that the
//! stream carries the page's messages both ways (as the page writes and reads them, the
//! relay handing them on). When the connection closes, the session ends: its capability is
//! forgotten and its lane closed.

use crate::event::{Bin, Button, ClipContent, InputEvent, KeyAct, MicMsg, Pixels, Press};
use crate::hub::{Client, Hub, Out};
use crate::session::sender::Sender;
use crate::wire::{Capability, Down, Framer, Up};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(target_os = "linux")]
use tokio::net::UnixListener;
use tokio::net::UnixStream;
use tokio::sync::mpsc;

/// What the daemon was started with that a session's commands need.
pub struct Daemon {
    pub hub: Arc<Hub>,
    pub audio: Audio,
    /// The microphone's thread.
    pub mic: std::sync::mpsc::Sender<MicMsg>,
    pub resize: Resizing,
    /// The longest recording: what one holds in memory is bound by it.
    pub max_record: std::time::Duration,
}

/// Whether desktop audio is sent (`--audio-enabled`, and a desktop that has it).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Audio {
    Sent,
    Off,
}

/// Whether the desktop follows a viewer's window: it does where the X server can change
/// its size; `hub.resize_max` says what it can take.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Resizing {
    Follows,
    Fixed,
}

#[cfg(target_os = "linux")]
/// Accepts control connections on `listener`, until the daemon ends.
pub async fn serve(listener: UnixListener, daemon: Arc<Daemon>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(connection(stream, daemon.clone()));
            }
            Err(e) => log::warn!("control accept: {e}"),
        }
    }
}

/// A control connection: only this user's processes are heard (anyone else's is closed
/// unread), and its first message says what it is, a session or a query.
pub(super) async fn connection(stream: UnixStream, d: Arc<Daemon>) {
    if !super::listen::own_user(&stream) {
        return;
    }
    let (mut read, write) = stream.into_split();
    match super::query::Opening::read(&mut read).await {
        Ok(super::query::Opening::Session) => session(read, write, d).await,
        Ok(super::query::Opening::Query(q)) => super::query::answer(q, read, write, d).await,
        Err(e) => log::warn!("a control connection that opened with no opening: {e:?}"),
    }
}

/// One session, from its control connection's open to its close.
async fn session(
    mut read: tokio::net::unix::OwnedReadHalf,
    mut write: tokio::net::unix::OwnedWriteHalf,
    d: Arc<Daemon>,
) {
    let capability = match Capability::mint() {
        Ok(c) => c,
        Err(e) => {
            log::error!("no capability for a session: the random source failed: {e}");
            return;
        }
    };
    let Ok(capability) = send_capability(capability, &mut write).await else {
        return;
    };
    let (tx, rx) = mpsc::unbounded_channel::<Out>();
    let client = d.hub.add_client(tx, Sender::new(capability));
    log::info!("session {} opened", client.id);
    greet(&d.hub, &client);
    let writer = tokio::spawn(control_writer(rx, write));
    let why = read_control(&mut read, &d, &client).await;
    // The session ends here, whatever ended it: its capability and lane first, so no
    // lane gets another byte, then its input, its microphone and its place in the hub.
    client.lane(Sender::close);
    writer.abort();
    send_input(&d.hub, InputEvent::ReleaseAll);
    send_mic(&d, MicMsg::Stop(client.id));
    d.hub.remove_client(client.id);
    log::info!("session {} closed: {why}", client.id);
}

/// Writes the capability as the session's first message, and hands it back to be kept.
/// The bytes it was written from are zeroed once written.
async fn send_capability(
    capability: Capability,
    write: &mut tokio::net::unix::OwnedWriteHalf,
) -> std::io::Result<Capability> {
    let mut out = Vec::with_capacity(4 + 1 + 32);
    Down::Capability(capability.lend()).encode(&mut out);
    let written = write.write_all(&out).await;
    for b in out.iter_mut() {
        // SAFETY: a write to a byte of a vector this function owns; volatile, so it is
        // not elided as dead before the vector is freed.
        unsafe { std::ptr::write_volatile(b, 0) };
    }
    written.map(|()| capability)
}

/// What a session is sent as it opens, after its capability: its hello, and the cursor's
/// image.
pub(crate) fn greet(hub: &Hub, c: &Client) {
    c.send(Out::Text(hub.hello()));
    if let Some(cursor) = hub.cursor.lock().clone() {
        c.send(Out::Bin(cursor));
    }
}

/// Writes what the session is sent on its control stream, until it closes.
async fn control_writer(
    mut rx: mpsc::UnboundedReceiver<Out>,
    mut write: tokio::net::unix::OwnedWriteHalf,
) {
    let mut out = Vec::new();
    while let Some(m) = rx.recv().await {
        out.clear();
        match &m {
            Out::Bin(b) => Down::Binary(b).encode(&mut out),
            Out::Text(t) => Down::Text(&t.json()).encode(&mut out),
            Out::Attached => Down::Attached.encode(&mut out),
            // A recording's alone: a session's audio goes on its lane.
            Out::Audio { .. } => continue,
        }
        if write.write_all(&out).await.is_err() {
            return;
        }
    }
}

/// Why a session's control connection ended.
#[derive(Debug)]
enum Ended {
    /// The relay closed it, or ssh did.
    Closed,
    Read(std::io::Error),
    /// It sent what is no control message.
    Malformed(crate::wire::Malformed),
}

impl std::fmt::Display for Ended {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Ended::Closed => write!(f, "its connection closed"),
            Ended::Read(e) => write!(f, "{e}"),
            Ended::Malformed(e) => write!(f, "a malformed control message: {e}"),
        }
    }
}

/// Reads the session's control stream and acts on each message, until it ends.
async fn read_control(
    read: &mut tokio::net::unix::OwnedReadHalf,
    d: &Daemon,
    c: &Arc<Client>,
) -> Ended {
    let mut framer = Framer::new();
    let mut buf = vec![0u8; 64 << 10];
    loop {
        let n = match read.read(&mut buf).await {
            Ok(0) => return Ended::Closed,
            Ok(n) => n,
            Err(e) => return Ended::Read(e),
        };
        framer.push(&buf[..n]);
        loop {
            let message = match framer.frame() {
                Ok(Some(m)) => m,
                Ok(None) => break,
                Err(e) => return Ended::Malformed(e),
            };
            let up = match Up::decode(message) {
                Ok(up) => up,
                Err(e) => return Ended::Malformed(e),
            };
            handle(d, c, up);
        }
    }
}

/// Acts on one message from the relay.
fn handle(d: &Daemon, c: &Arc<Client>, up: Up<'_>) {
    match up {
        Up::Text(t) => handle_text(d, c, t),
        // A binary message is its type byte and what it carries; an empty one, or one of
        // a type the page does not send, is dropped.
        Up::Binary([tag, data @ ..]) if !data.is_empty() => match Bin::up(*tag) {
            Some(Bin::ClipImageUp) => send_input(
                &d.hub,
                InputEvent::Clipboard(ClipContent::Image(data.to_vec())),
            ),
            Some(Bin::MicOpus) => send_mic(d, MicMsg::Opus(c.id, data.to_vec().into())),
            Some(Bin::Cursor | Bin::ClipImage) | None => {}
        },
        Up::Binary(_) => {}
    }
}

/// Hands `event` to the input thread. The thread ends only with the daemon, so an event
/// it cannot take is said, not dropped unsaid; in a test's daemon, with no input thread,
/// there is none to take it.
fn send_input(hub: &Hub, event: InputEvent) {
    if let Some(tx) = hub.input.lock().as_ref() {
        if tx.send(event).is_err() {
            log::error!("the input thread has ended: an input event is dropped");
        }
    }
}

/// Hands `m` to the microphone's thread, which ends only with the daemon.
fn send_mic(d: &Daemon, m: MicMsg) {
    if d.mic.send(m).is_err() {
        log::error!("the microphone's thread has ended: its message is dropped");
    }
}

fn num<T: std::str::FromStr>(field: &str) -> Option<T> {
    field.trim().parse().ok()
}

/// A text command from a viewer (web/src/protocol.ts), parsed once where it arrives.
enum Command {
    /// `m,<x>,<y>`: a desktop pixel, from the top left. X's coordinates are 16-bit (XTest
    /// takes them as they are).
    Move { x: i16, y: i16 },
    /// `b,<button>,<1|0>`
    Button { button: Button, press: Press },
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
            ("m", [x, y]) => Command::Move {
                x: num(x)?,
                y: num(y)?,
            },
            ("b", [button, press]) => Command::Button {
                button: Button::parse(button)?,
                press: Press::parse(press)?,
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
            _ => return None,
        })
    }
}

fn handle_text(d: &Daemon, c: &Arc<Client>, t: &str) {
    let Some(command) = Command::parse(t) else {
        return;
    };
    let input = |ev| send_input(&d.hub, ev);
    match command {
        Command::Move { x, y } => input(InputEvent::Move { x, y }),
        Command::Button { button, press } => input(InputEvent::Button { button, press }),
        Command::ScrollPx { dx, dy } => input(InputEvent::ScrollPx { dx, dy }),
        Command::Key { keysym, act } => input(InputEvent::Key { keysym, act }),
        Command::ReleaseAll => input(InputEvent::ReleaseAll),
        Command::Clipboard(text) => input(InputEvent::Clipboard(ClipContent::Text(text))),
        Command::Video(on) => {
            if on {
                c.watch();
            } else {
                c.stop_watching();
            }
            d.hub.wake();
        }
        Command::Audio(on) => d.hub.set_audio(c, on && d.audio == Audio::Sent),
        Command::Mic(on) => send_mic(
            d,
            match on {
                true => MicMsg::Start(c.id),
                false => MicMsg::Stop(c.id),
            },
        ),
        // The desktop size this client's window wants (dynamic resolution). The newest ask
        // wins: it is the request until a newer one, and no ask clears another, so the
        // screen ends at the size asked last whatever the capture loop was doing. The
        // capture loop gives it to the screen, which fits it to a size it can take (each
        // platform has its own rule) and changes nothing if it has that size already.
        Command::Resize { w, h } => {
            if d.resize == Resizing::Fixed {
                return;
            }
            if d.hub.resize_max.lock().is_some() {
                *d.hub.resize_req.lock() = Some((w, h));
                d.hub.wake();
            }
        }
    }
}
