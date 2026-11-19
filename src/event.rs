//! What a viewer sends the desktop: the input thread's events, a viewer's input as the
//! session's control stream parsed it, so each number is already in the range the
//! injector takes, and the daemon's own requests (`input::spawn` makes the injector and
//! starts the one input thread here, which injects these); the microphone's messages; and
//! the type bytes of the binary messages between the page and the daemon.

use std::sync::mpsc;

/// A clipboard's contents, as a viewer and the desktop exchange them.
pub enum ClipContent {
    Text(String),
    /// PNG bytes.
    Image(Vec<u8>),
}

/// The first byte of a binary message the daemon sends the page: what it is. `bin` in
/// web/src/wire.json, the page's copy, which a test checks against these.
#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(u8)]
pub enum Bin {
    /// The cursor's image.
    Cursor = 0x03,
    /// The desktop's clipboard image (PNG).
    ClipImage = 0x04,
}

/// The first byte of a binary message the page sends the daemon: what it is, read by
/// looking it up (`wire::Byte`). `bin` in web/src/wire.json, as for [`Bin`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BinUp {
    /// A clipboard image pasted on the page (PNG).
    ClipImage,
    /// A microphone Opus packet.
    MicOpus,
}

impl crate::wire::Byte for BinUp {
    const ALL: &'static [BinUp] = &[BinUp::ClipImage, BinUp::MicOpus];

    fn byte(self) -> u8 {
        match self {
            BinUp::ClipImage => 0x11,
            BinUp::MicOpus => 0x12,
        }
    }
}

/// What the microphone's thread is sent: a session takes the microphone, sends it audio,
/// and lets it go.
pub enum MicMsg {
    Start(u64),
    Opus(u64, bytes::Bytes),
    Stop(u64),
}

/// A closed vocabulary of a command's field: its members, and the word a command says each
/// by. A field is read by looking it up here, so a word that is no member's is no field.
pub(crate) trait Word: Copy + 'static {
    const ALL: &'static [Self];

    fn word(self) -> &'static str;

    /// The member `field` says, if it says one.
    fn of(field: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|w| w.word() == field)
    }
}

/// On or off, as a command says it: `1` or `0`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Switch {
    On,
    Off,
}

impl Word for Switch {
    const ALL: &'static [Switch] = &[Switch::On, Switch::Off];

