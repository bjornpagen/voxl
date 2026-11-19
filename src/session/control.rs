//! A session's control connection: `voxl attach`'s, over ssh. The daemon mints the
//! session's capability and sends it first, then the hello and the cursor; after that the
//! stream carries the page's messages both ways (as the page writes and reads them, the
//! relay handing them on), and the relay's word of each video update the page took. When
//! the connection closes, the session ends: its capability is forgotten and its lane
//! closed.

use crate::event::{
    BinUp, Button, ClipContent, InputEvent, KeyAct, MicMsg, Pixels, Press, Switch, Word,
};
use crate::hub::{Client, Hub, Out};
use crate::session::sender::Sender;
use crate::wire::{Byte, Capability, Down, Framer, Up};
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
    /// The longest recording: what one holds in memory is bound by it.
    pub max_record: std::time::Duration,
}

/// Whether desktop audio is sent (`--audio-enabled`, and a desktop that has it).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Audio {
    Sent,
    Off,
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

/// A control connection: only this user's processes are heard (anyone else's, and one
/// whose user cannot be told, is closed unread), and its first message says what it is, a
/// session or a query.
pub(super) async fn connection(stream: UnixStream, d: Arc<Daemon>) {
    match super::listen::peer(&stream) {
        super::listen::Peer::Ours => {}
        super::listen::Peer::Theirs(uid) => {
            log::warn!("a control connection from uid {uid}, closed unread");
            return;
        }
        super::listen::Peer::Untold(e) => {
            log::warn!("a control connection whose user cannot be told ({e}), closed unread");
            return;
        }
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
        // A binary message is its type byte and what it carries; one that carries nothing,
        // or is of a type the page does not send, is dropped.
        Up::Binary(message) => {
            if let Some((&tag, data @ [_, ..])) = message.split_first() {
                match BinUp::of(tag) {
                    Some(BinUp::ClipImage) => send_input(
                        &d.hub,
                        InputEvent::Clipboard(ClipContent::Image(data.to_vec())),
                    ),
                    Some(BinUp::MicOpus) => send_mic(d, MicMsg::Opus(c.id, data.to_vec().into())),
                    None => {}
                }
            }
        }
        // Delivery, told: the session's link counts the update taken, which may open the
        // capture gate.
        Up::Took { version } => {
            c.took(version);
            d.hub.wake();
        }
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

/// The commands a viewer sends as text, each by the word it begins with: `command` in
/// web/src/wire.json, the page's copy, which a test checks against these.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(test, derive(serde::Deserialize), serde(rename_all = "camelCase"))]
enum Name {
    Move,
    Button,
    ScrollPx,
    Key,
    ReleaseAll,
    Clipboard,
    Video,
    Audio,
    Mic,
    Resize,
}

impl Word for Name {
    const ALL: &'static [Name] = &[
        Name::Move,
        Name::Button,
        Name::ScrollPx,
        Name::Key,
        Name::ReleaseAll,
        Name::Clipboard,
        Name::Video,
        Name::Audio,
        Name::Mic,
        Name::Resize,
    ];

    fn word(self) -> &'static str {
        match self {
            Name::Move => "m",
            Name::Button => "b",
            Name::ScrollPx => "sp",
            Name::Key => "k",
            Name::ReleaseAll => "r",
            Name::Clipboard => "c",
            Name::Video => "v",
            Name::Audio => "au",
            Name::Mic => "mi",
            Name::Resize => "sz",
        }
    }
}

/// A text command from a viewer (web/src/protocol.ts), parsed once where it arrives.
#[derive(Debug, PartialEq)]
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
    Video(Switch),
    /// `au,<1|0>`
    Audio(Switch),
    /// `mi,<1|0>`
    Mic(Switch),
    /// `sz,<w>,<h>`: the desktop size the viewer's window wants.
    Resize { w: u32, h: u32 },
}

/// Why a text message is no command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NoCommand {
    /// No command begins with its first word.
    Name,
    /// It has a field more, or a field fewer, than its command's pattern names.
    Fields,
    /// A field is not what its place takes: no number, one out of range, or a word that
    /// place does not have.
    Field,
}

impl std::fmt::Display for NoCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            NoCommand::Name => write!(f, "no command has its name"),
            NoCommand::Fields => write!(f, "not as many fields as its command has"),
            NoCommand::Field => write!(f, "a field that is not what its place takes"),
        }
    }
}

/// What follows a command's name, cut at every comma into exactly `N` fields.
fn fields<const N: usize>(rest: Option<&str>) -> Result<[&str; N], NoCommand> {
    let mut all = rest.into_iter().flat_map(|rest| rest.split(','));
    let mut fields = [""; N];
    for field in &mut fields {
        *field = all.next().ok_or(NoCommand::Fields)?;
    }
    match all.next() {
        None => Ok(fields),
        Some(_) => Err(NoCommand::Fields),
    }
}

