//! A session's lane as the media producers use it: frames in, and on the lane's socket only as
//! fast as the socket accepts them.
//!
//! The lane is one stream through the transport's proxy (a QUIC stream under Hysteria 2 or
//! naive over HTTP/3, TCP under naive over HTTP/2), and the rate is the transport's: below the
//! daemon's socket every write waits on the transport's own congestion and flow control, and
//! the relay reads the stream only as fast as the page takes each frame, so nothing queues
//! there beyond the transport's window and the socket buffers. A frame waits in the lane's
//! one queue until the socket takes it. The queue is the capture gate ([`Sender::waits`]): a
//! capture may start only while none of the daemon's bytes wait for the socket. When the link
//! or the page slows, captures are skipped and the newest screen goes next: dropping beats
//! lag, at the source.
//!
//! A lane whose socket breaks ends: what it held is dropped, and it gates nothing. The next
//! lane the relay attaches starts its viewer over on a keyframe (`hub::Client::attach`), since
//! bytes the broken socket accepted may never have reached the relay.
//!
//! Everything here is synchronous, under the session's lock in the hub; the lane has a writer
//! task (`lane.rs`) that empties the queue into the socket.

use crate::hub::Out;
use crate::wire::{Capability, Frame, Track};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;

/// A viewer's way out, as `voxl stats` says it.
#[derive(Debug, serde::Serialize)]
#[serde(tag = "way", rename_all = "snake_case")]
pub enum Way {
    /// A session no lane has attached to yet.
    Unattached,
    /// Its lane: the bytes waiting for its socket.
    Lane {
        waiting_bytes: usize,
    },
    Recording,
    /// A session whose control connection closed.
    Closed,
}

/// A viewer's way out: a session's lane, or a recording in this process.
pub struct Sender {
    state: State,
}

enum State {
    /// Its control connection is open: the capability its lane attaches with, and the lane
    /// attached last, if any.
    Open {
        capability: Capability,
        lane: Option<Arc<Queue>>,
    },
    /// Its control connection closed: the capability is forgotten, and no lane is sent
    /// another byte.
    Closed,
    /// A recording (`voxl record`): each update goes to it whole, its header and its VP9, on
    /// its channel. It has no capability and no lane.
    Recording(tokio::sync::mpsc::UnboundedSender<Out>),
}

/// What wakes the capture loop: called where a lane's queue empties or ends, the places its
/// gate opens.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

/// One lane's frames waiting for its socket, written by its writer as the socket takes them.
/// Its writer writes only under the queue's lock and only while it is open, so once it ends
/// (`end`), not one byte more leaves on the lane.
pub struct Queue {
    state: Mutex<Waiting>,
    wake: tokio::sync::Notify,
}

pub(super) struct Waiting {
    pub(super) phase: Phase,
    /// The frames waiting, each as the lane carries it.
    pub(super) messages: VecDeque<Vec<u8>>,
    /// Bytes of the front message the socket has taken.
    pub(super) written: usize,
    /// The bytes waiting.
    waiting: usize,
    /// What to wake when the queue empties or ends.
    wake: Wake,
    /// Buffers of messages that left, for the next ones: a steady stream allocates none.
    spare: Vec<Vec<u8>>,
}

/// Whether a lane takes frames: open until its session closes, a newer lane replaces it, or
/// its socket breaks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Phase {
    Open,
    Ended,
}

impl Queue {
    /// A lane's queue, which wakes `wake` when it empties or ends.
    pub(crate) fn new(wake: Wake) -> Arc<Queue> {
        Arc::new(Queue {
            state: Mutex::new(Waiting {
                phase: Phase::Open,
                messages: VecDeque::new(),
                written: 0,
                waiting: 0,
                wake,
                spare: Vec::new(),
            }),
            wake: tokio::sync::Notify::new(),
        })
    }