    fn word(self) -> &'static str {
        match self {
            Switch::On => "1",
            Switch::Off => "0",
        }
    }
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

    /// The button X numbers `number`, if the pointer has it.
    pub(crate) fn numbered(number: u8) -> Option<Button> {
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

/// As a command's field says it: `1` down, `0` up.
impl Word for Press {
    const ALL: &'static [Press] = &[Press::Down, Press::Up];

    fn word(self) -> &'static str {
        match self {
            Press::Down => "1",
            Press::Up => "0",
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
    /// `px` pixels, if that is finite and within the bound.
    pub(crate) fn of(px: f64) -> Option<Pixels> {
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

/// As a command's field says it: `1` down, `2` repeat, `0` up.
impl Word for KeyAct {
    const ALL: &'static [KeyAct] = &[KeyAct::Down, KeyAct::Repeat, KeyAct::Up];

    fn word(self) -> &'static str {
        match self {
            KeyAct::Down => "1",
            KeyAct::Repeat => "2",
            KeyAct::Up => "0",
        }
    }
}

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
}

/// A platform's injector, as the input thread drives it.
pub trait Inject {
    /// Puts one event on the desktop.
    fn inject(&mut self, ev: InputEvent);
    /// Ends a batch: everything injected since the last one reaches the desktop.
    fn flush(&mut self);
    /// The input has ended: let go of everything held down, and put back what voxl
    /// changed on the desktop.
    fn finish(&mut self);
}

/// The input thread, as the daemon holds it: the one sender of its events, and the word
/// that it has finished. Dropping the sender ends the input; the thread then finishes
/// (`Inject::finish`) and says so.
pub struct Input {
    pub events: mpsc::Sender<InputEvent>,
    pub finished: tokio::sync::oneshot::Receiver<()>,
}

/// Starts the input thread. `start` runs first on that thread and makes the injector; the
/// thread then injects every event it is sent until every sender is gone, and finishes. A
/// thread that cannot be started is the error.
pub fn spawn<I: Inject>(start: impl FnOnce() -> I + Send + 'static) -> Result<Input, String> {
    let (events, rx) = mpsc::channel::<InputEvent>();
    let (done, finished) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("input".into())
        .spawn(move || {
            run(&rx, &mut start());
            // The daemon's stop waits on this; gone, it waits for nothing.
            if done.send(()).is_err() {
                log::info!("the daemon stopped before the input was let go");
            }
        })
        .map_err(|e| format!("no input thread: {e}"))?;
    Ok(Input { events, finished })
}

/// The input thread, a batch at a time. A batch begins with the next event to arrive and
/// takes every one waiting behind it, until none is; then it is flushed. Once every sender
/// is gone, the injector finishes.
fn run(rx: &mpsc::Receiver<InputEvent>, inj: &mut impl Inject) {
    while let Ok(first) = rx.recv() {
        for ev in coalesce(std::iter::once(first).chain(rx.try_iter())) {
            inj.inject(ev);
        }
        inj.flush();
    }
    inj.finish();
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
            | InputEvent::ReleaseAll => return Some(ev),
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
        match r % 11 {
            0..=5 => InputEvent::Move { x, y },
            6 => InputEvent::Button {
                button: Button::ALL[(r >> 40) as usize % Button::ALL.len()],
                press: Press::ALL[(r >> 50) as usize % 2],
            },
            7 => InputEvent::Key {
                keysym: (r >> 32) as u32 % 0x10000,
                act: KeyAct::ALL[(r >> 48) as usize % 3],
            },
            8 => InputEvent::Clipboard(ClipContent::Text(format!("{x}"))),
            9 => InputEvent::Clipboard(ClipContent::Image(vec![y as u8])),
            10.. => InputEvent::ReleaseAll,
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

    /// What an injector was given, in order, as a test reads it.
    struct Recorder(std::sync::Arc<parking_lot::Mutex<Vec<String>>>);

    impl Inject for Recorder {
        fn inject(&mut self, ev: InputEvent) {
            self.0.lock().push(said(&ev));
        }
        fn flush(&mut self) {
            self.0.lock().push("flush".into());
        }
        fn finish(&mut self) {
            self.0.lock().push("finish".into());
        }
    }

    /// The input thread injects what it is sent and finishes once its sender is gone, and
    /// says so then. Over 300 generated streams of up to 60 events, sent from another thread
    /// in bursts, and then the sender dropped: once the thread says it finished, the injector
    /// was given every event but a move, in the order sent; the last move sent, if any; a
    /// flush after the last event; and then `finish`, once, last.
    #[test]
    fn the_input_finishes_once_its_sender_is_gone() {
        let mut rng = crate::xorshift::Xorshift::new(0x428a_2f98_d728_ae22);
        for _ in 0..300 {
            let given = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
            let recorder = Recorder(given.clone());
            let Input { events, finished } = spawn(move || recorder).expect("a thread");
            let rs: Vec<u64> = (0..rng.below(60)).map(|_| rng.next()).collect();
            let sent: Vec<String> = rs.iter().map(|&r| said(&event(r))).collect();
            for (i, &r) in rs.iter().enumerate() {
                events.send(event(r)).expect("the thread runs");
                if i % 7 == 0 {
                    std::thread::yield_now();
                }
            }
            drop(events);
            finished
                .blocking_recv()
                .expect("the thread says it finished");
            let given = given.lock().clone();
            let is_move = |s: &String| s.starts_with("move");
            let injected: Vec<&String> = given.iter().filter(|s| *s != "flush").collect();
            let (last, before) = injected.split_last().expect("it finished");
            assert_eq!(*last, "finish");
            let others = |l: &[&String]| -> Vec<String> {
                l.iter()
                    .filter(|s| !is_move(s))
                    .map(|s| s.to_string())
                    .collect()
            };
            let sent_others: Vec<String> = sent.iter().filter(|s| !is_move(s)).cloned().collect();
            assert_eq!(others(before), sent_others);
            assert!(before.iter().all(|s| sent.contains(s)), "{given:?}");
            assert_eq!(
                before.iter().rev().find(|s| is_move(s)).map(|s| s.as_str()),
                sent.iter().rev().find(|s| is_move(s)).map(String::as_str),
                "{given:?}"
            );
            let flushed = given.len() < 2 || given[given.len() - 2] == "flush";
            assert!(sent.is_empty() || flushed, "{given:?}");
            assert_eq!(given.iter().filter(|s| *s == "finish").count(), 1);
        }
    }

    /// The type bytes are the page's: `bin` in web/src/wire.json names each the same; and
    /// a byte reads as a kind the page sends exactly when it is one's.
    #[test]
    fn the_type_bytes_are_the_pages() {
        use crate::wire::Byte;
        let wire: serde_json::Value =
            serde_json::from_str(include_str!("../web/src/wire.json")).unwrap();
        for (name, bin) in [("cursor", Bin::Cursor), ("clipImage", Bin::ClipImage)] {
            assert_eq!(wire["bin"][name], bin as u8, "{name}");
        }
        for (name, bin) in [
            ("clipImageUp", BinUp::ClipImage),
            ("micOpus", BinUp::MicOpus),
        ] {
            assert_eq!(wire["bin"][name], bin.byte(), "{name}");
        }
        for tag in 0..=u8::MAX {
            let read = BinUp::of(tag);
            let theirs = ["clipImageUp", "micOpus"].map(|name| wire["bin"][name] == tag);
            assert_eq!(read.is_some(), theirs.contains(&true), "{tag}");
            assert_eq!(read.map(BinUp::byte), read.map(|_| tag), "{tag}");
        }
    }
}
