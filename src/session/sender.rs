//! A session's way out, as the media producers use it: frames in, on its lane's socket as the
//! socket takes them, and each video update counted until the relay says the page took it.
//!
//! The lane is one stream through the transport (a QUIC stream, or TCP, under the proxy).
//! Its socket takes whatever the daemon writes, far past what has arrived: the transport's
//! window is megabytes deep. So nothing here reads the socket for what the link carries.
//! What was delivered is told on the control stream: the lane keeps what it was sent and
//! has not been told taken (`link::Outstanding`), and the session keeps what the tooks have
//! shown of the path (`estimate`). From those comes the capture gate (`link.rs`). When the
//! path slows or stalls, captures are held and the newest screen goes next: dropping beats
//! lag, at the source.
//!
//! A session has a lane or it has none: a lane whose socket breaks is the session's lane
//! no longer, and what it held goes with it. The next lane the relay attaches starts its
//! viewer over on a keyframe (`hub::Client::attach`), since bytes the broken socket
//! accepted may never have reached the relay.
//!
//! Everything here is synchronous, under the session's lock in the hub: the lane's writer
//! task (`lane.rs`) takes that lock for each write, so what it writes is its session's lane
//! as it is at that moment, and a lane that has ended gets not one byte more.

use super::estimate::Estimate;
use super::link::{Gate, Outstanding, Stats};
use crate::hub::Out;
use crate::wire::{Capability, Frame, Track, Version};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

/// A viewer's way out, as `voxl stats` says it.
#[derive(Debug, serde::Serialize)]
#[serde(tag = "way", rename_all = "snake_case")]
pub enum Way {
    /// A session with no lane: none has attached, or the last one's socket broke.
    Unattached,
    /// Its lane: what is on the way, the path as it is known, and the gate.
    Lane(Stats),
    Recording,
    /// A session whose control connection closed.
    Closed,
}

/// A viewer's way out: a session's lane, or a recording in this process.
pub struct Sender {
    state: State,
}

/// A way out is exactly one of these. A lane that ends is no lane: the state becomes
/// `Unattached`, and what the lane held goes with it.
enum State {
    /// Its control connection is open, and no lane takes its frames: none has attached, or
    /// the last one's socket broke.
    Unattached(Session),
    /// Its control connection is open, and a lane takes its frames.
    Attached(Session, Lane),
    /// A recording (`voxl record`): each update goes to it whole, its header and its VP9, on
    /// its channel. It has no capability and no lane.
    Recording(tokio::sync::mpsc::UnboundedSender<Out>),
    /// Its control connection closed: the capability is forgotten, and no lane is sent
    /// another byte.
    Closed,
}

/// What an open session holds, with a lane or without.
struct Session {
    /// What its lane attaches with.
    capability: Capability,
    /// What its lanes' tooks have shown of the path: it outlives a lane. On the heap: it is
    /// most of what a session holds.
    estimate: Box<Estimate>,
}

/// A session's lane: the frames waiting for its socket, and what it was sent and has not
/// been told taken. Dropped, it is the session's lane no longer, and its writer is woken
/// to find that out.
struct Lane {
    writer: Writer,
    /// The frames waiting, each as the lane carries it.
    messages: VecDeque<Vec<u8>>,
    /// Bytes of the front message the socket has taken.
    written: usize,
    /// Buffers of messages that left, for the next ones: a steady stream allocates none.
    spare: Vec<Vec<u8>>,
    sent: Outstanding,
}

/// A lane's writer's hold on its lane: what wakes it, and what its session knows the lane
/// by.
#[derive(Clone)]
pub struct Writer(Arc<tokio::sync::Notify>);

impl Writer {
    /// Woken: a frame came for its lane, or the lane is its session's no longer.
    pub(super) async fn woken(&self) {
        self.0.notified().await
    }

    fn is(&self, other: &Writer) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// What a lane's writer does next.
pub(super) enum Next {
    /// Write again: more waits, and the socket may take it.
    Write,
    /// Wait for the socket to take more bytes.
    Writable,
    /// Wait for a frame.
    Idle,
    /// The lane is its session's no longer: end.
    End,
}

impl Drop for Lane {
    fn drop(&mut self) {
        self.writer.0.notify_one();
    }
}

impl Lane {
    /// Queues a frame, and wakes the writer.
    fn push(&mut self, track: Track, bytes: &[u8]) {
        // A spare buffer if one is left, else a new one.
        let mut buf = self.spare.pop().unwrap_or_default();
        buf.clear();
        Frame::encode(track, bytes, &mut buf);
        self.messages.push_back(buf);
        self.writer.0.notify_one();
    }

