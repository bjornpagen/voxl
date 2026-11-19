//! Every message the lane and the control stream carry, and the one way each is written and
//! read.
//!
//! The lane is one stream through the transport's proxy: the session's [`Capability`] (relay
//! to daemon, the lane's first bytes and its only ones that way), then frames (daemon to
//! relay, [`Frame`]): each named by its [`Sequence`], and each a message for the page, a
//! video update (its [`Header`], then its picture) or an Opus packet.
//! The control stream is the stdio of `voxl attach` over ssh: [`Down`] (daemon to relay) and
//! [`Up`] (relay to daemon), which also tells the daemon each lane message the page took:
//! delivery is told, never read off the lane's socket, which accepts far more than arrives.
//! A frame and a control message each go after their length ([`Framer`]). Numbers are
//! big-endian, as the page's video header is.
//!
//! Reading never panics and never allocates by a length it read: every reader checks a
//! length against the bytes in hand and against [`MAX_MESSAGE`] first, and returns a
//! [`Malformed`] for anything else.

/// Where the lane goes when `--place` names nowhere else, as the proxy is asked for it: the
/// relay's port on the server's own loopback, which the server's proxy maps to the daemon's
/// lane socket.
pub const DESTINATION: &str = "localhost:6901";

/// The longest message, a frame or a control message: the most the page's WebSocket took of
/// one message before the control stream replaced it (`ws.max_message_size(64 << 20)`,
/// src/server.rs:334 at 8f460d3), which a clipboard image, a cursor or a keyframe crosses
/// whole. The relay hands each frame to the page as one WebSocket message, so a frame is held
/// to the same bound.
pub const MAX_MESSAGE: usize = 64 << 20;

/// The longest payload a [`Down::Binary`] carries: the longest message less its kind byte.
pub const MAX_DOWN_BINARY: usize = MAX_MESSAGE - std::mem::size_of::<u8>();

/// The length written before each frame and each control message: a `u32`, which holds
/// [`MAX_MESSAGE`].
pub const PREFIX: usize = std::mem::size_of::<u32>();

/// A capability's bytes: 256 bits, the contract's capability (§2).
pub const CAPABILITY: usize = 32;

/// The Opus packets of desktop audio the page's player holds (`desktopAudio.packets` in
/// web/src/wire.json, which the player reads; a test holds the two equal): what a lane
/// keeps of the sound while its gate is not open. Sound older than that the page would drop
/// on arrival, after it had waited ahead of the video on the lane.
pub const PLAYER_PACKETS: usize = 2;

/// A lane message's name: the session's count of the messages its lanes were sent, video
/// and audio alike, which the relay says back when the page took the message
/// ([`Up::Took`]). It is the one name a lane message has. Names are matched, never ordered:
/// a lane carries its messages in order, and a name is one message's for the session's
/// life (it wraps after 2^32 messages: 310 days of video at 60 a second and sound at 100).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Sequence(pub u32);

impl Sequence {
    /// The name of the message after this one.
    pub fn next(self) -> Sequence {
        Sequence(self.0.wrapping_add(1))
    }
}

/// A coded picture's number: the capture it was coded from, counted for the daemon, which
/// its header carries ([`Header::version`]). It names a picture, not a lane message: one
/// picture goes to every viewer's lane and to a recording, as a message of a different
/// [`Sequence`] on each, and nothing that tells delivery reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Version(pub u32);

/// The capability that lets a lane attach to a session: 32 random bytes the daemon mints
/// per control connection and forgets when it closes.
///
/// It has no `Debug`, `Display` or `Clone`: it never reaches a log line, a panic message or a
/// second copy. Two are compared in constant time, and its bytes are zeroed when it drops.
pub struct Capability([u8; CAPABILITY]);

/// A capability where it lies: the daemon's own, lent to a control message, or the bytes of
/// the control frame that brought it to the relay. The same guarantees as [`Capability`].
#[derive(Clone, Copy)]
pub struct CapabilityRef<'a>(&'a [u8; CAPABILITY]);

impl CapabilityRef<'_> {
    /// The relay's one copy of the capability the control stream brought, taken from the
    /// frame it arrived in, so that it outlives that frame's buffer.
    pub fn keep(self) -> Capability {
        Capability(*self.0)
    }

    /// Whether `other` is this capability, in time that does not depend on where they differ.
    pub fn matches(self, other: &Capability) -> bool {
        same(self.0, &other.0)
    }
}

