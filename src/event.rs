//! What a viewer sends the desktop: the input thread's events, a viewer's input as the
//! session's control stream parsed it, so each number is already in the range the
//! injector takes, and the daemon's own requests (`input::spawn` makes the injector and
//! starts the one input thread here, which injects these); the microphone's messages; and
//! the type bytes of the binary messages between the page and the daemon.

#[cfg(target_os = "linux")]
use std::sync::mpsc;

/// A clipboard's contents, as a viewer and the desktop exchange them.
pub enum ClipContent {
    Text(String),
    /// PNG bytes.
    Image(Vec<u8>),
}

/// The first byte of a binary message between the page and the daemon: what it is.
/// `bin` in web/src/wire.json, the page's copy, which a test checks against these.
#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(u8)]
pub enum Bin {
    /// Down: the cursor's image.
    Cursor = 0x03,
    /// Down: the desktop's clipboard image (PNG).
    ClipImage = 0x04,
    /// Up: a clipboard image pasted on the page (PNG).
    ClipImageUp = 0x11,
    /// Up: a microphone Opus packet.
    MicOpus = 0x12,
}

impl Bin {
    /// The kind a page's binary message is, by its first byte, if it is one the page sends.
    pub fn up(tag: u8) -> Option<Bin> {
        [Bin::ClipImageUp, Bin::MicOpus]
            .into_iter()
            .find(|b| *b as u8 == tag)
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // read by the Linux-only threads
/// What the microphone's thread is sent: a session takes the microphone, sends it audio,
/// and lets it go.
pub enum MicMsg {
    Start(u64),
    Opus(u64, bytes::Bytes),
    Stop(u64),
}

/// A button the viewer's pointer has, which a command names by X's number for it.
/// pointer.ts sends the browser's five (main, middle, secondary, back, forward) as 1, 2,
/// 3, 8 and 9. X has more (4 to 7 are its wheel, which `sp` turns) and no button 0.
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

    pub(crate) fn parse(field: &str) -> Option<Button> {
        let number: u8 = field.trim().parse().ok()?;
        Button::ALL.into_iter().find(|b| b.number() == number)
    }
}

/// Where a button of the viewer's pointer is: what a command says it went to, and what an
/// injector holds of each.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Press {
    Down,
    Up,
}

impl Press {
    /// From a command's field: `1` down, `0` up.
    pub(crate) fn parse(field: &str) -> Option<Press> {
        match field {
            "1" => Some(Press::Down),
            "0" => Some(Press::Up),
            _ => None,
        }
    }
}

/// The longest pixel scroll one message carries, either way on either axis: 2^20 desktop
/// pixels, about a million.
///
/// Who needs it: an input thread adds each pixel scroll to the remainder it keeps between
/// messages, scaled to wheel units (on X 120 a notch over
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
pub(crate) const MAX_SCROLL_PX: f64 = 1_048_576.0;

/// A distance in desktop pixels, finite and at most `MAX_SCROLL_PX` either way. Rust's
/// `f64` parser reads `NaN` and `inf` too (JavaScript's `toFixed` writes them as `NaN`
/// and `Infinity`); added to the input thread's scroll remainder, either one stays in it,
/// as a finite distance past the bound does once scaled, and every pixel scroll after it
/// moves nothing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pixels(f64);