    /// Its state, locked: its writer writes under this lock.
    pub(super) fn lock(&self) -> parking_lot::MutexGuard<'_, Waiting> {
        self.state.lock()
    }

    /// Wakes its writer: a frame came, or it ended.
    pub(super) async fn woken(&self) {
        self.wake.notified().await
    }

    /// Ends it: nothing more leaves, what waits is dropped, and its writer ends.
    pub(super) fn end(&self) {
        self.state.lock().end();
        self.wake.notify_one();
    }

    /// Queues a frame; an ended lane takes nothing.
    fn push(&self, track: Track, bytes: &[u8]) {
        let mut w = self.state.lock();
        if w.phase == Phase::Ended {
            return;
        }
        let mut buf = w.spare.pop().unwrap_or_default();
        buf.clear();
        Frame::encode(track, bytes, &mut buf);
        w.waiting += buf.len();
        w.messages.push_back(buf);
        drop(w);
        self.wake.notify_one();
    }

    /// The bytes waiting for its socket while it is open: an ended lane's socket takes
    /// nothing more, and gates nothing.
    fn waiting(&self) -> usize {
        let w = self.state.lock();
        match w.phase {
            Phase::Open => w.waiting,
            Phase::Ended => 0,
        }
    }
}

impl Waiting {
    /// The front message left, all of it: its buffer serves the next. The bytes waiting fall
    /// here and nowhere else, so here the capture loop is woken when they reach none.
    pub(super) fn sent(&mut self) {
        if let Some(m) = self.messages.pop_front() {
            self.waiting -= m.len();
            self.written = 0;
            self.spare.push(m);
            if self.waiting == 0 {
                (self.wake)();
            }
        }
    }

    /// The lane ends (its session closed, a newer lane replaced it, or its socket broke for
    /// good): it sends nothing more, what waits is dropped, and the capture loop is woken,
    /// since the lane gates nothing now.
    pub(super) fn end(&mut self) {
        if self.phase == Phase::Ended {
            return;
        }
        self.phase = Phase::Ended;
        self.messages.clear();
        self.waiting = 0;
        self.written = 0;
        (self.wake)();
    }
}

impl Sender {
    /// A session's, with the capability its lane attaches with.
    pub fn new(capability: Capability) -> Sender {
        Sender {
            state: State::Open {
                capability,
                lane: None,
            },
        }
    }

    /// A recording's, its updates sent on `to`.
    pub fn recording(to: tokio::sync::mpsc::UnboundedSender<Out>) -> Sender {
        Sender {
            state: State::Recording(to),
        }
    }

    /// Whether `presented` is its capability: never once it has closed, and never a
    /// recording's, which has none.
    pub fn is(&self, presented: &Capability) -> bool {
        match &self.state {
            State::Open { capability, .. } => capability.matches(presented),
            State::Closed | State::Recording(_) => false,
        }
    }

    /// Its control connection closed: the capability is dropped (and zeroed), and its lane
    /// ends before this returns.
    pub fn close(&mut self) {
        if let State::Open { lane: Some(q), .. } = std::mem::replace(&mut self.state, State::Closed)
        {
            q.end();
        }
    }

    /// Whether it has a way to be sent frames on: a lane whose socket takes them, or a
    /// recording.
    pub fn attached(&self) -> bool {
        match &self.state {
            State::Open { lane: Some(q), .. } => q.lock().phase == Phase::Open,
            State::Recording(_) => true,
            State::Open { lane: None, .. } | State::Closed => false,
        }
    }

    /// A lane attached, its queue `queue`: it replaces the lane before, which ends. A closed
    /// session's, or a recording's, is refused. Whether it was taken.
    pub fn attach(&mut self, queue: Arc<Queue>) -> bool {
        let State::Open { lane, .. } = &mut self.state else {
            return false;
        };
        if let Some(old) = lane.replace(queue) {
            old.end();
        }
        true
    }

    /// Whether any of its bytes wait for its lane's socket: the capture gate, shut while they
    /// do.
    pub fn waits(&self) -> bool {
        match &self.state {
            State::Open { lane: Some(q), .. } => q.waiting() > 0,
            State::Open { lane: None, .. } | State::Closed | State::Recording(_) => false,
        }
    }

    /// What `voxl stats` says of it.
    pub fn stats(&self) -> Way {
        match &self.state {
            State::Open { lane: None, .. } => Way::Unattached,
            State::Open { lane: Some(q), .. } => Way::Lane {
                waiting_bytes: q.waiting(),
            },
            State::Recording(_) => Way::Recording,
            State::Closed => Way::Closed,
        }
    }