/// Whether two capabilities' bytes are the same, looking at every byte whatever they hold.
fn same(a: &[u8; CAPABILITY], b: &[u8; CAPABILITY]) -> bool {
    let mut d = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        d |= x ^ y;
    }
    std::hint::black_box(d) == 0
}

impl Capability {
    /// This capability, lent.
    pub fn lend(&self) -> CapabilityRef<'_> {
        CapabilityRef(&self.0)
    }

    /// A fresh capability from the system's random source.
    pub fn mint() -> Result<Capability, getrandom::Error> {
        let mut c = Capability([0; CAPABILITY]);
        getrandom::fill(&mut c.0)?;
        Ok(c)
    }

    /// Whether `other` is this capability, in time that does not depend on where they differ.
    pub fn matches(&self, other: &Capability) -> bool {
        same(&self.0, &other.0)
    }

    /// Its bytes, as the lane's first bytes carry them.
    pub fn bytes(&self) -> &[u8; CAPABILITY] {
        &self.0
    }

    /// The capability a lane's first bytes present; the caller zeroes `bytes` once read.
    pub fn presented(bytes: &[u8; CAPABILITY]) -> Capability {
        Capability(*bytes)
    }
}

impl Drop for Capability {
    fn drop(&mut self) {
        for b in self.0.iter_mut() {
            // SAFETY: `b` is a valid, aligned, exclusive reference into this capability.
            unsafe { std::ptr::write_volatile(b, 0) };
        }
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }
}

/// What a frame is. Its byte is the page's type byte of the same message (`bin.video` and
/// `bin.audio` in web/src/wire.json), so a frame as the lane carries it is its name and then
/// the page's message as it is, which the relay hands on unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Track {
    /// One video update: its header ([`Header`]) and its AV1 temporal unit.
    Video,
    /// One Opus packet.
    Audio,
}

/// A closed vocabulary of one byte on the wire: its members, and the byte each is. A byte
/// is read by looking it up here, so a byte that is no member's is no member.
pub(crate) trait Byte: Copy + 'static {
    const ALL: &'static [Self];

    fn byte(self) -> u8;

    /// The member `byte` is, if it is one.
    fn of(byte: u8) -> Option<Self> {
        Self::ALL.iter().copied().find(|b| b.byte() == byte)
    }
}

impl Byte for Track {
    const ALL: &'static [Track] = &[Track::Video, Track::Audio];

    fn byte(self) -> u8 {
        match self {
            Track::Video => 1,
            Track::Audio => 2,
        }
    }
}

/// A frame on the lane, daemon to relay, after its length: its [`Sequence`], then the page's
/// message as it is, its track's byte and then its bytes. The lane is a stream: every frame
/// arrives whole, in the order sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame<'a> {
    pub sequence: Sequence,
    pub track: Track,
    pub bytes: &'a [u8],
    /// The track's byte and `bytes`, as the frame carries them: the page's message.
    page: &'a [u8],
}

impl<'a> Frame<'a> {
    /// Appends the frame named `sequence` of `track` and `bytes` to `out`, after its
    /// length. It is at most [`MAX_MESSAGE`].
    pub fn encode(sequence: Sequence, track: Track, bytes: &[u8], out: &mut Vec<u8>) {
        let start = open(out);
        out.extend_from_slice(&sequence.0.to_be_bytes());
        out.push(track.byte());
        out.extend_from_slice(bytes);
        close(out, start);
    }

    /// Reads one frame, as [`Framer::frame`] gives it (without its length).
    pub fn decode(b: &'a [u8]) -> Result<Frame<'a>, Malformed> {
        let mut r = Reader(b);
        let sequence = Sequence(u32::from_be_bytes(*r.chunk()?));
        let page = r.0;
        Ok(Frame {
            sequence,
            track: r.member(Malformed::Track)?,
            bytes: r.rest(),
            page,
        })
    }

    /// The page's message: the frame without its sequence, which the relay hands on as it
    /// is.
    pub fn page(&self) -> &'a [u8] {
        self.page
    }
}

/// What a video frame is coded as: what the encoder says of each frame it puts out, and the
/// frame's header carries in its flags byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Deserialize), serde(rename_all = "snake_case"))]
pub enum Coding {
    /// From nothing: a viewer starts on it.
    Key,
    /// From the frames before it.
    Delta,
}

/// Its flags byte: the keyframe's bit, and no other. A byte with any other bit set is no
/// coding's.
impl Byte for Coding {
    const ALL: &'static [Coding] = &[Coding::Key, Coding::Delta];

    fn byte(self) -> u8 {
        match self {
            Coding::Key => 1,
            Coding::Delta => 0,
        }
    }
}