/// A field that is a number, spaces around it or not.
fn num<T: std::str::FromStr>(field: &str) -> Result<T, NoCommand> {
    field.trim().parse().map_err(|_| NoCommand::Field)
}

/// A field that is one of a vocabulary's words.
fn word<W: Word>(field: &str) -> Result<W, NoCommand> {
    W::of(field).ok_or(NoCommand::Field)
}

/// A field that is a distance in pixels.
fn pixels(field: &str) -> Result<Pixels, NoCommand> {
    Pixels::of(num(field)?).ok_or(NoCommand::Field)
}

impl Command {
    /// The command `t` is. One rule for all of them: a command is its name and exactly
    /// the fields its pattern names, what follows the name's comma cut at every comma, so
    /// a field more or a field fewer is no command, as a malformed one is. A clipboard's
    /// one field is its text, the rest of the line, commas and all.
    fn parse(t: &str) -> Result<Command, NoCommand> {
        let (name, rest) = match t.split_once(',') {
            Some((name, rest)) => (name, Some(rest)),
            None => (t, None),
        };
        Ok(match Name::of(name).ok_or(NoCommand::Name)? {
            Name::Move => {
                let [x, y] = fields(rest)?;
                Command::Move {
                    x: num(x)?,
                    y: num(y)?,
                }
            }
            Name::Button => {
                let [button, press] = fields(rest)?;
                Command::Button {
                    button: Button::numbered(num(button)?).ok_or(NoCommand::Field)?,
                    press: word(press)?,
                }
            }
            Name::ScrollPx => {
                let [dx, dy] = fields(rest)?;
                Command::ScrollPx {
                    dx: pixels(dx)?,
                    dy: pixels(dy)?,
                }
            }
            Name::Key => {
                let [keysym, act] = fields(rest)?;
                Command::Key {
                    keysym: num(keysym)?,
                    act: word(act)?,
                }
            }
            Name::ReleaseAll => {
                let [] = fields(rest)?;
                Command::ReleaseAll
            }
            Name::Clipboard => Command::Clipboard(rest.ok_or(NoCommand::Fields)?.to_string()),
            Name::Video => {
                let [on] = fields(rest)?;
                Command::Video(word(on)?)
            }
            Name::Audio => {
                let [on] = fields(rest)?;
                Command::Audio(word(on)?)
            }
            Name::Mic => {
                let [on] = fields(rest)?;
                Command::Mic(word(on)?)
            }
            Name::Resize => {
                let [w, h] = fields(rest)?;
                Command::Resize {
                    w: num(w)?,
                    h: num(h)?,
                }
            }
        })
    }
}