    /// A video update, its header and its VP9: onto the lane, or to a recording as it is. A
    /// recording whose recorder has gone takes nothing: its viewer is removed with it.
    pub fn video(&mut self, bytes: &bytes::Bytes) {
        match &self.state {
            State::Recording(to) => {
                if to.send(Out::Bin(bytes.clone())).is_err() {
                    log::debug!("a recording ended before its viewer left");
                }
            }
            State::Open { lane: Some(q), .. } => q.push(Track::Video, bytes),
            State::Open { lane: None, .. } | State::Closed => {}
        }
    }

    /// An Opus packet, `silent` frames of silence after the one before: onto the lane as it
    /// comes, or to a recording as it is, with the silence it follows.
    pub fn audio(&mut self, packet: &[u8], silent: u32) {
        match &self.state {
            State::Recording(to) => {
                let packet = bytes::Bytes::copy_from_slice(packet);
                if to.send(Out::Audio { packet, silent }).is_err() {
                    log::debug!("a recording ended before its viewer left");
                }
            }
            State::Open { lane: Some(q), .. } => q.push(Track::Audio, packet),
            State::Open { lane: None, .. } | State::Closed => {}
        }
    }
}

/// The send buffer the daemon gives the lane's socket: the least, a byte asked for, which the
/// kernel raises to its own floor (Linux's `SOCK_MIN_SNDBUF`, about 4.6 KB). The socket is the
/// daemon's to the proxy on this machine: what it holds past acceptance waits where the gate
/// does not see it, so with the least every backlog is in the one queue the daemon reads
/// ([`Queue`]), and what feeds the path is the transport's own window. Linux's alone: macOS
/// has no such floor (a unix socket given a byte takes one a write), and the daemon runs only
/// on Linux.
///
/// @limitKind published
/// @owner the daemon's lane socket
/// @unit bytes
/// @scope `SO_SNDBUF` on the lane's socket
/// @constraint the kernel's floor of the option: whatever is asked under it is raised to it
/// @source https://man7.org/linux/man-pages/man7/socket.7.html "The minimum (doubled) value
/// for this option is 2048."
/// @verified 2026-11-18
#[cfg(target_os = "linux")]
pub const SEND_BUFFER: usize = 1;

#[cfg(test)]
impl Queue {
    /// The front message as the lane's writer gives it to its socket, taken out: its bytes.
    pub(crate) fn take(&self) -> Option<Vec<u8>> {
        let mut w = self.lock();
        let m = w.messages.front()?.clone();
        w.sent();
        Some(m)
    }

    /// The bytes of the front message, if one waits.
    pub(crate) fn front_len(&self) -> Option<usize> {
        self.lock().messages.front().map(Vec::len)
    }

    /// The video updates waiting.
    pub(crate) fn updates_waiting(&self) -> usize {
        self.lock()
            .messages
            .iter()
            .filter(|m| {
                Frame::decode(&m[crate::wire::PREFIX..]).is_ok_and(|f| f.track == Track::Video)
            })
            .count()
    }

    /// The bytes waiting.
    pub(crate) fn waiting_bytes(&self) -> usize {
        self.lock().waiting
    }

    /// Its socket broke, as a writer finds it.
    pub(crate) fn lose(&self) {
        self.lock().end();
    }

    /// The video updates it holds, in order, each its bytes; the queue is emptied, as if
    /// the socket took all.
    pub(crate) fn updates(&self) -> Vec<Vec<u8>> {
        std::iter::from_fn(|| self.take())
            .filter_map(|m| match Frame::decode(&m[crate::wire::PREFIX..]) {
                Ok(Frame {
                    track: Track::Video,
                    bytes,
                }) => Some(bytes.to_vec()),
                _ => None,
            })
            .collect()
    }
}

#[cfg(test)]
impl Sender {
    /// A session's sender with a lane attached, and the lane's queue (which no writer drains:
    /// a test reads it).
    pub fn with_lane() -> (Sender, Arc<Queue>) {
        let mut s = Sender::new(Capability::mint().expect("a random source"));
        let queue = Queue::new(Arc::new(|| {}));
        assert!(s.attach(queue.clone()));
        (s, queue)
    }
}