/// What a video frame says before its picture, on the lane: the daemon's encode loop writes
/// it, and the recorder, the relay (which refuses a lane whose update is not one) and
/// web/src/protocol.ts read it, each checking its reader against web/src/video-header.json.
/// What a frame is (video or audio) the lane's [`Track`] says, as the page's type byte. Every
/// update refers only to the keyframe before it on its lane and frames after that keyframe,
/// all of which the lane carried before it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Header {
    pub coding: Coding,
    /// The capture the picture was coded from.
    pub version: Version,
    pub pipe: Pipe,
}

/// Capture to send, as a video frame carries it: in tenths of a millisecond, at most
/// `u32::MAX` of them (119 hours).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pipe(u32);

impl Pipe {
    pub fn of(d: std::time::Duration) -> Pipe {
        Pipe((d.as_micros() / 100).min(u128::from(u32::MAX)) as u32)
    }

    /// In whole milliseconds, rounded down.
    pub fn millis(self) -> u64 {
        u64::from(self.0) / 10
    }
}

impl Header {
    /// The AV1 temporal unit starts this far into a frame.
    pub const LEN: usize = 9;

    /// Flags, then the version, then pipe, big-endian.
    pub fn bytes(&self) -> [u8; Header::LEN] {
        let mut b = [0; Header::LEN];
        b[0] = self.coding.byte();
        b[1..5].copy_from_slice(&self.version.0.to_be_bytes());
        b[5..9].copy_from_slice(&self.pipe.0.to_be_bytes());
        b
    }

    /// What a video frame's bytes `m` are ([`Frame::bytes`] of [`Track::Video`]). Bytes too
    /// few for a header, a flags byte with any bit set but the keyframe's, and a header with
    /// no picture after it, are refused: the page's reader refuses the same
    /// (web/src/protocol.ts), so no reader takes an update another does not.
    pub fn read(m: &[u8]) -> Result<Video<'_>, Malformed> {
        let Some((b, picture)) = m.split_first_chunk::<{ Header::LEN }>() else {
            return Err(Malformed::Truncated);
        };
        let header = Header {
            coding: Coding::of(b[0]).ok_or(Malformed::Flags(b[0]))?,
            version: Version(u32::from_be_bytes([b[1], b[2], b[3], b[4]])),
            pipe: Pipe(u32::from_be_bytes([b[5], b[6], b[7], b[8]])),
        };
        match picture {
            [] => Err(Malformed::NoPicture),
            [_, ..] => Ok(Video { header, picture }),
        }
    }
}

/// A video update, read: its header, and the picture after it, never empty (the encode loop
/// sends no update that coded nothing).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Video<'a> {
    pub header: Header,
    pub picture: &'a [u8],
}