fn handle_text(d: &Daemon, c: &Arc<Client>, t: &str) {
    let command = match Command::parse(t) {
        Ok(command) => command,
        // What is no command is dropped: a newer page may send what this daemon does not
        // know.
        Err(why) => {
            log::debug!("session {}: a text message dropped: {why}", c.id);
            return;
        }
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
            match on {
                Switch::On => c.watch(),
                Switch::Off => c.stop_watching(),
            }
            d.hub.wake();
        }
        // Audio is sent a session that asks for it, where the daemon sends audio at all.
        Command::Audio(on) => d.hub.set_audio(
            c,
            match (on, d.audio) {
                (Switch::On, Audio::Sent) => Switch::On,
                (Switch::Off, Audio::Sent | Audio::Off) | (Switch::On, Audio::Off) => Switch::Off,
            },
        ),
        Command::Mic(on) => send_mic(
            d,
            match on {
                Switch::On => MicMsg::Start(c.id),
                Switch::Off => MicMsg::Stop(c.id),
            },
        ),
        // The desktop size this client's window wants (dynamic resolution). The newest ask
        // wins: it is the request until a newer one, and no ask clears another, so the
        // screen ends at the size asked last whatever the capture loop was doing. The
        // capture loop gives it to the screen, which fits it to a size it can take (each
        // platform has its own rule) and changes nothing if it has that size already.
        Command::Resize { w, h } => {
            *d.hub.resize_req.lock() = Some((w, h));
            d.hub.wake();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xorshift::Xorshift;

    /// The words a command begins with are the page's: `command` in web/src/wire.json has
    /// each of the daemon's names with its word, and no other.
    #[test]
    fn the_command_names_are_the_pages() {
        let wire: serde_json::Value =
            serde_json::from_str(include_str!("../../web/src/wire.json")).unwrap();
        let theirs: std::collections::HashMap<Name, String> =
            serde_json::from_value(wire["command"].clone()).unwrap();
        let ours = Name::ALL.iter().map(|n| (*n, n.word().to_string()));
        assert_eq!(theirs, ours.collect());
    }

    /// A command and the text that says it, generated: each name's, with fields of every
    /// range its places take.
    fn said(rng: &mut Xorshift) -> (Command, String) {
        fn pick<W: Word>(rng: &mut Xorshift) -> W {
            W::ALL[rng.below(W::ALL.len() as u64) as usize]
        }
        let name: Name = pick(rng);
        let (command, fields) = match name {
            Name::Move => {
                let (x, y) = (rng.next() as i16, rng.next() as i16);
                (Command::Move { x, y }, format!(",{x},{y}"))
            }
            Name::Button => {
                let button = Button::ALL[rng.below(Button::ALL.len() as u64) as usize];
                let press: Press = pick(rng);
                let fields = format!(",{},{}", button.number(), press.word());
                (Command::Button { button, press }, fields)
            }
            Name::ScrollPx => {
                // Tenths of a pixel either way, as the page writes them.
                let mut px = || (rng.below(20_000_001) as f64 - 10_000_000.0) / 10.0;
                let (dx, dy) = (px(), px());
                let scroll = Command::ScrollPx {
                    dx: Pixels::of(dx).expect("within the bound"),
                    dy: Pixels::of(dy).expect("within the bound"),
                };
                (scroll, format!(",{dx:.1},{dy:.1}"))
            }
            Name::Key => {
                let (keysym, act) = (rng.next() as u32, pick::<KeyAct>(rng));
                let fields = format!(",{keysym},{}", act.word());
                (Command::Key { keysym, act }, fields)
            }
            Name::ReleaseAll => (Command::ReleaseAll, String::new()),
            Name::Clipboard => {
                let text: String = (0..rng.below(12))
                    .map(|_| [',', 'a', '1', ' ', 'é'][rng.below(5) as usize])
                    .collect();
                (Command::Clipboard(text.clone()), format!(",{text}"))
            }
            Name::Video => {
                let on: Switch = pick(rng);
                (Command::Video(on), format!(",{}", on.word()))
            }
            Name::Audio => {
                let on: Switch = pick(rng);
                (Command::Audio(on), format!(",{}", on.word()))
            }
            Name::Mic => {
                let on: Switch = pick(rng);
                (Command::Mic(on), format!(",{}", on.word()))
            }
            Name::Resize => {
                let (w, h) = (rng.next() as u32, rng.next() as u32);
                (Command::Resize { w, h }, format!(",{w},{h}"))
            }
        };
        (command, format!("{}{fields}", name.word()))
    }

    /// A command reads as what was said, and what is no command is refused for what is
    /// wrong with it. Over generated commands of every name: the text reads back as the
    /// command; with a field more, or its last field cut off, it has the wrong fields
    /// (but a clipboard's text holds every comma, and is cut only with its comma); with
    /// its last field a word no place takes, it has a wrong field; and under a name no
    /// command has, it has no name.
    #[test]
    fn a_command_reads_as_it_was_said() {
        let mut rng = Xorshift::new(0xbb67_ae85_84ca_a73b);
        let mut names = std::collections::HashSet::new();
        for _ in 0..20_000 {
            let (command, text) = said(&mut rng);
            assert_eq!(Command::parse(&text).as_ref(), Ok(&command), "{text}");
            let more = Command::parse(&format!("{text},1"));
            let cut = Command::parse(match text.rsplit_once(',') {
                Some((before, _)) => before,
                None => "",
            });
            let other = text
                .rsplit_once(',')
                .map(|(before, _)| format!("{before},x"));
            let other = other.as_deref().map(Command::parse);
            match command {
                Command::Clipboard(said) => {
                    assert_eq!(more, Ok(Command::Clipboard(format!("{said},1"))));
                    let comma = text.find(',').expect("a clipboard has its comma");
                    assert_eq!(Command::parse(&text[..comma]), Err(NoCommand::Fields));
                }
                Command::ReleaseAll => {
                    assert_eq!(more, Err(NoCommand::Fields), "{text}");
                    assert_eq!(cut, Err(NoCommand::Name), "nothing is no command");
                }
                Command::Move { .. }
                | Command::Button { .. }
                | Command::ScrollPx { .. }
                | Command::Key { .. }
                | Command::Video(_)
                | Command::Audio(_)
                | Command::Mic(_)
                | Command::Resize { .. } => {
                    assert_eq!(more, Err(NoCommand::Fields), "{text}");
                    assert_eq!(cut, Err(NoCommand::Fields), "{text}");
                    assert_eq!(other, Some(Err(NoCommand::Field)), "{text}");
                }
            }
            assert_eq!(
                Command::parse(&format!("x{text}")),
                Err(NoCommand::Name),
                "{text}"
            );
            names.insert(text.split(',').next().map(str::to_string));
        }
        assert_eq!(names.len(), Name::ALL.len());
    }
}
