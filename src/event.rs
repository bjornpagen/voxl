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
    use std::cell::RefCell;
    use std::rc::Rc;

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

    fn xorshift(seed: &mut u64) -> u64 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        *seed
    }

    /// Of each run of moves only the last goes out, and every other event goes out as it
    /// came: 2,000 streams of up to 40 events against a model that cuts the stream into
    /// runs.
    #[test]
    fn of_a_run_of_moves_only_the_last_goes_out() {
        let mut seed = 0x2545_F491_4F6C_DD1D;
        let mut dropped = 0;
        for _ in 0..2_000 {
            let n = xorshift(&mut seed) % 40;
            let rs: Vec<u64> = (0..n).map(|_| xorshift(&mut seed)).collect();
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

    /// The desktop and the viewers, as one thread can play them: everything the injector
    /// did, in order, and, each time it does something, a few more events sent to it,
    /// so that some arrive while a batch is injected and some after it is flushed.
    struct Desk {
        did: Vec<String>,
        tx: Option<mpsc::Sender<InputEvent>>,
        seed: u64,
        left: usize,
    }

    impl Desk {
        /// Sends none to three events, or at least one when `flushed` (the queue is then
        /// empty, and the thread would wait for ever); with none left, its sender goes.
        fn send(&mut self, flushed: bool) {
            let n = match (xorshift(&mut self.seed) % 4, flushed) {
                (0, true) => 1,
                (n, _) => n as usize,
            };
            for _ in 0..n.min(self.left) {
                let ev = event(xorshift(&mut self.seed));
                self.tx.as_ref().expect("a sender").send(ev).unwrap();
            }
            self.left -= n.min(self.left);
            if self.left == 0 {
                self.tx = None;
            }
        }

        fn did(&mut self, what: String) {
            let flushed = what == "flush";
            self.did.push(what);
            self.send(flushed);
        }
    }

    /// X's connection, as far as the input thread uses it.
    struct Conn(Rc<RefCell<Desk>>);

    impl Conn {
        fn flush(&self) -> Result<(), ()> {
            self.0.borrow_mut().did("flush".into());
            Ok(())
        }
    }

    struct Mock {
        conn: Conn,
    }

    impl Mock {
        fn handle(&mut self, ev: InputEvent) {
            self.conn.0.borrow_mut().did(said(&ev));
        }
    }

    impl Inject for Mock {
        fn inject(&mut self, ev: InputEvent) {
            self.handle(ev);
        }

        fn flush(&mut self) {
            let _ = self.conn.flush();
        }
    }

    /// The input thread's loop before this commit, as src/input.rs had it, `Injector` read
    /// as `Mock`; macOS and Windows had it without the flush, which their `Inject::flush`
    /// does nothing for.
    fn old(rx: mpsc::Receiver<InputEvent>, mut inj: Mock) {
        while let Ok(first) = rx.recv() {
            let mut pending_move: Option<(i16, i16)> = None;
            let handle = |inj: &mut Mock, ev: InputEvent, pm: &mut Option<(i16, i16)>| match ev {
                InputEvent::Move { x, y } => *pm = Some((x, y)),
                other => {
                    if let Some((x, y)) = pm.take() {
                        inj.handle(InputEvent::Move { x, y });
                    }
                    inj.handle(other);
                }
            };
            handle(&mut inj, first, &mut pending_move);
            // Coalesce whatever queued up meanwhile; moves collapse to the latest.
            while let Ok(ev) = rx.try_recv() {
                handle(&mut inj, ev, &mut pending_move);
            }
            if let Some((x, y)) = pending_move {
                inj.handle(InputEvent::Move { x, y });
            }
            let _ = inj.conn.flush();
        }
    }

    /// What the injector did, with the old loop or the new one, for one seed.
    fn trace(seed: u64, with_old: bool) -> Vec<String> {
        let (tx, rx) = mpsc::channel();
        let desk = Rc::new(RefCell::new(Desk {
            did: vec![],
            tx: Some(tx),
            seed,
            left: 1 + (seed % 60) as usize,
        }));
        desk.borrow_mut().send(true);
        let mut inj = Mock {
            conn: Conn(desk.clone()),
        };
        if with_old {
            old(rx, inj);
        } else {
            run(&rx, &mut inj);
        }
        let did = std::mem::take(&mut desk.borrow_mut().did);
        did
    }

    /// The one loop does what each platform's own did: over 5,000 seeds, each a stream of
    /// up to 60 events sent while the thread works, the injector is given the same events
    /// and flushed at the same points, in the same order, by the old loop and the new.
    #[test]
    fn the_input_thread_injects_as_it_did() {
        let (mut batches, mut injected) = (0, 0);
        for seed in 1..=5_000u64 {
            let (was, is) = (trace(seed, true), trace(seed, false));
            assert_eq!(was, is, "seed {seed}");
            batches += is.iter().filter(|s| *s == "flush").count();
            injected += is.len();
        }
        // Each seed sends 1 + seed % 60 events, 152,120 in all, half of them moves. Many
        // batches, many with more than one event, and many moves another replaced.
        let sent: usize = (1..=5_000).map(|seed| 1 + seed % 60).sum();
        injected -= batches;
        assert!(
            batches >= 10_000 && injected - batches >= 10_000 && sent - injected >= 10_000,
            "{sent} sent, {injected} injected in {batches} batches"
        );
    }
}