/// A control message from the daemon to the relay.
pub enum Down<'a> {
    /// The session's capability: the first message, and only then.
    Capability(CapabilityRef<'a>),
    /// A lane has attached with it. The relay reopens a lane that ends only once it was
    /// attached: one that ends before is the daemon or the transport refusing it, and opening
    /// it again would loop.
    Attached,
    /// A text message for the page (its hello, a clipboard, ...), as the page reads it.
    Text(&'a str),
    /// A binary message for the page (a cursor's image, ...).
    Binary(&'a [u8]),
}

/// The first byte of a message from the daemon to the relay: which it is.
#[derive(Clone, Copy)]
enum DownTag {
    Capability,
    Attached,
    Text,
    Binary,
}

impl Byte for DownTag {
    const ALL: &'static [DownTag] = &[
        DownTag::Capability,
        DownTag::Attached,
        DownTag::Text,
        DownTag::Binary,
    ];

    fn byte(self) -> u8 {
        match self {
            DownTag::Capability => 1,
            DownTag::Attached => 2,
            DownTag::Text => 3,
            DownTag::Binary => 4,
        }
    }
}

impl<'a> Down<'a> {
    /// Appends the message to `out`, after its length.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let start = open(out);
        match self {
            Down::Capability(c) => {
                out.push(DownTag::Capability.byte());
                out.extend_from_slice(c.0);
            }
            Down::Attached => out.push(DownTag::Attached.byte()),
            Down::Text(t) => {
                out.push(DownTag::Text.byte());
                out.extend_from_slice(t.as_bytes());
            }
            Down::Binary(b) => {
                out.push(DownTag::Binary.byte());
                out.extend_from_slice(b);
            }
        }
        close(out, start);
    }

    /// Reads one message, as [`Framer::frame`] gives it (without its length).
    pub fn decode(b: &'a [u8]) -> Result<Down<'a>, Malformed> {
        let mut r = Reader(b);
        match r.member::<DownTag>(Malformed::Tag)? {
            DownTag::Capability => {
                let capability = Down::Capability(CapabilityRef(r.chunk()?));
                r.end()?;
                Ok(capability)
            }
            DownTag::Attached => {
                r.end()?;
                Ok(Down::Attached)
            }
            DownTag::Text => text(r.rest()).map(Down::Text),
            DownTag::Binary => Ok(Down::Binary(r.rest())),
        }
    }
}

/// A control message from the relay to the daemon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Up<'a> {
    /// A text message from the page (input, a clipboard, ...), as the page wrote it.
    Text(&'a str),
    /// A binary message from the page (the microphone, a clipboard image).
    Binary(&'a [u8]),
    /// The page took a lane message: the one named `sequence` is done with (an update decoded
    /// and shown, a packet played into its ring), and so is every message the lane carried
    /// before it. The relay makes it of the page's own word (`bin.took` in
    /// web/src/wire.json), which it never hands on as it came. Sequences are unique for a
    /// session, so a took of a lane since replaced names nothing the daemon still waits for.
    Took { sequence: Sequence },
}

/// The first byte of a message from the relay to the daemon: which it is.
#[derive(Clone, Copy)]
enum UpTag {
    Text,
    Binary,
    Took,
}

impl Byte for UpTag {
    const ALL: &'static [UpTag] = &[UpTag::Text, UpTag::Binary, UpTag::Took];

    fn byte(self) -> u8 {
        match self {
            UpTag::Text => 1,
            UpTag::Binary => 2,
            UpTag::Took => 3,
        }
    }
}

impl<'a> Up<'a> {
    /// Appends the message to `out`, after its length.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let start = open(out);
        match self {
            Up::Text(t) => {
                out.push(UpTag::Text.byte());
                out.extend_from_slice(t.as_bytes());
            }
            Up::Binary(b) => {
                out.push(UpTag::Binary.byte());
                out.extend_from_slice(b);
            }
            Up::Took { sequence } => {
                out.push(UpTag::Took.byte());
                out.extend_from_slice(&sequence.0.to_be_bytes());
            }
        }
        close(out, start);
    }

    /// Reads one message, as [`Framer::frame`] gives it (without its length).
    pub fn decode(b: &'a [u8]) -> Result<Up<'a>, Malformed> {
        let mut r = Reader(b);
        match r.member::<UpTag>(Malformed::Tag)? {
            UpTag::Text => text(r.rest()).map(Up::Text),
            UpTag::Binary => Ok(Up::Binary(r.rest())),
            UpTag::Took => {
                let took = Up::Took {
                    sequence: Sequence(u32::from_be_bytes(*r.chunk()?)),
                };
                r.end()?;
                Ok(took)
            }
        }
    }
}

/// Why bytes are not a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Malformed {
    /// It ends before a field it must hold.
    Truncated,
    /// Bytes follow its end.
    Trailing,
    /// No message has this tag (here).
    Tag(u8),
    /// No track has this byte.
    Track(u8),
    /// A video header's flags with a bit no reader knows.
    Flags(u8),
    /// A video header with no picture after it.
    NoPicture,
    /// Text that is not UTF-8.
    Utf8,
    /// A framed length over [`MAX_MESSAGE`].
    Long(usize),
}

impl std::fmt::Display for Malformed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Malformed::Truncated => write!(f, "the message ends early"),
            Malformed::Trailing => write!(f, "bytes follow the message"),
            Malformed::Tag(t) => write!(f, "no message has tag {t}"),
            Malformed::Track(t) => write!(f, "no track has byte {t}"),
            Malformed::Flags(b) => write!(f, "a video header with flags {b:#04x}"),
            Malformed::NoPicture => write!(f, "a video header with no picture after it"),
            Malformed::Utf8 => write!(f, "text that is not UTF-8"),
            Malformed::Long(n) => write!(f, "a message of {n} bytes, over the longest"),
        }
    }
}

impl std::error::Error for Malformed {}

fn text(b: &[u8]) -> Result<&str, Malformed> {
    std::str::from_utf8(b).map_err(|_| Malformed::Utf8)
}

/// Opens a message in `out`: room for its length, and where it starts.
fn open(out: &mut Vec<u8>) -> usize {
    out.extend_from_slice(&[0; PREFIX]);
    out.len()
}

