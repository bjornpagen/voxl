//! What the input thread is sent, the same on X, macOS and Windows: a viewer's input as
//! server.rs parsed it, so each number is already in the range the injector takes, and
//! the server's own requests. Each platform's `input::spawn` makes its injector and
//! starts the one input thread here, which injects these.

use crate::server::{Button, Pixels};
use std::sync::mpsc;

/// What a viewer's key did. Its own keyboard decides when a held key repeats.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum KeyAct {
    Down,
    /// The viewer's keyboard repeated the key it holds.
    Repeat,
    Up,
}

pub enum InputEvent {
    /// A desktop pixel, in X's own 16-bit coordinates; macOS's CGFloat and Windows's LONG
    /// hold every one.
    Move {
        x: i16,
        y: i16,
    },
    Button {
        button: Button,
        down: bool,
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
    Clipboard(crate::desktop::ClipContent),
    ReleaseAll,
    /// Finish queued input on its original geometry before selecting another display.
    #[cfg(target_os = "macos")]
    DisplayBarrier(std::sync::mpsc::Sender<()>),
    /// Release everything before exit; on X, also put back the keymap and key repeat
    /// voxl changed.
    Shutdown,
}

/// A platform's injector, as the input thread drives it.
pub trait Inject {
    /// Puts one event on the desktop.
    fn inject(&mut self, ev: InputEvent);
    /// Ends a batch: everything injected since the last one reaches the desktop.
    fn flush(&mut self);
}

/// Starts the input thread and returns its sender. `start` runs first on that thread and
/// makes the injector; the thread then injects every event it is sent until every
/// sender is gone.
pub fn spawn<I: Inject>(start: impl FnOnce() -> I + Send + 'static) -> mpsc::Sender<InputEvent> {
    let (tx, rx) = mpsc::channel::<InputEvent>();
    std::thread::Builder::new()
        .name("input".into())
        .spawn(move || run(&rx, &mut start()))
        .unwrap();
    tx
}

/// The input thread, a batch at a time. A batch begins with the next event to arrive and
/// takes every one waiting behind it, until none is; then it is flushed.
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
            | InputEvent::Shutdown => return Some(ev),
            #[cfg(target_os = "macos")]
            InputEvent::DisplayBarrier(_) => return Some(ev),
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
                down: r & 1 << 50 != 0,
            },
            7 => InputEvent::Key {
                keysym: (r >> 32) as u32 % 0x10000,
                act: [KeyAct::Down, KeyAct::Repeat, KeyAct::Up][(r >> 48) as usize % 3],
            },
            8 => InputEvent::Clipboard(crate::desktop::ClipContent::Text(format!("{x}"))),
            9 => InputEvent::Clipboard(crate::desktop::ClipContent::Image(vec![y as u8])),
            10 => InputEvent::ReleaseAll,
            _ => InputEvent::Shutdown,
        }
    }

    /// What an injector was given, as a test can compare it.
    fn said(ev: &InputEvent) -> String {
        match ev {
            InputEvent::Move { x, y } => format!("move {x} {y}"),
            InputEvent::Button { button, down } => format!("button {} {down}", button.number()),
            InputEvent::ScrollPx { dx, dy } => format!("scroll px {} {}", dx.get(), dy.get()),
            InputEvent::Key { keysym, act } => format!("key {keysym} {act:?}"),
            InputEvent::Clipboard(crate::desktop::ClipContent::Text(t)) => format!("text {t}"),
            InputEvent::Clipboard(crate::desktop::ClipContent::Image(png)) => {
                format!("image {png:?}")
            }
            InputEvent::ReleaseAll => "release all".into(),
            #[cfg(target_os = "macos")]
            InputEvent::DisplayBarrier(_) => "barrier".into(),
            InputEvent::Shutdown => "shutdown".into(),
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
}