    /// Gives the front message's unwritten bytes to `socket`, which says how many it took:
    /// what the writer does next, or the error of a socket that broke.
    fn write(
        &mut self,
        socket: impl FnOnce(&[u8]) -> std::io::Result<usize>,
    ) -> std::io::Result<Next> {
        let Some(front) = self.messages.pop_front() else {
            return Ok(Next::Idle);
        };
        let rest = &front[self.written..];
        match socket(rest) {
            // All of it left: its buffer serves the next.
            Ok(n) if n == rest.len() => {
                self.written = 0;
                self.spare.push(front);
                Ok(Next::Write)
            }
            Ok(n) => {
                self.written += n;
                self.messages.push_front(front);
                Ok(Next::Write)
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                self.messages.push_front(front);
                Ok(Next::Writable)
            }
            Err(e) => Err(e),
        }
    }
}

impl Sender {
    /// A session's, with the capability its lane attaches with.
    pub fn new(capability: Capability) -> Sender {
        Sender {
            state: State::Unattached(Session {
                capability,
                estimate: Box::default(),
            }),
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
            State::Unattached(s) | State::Attached(s, _) => s.capability.matches(presented),
            State::Recording(_) | State::Closed => false,
        }
    }

    /// Its control connection closed: the capability is dropped (and zeroed), and its lane
    /// is its lane no longer before this returns.
    pub fn close(&mut self) {
        self.state = State::Closed;
    }

    /// Whether it has a way to be sent frames on: a lane, or a recording.
    pub fn sends(&self) -> bool {
        match &self.state {
            State::Attached(..) | State::Recording(_) => true,
            State::Unattached(_) | State::Closed => false,
        }
    }

    /// Whether its way out lets a capture start at `now`. None if it has no way to be sent
    /// frames on: it is sent nothing then, and holds nothing back. A recording is no link:
    /// it takes every frame at once.
    pub fn gate(&self, now: Instant) -> Option<Gate> {
        match &self.state {
            State::Attached(s, lane) => Some(s.estimate.gate(&lane.sent, now)),
            State::Recording(_) => Some(Gate::Open),
            State::Unattached(_) | State::Closed => None,
        }
    }

    /// A lane attaches: it takes the place of the lane before, which is the session's no
    /// longer, and starts with nothing on the way. Its writer's hold on it; none for a
    /// closed session or a recording, which take no lane.
    pub fn attach(&mut self) -> Option<Writer> {
        let writer = Writer(Arc::new(tokio::sync::Notify::new()));
        let lane = Lane {
            writer: writer.clone(),
            messages: VecDeque::new(),
            written: 0,
            spare: Vec::new(),
            sent: Outstanding::default(),
        };
        // The lane before, if any, is dropped here, which wakes its writer to end.
        let (state, attached) = match std::mem::replace(&mut self.state, State::Closed) {
            State::Unattached(s) | State::Attached(s, _) => {
                (State::Attached(s, lane), Some(writer))
            }
            way @ (State::Recording(_) | State::Closed) => (way, None),
        };
        self.state = state;
        attached
    }

    /// The relay said at `now` that the page took the update named `version`. A session
    /// with no lane has nothing on the way; a recording and a closed session are told
    /// nothing.
    pub fn took(&mut self, version: Version, now: Instant) {
        match &mut self.state {
            State::Attached(s, lane) => s.estimate.took(&mut lane.sent, version, now),
            State::Unattached(_) | State::Recording(_) | State::Closed => {}
        }
    }

    /// What `voxl stats` says of it at `now`.
    pub fn stats(&self, now: Instant) -> Way {
        match &self.state {
            State::Attached(s, lane) => Way::Lane(s.estimate.stats(&lane.sent, now)),
            State::Unattached(_) => Way::Unattached,
            State::Recording(_) => Way::Recording,
            State::Closed => Way::Closed,
        }
    }