/// Closes the message that starts at `start`: its length before it.
fn close(out: &mut [u8], start: usize) {
    let len = out.len() - start;
    debug_assert!(len <= MAX_MESSAGE, "a message of {len} bytes");
    out[start - PREFIX..start].copy_from_slice(&(len as u32).to_be_bytes());
}

/// Cuts a stream into its messages (the lane's frames, or the control stream's messages):
/// a 4-byte length before each, at most [`MAX_MESSAGE`].
///
/// It holds only bytes that arrived: a length is checked against the maximum as soon as it
/// is whole, and room grows only as bytes come, never by a length read.
#[derive(Default)]
pub struct Framer {
    buf: Vec<u8>,
    /// Bytes at the front already handed out.
    used: usize,
}

impl Framer {
    pub fn new() -> Framer {
        Framer::default()
    }

    /// Bytes as they came from the stream.
    pub fn push(&mut self, bytes: &[u8]) {
        self.compact();
        self.buf.extend_from_slice(bytes);
    }

    /// The next whole message, if one is in; an error is final (the stream is not framed).
    pub fn frame(&mut self) -> Result<Option<&[u8]>, Malformed> {
        self.compact();
        let Some(prefix) = self.buf.first_chunk::<PREFIX>() else {
            return Ok(None);
        };
        let len = u32::from_be_bytes(*prefix) as usize;
        if len > MAX_MESSAGE {
            return Err(Malformed::Long(len));
        }
        let end = PREFIX + len;
        if self.buf.len() < end {
            return Ok(None);
        }
        self.used = end;
        Ok(Some(&self.buf[PREFIX..end]))
    }

    fn compact(&mut self) {
        if self.used > 0 {
            self.buf.drain(..self.used);
            self.used = 0;
        }
    }
}

