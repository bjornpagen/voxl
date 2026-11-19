//! Every message the lane and the control stream carry, and the one way each is written and
//! read.
//!
//! The lane is one stream through the transport's proxy: the session's [`Capability`] (relay
//! to daemon, the lane's first bytes and its only ones that way), then frames (daemon to
//! relay, [`Frame`]).
//! The control stream is the stdio of `voxl attach` over ssh: [`Down`] (daemon to relay) and
//! [`Up`] (relay to daemon). A frame and a control message each go after their length
//! ([`Framer`]). Numbers are big-endian, as the page's video header is.
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
/// `bin.audio` in web/src/wire.json), so a frame as the lane carries it is the page's message
/// as it is, and the relay hands it on unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Track {
    /// One video update: its header (`video::Header`) and its VP9.
    Video,
    /// One Opus packet.
    Audio,
}

const VIDEO: u8 = 1;
const AUDIO: u8 = 2;

impl Track {
    fn byte(self) -> u8 {
        match self {
            Track::Video => VIDEO,
            Track::Audio => AUDIO,
        }
    }
}

/// A frame on the lane, daemon to relay, after its length: its track's byte, then its bytes.
/// The lane is a stream: every frame arrives whole, in the order sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame<'a> {
    pub track: Track,
    pub bytes: &'a [u8],
}

impl<'a> Frame<'a> {
    /// Appends the frame of `track` and `bytes` to `out`, after its length. Its track's byte
    /// and `bytes` are at most [`MAX_MESSAGE`].
    pub fn encode(track: Track, bytes: &[u8], out: &mut Vec<u8>) {
        let start = open(out);
        out.push(track.byte());
        out.extend_from_slice(bytes);
        close(out, start);
    }