    /// A video update named `version`, its header and its VP9, at `now`: onto the lane,
    /// counted until it is told taken, or to a recording as it is. A recording whose
    /// recorder has gone takes nothing: its viewer is removed with it.
    pub fn video(&mut self, bytes: &bytes::Bytes, version: Version, now: Instant) {
        match &mut self.state {
            State::Attached(s, lane) => {
                lane.push(Track::Video, bytes);
                s.estimate
                    .sent(&mut lane.sent, version, bytes.len() as u64, now);
            }
            State::Recording(to) => {
                if to.send(Out::Bin(bytes.clone())).is_err() {
                    log::debug!("a recording ended before its viewer left");
                }
            }
            State::Unattached(_) | State::Closed => {}
        }
    }

    /// An Opus packet, `silent` frames of silence after the one before: onto the lane as it
    /// comes, or to a recording as it is, with the silence it follows.
    pub fn audio(&mut self, packet: &[u8], silent: u32) {
        match &mut self.state {
            State::Attached(_, lane) => lane.push(Track::Audio, packet),
            State::Recording(to) => {
                let packet = bytes::Bytes::copy_from_slice(packet);
                if to.send(Out::Audio { packet, silent }).is_err() {
                    log::debug!("a recording ended before its viewer left");
                }
            }
            State::Unattached(_) | State::Closed => {}
        }
    }

    /// The session's lane, if it is the one `writer` writes. A writer whose lane was
    /// replaced, or whose session closed, has none: two writers live at once only until
    /// the older finds that out here.
    fn lane_of(&mut self, writer: &Writer) -> Option<&mut Lane> {
        match &mut self.state {
            State::Attached(_, lane) => Some(lane).filter(|lane| lane.writer.is(writer)),
            State::Unattached(_) | State::Recording(_) | State::Closed => None,
        }
    }

    /// One step of `writer`: the front of its lane's queue is given to `socket`, which
    /// says how many bytes it took. Under the session's lock, so a lane that is the
    /// session's no longer is given nothing. A socket that broke ends the lane.
    pub(super) fn write(
        &mut self,
        writer: &Writer,
        socket: impl FnOnce(&[u8]) -> std::io::Result<usize>,
    ) -> Next {
        let wrote = match self.lane_of(writer) {
            Some(lane) => lane.write(socket),
            None => return Next::End,
        };
        match wrote {
            Ok(next) => next,
            Err(_broken) => {
                self.lost(writer);
                Next::End
            }
        }
    }

    /// The socket of the lane `writer` writes broke: if that lane is still the session's,
    /// the session has no lane now, and what the lane held is dropped with it.
    pub(crate) fn lost(&mut self, writer: &Writer) {
        self.state = match std::mem::replace(&mut self.state, State::Closed) {
            State::Attached(s, lane) => match lane.writer.is(writer) {
                true => State::Unattached(s),
                false => State::Attached(s, lane),
            },
            way @ (State::Unattached(_) | State::Recording(_) | State::Closed) => way,
        };
    }
}

#[cfg(test)]
impl Sender {
    /// The messages waiting for its lane's socket.
    pub fn queued(&self) -> usize {
        match &self.state {
            State::Attached(_, lane) => lane.messages.len(),
            State::Unattached(_) | State::Recording(_) | State::Closed => 0,
        }
    }

    /// The front message of its lane as the writer gives it to its socket, taken out whole.
    pub(crate) fn take(&mut self) -> Option<Vec<u8>> {
        let lane = match &mut self.state {
            State::Attached(_, lane) => lane,
            State::Unattached(_) | State::Recording(_) | State::Closed => return None,
        };
        let mut taken = None;
        lane.write(|bytes| {
            taken = Some(bytes.to_vec());
            Ok(bytes.len())
        })
        .expect("a socket that takes all");
        taken
    }

    /// The video updates its lane holds, in order, each its bytes; the queue is emptied,
    /// as if the socket took all.
    pub(crate) fn updates(&mut self) -> Vec<Vec<u8>> {
        std::iter::from_fn(|| self.take())
            .filter_map(|m| match Frame::decode(&m[crate::wire::PREFIX..]) {
                Ok(Frame {
                    track: Track::Video,
                    bytes,
                }) => Some(bytes.to_vec()),
                Ok(Frame {
                    track: Track::Audio,
                    ..
                }) => None,
                Err(e) => panic!("a lane held what is no frame: {e}"),
            })
            .collect()
    }

    /// A session's sender with a lane attached, which no writer drains: a test reads it.
    pub fn with_lane() -> (Sender, Writer) {
        let mut s = Sender::new(Capability::mint().expect("a random source"));
        let writer = s.attach().expect("an open session takes a lane");
        (s, writer)
    }
}