/// Reads fields from the front of a message.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    /// The next `N` bytes.
    fn chunk<const N: usize>(&mut self) -> Result<&'a [u8; N], Malformed> {
        let (chunk, rest) = self.0.split_first_chunk().ok_or(Malformed::Truncated)?;
        self.0 = rest;
        Ok(chunk)
    }

    /// The next byte, as the member of the vocabulary `B` it is: a byte that is no
    /// member's is the error `wrong` makes of it.
    fn member<B: Byte>(&mut self, wrong: fn(u8) -> Malformed) -> Result<B, Malformed> {
        let [byte] = *self.chunk()?;
        B::of(byte).ok_or(wrong(byte))
    }

    fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.0)
    }

    fn end(&self) -> Result<(), Malformed> {
        match self.0 {
            [] => Ok(()),
            [_, ..] => Err(Malformed::Trailing),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xorshift::Xorshift;

    fn bytes(rng: &mut Xorshift, n: usize) -> Vec<u8> {
        (0..n).map(|_| rng.next() as u8).collect()
    }

    /// A valid frame, its bytes without its length.
    fn frame(rng: &mut Xorshift) -> Vec<u8> {
        let track = Track::ALL[rng.below(2) as usize];
        let n = rng.below(3000) as usize;
        let data = bytes(rng, n);
        let mut out = Vec::new();
        Frame::encode(Sequence(rng.next() as u32), track, &data, &mut out);
        out[PREFIX..].to_vec()
    }

    fn down(rng: &mut Xorshift) -> Vec<u8> {
        let mut out = Vec::new();
        let text: String = (0..rng.below(50))
            .map(|_| char::from_u32(rng.range(32, 0x2FFF) as u32).expect("no surrogate so low"))
            .collect();
        let bin = {
            let n = rng.below(100) as usize;
            bytes(rng, n)
        };
        let cap = Capability(bytes(rng, CAPABILITY).try_into().unwrap());
        match DownTag::ALL[rng.below(4) as usize] {
            DownTag::Capability => Down::Capability(cap.lend()).encode(&mut out),
            DownTag::Attached => Down::Attached.encode(&mut out),
            DownTag::Text => Down::Text(&text).encode(&mut out),
            DownTag::Binary => Down::Binary(&bin).encode(&mut out),
        }
        out[PREFIX..].to_vec()
    }

    fn up(rng: &mut Xorshift) -> Vec<u8> {
        let mut out = Vec::new();
        match UpTag::ALL[rng.below(3) as usize] {
            UpTag::Text => Up::Text("k,65,1").encode(&mut out),
            UpTag::Took => Up::Took {
                sequence: Sequence(rng.next() as u32),
            }
            .encode(&mut out),
            UpTag::Binary => Up::Binary(&{
                let n = rng.below(100) as usize;
                bytes(rng, n)
            })
            .encode(&mut out),
        }
        out[PREFIX..].to_vec()
    }

    /// A valid message of the relay's whose fields fix its length.
    fn took(rng: &mut Xorshift) -> Vec<u8> {
        loop {
            let u = up(rng);
            if u[0] == UpTag::Took.byte() {
                break u;
            }
        }
    }

    /// The kinds of message this module reads.
    #[derive(Clone, Copy, Debug)]
    enum Kind {
        Frame,
        Down,
        Up,
    }

    const KINDS: [Kind; 3] = [Kind::Frame, Kind::Down, Kind::Up];

    /// A valid message of `kind`.
    fn valid(kind: Kind, rng: &mut Xorshift) -> Vec<u8> {
        match kind {
            Kind::Frame => frame(rng),
            Kind::Down => down(rng),
            Kind::Up => up(rng),
        }
    }

    /// A kind's decoder re-encoded: why it refuses the bytes, or the bytes its decoding
    /// encodes to.
    fn again(kind: Kind, b: &[u8]) -> Result<Vec<u8>, Malformed> {
        let mut out = Vec::new();
        match kind {
            Kind::Frame => {
                let f = Frame::decode(b)?;
                Frame::encode(f.sequence, f.track, f.bytes, &mut out)
            }
            Kind::Down => Down::decode(b)?.encode(&mut out),
            Kind::Up => Up::decode(b)?.encode(&mut out),
        };
        Ok(out[PREFIX..].to_vec())
    }

    /// Every message of every kind decodes to what encodes it again, byte for byte; and
    /// every input at all (random bytes, valid messages with bytes flipped, cut or added)
    /// either is refused with a typed error or is such a message: nothing panics, and nothing
    /// decodes to a message that is not exactly its bytes.
    #[test]
    fn every_input_is_a_message_or_a_typed_error() {
        let mut rng = Xorshift::new(0x082E_FA98_EC4E_6C89);
        for _ in 0..20_000 {
            let kind = KINDS[rng.below(3) as usize];
            let valid = valid(kind, &mut rng);
            assert_eq!(again(kind, &valid), Ok(valid.clone()), "{kind:?}");
            // One time in four, random bytes; else the valid message.
            let mut b = match rng.below(4) {
                0 => {
                    let n = rng.below(1500) as usize;
                    bytes(&mut rng, n)
                }
                1.. => valid,
            };
            // A bit flipped, the end cut, bytes added, or the first byte another.
            match (rng.below(4), b.len() as u64) {
                (0 | 3.., 0) => {}
                (0, len @ 1..) => b[rng.below(len) as usize] ^= 1 << rng.below(8),
                (1, len) => b.truncate(rng.below(len + 1) as usize),
                (2, _) => b.extend({
                    let n = rng.range(1, 9) as usize;
                    bytes(&mut rng, n)
                }),
                (3.., 1..) => b[0] = rng.below(8) as u8,
            }
            for kind in KINDS {
                if let Ok(out) = again(kind, &b) {
                    assert_eq!(out, b, "{kind:?} decoded bytes it does not encode to");
                }
            }
        }
    }

    /// Every strict prefix of a message whose fields fix its length (the daemon's capability
    /// and attached, the relay's took) is refused. (A frame's and a text's length is the
    /// stream's framing's, not their own fields'.)
    #[test]
    fn every_cut_message_is_refused() {
        let mut rng = Xorshift::new(0x4528_21E6_38D0_1377);
        for _ in 0..2000 {
            let valid = loop {
                let d = down(&mut rng);
                if [DownTag::Capability, DownTag::Attached]
                    .map(DownTag::byte)
                    .contains(&d[0])
                {
                    break d;
                }
            };
            for cut in 0..valid.len() {
                assert!(again(Kind::Down, &valid[..cut]).is_err(), "cut at {cut}");
            }
            let valid = took(&mut rng);
            for cut in 0..valid.len() {
                assert!(again(Kind::Up, &valid[..cut]).is_err(), "took cut at {cut}");
            }
        }
    }

    /// Messages framed one after another and pushed in pieces of any size come out whole, in
    /// order; a length over the longest is an error as soon as it is whole.
    #[test]
    fn framing_cuts_a_stream_back_into_its_messages() {
        let mut rng = Xorshift::new(0xBE54_66CF_34E9_0C6C);
        for case in 0..300 {
            let messages: Vec<Vec<u8>> = (0..rng.range(1, 20))
                .map(|_| match case % 2 == 0 {
                    true => up(&mut rng),
                    false => frame(&mut rng),
                })
                .collect();
            let mut stream = Vec::new();
            for m in &messages {
                stream.extend_from_slice(&(m.len() as u32).to_be_bytes());
                stream.extend_from_slice(m);
            }
            let mut f = Framer::new();
            let mut got = Vec::new();
            let mut at = 0;
            while at < stream.len() {
                let step = rng.range(1, 700) as usize;
                let end = (at + step).min(stream.len());
                f.push(&stream[at..end]);
                at = end;
                while let Some(m) = f.frame().unwrap() {
                    got.push(m.to_vec());
                }
            }
            assert_eq!(got, messages);
        }
        let mut f = Framer::new();
        f.push(&((MAX_MESSAGE + 1) as u32).to_be_bytes());
        assert_eq!(f.frame(), Err(Malformed::Long(MAX_MESSAGE + 1)));
    }

    /// A frame as the lane carries it is the page's message as it is: its track's byte is
    /// web/src/wire.json's type byte of that message.
    #[test]
    fn a_frames_track_is_the_pages_type_byte() {
        let wire: serde_json::Value =
            serde_json::from_str(include_str!("../web/src/wire.json")).unwrap();
        assert_eq!(wire["bin"]["video"], u64::from(Track::Video.byte()));
        assert_eq!(wire["bin"]["audio"], u64::from(Track::Audio.byte()));
    }

    /// What a lane keeps of the sound while its gate is shut is what the page's player
    /// holds: web/src/wire.json's `desktopAudio.packets`.
    #[test]
    fn a_lane_holds_the_sound_the_players_ring_holds() {
        let wire: serde_json::Value =
            serde_json::from_str(include_str!("../web/src/wire.json")).unwrap();
        assert_eq!(wire["desktopAudio"]["packets"], PLAYER_PACKETS as u64);
    }

    /// The relay's took reads back as the sequence it was written from, over generated
    /// sequences.
    #[test]
    fn a_took_reads_back_as_its_sequence() {
        let mut rng = Xorshift::new(0x2FFD_72DB_D01A_DFB7);
        for _ in 0..20_000 {
            let sequence = Sequence(rng.next() as u32);
            let mut out = Vec::new();
            Up::Took { sequence }.encode(&mut out);
            assert_eq!(Up::decode(&out[PREFIX..]), Ok(Up::Took { sequence }));
        }
    }

    /// A frame reads back as the sequence, track and bytes it was written from, and its
    /// page message is exactly its track's byte and its bytes: what the relay hands the
    /// page carries no sequence, over generated frames.
    #[test]
    fn a_frame_is_its_sequence_then_the_pages_message() {
        let mut rng = Xorshift::new(0x7A1C_55E0_03D9_B2C4);
        for _ in 0..20_000 {
            let sequence = Sequence(rng.next() as u32);
            let track = Track::ALL[rng.below(2) as usize];
            let n = rng.below(300) as usize;
            let data = bytes(&mut rng, n);
            let mut out = Vec::new();
            Frame::encode(sequence, track, &data, &mut out);
            let f = Frame::decode(&out[PREFIX..]).expect("a frame written reads");
            assert_eq!((f.sequence, f.track, f.bytes), (sequence, track, &data[..]));
            assert_eq!(f.page(), [&[track.byte()][..], &data].concat());
        }
    }

    /// A number of any size: a random one, shifted right by a random amount.
    fn any(rng: &mut Xorshift) -> u64 {
        let shift = rng.next() % 64;
        rng.next() >> shift
    }

    fn random_header(rng: &mut Xorshift) -> Header {
        Header {
            coding: Coding::ALL[rng.below(2) as usize],
            version: Version(any(rng) as u32),
            pipe: Pipe(any(rng) as u32),
        }
    }

    #[test]
    fn a_header_reads_back_as_it_was_written() {
        let mut rng = Xorshift::new(0x9e37_79b9_7f4a_7c15);
        for _ in 0..100_000 {
            let header = random_header(&mut rng);
            let picture: Vec<u8> = (0..1 + rng.next() % 64).map(|_| rng.next() as u8).collect();
            let message = [&header.bytes()[..], &picture].concat();
            assert_eq!(
                Header::read(&message),
                Ok(Video {
                    header,
                    picture: &picture
                })
            );
        }
    }

    /// A header with no picture after it is no update, and fewer bytes than a header are
    /// cut short.
    #[test]
    fn a_header_alone_is_refused_and_less_is_cut_short() {
        let mut rng = Xorshift::new(0x2545_f491_4f6c_dd1d);
        for _ in 0..100_000 {
            let header = random_header(&mut rng);
            let bytes = header.bytes();
            assert_eq!(Header::read(&bytes), Err(Malformed::NoPicture));
            let cut = (rng.next() % Header::LEN as u64) as usize;
            assert_eq!(Header::read(&bytes[..cut]), Err(Malformed::Truncated));
        }
    }

    /// A header is read exactly when its flags set no bit but the keyframe's, and then its
    /// coding is that bit's: over every value of the flags byte, on generated headers, as
    /// web/src/video-header.test.ts holds the page's reader to.
    #[test]
    fn a_header_with_a_flag_no_reader_knows_is_refused() {
        let mut rng = Xorshift::new(0xa458_fea3_f493_3d7e);
        for flags in 0..=u8::MAX {
            let mut bytes = random_header(&mut rng).bytes().to_vec();
            bytes[0] = flags;
            bytes.extend((0..rng.range(1, 5)).map(|_| rng.next() as u8));
            match (flags, Header::read(&bytes)) {
                (0, Ok(v)) => assert_eq!(v.header.coding, Coding::Delta),
                (1, Ok(v)) => assert_eq!(v.header.coding, Coding::Key),
                (2.., Err(e)) => assert_eq!(e, Malformed::Flags(flags)),
                (0 | 1, read @ Err(_)) | (2.., read @ Ok(_)) => {
                    panic!("flags {flags}: {read:?}")
                }
            }
        }
    }

    #[test]
    fn a_pipe_is_whole_tenths_of_a_millisecond() {
        use std::time::Duration;
        let mut rng = Xorshift::new(0x6a09_e667_f3bc_c908);
        let tenth = Duration::from_micros(100);
        for _ in 0..100_000 {
            let d = Duration::from_nanos(any(&mut rng));
            let Pipe(p) = Pipe::of(d);
            // As many whole tenths as fit, or as many as a header holds.
            assert!(tenth * p <= d, "{d:?} as {p}");
            if p < u32::MAX {
                assert!(d < tenth * (p + 1), "{d:?} as {p}");
            }
        }
    }

    /// One case of web/src/video-header.json.
    #[derive(serde::Deserialize)]
    struct Case {
        header: String,
        picture: String,
        coding: Coding,
        version: u32,
        pipe: u32,
    }

    fn hex(s: &str) -> Vec<u8> {
        let digits: String = s.split_whitespace().collect();
        (0..digits.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&digits[i..i + 2], 16).unwrap())
            .collect()
    }

    /// The page messages web/src/protocol.ts checks its reader against carry what the
    /// daemon writes and reads: each is the frame's track byte (the page's type byte), then
    /// the header; and the frame as the lane carries it is that page message as it is.
    #[test]
    fn the_header_is_what_the_fixture_says() {
        let cases: Vec<Case> =
            serde_json::from_str(include_str!("../web/src/video-header.json")).unwrap();
        assert!(cases.len() >= 4);
        for c in cases {
            let header = Header {
                coding: c.coding,
                version: Version(c.version),
                pipe: Pipe(c.pipe),
            };
            let page = hex(&c.header);
            assert_eq!(page[0], Track::Video.byte(), "{}", c.header);
            let bytes = &page[1..];
            assert_eq!(header.bytes().to_vec(), bytes, "{}", c.header);
            let picture = hex(&c.picture);
            let message = [bytes, &picture].concat();
            assert_eq!(
                Header::read(&message),
                Ok(Video {
                    header,
                    picture: &picture
                }),
                "{}",
                c.header
            );
            let mut framed = Vec::new();
            Frame::encode(Sequence(c.version), Track::Video, &message, &mut framed);
            let frame = Frame::decode(&framed[PREFIX..]).expect("a frame written reads");
            assert_eq!(frame.page(), [&page[..], &picture].concat());
        }
    }

    /// Two capabilities match exactly when their bytes are equal, wherever they differ.
    #[test]
    fn capabilities_match_only_themselves() {
        let mut rng = Xorshift::new(0xC0AC_29B7_C97C_50DD);
        for _ in 0..1000 {
            let b = bytes(&mut rng, CAPABILITY);
            let mut c = b.clone();
            if rng.below(2) == 0 {
                c[rng.below(CAPABILITY as u64) as usize] ^= 1 << rng.below(8);
            }
            let x = Capability(b.clone().try_into().unwrap());
            let y = Capability(c.clone().try_into().unwrap());
            assert_eq!(x.matches(&y), b == c);
        }
    }
}