    /// Reads one frame, as [`Framer::frame`] gives it (without its length).
    pub fn decode(b: &'a [u8]) -> Result<Frame<'a>, Malformed> {
        let mut r = Reader(b);
        let track = match r.u8()? {
            VIDEO => Track::Video,
            AUDIO => Track::Audio,
            t => return Err(Malformed::Track(t)),
        };
        Ok(Frame {
            track,
            bytes: r.rest(),
        })
    }
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

const D_CAPABILITY: u8 = 1;
const D_ATTACHED: u8 = 2;
const D_TEXT: u8 = 3;
const D_BINARY: u8 = 4;

impl<'a> Down<'a> {
    /// Appends the message to `out`, after its length.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let start = open(out);
        match self {
            Down::Capability(c) => {
                out.push(D_CAPABILITY);
                out.extend_from_slice(c.0);
            }
            Down::Attached => out.push(D_ATTACHED),
            Down::Text(t) => {
                out.push(D_TEXT);
                out.extend_from_slice(t.as_bytes());
            }
            Down::Binary(b) => {
                out.push(D_BINARY);
                out.extend_from_slice(b);
            }
        }
        close(out, start);
    }

    /// Reads one message, as [`Framer::frame`] gives it (without its length).
    pub fn decode(b: &'a [u8]) -> Result<Down<'a>, Malformed> {
        let mut r = Reader(b);
        let d = match r.u8()? {
            D_CAPABILITY => {
                let bytes = <&[u8; CAPABILITY]>::try_from(r.bytes(CAPABILITY)?)
                    .map_err(|_| Malformed::Truncated)?;
                Down::Capability(CapabilityRef(bytes))
            }
            D_ATTACHED => Down::Attached,
            D_TEXT => return text(r.rest()).map(Down::Text),
            D_BINARY => return Ok(Down::Binary(r.rest())),
            t => return Err(Malformed::Tag(t)),
        };
        r.end()?;
        Ok(d)
    }
}

/// A control message from the relay to the daemon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Up<'a> {
    /// A text message from the page (input, a clipboard, ...), as the page wrote it.
    Text(&'a str),
    /// A binary message from the page (the microphone, a clipboard image).
    Binary(&'a [u8]),
}

const U_TEXT: u8 = 1;
const U_BINARY: u8 = 2;

impl<'a> Up<'a> {
    /// Appends the message to `out`, after its length.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let start = open(out);
        match self {
            Up::Text(t) => {
                out.push(U_TEXT);
                out.extend_from_slice(t.as_bytes());
            }
            Up::Binary(b) => {
                out.push(U_BINARY);
                out.extend_from_slice(b);
            }
        }
        close(out, start);
    }

    /// Reads one message, as [`Framer::frame`] gives it (without its length).
    pub fn decode(b: &'a [u8]) -> Result<Up<'a>, Malformed> {
        let mut r = Reader(b);
        match r.u8()? {
            U_TEXT => text(r.rest()).map(Up::Text),
            U_BINARY => Ok(Up::Binary(r.rest())),
            t => Err(Malformed::Tag(t)),
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
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], Malformed> {
        if self.0.len() < n {
            return Err(Malformed::Truncated);
        }
        let (b, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(b)
    }

    fn u8(&mut self) -> Result<u8, Malformed> {
        Ok(self.bytes(1)?[0])
    }

    fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.0)
    }

    fn end(&self) -> Result<(), Malformed> {
        match self.0 {
            [] => Ok(()),
            _ => Err(Malformed::Trailing),
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
        let track = match rng.below(2) {
            0 => Track::Audio,
            _ => Track::Video,
        };
        let n = rng.below(3000) as usize;
        let data = bytes(rng, n);
        let mut out = Vec::new();
        Frame::encode(track, &data, &mut out);
        out[PREFIX..].to_vec()
    }

    fn down(rng: &mut Xorshift) -> Vec<u8> {
        let mut out = Vec::new();
        let text: String = (0..rng.below(50))
            .map(|_| char::from_u32(rng.range(32, 0x2FFF) as u32).unwrap_or('x'))
            .collect();
        let bin = {
            let n = rng.below(100) as usize;
            bytes(rng, n)
        };
        let cap = Capability(bytes(rng, CAPABILITY).try_into().unwrap());
        match rng.below(4) {
            0 => Down::Capability(cap.lend()).encode(&mut out),
            1 => Down::Attached.encode(&mut out),
            2 => Down::Text(&text).encode(&mut out),
            _ => Down::Binary(&bin).encode(&mut out),
        }
        out[PREFIX..].to_vec()
    }

    fn up(rng: &mut Xorshift) -> Vec<u8> {
        let mut out = Vec::new();
        match rng.below(2) {
            0 => Up::Text("k,65,1").encode(&mut out),
            _ => Up::Binary(&{
                let n = rng.below(100) as usize;
                bytes(rng, n)
            })
            .encode(&mut out),
        }
        out[PREFIX..].to_vec()
    }

    /// Each kind's decoder re-encoded: `None` if it refuses the bytes, else the bytes its
    /// decoding encodes to.
    fn again(kind: usize, b: &[u8]) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        match kind {
            0 => Frame::decode(b)
                .ok()
                .map(|f| Frame::encode(f.track, f.bytes, &mut out)),
            1 => Down::decode(b).ok().map(|d| d.encode(&mut out)),
            _ => Up::decode(b).ok().map(|u| u.encode(&mut out)),
        }?;
        Some(out[PREFIX..].to_vec())
    }

    /// Every message of every kind decodes to what encodes it again, byte for byte; and
    /// every input at all (random bytes, valid messages with bytes flipped, cut or added)
    /// either is refused with a typed error or is such a message: nothing panics, and nothing
    /// decodes to a message that is not exactly its bytes.
    #[test]
    fn every_input_is_a_message_or_a_typed_error() {
        let mut rng = Xorshift::new(0x082E_FA98_EC4E_6C89);
        let makers: [fn(&mut Xorshift) -> Vec<u8>; 3] = [frame, down, up];
        for _ in 0..20_000 {
            let kind = rng.below(3) as usize;
            let valid = makers[kind](&mut rng);
            assert_eq!(
                again(kind, &valid).as_deref(),
                Some(&valid[..]),
                "kind {kind}"
            );
            let mut b = match rng.below(4) {
                0 => {
                    let n = rng.below(1500) as usize;
                    bytes(&mut rng, n)
                }
                _ => valid.clone(),
            };
            match rng.below(4) {
                0 if !b.is_empty() => {
                    let i = rng.below(b.len() as u64) as usize;
                    b[i] ^= 1 << rng.below(8);
                }
                1 => b.truncate(rng.below(b.len() as u64 + 1) as usize),
                2 => b.extend({
                    let n = rng.range(1, 9) as usize;
                    bytes(&mut rng, n)
                }),
                _ => {
                    if !b.is_empty() {
                        b[0] = rng.below(8) as u8;
                    }
                }
            }
            for k in 0..3 {
                if let Some(out) = again(k, &b) {
                    assert_eq!(out, b, "kind {k} decoded bytes it does not encode to");
                }
            }
        }
    }

    /// Every strict prefix of a message whose fields fix its length (the daemon's capability
    /// and attached) is refused. (A frame's and a text's length is the stream's framing's,
    /// not their own fields'.)
    #[test]
    fn every_cut_message_is_refused() {
        let mut rng = Xorshift::new(0x4528_21E6_38D0_1377);
        for _ in 0..2000 {
            let valid = loop {
                let d = down(&mut rng);
                if !matches!(d[0], D_TEXT | D_BINARY) {
                    break d;
                }
            };
            for cut in 0..valid.len() {
                assert!(again(1, &valid[..cut]).is_none(), "cut at {cut}");
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
                .map(|_| match case % 2 {
                    0 => up(&mut rng),
                    _ => frame(&mut rng),
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