impl Pixels {
    pub(crate) fn parse(field: &str) -> Option<Pixels> {
        let px: f64 = field.trim().parse().ok()?;
        (px.abs() <= MAX_SCROLL_PX).then_some(Pixels(px))
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

/// What a viewer's key did. Its own keyboard decides when a held key repeats.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum KeyAct {
    Down,
    /// The viewer's keyboard repeated the key it holds.
    Repeat,
    Up,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // read by the Linux-only threads
pub enum InputEvent {
    /// A desktop pixel, in X's own 16-bit coordinates; macOS's CGFloat holds
    /// every one.
    Move {
        x: i16,
        y: i16,
    },
    Button {
        button: Button,
        press: Press,
    },
    /// Precise scroll in desktop pixels (trackpads, smooth wheels).
    ScrollPx {
        dx: Pixels,
        dy: Pixels,
    },
    Key {
        keysym: u32,
        act: KeyAct,
    },
    /// Remote clipboard update, ordered with the key events around it.
    Clipboard(ClipContent),
    ReleaseAll,
    /// Release everything before exit, and put back the keymap and key repeat voxl
    /// changed; then say so on the channel, which the daemon's stop waits on.
    Shutdown(tokio::sync::oneshot::Sender<()>),
}

#[cfg(target_os = "linux")]
/// A platform's injector, as the input thread drives it.
pub trait Inject {
    /// Puts one event on the desktop.
    fn inject(&mut self, ev: InputEvent);
    /// Ends a batch: everything injected since the last one reaches the desktop.
    fn flush(&mut self);
}

/// Starts the input thread and returns its sender. `start` runs first on that thread and
/// makes the injector; the thread then injects every event it is sent until every
/// sender is gone. A thread that cannot be started is the error.
#[cfg(target_os = "linux")]
pub fn spawn<I: Inject>(
    start: impl FnOnce() -> I + Send + 'static,
) -> Result<mpsc::Sender<InputEvent>, String> {
    let (tx, rx) = mpsc::channel::<InputEvent>();
    std::thread::Builder::new()
        .name("input".into())
        .spawn(move || run(&rx, &mut start()))
        .map_err(|e| format!("no input thread: {e}"))?;
    Ok(tx)
}

/// The input thread, a batch at a time. A batch begins with the next event to arrive and
/// takes every one waiting behind it, until none is; then it is flushed.
#[cfg(target_os = "linux")]
fn run(rx: &mpsc::Receiver<InputEvent>, inj: &mut impl Inject) {
    while let Ok(first) = rx.recv() {
        for ev in coalesce(std::iter::once(first).chain(rx.try_iter())) {
            inj.inject(ev);
        }
        inj.flush();
    }
}

/// `events` without each move that another move follows at once: the pointer goes where
/// the last of a run of moves puts it. An event is taken from `events` only when the one
/// before it has gone out, or to see whether a move is the last of its run.
fn coalesce(events: impl Iterator<Item = InputEvent>) -> impl Iterator<Item = InputEvent> {
    let mut events = events.peekable();
    std::iter::from_fn(move || loop {
        let ev = events.next()?;
        match ev {
            InputEvent::Move { .. } if matches!(events.peek(), Some(InputEvent::Move { .. })) => {}
            InputEvent::Move { .. }
            | InputEvent::Button { .. }
            | InputEvent::ScrollPx { .. }
            | InputEvent::Key { .. }
            | InputEvent::Clipboard(_)
            | InputEvent::ReleaseAll
            | InputEvent::Shutdown(_) => return Some(ev),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An event from a random number: half of them moves, so that runs of moves are common,
    /// and the rest every other kind a test can make (a pixel scroll is made only by
    /// server.rs's parser).
    fn event(r: u64) -> InputEvent {
        let (x, y) = ((r >> 8) as i16, (r >> 24) as i16);
        match r % 12 {
            0..=5 => InputEvent::Move { x, y },
            6 => InputEvent::Button {
                button: Button::ALL[(r >> 40) as usize % Button::ALL.len()],
                press: [Press::Up, Press::Down][(r >> 50) as usize % 2],
            },
            7 => InputEvent::Key {
                keysym: (r >> 32) as u32 % 0x10000,
                act: [KeyAct::Down, KeyAct::Repeat, KeyAct::Up][(r >> 48) as usize % 3],
            },
            8 => InputEvent::Clipboard(ClipContent::Text(format!("{x}"))),
            9 => InputEvent::Clipboard(ClipContent::Image(vec![y as u8])),
            10 => InputEvent::ReleaseAll,
            _ => InputEvent::Shutdown(tokio::sync::oneshot::channel().0),
        }
    }

    /// What an injector was given, as a test can compare it.
    fn said(ev: &InputEvent) -> String {
        match ev {
            InputEvent::Move { x, y } => format!("move {x} {y}"),
            InputEvent::Button { button, press } => format!("button {} {press:?}", button.number()),
            InputEvent::ScrollPx { dx, dy } => format!("scroll px {} {}", dx.get(), dy.get()),
            InputEvent::Key { keysym, act } => format!("key {keysym} {act:?}"),
            InputEvent::Clipboard(ClipContent::Text(t)) => format!("text {t}"),
            InputEvent::Clipboard(ClipContent::Image(png)) => {
                format!("image {png:?}")
            }
            InputEvent::ReleaseAll => "release all".into(),
            InputEvent::Shutdown(_) => "shutdown".into(),
        }
    }

    /// Of each run of moves only the last goes out, and every other event goes out as it
    /// came: 2,000 streams of up to 40 events against a model that cuts the stream into
    /// runs.
    #[test]
    fn of_a_run_of_moves_only_the_last_goes_out() {
        let mut rng = crate::xorshift::Xorshift::new(0x2545_F491_4F6C_DD1D);
        let mut dropped = 0;
        for _ in 0..2_000 {
            let n = rng.below(40);
            let rs: Vec<u64> = (0..n).map(|_| rng.next()).collect();
            let sent: Vec<String> = rs.iter().map(|&r| said(&event(r))).collect();
            let moves = |a: &String, b: &String| a.starts_with("move") && b.starts_with("move");
            let model: Vec<&String> = sent
                .chunk_by(moves)
                .flat_map(|run| {
                    if run[0].starts_with("move") {
                        &run[run.len() - 1..]
                    } else {
                        run
                    }
                })
                .collect();
            let out: Vec<String> = coalesce(rs.iter().map(|&r| event(r)))
                .map(|ev| said(&ev))
                .collect();
            assert_eq!(out.iter().collect::<Vec<_>>(), model, "{sent:?}");
            dropped += sent.len() - out.len();
        }
        assert!(dropped >= 5_000, "{dropped} moves replaced");
    }

    /// The type bytes are the page's: `bin` in web/src/wire.json names each the same.
    #[test]
    fn the_type_bytes_are_the_pages() {
        let wire: serde_json::Value =
            serde_json::from_str(include_str!("../web/src/wire.json")).unwrap();
        for (name, bin) in [
            ("cursor", Bin::Cursor),
            ("clipImage", Bin::ClipImage),
            ("clipImageUp", Bin::ClipImageUp),
            ("micOpus", Bin::MicOpus),
        ] {
            assert_eq!(wire["bin"][name], bin as u8, "{name}");
        }
        for tag in 0..=u8::MAX {
            let up = [Bin::ClipImageUp, Bin::MicOpus].map(|b| b as u8);
            assert_eq!(Bin::up(tag).is_some(), up.contains(&tag), "{tag}");
        }
    }
}
