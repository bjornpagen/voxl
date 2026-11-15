//! What the input thread is sent, the same on X, macOS and Windows: a viewer's input as
//! server.rs parsed it, so each number is already in the range the injector takes, and
//! the server's own requests. Each platform's `input::spawn` injects these.

use crate::server::{Button, Pixels};

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
