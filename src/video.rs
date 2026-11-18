//! Video pipeline: damage-driven capture -> I420 -> x264 -> each session's lanes, a capture
//! held while a session's lanes have no room for it.

#[cfg(target_os = "linux")]
use crate::capture::Capturer;
use crate::convert::I420;
use crate::encoder::{
    Encoded, Encoder, EncoderConfig, Fps, Pictures, Planes, Pts, Quality, Settling, DPB,
};
use crate::env::{Debugging, Marker};
#[cfg(target_os = "linux")]
use crate::hub::wait_any;
#[cfg(target_os = "linux")]
use crate::hub::Seen;
#[cfg(target_os = "linux")]
use crate::hub::Viewer;
#[cfg(target_os = "linux")]
use crate::hub::Waitable;
use crate::hub::{Client, Hub};
use crate::lanes::wire::{Epoch, Number};
use crate::path::{Bits, Kbps};
#[cfg(target_os = "linux")]
use crate::session::sender::{KeyRoom, Room};
use bytes::{BufMut, BytesMut};
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // read by the Linux-only capture loop
#[derive(Clone)]
pub struct VideoConfig {
    /// The X display.
    pub display: String,
    pub fps: Fps,
    /// x264's sliced threads.
    pub threads: u32,
    /// The threads that convert a capture to I420.
    pub convert_threads: usize,
    pub crf: f32,
    /// The most the encoder sends: what it opens capped at.
    pub ceiling: Kbps,
    pub paintover_crf: f32,
    /// The debugging variables (`VOXL_*`), read when voxl started.
    pub debug: Debugging,
}

#[cfg(target_os = "linux")]
/// The most frames a second. Provisional: the refresh of the common display, which is
/// the most a viewer's screen shows of most desktops; what would measure it is the
/// bench's glass-to-glass frame rate and CPU at 60 against 120 on a 120 Hz viewer.
const FRAMERATE: Fps = match Fps::new(60) {
    Some(fps) => fps,
    None => panic!("60 frames a second is past Fps::MAX"),
};

#[cfg(target_os = "linux")]
/// The quality a change is coded at (x264's CRF, lower is sharper): upstream's default,
/// provisional; what would measure it is the bench's bytes per frame against a viewer's
/// judgement of text at each value.
const CRF: f32 = 24.0;

#[cfg(target_os = "linux")]
/// The quality still blocks are sharpened to, past `CRF`: upstream's default,
/// provisional, measured as `CRF` would be.
const PAINTOVER_CRF: f32 = 16.0;

#[cfg(target_os = "linux")]
/// The most kbit/s the encoder sends: what x264's VBV lets a frame take, never what the
/// lanes carry. Provisional: upstream's default since voxl's first release; what would
/// measure it is the largest frame a full-screen change codes to at `CRF`.
const MAX_KBPS: f64 = 30_000.0;

#[cfg(target_os = "linux")]
/// The threads converting a capture to I420: libyuv's row converters each take a band of
/// rows. Provisional: upstream's choice; what would measure it is the bench's convert
/// time per frame at 1, 2, 4 and 8.
const CONVERT_THREADS: usize = 4;

#[cfg(target_os = "linux")]
/// x264's sliced threads on a machine of `cores`: half of them, one to four. Measured
/// with `bench_hot_path` (one 3024x1898 picture at 60 captures a second, typing and
/// scrolling) on a 12-core laptop, latency p50 of a capture, typing / scrolling, and the
/// CPU each second of screen took: 1 thread 8.2 / 12.2 ms (0.50 / 0.72 CPU s), 2 threads
/// 3.9 / 5.4 ms (0.49 / 0.69), 3 threads 2.4 / 3.7 ms (0.42 / 0.65), 4 threads 2.5 / 3.1 ms
/// (0.51 / 0.66), 6 threads 2.1 / 2.7 ms (0.55 / 0.73). Past two the latency halves no
/// more, and past four each thread costs CPU for a fraction of a millisecond; under four
/// cores, half leaves the capture loop, the sessions and the desktop theirs. Provisional
/// on the server's own cores until the benchmark runs there.
fn encode_threads(cores: std::num::NonZeroUsize) -> u32 {
    (cores.get() as u32 / 2).clamp(1, 4)
}

#[cfg(target_os = "linux")]
impl VideoConfig {
    /// The daemon's: every value above, and `ENCODE_THREADS` of `cores`.
    pub fn new(display: String, debug: Debugging, cores: std::num::NonZeroUsize) -> Self {
        VideoConfig {
            display,
            fps: FRAMERATE,
            threads: encode_threads(cores),
            convert_threads: CONVERT_THREADS,
            crf: CRF,
            ceiling: Kbps::new(MAX_KBPS),
            paintover_crf: PAINTOVER_CRF,
            debug,
        }
    }
}

/// What a video frame says before its H.264, on the lanes: the encode loop writes it, and
/// the recorder, the relay (which hands it to the page) and web/src/protocol.ts read it,
/// each checking its reader against web/src/video-header.json. What a frame is (video or
/// audio) the lane's `Track` says: the page's type byte is the relay's to add.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Header {
    /// The frame is a keyframe.
    pub key: bool,
    /// The frame sharpens what it codes (the bench's "paint-over").
    pub sharpens: bool,
    /// The frame's number: the encode loop's count of the frames it sent, wrapping.
    pub frame: u32,
    pub pipe: Pipe,
}

/// Capture to send, as a video frame carries it: in tenths of a millisecond, at most
/// `u32::MAX` of them (119 hours).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pipe(u32);

impl Pipe {
    pub fn of(d: Duration) -> Pipe {
        Pipe((d.as_micros() / 100).min(u32::MAX as u128) as u32)
    }

    /// In whole milliseconds, rounded down.
    pub fn millis(self) -> u64 {
        self.0 as u64 / 10
    }
}

impl Header {
    /// The H.264 starts this far into a frame.
    pub const LEN: usize = 9;
    /// The flags byte's bits.
    const KEY: u8 = 1;
    const SHARPENS: u8 = 2;

    /// Flags, frame, pipe: the integers big-endian.
    pub fn bytes(&self) -> [u8; Header::LEN] {
        let mut b = [0; Header::LEN];
        b[0] = if self.key { Header::KEY } else { 0 }
            | if self.sharpens { Header::SHARPENS } else { 0 };
        b[1..5].copy_from_slice(&self.frame.to_be_bytes());
        b[5..9].copy_from_slice(&self.pipe.0.to_be_bytes());
        b
    }

    /// What frame `m` is to a reader of video: the recorder, and the fixture's check of
    /// the writer.
    pub fn read(m: &[u8]) -> Result<Video<'_>, Short> {
        match m.split_first_chunk::<{ Header::LEN }>() {
            None => Err(Short),
            Some((b, [])) => Ok(Video::Empty(Header::of(b))),
            Some((b, picture)) => Ok(Video::Picture(Header::of(b), picture)),
        }
    }

    fn of(b: &[u8; Header::LEN]) -> Header {
        Header {
            key: b[0] & Header::KEY != 0,
            sharpens: b[0] & Header::SHARPENS != 0,
            frame: u32::from_be_bytes([b[1], b[2], b[3], b[4]]),
            pipe: Pipe(u32::from_be_bytes([b[5], b[6], b[7], b[8]])),
        }
    }
}

/// A video frame, read.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Video<'a> {
    /// Its header, and its H.264 after it.
    Picture(Header, &'a [u8]),
    /// A header with nothing after it. The encode loop sends no frame that coded nothing.
    Empty(Header),
}

/// Bytes too few for a video frame's header.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Short;

struct Job {
    /// The picture: the encode loop's while it codes it, and the capture loop's again once
    /// it comes back.
    buf: Box<I420>,
    frame: Frame,
    /// The bytes it may take, as the sessions' lanes had room when it was captured.
    allowance: usize,
    /// When the screen first changed for it: its x264 timestamp, and the start of its
    /// pipeline time (`Pipe`).
    damage: Instant,
    /// What the bench loadgen's marker said when the frame was captured, while
    /// `VOXL_DEBUG_MARKER` is set (see `marker_reading`).
    marker_ms: Option<u32>,
}

/// What a captured frame is for. Each kind carries what only it has.
enum Frame {
    /// A new viewer, or a decoder that lost its place: every stripe, from nothing.
    /// `blocks`: it went out in place of a sharpening pass, so it is coded at paint-over
    /// quality and sharpens every block, as that pass would have.
    /// `starts`: the viewers its plan promised it to, which it starts if it comes out whole.
    Key {
        blocks: Option<Sharpen>,
        starts: Vec<Arc<Client>>,
    },
    /// What changed on screen, and which blocks.
    Motion { dirty: Vec<u8> },
    /// Nothing moved and the lanes have room to spare: re-encode `blocks` at paint-over
    /// quality.
    Sharpen { blocks: Sharpen },
}

impl Frame {
    /// How the encoder codes it.
    fn coding(&self) -> Coding {
        match self {
            Frame::Key { blocks, .. } => Coding::Key {
                still: blocks.is_some(),
            },
            Frame::Motion { .. } => Coding::Motion,
            Frame::Sharpen { .. } => Coding::Sharpen,
        }
    }

    /// Whether it is coded at paint-over quality (a flag in each message's header).
    fn sharpens(&self) -> bool {
        match self {
            Frame::Key { blocks, .. } => blocks.is_some(),
            Frame::Motion { .. } => false,
            Frame::Sharpen { .. } => true,
        }
    }

    /// The blocks x264 analyses.
    fn mb_dirty(&self) -> Dirty<'_> {
        match self {
            Frame::Key { .. } => Dirty::All,
            Frame::Motion { dirty, .. } => Dirty::Blocks(dirty),
            Frame::Sharpen { blocks, .. } => Dirty::Blocks(&blocks.map),
        }
    }

    /// Its parts once coded: the blocks it sharpens, cleared once it came out sharp; the
    /// viewers it starts; and its map of changed blocks, to be used again.
    fn into_parts(self) -> Parts {
        match self {
            Frame::Motion { dirty } => Parts {
                blocks: None,
                starts: None,
                dirty: Some(dirty),
            },
            Frame::Key { blocks, starts } => Parts {
                blocks,
                starts: Some(starts),
                dirty: None,
            },
            Frame::Sharpen { blocks } => Parts {
                blocks: Some(blocks),
                starts: None,
                dirty: None,
            },
        }
    }
}

/// A coded frame's parts, as `Frame::into_parts` gives them.
struct Parts {
    blocks: Option<Sharpen>,
    starts: Option<Vec<Arc<Client>>>,
    dirty: Option<Vec<u8>>,
}

/// The blocks a frame's encoder analyses.
#[derive(Clone, Copy)]
enum Dirty<'a> {
    /// Every block: a keyframe.
    All,
    /// Those the map marks (1 = changed), one byte per 16x16 block, row-major.
    Blocks(&'a [u8]),
}

/// What a stripe worker is asked to code. The worker turns the kind into a CRF.
#[derive(Clone, Copy)]
enum Coding {
    /// An IDR, which may take what the slowest viewer's budget sends in `KEY_TIME`. `still`:
    /// in place of a sharpening pass, so at paint-over quality.
    Key { still: bool },
    /// Motion, at the configured CRF, which MotionQuality may lower.
    Motion,
    /// Sharpening, at the paint-over CRF.
    Sharpen,
}

impl Coding {
    /// The ceiling a frame's own encoder opens at, if it gets one: a keyframe under a cap.
    /// The running encoder can give a keyframe only what its last frame left; with no
    /// cap there is nothing to give, and an encoder opened capped would cap the stream
    /// for good.
    fn own_encoder(self, ceiling: Option<Kbps>) -> Option<Kbps> {
        match self {
            Coding::Key { .. } => ceiling,
            Coding::Motion | Coding::Sharpen => None,
        }
    }
}

/// The two CRFs a pipeline codes at, for its whole life.
#[derive(Clone, Copy)]
struct Crfs {
    configured: f32,
    paintover: f32,
}

impl Crfs {
    fn of(&self, coding: Coding) -> f32 {
        match coding {
            Coding::Key { still: false } | Coding::Motion => self.configured,
            Coding::Key { still: true } | Coding::Sharpen => self.paintover,
        }
    }
}

/// Rows `y0..y1` of the screen.
#[derive(Clone, Copy, Debug)]
pub struct Band {
    pub y0: u32,
    pub y1: u32,
}

/// What a capture is sent as, decided before a buffer is taken for it.
enum Plan {
    /// A keyframe, and what it is to sharpening.
    Key(Keyframe),
    /// Something moved.
    Motion,
    /// Nothing moved, and the lanes have room past what the next capture needs: a pass of
    /// the pending blocks.
    Sharpen,
}

/// Blocks a frame sharpens, as the Refiner saw them when the frame was captured.
struct Sharpen {
    map: Vec<u8>,
    /// When the frame was captured: blocks that changed after it stay pending.
    at: Instant,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // read by the Linux-only capture loop
/// A buffer back from the encoder, what came of the blocks its frame sharpened, and the
/// lists its frame carried, empty, for the next frames to fill: a frame allocates none.
struct Returned {
    buf: Box<I420>,
    sharpened: Sharpened,
    /// A motion frame's map of changed blocks.
    dirty: Option<Vec<u8>>,
    /// A keyframe's list of the viewers it starts.
    starts: Option<Vec<Arc<Client>>>,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // read by the Linux-only capture loop
/// What came of the blocks a frame sharpened.
enum Sharpened {
    /// It sharpened none.
    Nothing,
    /// They came out at paint-over quality: they are sharp.
    Sharp(Sharpen),
    /// The rate cap squeezed them: they stay pending, and the next pass takes fewer.
    Squeezed(Sharpen),
}

#[cfg(target_os = "linux")]
fn union(a: Option<Band>, b: Band) -> Band {
    match a {
        None => b,
        Some(a) => Band {
            y0: a.y0.min(b.y0),
            y1: a.y1.max(b.y1),
        },
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // read by the Linux-only capture loop
/// What the viewers watching are sent: the encoders, and the buffer captures are
/// converted into for them. There is one only while somebody watches.
///
/// One buffer, since a capture goes only to an idle encoder (`step`): a second, filled
/// while the encoder codes the first, only queued a frame behind it, and converted every
/// band twice, once into each.
struct Stream {
    /// Dropped first (fields drop in order), which ends the encode loop once it has sent
    /// what it was given.
    job_tx: mpsc::SyncSender<Job>,
    ret_rx: mpsc::Receiver<Returned>,
    buf: Buffer<Box<I420>>,
    /// The rows that changed on screen since the buffer was last converted into: the
    /// whole screen, until the first capture.
    pending: Option<Band>,
    /// Block maps and viewer lists back from the encoder, used again: the stream's
    /// frames allocate none once there are as many as are in flight.
    spare: Spare,
    /// Dropped last: waits until the encode loop has stopped.
    _encode: Stopped,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // read by the Linux-only capture loop
/// Lists a frame carries, kept between frames.
#[derive(Default)]
struct Spare {
    maps: Vec<Vec<u8>>,
    starts: Vec<Vec<Arc<Client>>>,
}

#[cfg(target_os = "linux")]
impl Spare {
    /// A map of `blocks` blocks, every one 0.
    fn map(&mut self, blocks: usize) -> Vec<u8> {
        let mut map = self.maps.pop().unwrap_or_default();
        map.clear();
        map.resize(blocks, 0);
        map
    }
    /// An empty list of viewers.
    fn starts(&mut self) -> Vec<Arc<Client>> {
        self.starts.pop().unwrap_or_default()
    }
}

/// A thread that is waited for when this is dropped. The thread holds the sending side
/// of a channel nothing is sent on, which closes when the thread's work returns.
struct Stopped(mpsc::Receiver<std::convert::Infallible>);

impl Stopped {
    /// `work` on a thread named `name`; a thread that cannot be started is the error.
    fn spawn(name: &str, work: impl FnOnce() + Send + 'static) -> Result<Stopped, String> {
        let (running, stopped) = mpsc::channel();
        std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let _running = running;
                work()
            })
            .map_err(|e| format!("no {name} thread: {e}"))?;
        Ok(Stopped(stopped))
    }
}

impl Drop for Stopped {
    fn drop(&mut self) {
        // Nothing is sent on it: it ends, empty, when the thread's work has returned.
        if let Ok(never) = self.0.recv() {
            match never {}
        }
    }
}

impl Stream {
    /// The encoder for a screen of `w` x `h`, on its thread, and the buffer, which holds
    /// nothing of it yet.
    fn new(cfg: &VideoConfig, hub: &Arc<Hub>, w: u32, h: u32) -> Result<Self, String> {
        log::info!("streaming");
        let (job_tx, job_rx) = mpsc::sync_channel::<Job>(1);
        // One picture goes round, so no more than one comes back at once.
        let (ret_tx, ret_rx) = mpsc::sync_channel::<Returned>(1);
        let picture = Picture {
            cfg: EncoderConfig {
                pictures: Pictures {
                    width: w,
                    height: h,
                    fps: cfg.fps,
                    threads: cfg.threads,
                    debug: cfg.debug,
                },
                crf: cfg.crf,
                max_kbps: Some(cfg.ceiling),
            },
            running: None,
        };
        let hub = hub.clone();
        let crfs = Crfs {
            configured: cfg.crf,
            paintover: cfg.paintover_crf,
        };
        let (fps, debug) = (cfg.fps, cfg.debug);
        let encode = encoder_thread(hub.clone(), move || {
            encode_loop(picture, fps, job_rx, ret_tx, hub, crfs, debug)
        })?;
        Ok(Stream {
            job_tx,
            ret_rx,
            buf: Buffer::Home(Box::new(I420::new(w as usize, h as usize))),
            pending: Some(Band { y0: 0, y1: h }),
            spare: Spare::default(),
            _encode: encode,
        })
    }

    #[cfg(target_os = "linux")]
    /// The buffer the encoder is done with: back home, and the blocks it sharpened are
    /// sharp.
    fn returned(&mut self, r: Returned, refiner: &mut Refiner) {
        self.buf = Buffer::Home(r.buf);
        let sharpened = match r.sharpened {
            Sharpened::Nothing => None,
            Sharpened::Sharp(s) => {
                refiner.sharpened(&s.map, s.at);
                Some(s.map)
            }
            Sharpened::Squeezed(s) => {
                refiner.squeezed(&s.map);
                Some(s.map)
            }
        };
        self.spare.maps.extend(sharpened.into_iter().chain(r.dirty));
        if let Some(mut starts) = r.starts {
            starts.clear();
            self.spare.starts.push(starts);
        }
    }
}

#[cfg(target_os = "linux")]
/// Runs the pipeline. With a `Screen` (dynamic resolution), the capture loop gives a
/// client's size request to the screen; only if the screen's size really changes does
/// the loop end: the encoder drains, the pipeline is rebuilt at the new size, and clients
/// get the new geometry and a keyframe.
pub fn spawn(
    cfg: VideoConfig,
    hub: Arc<Hub>,
    mut screen: Option<crate::screen::Screen>,
) -> Result<(), String> {
    if let Some(s) = &screen {
        *hub.resize_max.lock() = Some(s.max());
    }
    let mut cap = build(&cfg, &hub)?;
    std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || loop {
            if let Err(e) = capture_loop(&mut cap, cfg.clone(), hub.clone(), screen.as_mut()) {
                match cap.geometry_changed() {
                    Ok(true) => log::info!("capture geometry changed: {e}"),
                    Ok(false) => {
                        log::error!("capture loop ended: {e}");
                        crate::logging::exit(1);
                    }
                    Err(asked) => {
                        log::error!("capture loop ended: {e}; its geometry: {asked}");
                        crate::logging::exit(1);
                    }
                }
            }
            drop(cap);
            cap = match build(&cfg, &hub) {
                Ok(cap) => cap,
                Err(e) => {
                    log::error!("video restart failed: {e}");
                    crate::logging::exit(1);
                }
            };
        })
        .map_err(|e| format!("no capture thread: {e}"))?;
    Ok(())
}

#[cfg(target_os = "linux")]
/// The capture of the screen as it is now, and its geometry for the viewers.
fn build(cfg: &VideoConfig, hub: &Arc<Hub>) -> Result<Capturer, String> {
    let cap = Capturer::new(&cfg.display, cfg.debug, cfg.convert_threads)?;
    let (w, h) = (cap.width, cap.height);
    hub.width.store(w, Ordering::Relaxed);
    hub.height.store(h, Ordering::Relaxed);
    log::info!(
        "capturing {}x{} @ {} fps, threads {}",
        w,
        h,
        cfg.fps.get(),
        cfg.threads
    );
    // New geometry (and a keyframe) for clients already connected; no-op at startup.
    hub.restart_clients();
    Ok(cap)
}

impl Gated {
    #[cfg(target_os = "linux")]
    /// What the look at the viewers saw of their lanes: the least room of them, or held
    /// by the first with none, until a report makes some (a wake). A recording bounds
    /// nothing; with none but recordings, a frame may take `most`, what the encoders' cap
    /// lets one frame have.
    fn of(rooms: impl IntoIterator<Item = Room>, most: usize) -> Gated {
        let mut least = most;
        for room in rooms {
            match room {
                Room::Full => return Gated::Held,
                Room::Open(room) => least = least.min(room),
                Room::Unbounded => {}
            }
        }
        Gated::Open(least)
    }
}

/// Regional sharpening. Blocks encoded while they changed carry motion-level quality, and
/// are pending; a pending block is re-encoded at paint-over quality by a pass, which goes
/// only into room the lanes have past what the next capture needs (`Gated` of the viewers'
/// spare room): no time decides it. A block stays pending until a pass that re-encoded it
/// came out at paint-over quality (`sharpened`); one the rate cap squeezed leaves its blocks
/// pending and halves the next pass, oldest blocks first, until a pass fits.
struct Refiner {
    blocks: Vec<Block>,
    /// How many pending blocks the next pass takes.
    size: Size,
    /// A pass's scratch: the pending blocks, by when they changed.
    order: Vec<(Instant, usize)>,
}

/// A macroblock, as sharpening sees it.
#[derive(Clone, Copy)]
enum Block {
    Sharp,
    /// Sent at motion quality, or by a pass the cap squeezed; last changed at `changed`,
    /// which orders the pending oldest first.
    Pending {
        changed: Instant,
    },
}

/// How many pending blocks a pass takes: all of them, or (after a pass the cap squeezed)
/// the oldest so many, half of what that pass took. Halving finds the pass that fits in as
/// many passes as the bits of the count; one block is the least.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Size {
    All,
    Oldest(std::num::NonZeroUsize),
}

/// Whether any block waits for a pass.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Pending {
    Sharp,
    Blocks,
}

impl Refiner {
    fn new(blocks: usize) -> Self {
        Refiner {
            blocks: vec![Block::Sharp; blocks],
            size: Size::All,
            order: Vec::with_capacity(blocks),
        }
    }
    fn mark(&mut self, dirty: &[u8], now: Instant) {
        for (b, &d) in self.blocks.iter_mut().zip(dirty) {
            if d != 0 {
                *b = Block::Pending { changed: now };
            }
        }
    }
    fn mark_all(&mut self, now: Instant) {
        self.blocks.fill(Block::Pending { changed: now });
    }
    fn pending(&self) -> Pending {
        match self
            .blocks
            .iter()
            .any(|b| matches!(b, Block::Pending { .. }))
        {
            true => Pending::Blocks,
            false => Pending::Sharp,
        }
    }
    /// Every block, as the blocks a frame captured at `now` sharpens: a keyframe codes
    /// them all.
    fn every_block(&self, now: Instant, mut map: Vec<u8>) -> Sharpen {
        map.clear();
        map.resize(self.blocks.len(), 1);
        Sharpen { map, at: now }
    }
    /// A frame captured at `at` coded `map`'s blocks at paint-over quality: they are
    /// sharp now, unless they changed again since, and the next pass takes all there are.
    fn sharpened(&mut self, map: &[u8], at: Instant) {
        for (b, &m) in self.blocks.iter_mut().zip(map) {
            if let Block::Pending { changed } = *b {
                if m != 0 && changed <= at {
                    *b = Block::Sharp;
                }
            }
        }
        self.size = Size::All;
    }
    /// A pass of `map`'s blocks came out coarser than asked: the next takes the oldest
    /// half of them.
    fn squeezed(&mut self, map: &[u8]) {
        // Half, rounded up: a pass takes at least one block.
        let took = map.iter().filter(|&&m| m != 0).count();
        self.size =
            Size::Oldest(std::num::NonZeroUsize::MIN.saturating_add(took.saturating_sub(1) / 2));
    }
    /// The blocks the next pass takes (1 = encode), oldest first as `size` says, written
    /// over `map`; none when every block is sharp. Its scratch is the refiner's own, so a
    /// pass allocates nothing once it has had as many blocks pending.
    fn pass(&mut self, map: &mut Vec<u8>) -> Pending {
        let order = &mut self.order;
        order.clear();
        order.extend(
            self.blocks
                .iter()
                .enumerate()
                .filter_map(|(i, b)| match *b {
                    Block::Pending { changed } => Some((changed, i)),
                    Block::Sharp => None,
                }),
        );
        let take = match self.size {
            Size::All => order.len(),
            Size::Oldest(n) => n.get().min(order.len()),
        };
        order.sort_unstable();
        map.clear();
        map.resize(self.blocks.len(), 0);
        for &(_, i) in &order[..take] {
            map[i] = 1;
        }
        match take {
            0 => Pending::Sharp,
            _ => Pending::Blocks,
        }
    }
}

/// What a keyframe is to sharpening.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Keyframe {
    /// For a viewer or a decoder that needs one: every block at keyframe quality, pending
    /// sharpening after it.
    Fresh,
    /// While blocks pend and nothing moved: coded at paint-over quality, sharpening every
    /// block.
    Pass,
}

/// Captures may come this share of the frame interval apart, so that a source's jitter
/// does not alias into dropped frames. Upstream's guess, unmeasured. It caps the capture
/// rate at the frame rate over it (70.6 a second at 60): at 1, a frame that lands a
/// little early waits a whole interval; at 0.7, up to 86 captures a second.
const MIN_INTERVAL_SHARE: f64 = 0.85;

/// What the capture loop does next.
#[derive(Debug, PartialEq)]
enum Step<B> {
    /// Wait, the buffer where it was.
    Wait(Wait, Buffer<B>),
    /// Capture a change or a pass into the buffer that was home, a frame that may take this
    /// many bytes.
    Capture(Next, B, usize),
}

/// What the capture loop waits for before it looks again. Damage wakes it through the
/// capturer; the hub's waker, through a viewer's command or ack, a viewer leaving, a
/// screenshot or recording asked for, or a buffer coming home from the encoder.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Wait {
    /// Damage or a wake: nothing is due.
    Change,
    /// The rest of the least interval since the capture that gates this one (`Captured`),
    /// or what the capture it holds back is woken by (`Next::interval_ends`).
    Interval { rest: Duration, next: Next },
    /// The lanes: a report that makes room, a viewer going or a keyframe asked for (a
    /// wake), or damage to take in. No time: a capture held by the lanes waits for what
    /// they answer, and with nothing answered nothing goes.
    Gate,
    /// A buffer back from the encoder, which wakes the waker as it hands it back; damage
    /// waits for the capture this leads to.
    Buffer,
}

/// What a wait wakes on besides its time.
#[derive(Clone, Copy, Debug, PartialEq)]
enum On {
    /// Damage or the hub's waker.
    Event,
    /// The hub's waker only.
    Wake,
}

impl Wait {
    #[cfg(target_os = "linux")]
    fn wait(self, cap: &impl Capture, hub: &Hub) -> Result<(), String> {
        let mut waited = Ok(());
        let poll = |on, timeout| {
            waited = match on {
                On::Event => wait_any([cap.fd(), hub.waker.waitable()], timeout),
                On::Wake => wait_any([hub.waker.waitable()], timeout),
            }
        };
        self.through(poll);
        waited.map_err(|e| format!("the capture loop's wait: {e}"))
    }

    /// The wait, made through `poll`: until its time, or with no limit.
    fn through(self, poll: impl FnOnce(On, Option<Duration>)) {
        match self {
            Wait::Change => poll(On::Event, None),
            Wait::Interval { rest, next } => poll(next.interval_ends(), Some(rest)),
            Wait::Gate => poll(On::Event, None),
            Wait::Buffer => poll(On::Wake, None),
        }
    }
}

/// Whether a viewer waits for a keyframe.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Key {
    /// One does: the keyframe goes to every viewer, and it may take this many bytes, what
    /// the slowest of their budgets sends in `KEY_TIME`.
    Wanted {
        bytes: usize,
    },
    NotWanted,
}

impl Key {
    #[cfg(target_os = "linux")]
    /// What the look at the viewers saw: a keyframe is wanted if one waits for it, and it
    /// goes to every viewer, so the slowest of their budgets bounds it; a recording bounds
    /// nothing, and with none but recordings it may take `most`.
    fn of(seen: &[(Arc<Client>, Seen)], most: usize) -> Key {
        let wanted = seen.iter().any(|(_, v)| v.viewer == Viewer::Waiting);
        let bytes = seen.iter().fold(most, |least, (_, v)| match v.key {
            KeyRoom::Bytes(b) => least.min(b),
            KeyRoom::Unbounded => least,
        });
        match wanted {
            true => Key::Wanted { bytes },
            false => Key::NotWanted,
        }
    }
}

/// What changed on screen since the last capture, as each platform's capturer holds it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Damage {
    None,
    /// To be captured though no area was reported: a new capturer, or the screen grabbed
    /// whole for a new stream.
    Unreported,
    /// Rows `y0..y1` drawn, the first of them at `first`.
    Rows {
        first: Instant,
        y0: u32,
        y1: u32,
    },
}

impl Damage {
    /// Rows `top..bot` were drawn at `now`.
    pub(crate) fn add(&mut self, top: u32, bot: u32, now: Instant) {
        *self = match *self {
            Damage::Rows { first, y0, y1 } => Damage::Rows {
                first,
                y0: y0.min(top),
                y1: y1.max(bot),
            },
            Damage::None | Damage::Unreported => Damage::Rows {
                first: now,
                y0: top,
                y1: bot,
            },
        };
    }

    /// To be captured whether or not anything is drawn.
    pub(crate) fn mark(&mut self) {
        if let Damage::None = self {
            *self = Damage::Unreported;
        }
    }

    /// The rows drawn since the last take, aligned to even rows for 4:2:0 within a
    /// picture `height` tall; nothing is left.
    pub(crate) fn take(&mut self, height: u32) -> Option<Band> {
        match std::mem::replace(self, Damage::None) {
            Damage::Rows { y0, y1, .. } => Some(Band {
                y0: y0 & !1,
                y1: ((y1 + 1) & !1).min(height),
            }),
            Damage::None | Damage::Unreported => None,
        }
    }
}

/// Whether the sessions' lanes let a capture go: open while every viewer has room.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Gated {
    /// With room for a frame of this many bytes.
    Open(usize),
    /// Held by a viewer whose lanes are full: only a report, a viewer coming or going or
    /// a keyframe asked for can open the gate, and each of those wakes the hub.
    Held,
}

/// Where the capture loop's buffer is: home, with what is home (the buffer itself in the
/// loop; nothing in a test of what the loop decides), or away at the encoder.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Buffer<B> {
    Home(B),
    Away,
}

/// What the capture loop knows when it looks, while somebody watches.
struct Look<B> {
    key: Key,
    damage: Damage,
    pending: Pending,
    now: Instant,
    captured: Captured,
    /// The least time between two captures.
    min_interval: Duration,
    gate: Gated,
    /// The room the lanes have past what the next capture needs: what a pass may take.
    spare: Gated,
    buffer: Buffer<B>,
}

/// What the loop does next, from what it knows. A capture takes the buffer, so one is
/// made only when it is home; a wait leaves it where it was.
fn step<B>(look: Look<B>) -> Step<B> {
    let Look {
        key,
        damage,
        pending,
        now,
        captured,
        min_interval,
        gate,
        spare,
        buffer,
    } = look;
    let next = match (damage, key) {
        // Nothing moved and no keyframe is wanted: a pass of the pending blocks, into the
        // room the lanes have past what the next capture needs. A pass goes only to an
        // idle encoder, as every capture does (below); until then a change may come, and
        // with it the capture is motion's. With no room to spare it waits for a report to
        // make some; with nothing pending, for a change. No time decides it.
        (Damage::None, Key::NotWanted) => match (pending, &buffer, spare) {
            (Pending::Blocks, Buffer::Home(_), Gated::Open(_)) => Next::Pass,
            (Pending::Blocks, Buffer::Home(_), Gated::Held) => {
                return Step::Wait(Wait::Gate, buffer)
            }
            (Pending::Blocks, Buffer::Away, _) | (Pending::Sharp, _, _) => {
                return Step::Wait(Wait::Change, buffer)
            }
        },
        (Damage::Unreported | Damage::Rows { .. }, Key::Wanted { .. } | Key::NotWanted)
        | (Damage::None, Key::Wanted { .. }) => Next::Change,
    };
    let since = now.saturating_duration_since(captured.gate(next));
    if since < min_interval {
        let rest = min_interval - since;
        return Step::Wait(Wait::Interval { rest, next }, buffer);
    }
    // A keyframe is not held by the gate: it may take what the slowest budget sends in
    // `KEY_TIME`. A change may take the room the viewers' lanes leave; a pass, only the
    // room past what the next capture needs.
    let allowance = match (key, next, gate, spare) {
        (Key::Wanted { bytes }, _, _, _) => bytes,
        (Key::NotWanted, Next::Change, Gated::Open(room), _) => room,
        (Key::NotWanted, Next::Pass, _, Gated::Open(room)) => room,
        (Key::NotWanted, Next::Change, Gated::Held, _)
        | (Key::NotWanted, Next::Pass, _, Gated::Held) => return Step::Wait(Wait::Gate, buffer),
    };
    // A capture goes only to an idle encoder. One made while it codes the frame before
    // would wait for it, a whole encode older by the time it is coded; made once it is
    // done, it holds what changed meanwhile, and goes at once.
    match buffer {
        Buffer::Home(buf) => Step::Capture(next, buf, allowance),
        Buffer::Away => Step::Wait(Wait::Buffer, Buffer::Away),
    }
}

/// What a capture is sent as, or why nothing is.
enum Planned {
    Send(Plan),
    /// The damage held no area after all, and nothing is pending.
    Empty,
}

/// What capture `next`, whose damage came to `band`, is sent as. A keyframe of a screen
/// with blocks pending and nothing moved is a pass of them all.
fn plan(key: Key, band: Option<Band>, next: Next, refiner: &Refiner) -> Planned {
    match (key, band, next, refiner.pending()) {
        (Key::Wanted { .. }, Some(_), _, _) | (Key::Wanted { .. }, None, _, Pending::Sharp) => {
            Planned::Send(Plan::Key(Keyframe::Fresh))
        }
        (Key::Wanted { .. }, None, _, Pending::Blocks) => Planned::Send(Plan::Key(Keyframe::Pass)),
        (Key::NotWanted, Some(_), _, _) => Planned::Send(Plan::Motion),
        (Key::NotWanted, None, Next::Pass, Pending::Blocks) => Planned::Send(Plan::Sharpen),
        (Key::NotWanted, None, Next::Pass, Pending::Sharp) => Planned::Empty,
        (Key::NotWanted, None, Next::Change, _) => Planned::Empty,
    }
}

/// What the capture loop is about to capture.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Next {
    /// Something moved, or a viewer wants a keyframe.
    Change,
    /// Nothing moved, blocks are pending, and the lanes have room to spare.
    Pass,
}

impl Next {
    /// What ends the wait for the frame interval before capturing this, besides its time.
    /// Before a pass, damage too: it makes the capture a change, which the pass does not
    /// hold back.
    fn interval_ends(self) -> On {
        match self {
            Next::Change => On::Wake,
            Next::Pass => On::Event,
        }
    }
}

/// When the last frames were captured: what holds the next one to the frame rate.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Captured {
    /// Any frame, a pass included.
    any: Instant,
    /// The last change: motion or a keyframe.
    change: Instant,
}

impl Captured {
    fn new(at: Instant) -> Self {
        Captured {
            any: at,
            change: at,
        }
    }
    /// The capture the next frame waits a frame interval after. A pass waits after any
    /// frame, so passes go at most at the frame rate and never right behind a change. A
    /// change waits only after the last change: a keystroke that comes just after a pass
    /// is captured at once and waits for that pass's encode at most, not for a frame.
    fn gate(&self, next: Next) -> Instant {
        match next {
            Next::Pass => self.any,
            Next::Change => self.change,
        }
    }
    fn on_capture(&mut self, next: Next, at: Instant) {
        self.any = at;
        match next {
            Next::Pass => {}
            Next::Change => self.change = at,
        }
    }
}

#[cfg(target_os = "linux")]
/// What the capture loop asks of the screen it captures: the platform's `Capturer`, or a
/// screen a test draws on.
trait Capture {
    /// Width and height, in pixels.
    fn size(&self) -> (u32, u32);
    /// Readable when the screen has something to report.
    fn fd(&self) -> Waitable<'_>;
    /// Takes in what the screen reported.
    fn pump(&mut self) -> Result<(), String>;
    /// The screen changed size.
    fn needs_rebuild(&self) -> bool;
    /// What changed since the last capture.
    fn damage(&mut self) -> &mut Damage;
    /// The rows damaged since the last take, if any were.
    fn take_damage(&mut self) -> Result<Option<Band>, String>;
    /// The macroblocks damaged since the last take, one byte each, row-major; `fresh`, a
    /// map of none, takes its place.
    fn take_mb_map(&mut self, fresh: Vec<u8>) -> Vec<u8>;
    /// Every row.
    fn full(&self) -> Band;
    /// The whole screen as BGRX, for a screenshot.
    fn snapshot(&mut self) -> Result<Vec<u8>, String>;
    /// Reads `band` of the screen into the frame `convert` reads.
    fn grab(&mut self, band: Band) -> Result<(), String>;
    /// Converts rows [y0, y1) of the grabbed frame into `dst`, on the capturer's threads.
    fn convert(&self, dst: &mut I420, y0: usize, y1: usize) -> Result<(), String>;
    /// The grabbed frame as BGRX and its stride (the debug marker's probe).
    fn bgrx(&self) -> Option<(&[u8], usize)>;
}

#[cfg(target_os = "linux")]
impl Capture for Capturer {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    fn fd(&self) -> Waitable<'_> {
        Capturer::fd(self)
    }
    fn pump(&mut self) -> Result<(), String> {
        Capturer::pump(self)
    }
    fn needs_rebuild(&self) -> bool {
        Capturer::needs_rebuild(self)
    }
    fn damage(&mut self) -> &mut Damage {
        &mut self.damage
    }
    fn take_damage(&mut self) -> Result<Option<Band>, String> {
        Capturer::take_damage(self)
    }
    fn take_mb_map(&mut self, fresh: Vec<u8>) -> Vec<u8> {
        Capturer::take_mb_map(self, fresh)
    }
    fn full(&self) -> Band {
        Capturer::full(self)
    }
    fn snapshot(&mut self) -> Result<Vec<u8>, String> {
        Capturer::snapshot(self)
    }
    fn grab(&mut self, band: Band) -> Result<(), String> {
        Capturer::grab(self, band)
    }
    fn convert(&self, dst: &mut I420, y0: usize, y1: usize) -> Result<(), String> {
        Capturer::convert(self, dst, y0, y1)
    }
    fn bgrx(&self) -> Option<(&[u8], usize)> {
        Capturer::bgrx(self)
    }
}

/// What sets the screen's size for dynamic resolution: the platform's `screen::Screen`.
pub(crate) trait Resize {
    /// Asks for `w`x`h`: the size the screen has now.
    fn resize(&mut self, w: u32, h: u32) -> Result<(u32, u32), String>;
}

#[cfg(target_os = "linux")]
impl Resize for crate::screen::Screen {
    fn resize(&mut self, w: u32, h: u32) -> Result<(u32, u32), String> {
        crate::screen::Screen::resize(self, w, h)
    }
}

/// A size asked for, given to the screen: whether the stream, at `streamed`, is rebuilt.
/// Only a screen whose size really changed rebuilds it and says hello to the viewers; an
/// ask the screen refuses, or one for the size it has, changes nothing: a rebuild for
/// either would restart every viewer's stream and say hello for nothing.
pub(crate) fn rebuilds_for(
    screen: &mut impl Resize,
    (w, h): (u32, u32),
    streamed: (u32, u32),
) -> bool {
    match screen.resize(w, h) {
        Ok(size) => size != streamed,
        Err(e) => {
            log::warn!("resize to {w}x{h} failed: {e}");
            false
        }
    }
}

#[cfg(target_os = "linux")]
fn capture_loop(
    cap: &mut impl Capture,
    cfg: VideoConfig,
    hub: Arc<Hub>,
    mut screen: Option<&mut crate::screen::Screen>,
) -> Result<(), String> {
    let (w, h) = cap.size();
    let (w, h) = (w as usize, h as usize);
    let interval = Duration::from_secs_f64(1.0 / cfg.fps.get() as f64);
    // What the encoders' cap (`--max-kbps`) lets a frame have, and a keyframe in
    // `KEY_TIME`: the most a capture takes when no viewer's lanes bound it.
    let bytes_at_ceiling =
        |d: Duration| (cfg.ceiling.get() * 1000.0 / 8.0 * d.as_secs_f64()) as usize;
    let (frame_most, key_most) = (
        bytes_at_ceiling(interval),
        bytes_at_ceiling(crate::hub::KEY_TIME),
    );
    let min_interval = interval.mul_f64(MIN_INTERVAL_SHARE);
    let mut captured = Captured::new(Instant::now() - interval);
    let blocks = w.div_ceil(16) * h.div_ceil(16);
    let mut refiner = Refiner::new(blocks);
    let marker = cfg.debug.marker;
    // The marker is read only where all of it fits this capture (a resize builds a new
    // loop): past an edge its squares are outside the picture or in the next row.
    let probe = marker.and_then(|at| {
        let probe = MarkerProbe::fit(at, w, h);
        if probe.is_none() {
            log::error!(
                "VOXL_DEBUG_MARKER={},{}: the marker does not fit the {w}x{h} capture; \
                 it is not read",
                at.x,
                at.y
            );
        }
        probe
    });
    // The encoders, while somebody watches. Every way out of the loop drops it, and with it
    // waits for the encoder to send what it holds.
    let mut stream: Option<Stream> = None;
    // What the loop saw of each watching viewer when it last looked: kept, so that a look
    // allocates nothing once there is room for every viewer.
    let mut seen: Vec<(Arc<Client>, Seen)> = Vec::new();

    loop {
        // The wake is taken before anything it may stand for is read: a request that
        // lands after this line wakes the next wait, and one that landed before it is
        // seen below. Read first and drained after, a resize asked for in between lost
        // its wake, and the waits below have no time limit.
        hub.drain_wake();
        if cap.needs_rebuild() {
            return Ok(());
        }
        let asked = hub.resize_req.lock().take();
        if let (Some(s), Some(want)) = (screen.as_deref_mut(), asked) {
            if rebuilds_for(s, want, cap.size()) {
                return Ok(());
            }
        }
        cap.pump()?;
        if cap.needs_rebuild() {
            return Ok(());
        }
        let snaps = std::mem::take(&mut *hub.snapshots.lock());
        if !snaps.is_empty() {
            // The whole current screen, whether or not anyone is watching. Damage stays
            // pending, so the stream still sends what changed. Only the pixels are copied
            // here (the framebuffer is live); the PNG is made off this thread.
            let shot = Arc::new(crate::record::Snapshot {
                width: w,
                height: h,
                bgrx: cap.snapshot()?,
            });
            for tx in snaps {
                if tx.send(shot.clone()).is_err() {
                    log::info!("a screenshot's asker left before its answer");
                }
            }
        }
        // The encoder holds the sender for as long as it runs: once it is gone, no buffer
        // will come back to wake the loop.
        if let Some(s) = stream.as_mut() {
            loop {
                match s.ret_rx.try_recv() {
                    Ok(r) => s.returned(r, &mut refiner),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => return Err("encoder gone".into()),
                }
            }
        }
        // One reading of the clock and one look at each viewer: the stream, the keyframe and
        // whom it is promised to, the gate and a pass are all decided from them. The
        // capture itself is stamped `t0` below, after any wait for a buffer.
        let now = Instant::now();
        hub.look_into(interval, &mut seen);
        match (stream.is_some(), seen.is_empty()) {
            // Nobody watches: a stream is dropped here, once its encoder has sent what it
            // holds.
            (_, true) => stream = None,
            (true, false) => {}
            // A new stream starts from the whole screen: our shm copy may be stale, and
            // none of it has been sent, so every block is pending.
            (false, false) => {
                let (cw, ch) = cap.size();
                stream = Some(Stream::new(&cfg, &hub, cw, ch)?);
                cap.grab(cap.full())?;
                // The whole screen is grabbed and pending: it holds what was drawn while
                // nobody watched, so that damage is taken with it, and the stream's first
                // frame counts from its capture, not from damage no frame took.
                cap.damage().take(ch);
                cap.damage().mark();
                refiner.mark_all(Instant::now());
                // The look is older than the stream: its first capture is decided from the
                // next.
                continue;
            }
        }
        let Some(s) = stream.as_mut() else {
            Wait::Change.wait(cap, &hub)?;
            continue;
        };
        let key = Key::of(&seen, key_most);
        let look = Look {
            key,
            damage: *cap.damage(),
            pending: refiner.pending(),
            now,
            captured,
            min_interval,
            gate: Gated::of(seen.iter().map(|(_, v)| v.room), frame_most),
            spare: Gated::of(seen.iter().map(|(_, v)| v.spare), frame_most),
            buffer: std::mem::replace(&mut s.buf, Buffer::Away),
        };
        let step = step(look);
        let (next, mut buf, allowance) = match step {
            Step::Wait(wait, buffer) => {
                s.buf = buffer;
                wait.wait(cap, &hub)?;
                continue;
            }
            Step::Capture(next, buf, allowance) => (next, buf, allowance),
        };
        let t0 = Instant::now();
        let origin = match *cap.damage() {
            Damage::Rows { first, .. } => first,
            Damage::None | Damage::Unreported => t0,
        };
        hub.stats
            .damage_wait_us
            .fetch_add((t0 - origin).as_micros() as u64, Ordering::Relaxed);
        let mb_dirty = cap.take_mb_map(s.spare.map(blocks));
        refiner.mark(&mb_dirty, t0);
        let band = cap.take_damage()?;
        if let Some(b) = band {
            cap.grab(b)?;
        }
        let t1 = Instant::now();
        if marker.is_some() {
            log::info!(
                "cap dt {:.1} ms band {:?} grab {} us",
                captured.any.elapsed().as_secs_f64() * 1000.0,
                band,
                (t1 - t0).as_micros()
            );
        }
        let marker_ms = marker_reading(cap.bgrx(), probe);
        // What this capture is sent as, decided once; with nothing to send, the buffer
        // goes back home.
        let plan = match plan(key, band, next, &refiner) {
            Planned::Send(plan) => plan,
            Planned::Empty => {
                s.buf = Buffer::Home(buf);
                s.spare.maps.push(mb_dirty);
                continue;
            }
        };
        let todo = match (s.pending.take(), band) {
            (p, Some(b)) => Some(union(p, b)),
            (p, None) => p,
        };
        if let Some(t) = todo {
            let pic: &mut I420 = &mut buf;
            cap.convert(pic, t.y0 as usize, t.y1 as usize)?;
        }
        let t2 = Instant::now();
        hub.stats
            .grab_us
            .fetch_add((t1 - t0).as_micros() as u64, Ordering::Relaxed);
        hub.stats
            .convert_us
            .fetch_add((t2 - t1).as_micros() as u64, Ordering::Relaxed);
        hub.stats.captured.fetch_add(1, Ordering::Relaxed);

        let frame = match plan {
            Plan::Key(keyframe) => {
                // Every block goes out at keyframe quality, which the rate cap squeezes:
                // all of it is pending. In place of a pass the keyframe is coded at
                // paint-over quality, and what comes back as asked is sharp.
                refiner.mark_all(t0);
                let sharpens = match keyframe {
                    Keyframe::Pass => {
                        hub.stats.paintovers.fetch_add(1, Ordering::Relaxed);
                        Some(refiner.every_block(t0, s.spare.map(blocks)))
                    }
                    Keyframe::Fresh => None,
                };
                s.spare.maps.push(mb_dirty);
                let mut starts = s.spare.starts();
                starts.extend(
                    seen.iter()
                        .filter(|(c, v)| v.viewer == Viewer::Waiting && c.keyframe_planned())
                        .map(|(c, _)| c.clone()),
                );
                Frame::Key {
                    blocks: sharpens,
                    starts,
                }
            }
            Plan::Sharpen => {
                hub.stats.paintovers.fetch_add(1, Ordering::Relaxed);
                s.spare.maps.push(mb_dirty);
                let mut map = s.spare.map(blocks);
                refiner.pass(&mut map);
                Frame::Sharpen {
                    blocks: Sharpen { map, at: t0 },
                }
            }
            Plan::Motion => Frame::Motion { dirty: mb_dirty },
        };
        captured.on_capture(next, t0);
        if s.job_tx
            .send(Job {
                buf,
                frame,
                allowance,
                damage: origin,
                marker_ms,
            })
            .is_err()
        {
            return Err("encoder gone".into());
        }
    }
}

/// The stream's picture: what its encoders are opened with, and the running one.
struct Picture {
    cfg: EncoderConfig,
    /// None until the stream's first frame opens it: on a new stream that is a viewer's
    /// keyframe, so one open, for its budget. An open that fails ends the pipeline, so it
    /// is None before that only.
    running: Option<Running>,
}

/// The stream's encoder, and the rate x264 was last told for it.
struct Running {
    enc: Encoder,
    rate: Applied,
}

impl Running {
    /// The stream's encoder, as configured, told `rate`.
    fn open(cfg: &EncoderConfig, rate: Applied) -> Result<Running, String> {
        let mut enc = Encoder::new(cfg)?;
        enc.set_rate(rate.crf, rate.kbps);
        Ok(Running { enc, rate })
    }

    /// An encoder of a keyframe's own, at `crf` under `cap`, in a stream whose encoders
    /// open at `ceiling` (`Encoder::for_keyframe`).
    fn for_keyframe(
        cfg: &EncoderConfig,
        crf: f32,
        ceiling: Kbps,
        cap: u32,
    ) -> Result<Running, String> {
        let enc = Encoder::for_keyframe(&cfg.pictures, crf, ceiling, Kbps::new(cap as f64))?;
        Ok(Running {
            enc,
            rate: Applied { crf, kbps: cap },
        })
    }

    /// Before a frame at `crf` under `cap`: tells x264, unless what it was told holds.
    fn set(&mut self, crf: f32, cap: u32) {
        if let Some(rate) = self.rate.next(crf, cap) {
            self.enc.set_rate(rate.crf, rate.kbps);
            self.rate = rate;
        }
    }
}

/// The CRF and cap the encoder was set to.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Applied {
    crf: f32,
    kbps: u32,
}

/// How far a stripe's cap may move, as a share of the one set, before x264 is told:
/// each change reconfigures the encoder. Upstream's guess, unmeasured. At none, every
/// wobble of what a frame may take reconfigures x264; at twice this, a frame may be coded
/// under a cap 8% off its share of it.
const CAP_SLACK: f64 = 0.04;

impl Applied {
    /// The rate to set before a frame at `crf` under `cap`, unless this one still holds:
    /// the same CRF, and a cap within CAP_SLACK of it.
    fn next(self, crf: f32, cap: u32) -> Option<Applied> {
        let holds = self.crf == crf
            && (cap as f64 - self.kbps as f64).abs() <= self.kbps as f64 * CAP_SLACK;
        (!holds).then_some(Applied { crf, kbps: cap })
    }
}

/// What came of a frame.
enum Coded {
    /// The encode failed.
    Failed,
    /// x264 put out nothing for the picture. With no B-frames, lookahead or frame
    /// threads it holds no picture back, so this doesn't come.
    Empty,
    /// The frame: a header's room, then what x264 put out, its update's number and the
    /// epoch it was coded in.
    Sent {
        key: bool,
        bytes: BytesMut,
        quality: Quality,
        number: Number,
        epoch: Epoch,
    },
}

impl Coded {
    /// What x264 made of a picture, asked for at `crf` after a frame that left it
    /// `before`; `bytes` is a header's room and what it put out, as update `number` in
    /// `epoch`.
    fn of(
        e: &Encoded,
        crf: f32,
        before: Settling,
        bytes: BytesMut,
        (number, epoch): (Number, Epoch),
    ) -> Coded {
        match bytes.len() {
            Header::LEN => Coded::Empty,
            _ => Coded::Sent {
                key: e.keyframe(),
                quality: e.quality(crf, before),
                bytes,
                number,
                epoch,
            },
        }
    }

    /// Whether the frame came out sharp: as asked. A failed one did not; an empty one is
    /// as the picture x264 didn't fill in reads, a CRF average of 0, so as asked.
    fn sharp(&self) -> bool {
        match self {
            Coded::Sent { quality, .. } => *quality == Quality::AsAsked,
            Coded::Failed => false,
            Coded::Empty => true,
        }
    }
}

/// x264's CRF raises the QP of frames it judges complex, and its quick half-resolution
/// estimate can't follow a dragged window or a fast scroll: the moved area counts as new
/// pixels and the QP lands 7-10 steps above `--crf`, although the real search codes the
/// move as cheap motion vectors (a few KB a frame). Big moves went blocky, with nothing
/// saved that the bitrate cap needed. Motion frames that come out that cheap get a CRF
/// lowered by what the estimate adds, so they stay at the configured quality. Video and
/// new content really are complex, and x264's higher QP there saves real bits, so those
/// are left alone, as are the lower QPs x264 picks for small updates (typing) and
/// paint-overs. VBV still raises the QP when the link is short.
#[derive(Default)]
struct MotionQuality {
    /// QP steps x264's complexity estimate adds to the next frame (negative for simple
    /// ones). x264 blurs it over the frames it encodes, paint-overs included, so each
    /// frame's is a good guess at the next one's.
    complexity: f32,
    /// log2 of the bits per changed macroblock of recent motion frames, scaled to the
    /// configured CRF. Averaged in the log domain: one frame of new content (a menu
    /// opening mid-drag) shouldn't outweigh the run of cheap frames around it.
    mb_bits_log2: f32,
    /// QP steps taken off the CRF of motion frames now.
    applied: f32,
}

/// Most QP steps taken off a motion frame's CRF: two of H.264's doublings of the
/// quantiser step (one every 6 QP), so motion is at most four times finer than the
/// estimate would make it. Provisional: upstream's value, unmeasured here; what would
/// measure it is the bench's drag test's bytes and PSNR at 6, 12 and 18.
const MAX_MOTION_EXCESS: f32 = 12.0;

/// Bits per changed macroblock (at the configured CRF) under which motion counts as
/// cheap. Provisional: upstream saw a dragged window or a scrolled page at 10-30 and video
/// and new content at 50 and more, and put the line between; the record is gone. What would
/// measure it is `VOXL_DEBUG_ENCODES`'s bits per macroblock over the bench's drag, scroll
/// and video workloads.
const CHEAP_MB_BITS: f32 = 40.0;

impl MotionQuality {
    /// The CRF to encode a motion frame with, for the configured `crf`.
    fn crf(&mut self, crf: f32) -> f32 {
        let want = if self.mb_bits_log2 < CHEAP_MB_BITS.log2() {
            self.complexity.clamp(0.0, MAX_MOTION_EXCESS)
        } else {
            0.0
        };
        // Whole steps, and only once the estimate has moved a full step: every change
        // reconfigures x264.
        if (want - self.applied).abs() >= 1.0 {
            self.applied = want.round();
        }
        (crf - self.applied).max(0.0)
    }

    /// Notes what x264 made of a frame it was given `crf` for. `size` is set for a
    /// motion frame (not a paint-over).
    fn note(&mut self, e: &Encoded, crf: f32, size: Option<MotionSize>) {
        // A keyframe's QP isn't picked from the complexity estimate.
        let Encoded::Picture {
            keyframe: false,
            qp,
            crf_avg,
        } = *e
        else {
            return;
        };
        self.observe(crf, qp, crf_avg);
        // Small updates cost more per macroblock (headers, a cursor's edges) and never
        // get x264's QP up; only big ones tell what kind of motion this is.
        if let Some(m) = size.filter(|m| m.changed * 16 >= m.mbs) {
            // Halving the quantizer step (6 QP) about doubles the bits.
            let b = (m.bits.max(1) as f32 / m.changed.max(1) as f32).log2() + (qp - m.crf) / 6.0;
            self.mb_bits_log2 = self.mb_bits_log2 * 0.7 + b * 0.3;
        }
    }

    /// x264 starts from QP0 = `crf` + complexity. VBV may raise that before the frame
    /// (to `qp`) and row by row within it (to an average x264 doesn't report), never
    /// below QP0 or, with a VBV buffer over 1.1 frames, below the frame's starting QP,
    /// and `crf_avg` is `crf` + average - QP0. So the complexity lies between
    /// `qp - crf_avg` (no row raised) and `qp - crf` (nothing raised before the
    /// frame), give or take the rounding of `qp`: one value when VBV stayed out, a range
    /// when it didn't. Then the last estimate is kept while it fits, else moved to the
    /// nearest end, so a frame VBV squeezed doesn't throw away what earlier frames showed.
    fn observe(&mut self, crf: f32, qp: f32, crf_avg: f32) {
        let lo = qp - 0.5 - crf_avg;
        let hi = qp + 0.5 - crf;
        self.complexity = if hi - lo < 1.25 {
            (lo + hi) / 2.0
        } else {
            self.complexity.max(lo).min(hi)
        };
    }
}

/// How big a motion frame came out, for MotionQuality.
#[derive(Clone, Copy)]
struct MotionSize {
    bits: usize,
    /// Macroblocks that changed, of the stripe's `mbs`.
    changed: usize,
    mbs: usize,
    /// The configured CRF.
    crf: f32,
}

/// The misses sessions reported of the stream's updates, for its coder to act on before it
/// codes its next picture (x264 is not told during an encode, and a still picture needs
/// nothing until it changes), and the count of its updates and its epoch. Both go on from
/// stream to stream: an update is never taken for another, and a relay that reported an
/// update missed drops every later update whose epoch is under it, so a new stream's
/// updates carry every miss reported before it (its encoder refers to none of them).
pub struct Misses(parking_lot::Mutex<Count>);

impl Default for Misses {
    fn default() -> Misses {
        Misses(parking_lot::Mutex::new(Count {
            next: Number::first(),
            missed: Missed::None,
            epoch: Epoch::Start,
        }))
    }
}

/// The number the next update gets, the misses not yet acted on, and the epoch.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Count {
    next: Number,
    missed: Missed,
    epoch: Epoch,
}

/// The misses not yet acted on: the oldest, which the encoder forgets from, and the
/// newest, which becomes the epoch.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Missed {
    None,
    Some { oldest: Number, newest: Number },
}

impl Misses {
    /// A new stream: its encoder is new and refers to nothing before it, so a miss not yet
    /// acted on is only its epoch now; the count and the epoch go on.
    fn reset(&self) {
        let mut c = self.0.lock();
        if let Missed::Some { newest, .. } = c.missed {
            c.epoch = c.epoch.max(Epoch::After(newest));
        }
        c.missed = Missed::None;
    }

    /// A session reported update `number` missed.
    pub fn missed(&self, number: Number) {
        let mut c = self.0.lock();
        c.missed = match c.missed {
            Missed::None => Missed::Some {
                oldest: number,
                newest: number,
            },
            Missed::Some { oldest, newest } => Missed::Some {
                oldest: oldest.min(number),
                newest: newest.max(number),
            },
        };
    }

    /// The next update: the misses to forget from before it is coded, taken, its number,
    /// and its epoch, which takes in those misses.
    fn take(&self) -> (Missed, Number, Epoch) {
        let mut c = self.0.lock();
        let number = c.next;
        c.next = number.next();
        let missed = std::mem::replace(&mut c.missed, Missed::None);
        if let Missed::Some { newest, .. } = missed {
            c.epoch = c.epoch.max(Epoch::After(newest));
        }
        (missed, number, c.epoch)
    }
}

/// The pts of the last pictures by their updates' numbers, newest last: as many as the
/// encoder keeps (`DPB`), which are all a miss can still be referred to through.
struct Kept {
    pictures: std::collections::VecDeque<(Number, Pts)>,
    /// The first update this coder coded: a miss of an update before it is of the
    /// stream before, which this one never referred to.
    first: Option<Number>,
}

impl Kept {
    fn new() -> Kept {
        Kept {
            pictures: std::collections::VecDeque::with_capacity(DPB),
            first: None,
        }
    }

    fn coded(&mut self, number: Number, pts: Pts) {
        if self.pictures.len() == DPB {
            self.pictures.pop_front();
        }
        self.pictures.push_back((number, pts));
        self.first.get_or_insert(number);
    }

    /// Where the encoder forgets from for a miss of update `number`: that update's pts;
    /// the oldest picture kept for an update older than all of them (every picture kept
    /// may refer to it through others), so x264 keeps nothing and codes a keyframe; nothing
    /// for an update of the stream before.
    fn forget_from(&self, number: Number) -> Option<Pts> {
        if self.first.is_none_or(|first| number < first) {
            return None;
        }
        let kept = self.pictures.iter().find(|(n, _)| *n >= number);
        kept.or(self.pictures.front()).map(|&(_, pts)| pts)
    }
}

/// Codes each frame: the encoder, and what it has learned of the frames it coded (how
/// cheap their motion is, where x264 started the last one, the pictures a miss may name).
struct Coder {
    picture: Picture,
    /// The misses sessions report, and the updates' count.
    misses: Arc<Misses>,
    pictures: Kept,
    crfs: Crfs,
    debug: bool,
    motion: MotionQuality,
    settling: Settling,
    /// What x264 put out for the last frame: kept, so a frame allocates nothing for it.
    out: Vec<u8>,
    /// x264's hint for each macroblock (1: unchanged), kept likewise.
    hints: Vec<u8>,
    arenas: Arenas,
    /// Takes the encoders a keyframe replaces, to close them on a thread of their own:
    /// closing one joins x264's threads and frees its memory, which took 1.8-15.4 ms on the
    /// coding thread, before the keyframe.
    close: mpsc::SyncSender<Running>,
}

/// The bytes frames are cut from, in two arenas taken in turn. A message
/// is the next stretch of the arena in use; when that one is full the other is taken
/// back, once every message cut from it has been sent and dropped. So a stream whose
/// viewers keep up allocates nothing for its messages.
struct Arenas {
    two: [BytesMut; 2],
    at: usize,
}

/// What each of the two arenas holds. Sized from the four numbers' scroll scene:
/// a message there is about 11 KB, so an arena holds some 90 frames' messages, and is
/// taken back after the other has held as many, long after a viewer that keeps up has
/// been sent them; a keyframe of 300 KB fits three times. Half would still hold 45
/// frames; one message bigger than an arena gets an arena of its own size. Unmeasured
/// beyond that.
const ARENA_BYTES: usize = 1 << 20;

impl Arenas {
    fn new() -> Arenas {
        // Cutting from an arena the first time allocates bytes' count of who shares it:
        // done here, once, and not on some later frame.
        let shared = || {
            let mut arena = BytesMut::with_capacity(ARENA_BYTES);
            drop(arena.split());
            arena
        };
        Arenas {
            two: [shared(), shared()],
            at: 0,
        }
    }

    /// A message: a header's room, then `payload`.
    fn message(&mut self, payload: &[u8]) -> BytesMut {
        let len = Header::LEN + payload.len();
        if !self.two[self.at].try_reclaim(len) {
            self.at = 1 - self.at;
            if !self.two[self.at].try_reclaim(len) {
                self.two[self.at] = BytesMut::with_capacity(ARENA_BYTES.max(len));
            }
        }
        let arena = &mut self.two[self.at];
        arena.put_bytes(0, Header::LEN);
        arena.put_slice(payload);
        arena.split()
    }
}

/// What the coder is asked to code.
struct Asked<'a> {
    buf: &'a I420,
    ts: i64,
    coding: Coding,
    cap: u32,
    mb_dirty: Dirty<'a>,
}

impl Coder {
    fn new(
        picture: Picture,
        crfs: Crfs,
        debug: bool,
        close: mpsc::SyncSender<Running>,
        misses: Arc<Misses>,
    ) -> Coder {
        Coder {
            picture,
            misses,
            pictures: Kept::new(),
            crfs,
            debug,
            motion: MotionQuality::default(),
            settling: Settling::Unknown,
            out: Vec::new(),
            hints: Vec::new(),
            arenas: Arenas::new(),
            close,
        }
    }

    /// Codes a frame; or says why there is no encoder to code it with, now or after.
    fn code(&mut self, job: Asked) -> Result<Coded, String> {
        let Coder {
            picture,
            misses,
            pictures,
            crfs,
            debug,
            motion,
            settling,
            out,
            hints,
            arenas,
            close,
        } = self;
        let buf = job.buf;
        // The CRF the job asks for, and the one x264 is given: MotionQuality may lower
        // a motion frame's.
        let asked = crfs.of(job.coding);
        let crf = match job.coding {
            Coding::Key { .. } | Coding::Sharpen => asked,
            Coding::Motion => motion.crf(asked),
        };
        // A keyframe under a cap gets an encoder of its own, opened for its budget `cap`
        // while the running one is closed elsewhere; any other frame the running one,
        // told its rate; the stream's first frame opens it.
        let running = match (
            job.coding.own_encoder(picture.cfg.max_kbps),
            picture.running.take(),
        ) {
            (Some(ceiling), was) => {
                if let Some(old) = was {
                    // The closing thread holds one encoder a frame: a full queue is a
                    // closing thread gone, and the old encoder then closes here.
                    match close.try_send(old) {
                        Ok(()) => {}
                        Err(
                            mpsc::TrySendError::Full(old) | mpsc::TrySendError::Disconnected(old),
                        ) => drop(old),
                    }
                }
                Running::for_keyframe(&picture.cfg, asked, ceiling, job.cap)
            }
            (None, Some(mut running)) => {
                running.set(crf, job.cap);
                Ok(running)
            }
            (None, None) => Running::open(&picture.cfg, Applied { crf, kbps: job.cap }),
        };
        let enc = &mut picture.running.insert(running?).enc;
        // What sessions reported missed since the last picture: everything from the oldest
        // on is forgotten, so this picture refers only to what came before it (or is a
        // keyframe), and it carries the newest as its epoch. Forgotten in whichever encoder
        // codes it: a keyframe coded after a miss but before its report was dropped by the
        // relay with the rest, so a new encoder forgets it too.
        let (missed, number, epoch) = misses.take();
        if let Missed::Some { oldest, .. } = missed {
            if let Some(pts) = pictures.forget_from(oldest) {
                enc.invalidate(pts)?;
            }
        }
        let planes = Planes {
            y: &buf.y,
            u: &buf.u,
            v: &buf.v,
            stride_y: buf.width,
            stride_uv: buf.stride_uv(),
        };
        out.clear();
        // x264 wants X264_MBINFO_CONSTANT (1) on unchanged macroblocks.
        let info = match &job.mb_dirty {
            Dirty::All => None,
            Dirty::Blocks(d) => {
                hints.clear();
                hints.extend(d.iter().map(|&x| (x == 0) as u8));
                Some(&mut hints[..])
            }
        };
        // Changed macroblocks (all of them without a damage map).
        let mbs = buf.width.div_ceil(16) * (buf.y.len() / buf.width).div_ceil(16);
        let changed = info
            .as_ref()
            .map_or(mbs, |m| m.iter().filter(|&&c| c == 0).count());
        let te = Instant::now();
        let r = enc.encode(
            &planes,
            job.ts,
            matches!(job.coding, Coding::Key { .. }),
            None,
            info,
            out,
        );
        if *debug {
            match thread_cpu_us() {
                Ok(cpu) => log::info!(
                    "encode {} us wall, thread cpu total {cpu} us, {} bytes",
                    te.elapsed().as_micros(),
                    Header::LEN + out.len()
                ),
                Err(e) => log::error!("this thread's CPU time: {e}"),
            }
        }
        Ok(match r {
            Ok(e) => {
                pictures.coded(number, enc.last_pts());
                let size = matches!(job.coding, Coding::Motion).then_some(MotionSize {
                    bits: out.len() * 8,
                    changed,
                    mbs,
                    crf: asked,
                });
                motion.note(&e, crf, size);
                let before = std::mem::replace(settling, e.settling(crf));
                Coded::of(&e, crf, before, arenas.message(out), (number, epoch))
            }
            Err(e) => {
                log::error!("{e}");
                *settling = Settling::Unknown;
                Coded::Failed
            }
        })
    }
}

/// Starts the encode loop's thread with `run`, which owns the way home for the capture
/// loop's pictures. However `run` ends, by returning or by a panic, it has closed that
/// way by then, and the capture loop is woken to find it closed: it ends on "encoder
/// gone", and never waits for a picture that cannot come. Dropped, it waits for `run` to
/// end.
fn encoder_thread(hub: Arc<Hub>, run: impl FnOnce() + Send + 'static) -> Result<Stopped, String> {
    let woken = Woken(hub);
    Stopped::spawn("encode", move || {
        let _woken = woken;
        run()
    })
}

/// Wakes the hub when it is dropped.
struct Woken(Arc<Hub>);

impl Drop for Woken {
    fn drop(&mut self) {
        self.0.wake();
    }
}

fn encode_loop(
    picture: Picture,
    fps: Fps,
    rx: mpsc::Receiver<Job>,
    ret: mpsc::SyncSender<Returned>,
    hub: Arc<Hub>,
    crfs: Crfs,
    debug: Debugging,
) {
    // The encoders keyframes replace are closed here, off the coding thread; one at a
    // time, and the thread ends with the coder.
    let (close, closing) = mpsc::sync_channel::<Running>(1);
    let closer = std::thread::Builder::new()
        .name("enc-close".into())
        .spawn(move || closing.into_iter().for_each(drop));
    if let Err(e) = closer {
        log::error!("no thread to close encoders on: {e}");
        return;
    }
    // The misses are this stream's from here.
    hub.misses.reset();
    let mut coder = Coder::new(picture, crfs, debug.encodes, close, hub.misses.clone());
    if let Err(e) = encode_frames(&mut coder, fps, rx, ret, &hub) {
        log::error!("the encoder is gone: {e}");
    }
}

/// Codes each frame the capture loop sends, and sends it to the viewers, until the
/// capture loop is gone or the encoder is.
fn encode_frames(
    coder: &mut Coder,
    fps: Fps,
    rx: mpsc::Receiver<Job>,
    ret: mpsc::SyncSender<Returned>,
    hub: &Hub,
) -> Result<(), String> {
    let epoch = Instant::now();
    // Kept from frame to frame, so a frame allocates nothing for it.
    let mut viewers = Vec::new();
    // A frame's number, which its header carries: the encode loop's own count, wrapping.
    let mut frame_number = 0u32;
    // A picture goes home only once its frame is on its viewers' lanes: the capture loop,
    // woken by it, asks the lanes for room next, and they must count that frame. A
    // capture loop gone has no home to send it to, and ends this loop at its next job.
    let home = |r: Returned| {
        if ret.send(r).is_ok() {
            hub.wake();
        }
    };
    while let Ok(job) = rx.recv() {
        let t0 = Instant::now();
        let coding = job.frame.coding();
        let mb_dirty = job.frame.mb_dirty();
        // The frame may take its allowance, told to x264 as a rate at the frame rate.
        let cap = Bits::of_bytes(job.allowance).a_frame_at(fps.get()).get() as u32;
        let ts = job.damage.duration_since(epoch).as_millis() as i64;
        // Sharpening spends from the same room as motion: what the room doesn't cover now
        // is left pending for a later pass.
        let out = coder.code(Asked {
            buf: &job.buf,
            ts,
            coding,
            cap: cap.max(1),
            mb_dirty,
        })?;
        let sharp = out.sharp();
        let sharpens = job.frame.sharpens();
        let Parts {
            blocks,
            starts,
            dirty,
        } = job.frame.into_parts();
        let sharpened = match (blocks, sharp) {
            (None, _) => Sharpened::Nothing,
            (Some(b), true) => Sharpened::Sharp(b),
            (Some(b), false) => Sharpened::Squeezed(b),
        };
        // The viewers a keyframe starts, told whether it did; then the list goes home empty.
        let start = |whole: bool| {
            for c in starts.iter().flatten() {
                c.keyframe_sent(whole);
            }
        };
        let back = |starts: Option<Vec<Arc<Client>>>| Returned {
            buf: job.buf,
            sharpened,
            dirty,
            starts,
        };
        let (key, mut bytes, number, epoch) = match out {
            Coded::Sent {
                key,
                bytes,
                number,
                epoch,
                ..
            } => (key, bytes, number, epoch),
            // A frame with nothing to send is not sent, and starts nobody.
            Coded::Failed | Coded::Empty => {
                start(false);
                home(back(starts));
                continue;
            }
        };
        // The viewers this keyframe was promised to start on it, or wait for another.
        start(key);
        let back = back(starts);
        let enc_us = t0.elapsed().as_micros() as u64;
        frame_number = frame_number.wrapping_add(1);
        let header = Header {
            key,
            sharpens,
            frame: frame_number,
            pipe: Pipe::of(job.damage.elapsed()),
        };
        bytes[..Header::LEN].copy_from_slice(&header.bytes());
        let bytes = bytes.freeze();
        hub.stats.encode_us.fetch_add(enc_us, Ordering::Relaxed);
        hub.stats.encoded.fetch_add(1, Ordering::Relaxed);
        hub.stats
            .bytes
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        if key {
            hub.stats.keyframes.fetch_add(1, Ordering::Relaxed);
        }
        let now = Instant::now();
        hub.clients_into(&mut viewers);
        for c in &viewers {
            c.takes((number, epoch), &bytes, now);
        }
        home(back);
        // What the viewers were sent is on their lanes now: dropped here, its arena is
        // taken back.
        drop(bytes);
        viewers.clear();
        hub.stats
            .send_lat_us
            .fetch_add(job.damage.elapsed().as_micros() as u64, Ordering::Relaxed);
        if let Some(ms) = job.marker_ms {
            match marker_age(ms) {
                Ok(age) => log::info!("marker sent age {age} ms (encode {enc_us} us)"),
                Err(e) => log::error!("marker sent: the wall clock: {e}"),
            }
        }
    }
    Ok(())
}

/// CPU time this thread has used, in microseconds.
fn thread_cpu_us() -> nix::Result<i64> {
    use nix::time::{clock_gettime, ClockId};
    clock_gettime(ClockId::CLOCK_THREAD_CPUTIME_ID)
        .map(|t| t.tv_sec() * 1_000_000 + t.tv_nsec() / 1000)
}

/// The bench loadgen's marker as `bench/loadgen.c` draws it: 40 bits (the wall clock's
/// milliseconds in the low 24, the frame in the next 16), each a square of 24 px, 20 to
/// a row, read at each square's centre. These are loadgen's numbers, not chosen here: any
/// other value reads pixels it did not draw.
const MARKER_BITS: usize = 40;
const MARKER_ROW: usize = 20;
const MARKER_SQUARE: usize = 24;

/// A `VOXL_DEBUG_MARKER` place on a capture the whole marker fits, so every square it
/// reads is a pixel of the picture in the square's own row.
#[derive(Clone, Copy, Debug)]
struct MarkerProbe(Marker);

impl MarkerProbe {
    /// The marker at `at` on a `width` x `height` capture, if the centre of its last
    /// square across and its last square down are inside it.
    fn fit(at: Marker, width: usize, height: usize) -> Option<MarkerProbe> {
        let last = |from: usize, squares: usize| {
            from.checked_add((squares - 1) * MARKER_SQUARE + MARKER_SQUARE / 2)
        };
        (last(at.x, MARKER_ROW)? < width && last(at.y, MARKER_BITS / MARKER_ROW)? < height)
            .then_some(MarkerProbe(at))
    }
}

/// What the bench loadgen's marker says on this capture: none unless `VOXL_DEBUG_MARKER`
/// placed it where it fits and the capture has pixels to read it from.
fn marker_reading(bgrx: Option<(&[u8], usize)>, probe: Option<MarkerProbe>) -> Option<u32> {
    match (bgrx, probe) {
        (Some((px, stride)), Some(at)) => Some(marker_probe(px, stride, at, "grab")),
        _ => None,
    }
}

/// Debug: decode the bench loadgen marker at `at` and log its age.
fn marker_probe(buf: &[u8], stride: usize, MarkerProbe(at): MarkerProbe, what: &str) -> u32 {
    let mut v: u64 = 0;
    for b in 0..MARKER_BITS {
        let x = at.x + (b % MARKER_ROW) * MARKER_SQUARE + MARKER_SQUARE / 2;
        let y = at.y + (b / MARKER_ROW) * MARKER_SQUARE + MARKER_SQUARE / 2;
        let i = y * stride + x * 4;
        if buf[i + 2] > 128 {
            v |= 1 << b;
        }
    }
    let ms = (v & 0xffffff) as u32;
    match marker_age(ms) {
        Ok(age) => log::info!("marker {} frame {} age {age} ms", what, v >> 24),
        Err(e) => log::error!("marker {what}: the wall clock: {e}"),
    }
    ms
}

/// How long ago the loadgen drew a marker of `ms`, the low 24 bits of the wall clock's
/// milliseconds; a wall clock before 1970 is the error.
fn marker_age(ms: u32) -> Result<i64, std::time::SystemTimeError> {
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?;
    Ok((since.as_millis() as i64 & 0xffffff) - i64::from(ms))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    use crate::lanes::wire::Outcome;
    use crate::lanes::wire::Track;
    use crate::path::Kbps;
    use crate::session::sender::{Queue, Sender};
    use crate::xorshift::Xorshift;

    /// A session of `hub` with one lane, which no writer drains, and the lane's queue.
    fn viewer(hub: &Hub) -> (Arc<Client>, Arc<Queue>) {
        let (sender, queue, _control) = Sender::with_lane(Instant::now());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        (hub.add_client(tx, sender), queue)
    }

    /// Misses for a coder that is never told of one.
    fn unmissed() -> Arc<Misses> {
        Arc::new(Misses::default())
    }

    /// A config for a stream of tests: one encoder thread, conversion on one thread.
    fn config() -> VideoConfig {
        VideoConfig {
            display: ":0".into(),
            fps: Fps::new(60).unwrap(),
            threads: 1,
            convert_threads: 1,
            crf: 23.0,
            ceiling: Kbps::new(30000.0),
            paintover_crf: 18.0,
            debug: Debugging::read(|_| None).unwrap(),
        }
    }

    /// A miss leads to invalidation and to the epoch in the next pieces: the relay reports
    /// every block of a stream, in order, some of them missed (or unseen), and each update
    /// the stream codes after a report carries as its epoch the newest update reported
    /// missed before it, `Start` before any; and the update coded right after a miss refers
    /// past it, as a predicted frame, unless the miss was of the last keyframe or an update
    /// before it: x264 keeps nothing from before a keyframe, so nothing is left to refer to,
    /// and it codes a keyframe. Through the real encode loop and lanes, the reports through
    /// the control stream's handling. Over generated runs of 30 frames.
    #[test]
    fn a_miss_is_forgotten_and_named_in_the_next_epoch() {
        use crate::lanes::wire::{Epoch, Outcome};
        let mut rng = Xorshift::new(0x510e_527f_ade6_82d1);
        let (w, h) = (64u32, 64u32);
        let blocks = (w.div_ceil(16) * h.div_ceil(16)) as usize;
        let mut reached = [0usize; 3];
        for case in 0..12 {
            let hub = Hub::new().unwrap();
            let (viewer, queue) = viewer(&hub);
            viewer.watch();
            assert!(viewer.keyframe_planned());
            let s = Stream::new(&config(), &hub, w, h).unwrap();
            // The newest update reported missed, the oldest miss since the last update
            // coded, and the last keyframe.
            let (mut newest, mut after_miss, mut last_key) = (None, None, None);
            for f in 0..30 {
                let mut buf = I420::new(w as usize, h as usize);
                for p in buf.y.iter_mut() {
                    *p = rng.below(256) as u8;
                }
                let frame = match f {
                    0 => super::Frame::Key {
                        blocks: None,
                        starts: vec![viewer.clone()],
                    },
                    _ => super::Frame::Motion {
                        dirty: vec![1; blocks],
                    },
                };
                let job = Job {
                    buf: Box::new(buf),
                    frame,
                    allowance: 100_000,
                    damage: Instant::now(),
                    marker_ms: None,
                };
                s.job_tx.send(job).unwrap();
                s.ret_rx.recv().unwrap();
                for got in queue.frames() {
                    let video: Vec<Number> = got
                        .members
                        .iter()
                        .filter_map(|(track, bytes)| {
                            let Track::Video { number, epoch } = *track else {
                                return None;
                            };
                            let want = newest.map_or(Epoch::Start, Epoch::After);
                            assert_eq!(epoch, want, "case {case}: update {number:?}");
                            let key = bytes[0] & 1 == 1;
                            if let Some(missed) = after_miss.take() {
                                let nothing_left = last_key.is_some_and(|k| missed <= k);
                                assert_eq!(
                                    key, nothing_left,
                                    "case {case}: after a miss of {missed:?}"
                                );
                                reached[usize::from(nothing_left)] += 1;
                            }
                            if key {
                                last_key = Some(number);
                            }
                            Some(number)
                        })
                        .collect();
                    if video.is_empty() {
                        continue;
                    }
                    let outcome = match rng.below(5) {
                        0 => None,
                        1 => Some(Outcome::Missed),
                        _ => Some(Outcome::Decoded),
                    };
                    let now = Instant::now();
                    let r = got.report(outcome, |r| {
                        crate::session::control::reported(&hub, &viewer, r, now)
                    });
                    assert_eq!(r, Ok(()));
                    if outcome != Some(Outcome::Decoded) {
                        for number in video {
                            newest = newest.max(Some(number));
                            after_miss = Some(after_miss.map_or(number, |n: Number| n.min(number)));
                            reached[2] += 1;
                        }
                    }
                }
            }
        }
        assert!(reached.iter().all(|&n| n > 0), "{reached:?}");
    }

    /// The top-level boxes of an MP4, each its type and body, if they tile it exactly.
    fn boxes(b: &[u8]) -> Option<Vec<([u8; 4], &[u8])>> {
        let mut out = Vec::new();
        let mut at = 0;
        while at < b.len() {
            let size = u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?) as usize;
            let name: [u8; 4] = b.get(at + 4..at + 8)?.try_into().ok()?;
            out.push((name, b.get(at + 8..at + size)?));
            at += size.max(8);
        }
        Some(out)
    }

    /// The body of the box at `path` inside `b`, through containers.
    fn inside<'a>(b: &'a [u8], path: &[&[u8; 4]]) -> Option<&'a [u8]> {
        let (first, rest) = path.split_first()?;
        let (_, body) = boxes(b)?.into_iter().find(|(n, _)| n == *first)?;
        match rest {
            [] => Some(body),
            _ => inside(body, rest),
        }
    }

    /// A recording is an MP4 whose boxes parse and hold the frames recorded: `ftyp`, then
    /// `moov`, then `mdat`, tiling the file; one track as large as the screen, whose sample
    /// table counts every frame the recorder took and whose one chunk lies inside `mdat`;
    /// through the real encode loop with the recorder as a viewer. Over runs of 1 to 20
    /// frames after the keyframe that starts it.
    #[tokio::test]
    async fn a_recording_is_an_mp4_of_its_frames() {
        let (w, h) = (64u32, 64u32);
        let blocks = (w.div_ceil(16) * h.div_ceil(16)) as usize;
        let mut rng = Xorshift::new(0x1f83_d9ab_fb41_bd6b);
        for _ in 0..6 {
            let hub = Hub::new().unwrap();
            hub.width.store(w, Ordering::Relaxed);
            hub.height.store(h, Ordering::Relaxed);
            let (stop, stopped) = tokio::sync::oneshot::channel();
            let recording = tokio::spawn({
                let hub = hub.clone();
                async move {
                    let until = crate::record::Until::Stopped(stopped);
                    crate::record::record(&hub, until, Duration::from_secs(3600)).await
                }
            });
            while hub.clients.lock().is_empty() {
                tokio::task::yield_now().await;
            }
            let recorder = hub.clients.lock()[0].clone();
            assert!(recorder.keyframe_planned());
            let s = Stream::new(&config(), &hub, w, h).unwrap();
            let n = 1 + rng.below(20) as usize;
            for f in 0..=n {
                let mut buf = I420::new(w as usize, h as usize);
                for p in buf.y.iter_mut() {
                    *p = rng.below(256) as u8;
                }
                let frame = match f {
                    0 => super::Frame::Key {
                        blocks: None,
                        starts: vec![recorder.clone()],
                    },
                    _ => super::Frame::Motion {
                        dirty: vec![1; blocks],
                    },
                };
                let job = Job {
                    buf: Box::new(buf),
                    frame,
                    allowance: 100_000,
                    damage: Instant::now(),
                    marker_ms: None,
                };
                s.job_tx.send(job).unwrap();
                s.ret_rx.recv().unwrap();
            }
            drop(s);
            stop.send(()).unwrap();
            let mp4 = recording.await.unwrap().unwrap();
            let top = boxes(&mp4).expect("the boxes tile the file");
            let names: Vec<[u8; 4]> = top.iter().map(|(n, _)| *n).collect();
            assert_eq!(names, [*b"ftyp", *b"moov", *b"mdat"]);
            let mdat_at = mp4.len() - top[2].1.len();
            let traks: Vec<&[u8]> = boxes(top[1].1)
                .unwrap()
                .into_iter()
                .filter(|(n, _)| n == b"trak")
                .map(|(_, b)| b)
                .collect();
            assert_eq!(traks.len(), 1, "one track");
            let u32_at = |b: &[u8], i: usize| u32::from_be_bytes(b[i..i + 4].try_into().unwrap());
            let tkhd = inside(traks[0], &[b"tkhd"]).unwrap();
            // Version and flags, times, id, reserved, duration, reserved (8), layer and
            // group, volume and reserved, the matrix (9), then width and height.
            let size = 4 + 20 + 8 + 4 + 4 + 9 * 4;
            assert_eq!(
                (u32_at(tkhd, size), u32_at(tkhd, size + 4)),
                (w << 16, h << 16)
            );
            let path: [&[u8; 4]; 4] = [b"mdia", b"minf", b"stbl", b"stsz"];
            let stsz = inside(traks[0], &path).expect("a sample table");
            assert_eq!(u32_at(stsz, 8) as usize, n + 1, "samples against frames");
            let stco = inside(traks[0], &[b"mdia", b"minf", b"stbl", b"stco"]).unwrap();
            let at = u32_at(stco, 8) as usize;
            assert!(at >= mdat_at && at < mp4.len(), "a chunk outside mdat");
        }
    }

    /// Updates are numbered on from stream to stream, never again, and each carries as its
    /// epoch at least every miss reported before it was taken, a new stream's included; the
    /// misses its coder forgets from are exactly those reported since the update before it
    /// of the same stream. Over generated runs of updates taken, misses reported (of any
    /// update numbered so far) and streams begun anew, against a model that keeps every miss.
    #[test]
    fn an_update_carries_every_miss_before_it() {
        let mut rng = Xorshift::new(0x6a09_e667_f3bc_c908);
        let mut reached = [0usize; 3];
        for _ in 0..2_000 {
            let misses = Misses::default();
            let (mut given, mut newest, mut pending): (Vec<Number>, Option<Number>, Vec<Number>) =
                (vec![], None, vec![]);
            for _ in 0..60 {
                match rng.below(5) {
                    0 | 1 => {
                        let (missed, number, epoch) = misses.take();
                        if let Some(&last) = given.last() {
                            assert!(number > last, "a number given again");
                        }
                        given.push(number);
                        if let Some(n) = newest {
                            assert!(epoch >= Epoch::After(n), "{epoch:?} under a miss of {n:?}");
                            reached[0] += 1;
                        }
                        let want = match (pending.iter().min(), pending.iter().max()) {
                            (Some(&oldest), Some(&newest)) => Missed::Some { oldest, newest },
                            _ => Missed::None,
                        };
                        assert_eq!(missed, want);
                        pending.clear();
                    }
                    2 | 3 => {
                        if let Some(&n) = given.get(rng.below(given.len().max(1) as u64) as usize) {
                            misses.missed(n);
                            newest = newest.max(Some(n));
                            pending.push(n);
                            reached[1] += 1;
                        }
                    }
                    _ => {
                        misses.reset();
                        pending.clear();
                        reached[2] += 1;
                    }
                }
            }
        }
        assert!(reached.iter().all(|&n| n > 1000), "{reached:?}");
    }

    /// The video updates a lane's queue holds, in order: each update's bytes.
    fn updates(queue: &Queue) -> Vec<Vec<u8>> {
        queue
            .frames()
            .into_iter()
            .flat_map(|f| f.members)
            .filter_map(|(track, bytes)| match track {
                Track::Video { .. } => Some(bytes),
                Track::Audio => None,
            })
            .collect()
    }

    /// A marker is read where all of it fits the capture, as the time painted on it, and
    /// nowhere else: a place past an edge (by a pixel, by far, or so far the sum overflows)
    /// reads none and never leaves its square's row or the picture. Captures of random sizes
    /// and padding, either side of the marker's own, with random pixels (a fixed seed); the
    /// places are drawn around the last one that fits, anywhere, and near the largest. The
    /// model is the 40 squares' centres as loadgen draws them.
    #[test]
    fn a_marker_is_read_where_it_fits_and_nowhere_else() {
        let mut state = 0u64;
        let mut next = || {
            // splitmix64
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            (z ^ (z >> 31)) as usize
        };
        // Places that fit, that do not, and one pixel either side of the last that fits.
        let (mut fits, mut not, mut edge) = (0, 0, 0);
        for _ in 0..4000 {
            let (w, h) = (440 + next() % 100, 30 + next() % 40);
            let stride = w * 4 + next() % 3 * 4;
            let mut buf = vec![0u8; stride * h];
            for chunk in buf.chunks_mut(8) {
                chunk.copy_from_slice(&next().to_le_bytes()[..chunk.len()]);
            }
            let mut place = |room: usize| match next() % 8 {
                0..=2 => room.saturating_sub(4) + next() % 8,
                3..=5 => next() % (room + 8),
                6 => usize::MAX - next() % 600,
                _ => next(),
            };
            let at = Marker {
                x: place(w.saturating_sub(468)),
                y: place(h.saturating_sub(36)),
            };
            let centres: Vec<_> = (0..40)
                .map(|b| {
                    let x = at.x.checked_add(b % 20 * 24 + 12).filter(|&x| x < w)?;
                    let y = at.y.checked_add(b / 20 * 24 + 12).filter(|&y| y < h)?;
                    Some(y * stride + x * 4)
                })
                .collect::<Option<_>>()
                .unwrap_or_default();
            let painted = match next() % 4 {
                0 => next() & !0xff_ffff,
                1 => next() | 0xff_ffff,
                _ => next(),
            } & 0xff_ffff_ffff;
            for (b, &i) in centres.iter().enumerate() {
                let r = next() as u8 % 127;
                buf[i + 2] = if painted >> b & 1 == 1 { 129 + r } else { r };
            }
            let probe = MarkerProbe::fit(at, w, h);
            assert_eq!(probe.is_some(), !centres.is_empty(), "{at:?} on {w}x{h}");
            if probe.is_some() {
                fits += 1;
            } else {
                not += 1;
            }
            if [469, 468].contains(&w.wrapping_sub(at.x))
                || [37, 36].contains(&h.wrapping_sub(at.y))
            {
                edge += 1;
            }
            assert_eq!(
                marker_reading(Some((&buf[..], stride)), probe),
                probe.map(|_| (painted & 0xff_ffff) as u32),
                "{at:?} on {w}x{h} {painted:#x}"
            );
            assert_eq!(marker_reading(None, probe), None);
        }
        assert!(
            fits > 300 && not > 300 && edge > 300,
            "{fits} fit, {not} do not, {edge} at an edge"
        );
    }

    /// Dropping a `Stopped` waits for its work: whatever the work did before it returned
    /// is done once the drop returns, for work of every length up to 64 steps.
    #[test]
    fn a_stopped_thread_has_done_its_work_once_dropped() {
        for steps in 0..64u32 {
            let (tx, rx) = mpsc::channel();
            drop(
                Stopped::spawn("work", move || {
                    for i in 0..steps {
                        tx.send(i).unwrap();
                    }
                })
                .unwrap(),
            );
            assert_eq!(rx.try_iter().count(), steps as usize, "{steps} steps");
        }
    }

    /// A stream is made when somebody starts watching, so its encoder is new: whatever its
    /// first frame was planned as, and whatever blocks it was told are unchanged, it codes
    /// its first frame from nothing, and a viewer is sent nothing that depends on a frame
    /// from before the stream. For every kind of frame, with every block marked unchanged,
    /// then a motion frame with every block changed: the first is coded from nothing and
    /// the second is not.
    #[test]
    fn a_new_streams_first_frame_is_coded_from_nothing() {
        let (w, h) = (64u32, 128u32);
        let blocks = (w.div_ceil(16) * h.div_ceil(16)) as usize;
        let unchanged = || vec![0u8; blocks];
        let sharpen = || Sharpen {
            map: vec![1u8; blocks],
            at: Instant::now(),
        };
        let frames = [
            super::Frame::Key {
                blocks: None,
                starts: vec![],
            },
            super::Frame::Key {
                blocks: Some(sharpen()),
                starts: vec![],
            },
            super::Frame::Motion { dirty: unchanged() },
            super::Frame::Sharpen { blocks: sharpen() },
        ];
        for frame in frames {
            let what = frame_kind(&frame);
            let hub = Hub::new().unwrap();
            let (viewer, queue) = viewer(&hub);
            // A viewer that has started, whatever the stream sends first.
            viewer.watch();
            assert!(viewer.keyframe_planned());
            viewer.keyframe_sent(true);
            let s = Stream::new(&config(), &hub, w, h).unwrap();
            let next = super::Frame::Motion {
                dirty: vec![1u8; blocks],
            };
            for frame in [frame, next] {
                s.job_tx
                    .send(Job {
                        buf: Box::new(I420::new(w as usize, h as usize)),
                        frame,
                        allowance: 10_000,
                        damage: Instant::now(),
                        marker_ms: None,
                    })
                    .unwrap();
                // Its picture comes home before the next is sent, as the capture loop's
                // one picture does: the way home holds one.
                s.ret_rx.recv().unwrap();
            }
            drop(s);
            let keys: Vec<bool> = updates(&queue).iter().map(|b| b[0] & 1 == 1).collect();
            assert_eq!(keys, [true, false], "{what}");
        }
    }

    fn frame_kind(f: &super::Frame) -> &'static str {
        match f {
            super::Frame::Key { blocks: None, .. } => "key",
            super::Frame::Key {
                blocks: Some(_), ..
            } => "key in place of a pass",
            super::Frame::Motion { .. } => "motion",
            super::Frame::Sharpen { .. } => "sharpen",
        }
    }

    const W: usize = 1280;
    const H: usize = 768;

    /// What the screen shows in a test sequence.
    #[derive(Clone, Copy)]
    enum Scene {
        /// A window of text-like detail dragged 48 px a frame across a gradient desktop.
        Drag,
        /// Noise over the whole screen, new every frame: content that really is complex.
        Noise,
    }

    fn draw(y: &mut [u8], scene: Scene, frame: usize) {
        match scene {
            Scene::Drag => {
                let (ww, wh) = (W / 2, H / 2);
                let x0 = 48 * frame % (W - ww);
                let y0 = H / 4;
                for row in 0..H {
                    let line = &mut y[row * W..(row + 1) * W];
                    line.fill((40 + row * 120 / H) as u8);
                    if (y0..y0 + wh).contains(&row) {
                        let r = row - y0;
                        for (x, p) in line[x0..x0 + ww].iter_mut().enumerate() {
                            // Lines of "glyphs": dark strokes on white, a gap between lines.
                            let glyph =
                                ((x / 9) as u64 * 2654435761 + (r / 18) as u64 * 40503) >> 7;
                            let ink = r % 18 < 12 && (glyph >> (x % 9 + r % 18 % 6)) & 1 == 1;
                            *p = if ink { 30 } else { 235 };
                        }
                    }
                }
            }
            Scene::Noise => {
                let mut rng = Xorshift::new((frame as u64 + 1).wrapping_mul(0x9E3779B97F4A7C15));
                for p in y.iter_mut() {
                    *p = 64 + rng.below(128) as u8;
                }
            }
        }
    }

    /// One frame as a stripe worker saw it: the CRF it handed x264 and what came back.
    struct Frame {
        crf: f32,
        e: Encoded,
        complexity: f32,
    }

    /// Encodes frames the way a stripe worker does, under a bitrate cap of `max_kbps`
    /// if there is one, with or without MotionQuality.
    struct Run {
        enc: Encoder,
        motion: MotionQuality,
        correct: bool,
        crf: f32,
        applied: f32,
        y: Vec<u8>,
        n: usize,
    }

    impl Run {
        fn new(correct: bool, crf: f32, max_kbps: Option<Kbps>) -> Run {
            Run {
                enc: Encoder::new(&EncoderConfig {
                    pictures: Pictures {
                        width: W as u32,
                        height: H as u32,
                        fps: Fps::new(60).unwrap(),
                        threads: 1,
                        debug: Debugging::read(|_| None).unwrap(),
                    },
                    crf,
                    max_kbps,
                })
                .unwrap(),
                motion: MotionQuality::default(),
                correct,
                crf,
                applied: crf,
                y: vec![0; W * H],
                n: 0,
            }
        }

        /// Encodes the next frame of `scene` as a motion frame, or the current picture
        /// again as a paint-over at `paintover_crf`.
        fn frame(&mut self, scene: Scene, paintover_crf: Option<f32>) -> Frame {
            let motion = paintover_crf.is_none();
            if motion {
                draw(&mut self.y, scene, self.n);
            }
            let c = match paintover_crf {
                Some(c) => c,
                None if self.correct => self.motion.crf(self.crf),
                None => self.crf,
            };
            if c != self.applied {
                self.enc.set_crf(c);
                self.applied = c;
            }
            let (u, v) = (vec![128u8; W * H / 4], vec![128u8; W * H / 4]);
            let planes = Planes {
                y: &self.y,
                u: &u,
                v: &v,
                stride_y: W,
                stride_uv: W / 2,
            };
            let mut out = Vec::new();
            let e = self
                .enc
                .encode(
                    &planes,
                    self.n as i64 * 16,
                    self.n == 0,
                    None,
                    None,
                    &mut out,
                )
                .unwrap();
            let mbs = W / 16 * H / 16;
            let size = motion.then_some(MotionSize {
                bits: out.len() * 8,
                changed: mbs,
                mbs,
                crf: self.crf,
            });
            self.motion.note(&e, c, size);
            self.n += 1;
            Frame {
                crf: c,
                complexity: self.motion.complexity,
                e,
            }
        }

        /// 40 motion frames of `scene`; returns the last 30, once rate control settled.
        fn motion(mut self, scene: Scene) -> Vec<Frame> {
            (0..40).map(|_| self.frame(scene, None)).skip(10).collect()
        }
    }

    fn mean(v: impl Iterator<Item = f32>) -> f32 {
        let (sum, n) = v.fold((0.0, 0), |(s, n), x| (s + x, n + 1));
        sum / n as f32
    }

    fn qp(f: &Frame) -> f32 {
        match f.e {
            Encoded::Picture { qp, .. } => qp,
            Encoded::Nothing => panic!("x264 put nothing out"),
        }
    }

    fn crf_avg(f: &Frame) -> f32 {
        match f.e {
            Encoded::Picture { crf_avg, .. } => crf_avg,
            Encoded::Nothing => panic!("x264 put nothing out"),
        }
    }

    #[test]
    fn dragged_window_stays_at_crf() {
        let crf = 24.0;
        let plain = mean(
            Run::new(false, crf, None)
                .motion(Scene::Drag)
                .iter()
                .map(qp),
        );
        let corrected = mean(Run::new(true, crf, None).motion(Scene::Drag).iter().map(qp));
        // The scene has to show the problem for the check below to mean anything.
        assert!(
            plain > crf + 3.0,
            "x264 coded the drag at QP {plain:.1} without the correction (CRF {crf})"
        );
        assert!(
            corrected < crf + 1.0,
            "drag frames at QP {corrected:.1} for CRF {crf}"
        );
    }

    /// Under a bitrate cap VBV raises rows within a frame, which the frame's starting QP
    /// doesn't show; the estimate must not take that for a simple frame and drop the
    /// correction.
    #[test]
    fn capped_drag_keeps_its_estimate() {
        let crf = 24.0;
        // Without a cap the complexity is exactly what x264 added to the QP.
        let plain = Run::new(false, crf, None).motion(Scene::Drag);
        let complexity = mean(plain.iter().map(|f| qp(f) - f.crf));
        let capped = Run::new(true, crf, Some(Kbps::new(20_000.0))).motion(Scene::Drag);
        let start_qp_says = capped
            .iter()
            .map(|f| qp(f) - crf_avg(f))
            .fold(f32::MAX, f32::min);
        assert!(
            start_qp_says < complexity - 4.0,
            "VBV never raised rows enough to matter (lowest {start_qp_says:.1}, complexity {complexity:.1})"
        );
        let lowest = capped.iter().map(|f| f.complexity).fold(f32::MAX, f32::min);
        assert!(
            lowest > complexity - 3.0,
            "estimate fell to {lowest:.1} under the cap, complexity {complexity:.1}"
        );
    }

    /// Content that really is complex keeps x264's QP: lowering it would only spend bits.
    #[test]
    fn complex_content_is_left_alone() {
        let crf = 24.0;
        let plain = mean(
            Run::new(false, crf, None)
                .motion(Scene::Noise)
                .iter()
                .map(qp),
        );
        let corrected = Run::new(true, crf, None).motion(Scene::Noise);
        assert!(
            plain > crf + 3.0,
            "x264 didn't raise the QP for noise ({plain:.1}, CRF {crf})"
        );
        let lowered = corrected.iter().filter(|f| f.crf != crf).count();
        assert_eq!(lowered, 0, "{lowered} noise frames got a lowered CRF");
    }

    /// The blocks a pass of `r` takes, if it takes any.
    fn taken(r: &mut Refiner) -> Option<Vec<u8>> {
        let mut map = Vec::new();
        match r.pass(&mut map) {
            Pending::Blocks => Some(map),
            Pending::Sharp => None,
        }
    }

    /// The refiner against a model of its blocks, each sharp or pending since a time: over
    /// generated runs of changes (any blocks, at increasing times), passes that come back
    /// sharp as of a generated capture time, and keyframes in place of a pass (every block
    /// pending, then sharp as of the keyframe's capture), a pass takes exactly the pending
    /// blocks, a block is cleared only by a frame that coded it captured no earlier than its
    /// change, and the screen is sharp exactly when no block pends.
    #[test]
    fn a_block_is_sharp_once_a_frame_captured_after_its_change_came_back_sharp() {
        let mut rng = Xorshift::new(0x510e_527f_ade6_82d1);
        let t0 = Instant::now();
        for _ in 0..2_000 {
            let n = 1 + rng.below(64) as usize;
            let mut r = Refiner::new(n);
            let mut model: Vec<Option<Instant>> = vec![None; n];
            let mut now = t0;
            for _ in 0..rng.range(1, 40) {
                now += Duration::from_micros(rng.range(1, 10_000));
                match rng.below(3) {
                    0 => {
                        let map: Vec<u8> = (0..n).map(|_| (rng.below(3) == 0) as u8).collect();
                        r.mark(&map, now);
                        for (m, &d) in model.iter_mut().zip(&map) {
                            if d != 0 {
                                *m = Some(now);
                            }
                        }
                    }
                    1 => {
                        let want: Vec<u8> = model.iter().map(|m| m.is_some() as u8).collect();
                        let got = taken(&mut r);
                        assert_eq!(got, want.contains(&1).then_some(want.clone()));
                        // It comes back sharp as of a capture up to 20 ms back.
                        if let Some(map) = got {
                            let at = now - Duration::from_micros(rng.below(20_000));
                            r.sharpened(&map, at);
                            for (m, &t) in model.iter_mut().zip(&map) {
                                if t != 0 && m.is_some_and(|c| c <= at) {
                                    *m = None;
                                }
                            }
                        }
                    }
                    _ => {
                        r.mark_all(now);
                        let s = r.every_block(now, Vec::new());
                        assert_eq!(s.map, vec![1; n]);
                        r.sharpened(&s.map, s.at);
                        model.fill(None);
                    }
                }
                let sharp = model.iter().all(Option::is_none);
                assert_eq!(r.pending() == Pending::Sharp, sharp);
            }
        }
    }

    /// A pass the cap squeezed halves the next (rounded up), oldest blocks first, until
    /// one fits, and a pass that fits takes all that pend again. A model link fits a pass
    /// of `fits` blocks or fewer: over generated screens of pending blocks changed at
    /// generated times, each pass takes exactly what that rule says, the oldest blocks
    /// pending, and every block is sharp in the end.
    #[test]
    fn a_squeezed_pass_halves_until_one_fits_oldest_first() {
        let mut rng = Xorshift::new(0x7137_449f_31b1_c3a3);
        let t0 = Instant::now();
        for _ in 0..2_000 {
            let n = 1 + rng.below(200) as usize;
            let fits = 1 + rng.below(n as u64) as usize;
            let mut r = Refiner::new(n);
            let mut changed = vec![None; n];
            for (b, c) in changed.iter_mut().enumerate() {
                if rng.below(4) > 0 {
                    let at = t0 + Duration::from_micros(rng.below(1_000_000));
                    let mut map = vec![0; n];
                    map[b] = 1;
                    r.mark(&map, at);
                    *c = Some(at);
                }
            }
            // What the next pass takes: all that pend, or after a squeeze, half the last.
            let mut most: Option<usize> = None;
            let mut passes = 0;
            while let Some(map) = taken(&mut r) {
                passes += 1;
                // Each pass that fits sharpens a block at least, and the halvings before
                // it are at most as many as halve `n` to one.
                let halvings = (usize::BITS - n.leading_zeros()) as usize;
                assert!(
                    passes <= n * (halvings + 1),
                    "{passes} passes of {n} blocks"
                );
                let pending = changed.iter().flatten().count();
                let took = map.iter().filter(|&&m| m != 0).count();
                assert_eq!(took, most.map_or(pending, |m| m.min(pending)));
                // Oldest first: no block left out is older than one taken.
                let taken = (0..n)
                    .filter(|&b| map[b] != 0)
                    .filter_map(|b| changed[b])
                    .max();
                let left = (0..n)
                    .filter(|&b| map[b] == 0)
                    .filter_map(|b| changed[b])
                    .min();
                if let (Some(t), Some(l)) = (taken, left) {
                    assert!(t <= l, "a newer block taken before an older one");
                }
                match took <= fits {
                    true => {
                        r.sharpened(&map, t0 + Duration::from_secs(2));
                        for b in (0..n).filter(|&b| map[b] != 0) {
                            changed[b] = None;
                        }
                        most = None;
                    }
                    false => {
                        r.squeezed(&map);
                        most = Some(took.div_ceil(2));
                    }
                }
            }
            assert_eq!(r.pending(), Pending::Sharp);
            assert!(changed.iter().all(Option::is_none));
        }
    }

    /// A still screen with blocks pending sharpens them in room the lanes have past what
    /// the next capture needs, and only there; once all are sharp it sends nothing more,
    /// and waits with no time: the loop's own decisions (`step` and `plan`) over a model
    /// link whose spare room comes and goes with reports, passes that fit or are squeezed
    /// by what their room carries, and a long still run after. A pass takes exactly the
    /// spare room, never the room a change would have; with none spare it waits for a
    /// report; with nothing pending, for a change. No wait it makes has a time.
    #[test]
    fn a_still_screen_sharpens_in_spare_room_then_sends_nothing() {
        let mut rng = Xorshift::new(0x3956_c25b_f348_b538);
        let ms = Duration::from_millis;
        let t0 = Instant::now() + Duration::from_secs(10);
        let mut reached = [0usize; 3];
        for _ in 0..500 {
            let n = 1 + rng.below(100) as usize;
            let mut r = Refiner::new(n);
            r.mark_all(t0);
            // Bytes a block costs at paint-over quality on this model link.
            let per_block = 1 + rng.below(400) as usize;
            let mut now = t0;
            for _ in 0..400 {
                now += ms(17);
                let (room, spare) = match rng.below(3) {
                    0 => (Gated::Held, Gated::Held),
                    1 => (Gated::Open(ROOM), Gated::Held),
                    _ => {
                        let s = 1 + rng.below(ROOM as u64) as usize;
                        (Gated::Open(ROOM + s), Gated::Open(s))
                    }
                };
                let look = Look {
                    key: Key::NotWanted,
                    damage: Damage::None,
                    pending: r.pending(),
                    now,
                    captured: Captured::new(now - Duration::from_secs(1)),
                    min_interval: ms(14),
                    gate: room,
                    spare,
                    buffer: Buffer::Home(()),
                };
                let pending = r.pending();
                match step(look) {
                    Step::Capture(next, (), allowance) => {
                        assert_eq!(next, Next::Pass);
                        assert_eq!(pending, Pending::Blocks, "a pass of a sharp screen");
                        let Gated::Open(s) = spare else {
                            panic!("a pass with no room to spare");
                        };
                        assert_eq!(allowance, s, "a pass took other than the spare room");
                        let Planned::Send(Plan::Sharpen) = plan(Key::NotWanted, None, next, &r)
                        else {
                            panic!("a pass planned as other than a pass");
                        };
                        let map = taken(&mut r).expect("blocks pend");
                        let took = map.iter().filter(|&&m| m != 0).count();
                        match took * per_block <= allowance {
                            true => r.sharpened(&map, now),
                            false => r.squeezed(&map),
                        }
                        reached[0] += 1;
                    }
                    Step::Wait(Wait::Gate, _) => {
                        assert_eq!((pending, spare), (Pending::Blocks, Gated::Held));
                        reached[1] += 1;
                    }
                    Step::Wait(Wait::Change, _) => {
                        assert_eq!(pending, Pending::Sharp);
                        reached[2] += 1;
                    }
                    other => panic!("a still screen made {other:?}"),
                }
            }
            // Then room to spare at every look: every block goes sharp, and after that
            // the still screen captures nothing, however long it stays still.
            let mut sharp_after = None;
            for look in 0..1_000 {
                now += ms(17);
                let pending = r.pending();
                let still = Look {
                    key: Key::NotWanted,
                    damage: Damage::None,
                    pending,
                    now,
                    captured: Captured::new(now - Duration::from_secs(1)),
                    min_interval: ms(14),
                    gate: Gated::Open(2 * ROOM),
                    spare: Gated::Open(ROOM),
                    buffer: Buffer::Home(()),
                };
                match (step(still), pending) {
                    (Step::Capture(Next::Pass, (), ROOM), Pending::Blocks) => {
                        let Planned::Send(Plan::Sharpen) =
                            plan(Key::NotWanted, None, Next::Pass, &r)
                        else {
                            panic!("a pass planned as other than a pass");
                        };
                        let map = taken(&mut r).expect("blocks pend");
                        let took = map.iter().filter(|&&m| m != 0).count();
                        match took * per_block <= ROOM {
                            true => r.sharpened(&map, now),
                            false => r.squeezed(&map),
                        }
                    }
                    (Step::Wait(Wait::Change, _), Pending::Sharp) => {
                        sharp_after.get_or_insert(look);
                    }
                    (other, pending) => panic!("{pending:?}: {other:?}"),
                }
            }
            assert!(sharp_after.is_some_and(|l| l < 100), "{sharp_after:?}");
        }
        assert!(reached.iter().all(|&n| n > 100), "{reached:?}");
    }

    /// The frame interval as the capture loop applies it, over 20,000 random requests: a
    /// change asked for at `t` is captured at `t`, or a frame after the last change if that
    /// is later, and never later for a pass in between; a pass goes a frame after any
    /// capture, so passes never come faster than the frame rate or right behind a change.
    #[test]
    fn a_pass_never_holds_a_change_back() {
        let t0 = Instant::now();
        let frame = Duration::from_micros(14_167);
        let mut rng = Xorshift::new(0x9E3779B97F4A7C15);
        let mut next = |n: u64| rng.below(n);
        let mut captured = Captured::new(t0);
        let (mut now, mut last_any, mut last_change) = (t0, t0, t0);
        for _ in 0..20_000 {
            now += Duration::from_micros(next(30_000));
            let what = match next(3) {
                0 => Next::Change,
                _ => Next::Pass,
            };
            // The loop waits until a frame has passed since the capture that gates it.
            let at = now.max(captured.gate(what) + frame);
            match what {
                Next::Change => {
                    assert_eq!(at, now.max(last_change + frame), "a change held back");
                    last_change = at;
                }
                Next::Pass => assert!(at >= last_any + frame, "a pass within a frame"),
            }
            captured.on_capture(what, at);
            (now, last_any) = (at, at);
        }
    }

    /// The tests' generator, drawing the stripes' cases.
    struct Gen(Xorshift);
    impl Gen {
        fn below(&mut self, n: u64) -> u64 {
            self.0.below(n)
        }
        fn bytes(&mut self, len: usize) -> Vec<u8> {
            (0..len).map(|_| self.below(256) as u8).collect()
        }
    }

    /// A picture of `height` rows of `width`, whose encoder opens at `ceiling`.
    fn picture(width: usize, height: usize, ceiling: Option<Kbps>) -> Picture {
        Picture {
            cfg: EncoderConfig {
                pictures: Pictures {
                    width: width as u32,
                    height: height as u32,
                    fps: Fps::new(60).unwrap(),
                    threads: 1,
                    debug: Debugging::read(|_| None).unwrap(),
                },
                crf: 23.0,
                max_kbps: ceiling,
            },
            running: None,
        }
    }

    /// What the coder did with `jobs` (a kind and a cap each) of one picture, coded
    /// one after another: what it put out for each, and how many encoders it handed over
    /// to be closed.
    struct PutOut {
        sent: Vec<Vec<u8>>,
        closed: usize,
    }

    /// What the coder put out (x264's bytes, past the header's room) for `jobs` of `buf`.
    fn put_out(picture: Picture, buf: &I420, jobs: &[(Coding, u32)]) -> PutOut {
        let crfs = Crfs {
            configured: 23.0,
            paintover: 18.0,
        };
        // Room for every encoder the jobs could replace, so the coder never waits.
        let (close, closing) = mpsc::sync_channel(jobs.len());
        let mut coder = Coder::new(picture, crfs, false, close, unmissed());
        let sent = jobs
            .iter()
            .enumerate()
            .map(|(n, &(coding, cap))| {
                let job = Asked {
                    buf,
                    ts: n as i64 * 16,
                    coding,
                    cap,
                    mb_dirty: Dirty::All,
                };
                match coder.code(job) {
                    Ok(Coded::Sent { bytes, .. }) => bytes[Header::LEN..].to_vec(),
                    Ok(Coded::Failed | Coded::Empty) | Err(_) => {
                        panic!("frame {n} came out with nothing")
                    }
                }
            })
            .collect();
        drop(coder);
        PutOut {
            sent,
            closed: closing.into_iter().count(),
        }
    }

    /// The SPS of a keyframe: from its NAL header to the next start code.
    fn sps(keyframe: &[u8]) -> &[u8] {
        let at = keyframe
            .windows(4)
            .position(|w| w == [0, 0, 1, 0x67])
            .expect("a keyframe carries its SPS")
            + 3;
        let len = keyframe[at..]
            .windows(3)
            .position(|w| w == [0, 0, 1])
            .unwrap_or(keyframe.len() - at);
        &keyframe[at..at + len]
    }

    /// `--max-kbps 0` is no ceiling: encoders open without a rate cap, so nothing applies
    /// the target, and a viewer on a slow path gets every frame at its CRF. So with no
    /// ceiling, what a stripe sends cannot depend on the cap its jobs carry, keyframes
    /// included: over generated runs of keyframes, motion and passes of new noise, the
    /// same bytes under caps of 100 kbit/s and of 20 Mbit/s. (RED's, for this commit's
    /// coder.) One thread, no VBV: x264's output is byte-identical run to run.
    #[test]
    fn with_no_ceiling_a_frame_is_the_same_whatever_the_cap() {
        let mut g = Gen(Xorshift::new(0xD1B5_4A32_D192_ED03));
        for case in 0..6 {
            let mut buf = Box::new(I420::new(256, 64));
            buf.y = g.bytes(256 * 64);
            let codings: Vec<Coding> = (0..2 + g.below(8))
                .map(|_| match g.below(3) {
                    0 => Coding::Key { still: false },
                    1 => Coding::Motion,
                    _ => Coding::Sharpen,
                })
                .collect();
            let at = |cap: u32| codings.iter().map(|&c| (c, cap)).collect::<Vec<_>>();
            let slow = put_out(picture(256, 64, None), &buf, &at(100)).sent;
            let fast = put_out(picture(256, 64, None), &buf, &at(20_000)).sent;
            assert!(
                slow == fast,
                "case {case}: with no ceiling, frames came out apart under 100 and 20000 kbit/s"
            );
        }
    }

    /// A keyframe gets its cap, one frame at it, whatever came before it. A stripe whose
    /// stream is capped at a ceiling codes a picture at a low cap, then a keyframe of it
    /// under a generated cap, from under the ceiling to past five frames of it in one. The
    /// picture (noise) wants more than any of them. The keyframe takes most of one frame
    /// at its cap and no more, where the running encoder had only the low cap's buffer
    /// for it; and it carries the stream's own SPS, so the viewer keeps its decoder, even
    /// under a cap past what the stream's level allows.
    #[test]
    fn a_keyframe_gets_its_cap() {
        let mut buf = Box::new(I420::new(W, H));
        draw(&mut buf.y, Scene::Noise, 0);
        let key = Coding::Key { still: false };
        // The level x264 picks for 1280x768 at 60 fps and this ceiling allows 20 Mbit/s.
        let ceiling = Some(Kbps::new(20_000.0));
        let low = 1200;
        let frame = |cap: u32| cap as usize * 1000 / 60 / 8;
        let mut g = Gen(Xorshift::new(0x9E37_79B9_7F4A_7C15));
        for _ in 0..10 {
            let cap = 2_000 + g.below(150_000) as u32;
            let mut jobs = vec![(key, low)];
            jobs.extend([(Coding::Sharpen, low); 6]);
            jobs.push((key, cap));
            let sent = put_out(picture(W, H, ceiling), &buf, &jobs).sent;
            let keyframe = sent.last().unwrap();
            assert!(
                keyframe.len() * 4 >= frame(cap) * 3 && keyframe.len() <= frame(cap),
                "a keyframe of {} bytes under a cap of {cap} kbit/s, a frame of {}",
                keyframe.len(),
                frame(cap)
            );
            assert!(
                sps(keyframe) == sps(&sent[0]),
                "a keyframe under a cap of {cap} kbit/s names another SPS"
            );
        }
    }

    /// An encoder a keyframe replaces is handed over to be closed, not closed on the
    /// stripe's thread before the keyframe. Over generated runs of keyframes, motion and
    /// passes: under a cap, every keyframe after the stripe's first frame hands over the
    /// encoder it replaces, and nothing else does; with no cap a keyframe keeps the
    /// stream's encoder, and none is.
    #[test]
    fn a_replaced_encoder_is_closed_off_the_coding_thread() {
        let mut g = Gen(Xorshift::new(0x51_7CC1_B727_220A));
        for case in 0..12 {
            let mut buf = Box::new(I420::new(64, 64));
            buf.y = g.bytes(64 * 64);
            let jobs: Vec<(Coding, u32)> = (0..4 + g.below(10))
                .map(|_| match g.below(3) {
                    0 => (Coding::Key { still: false }, 2000),
                    1 => (Coding::Motion, 300),
                    _ => (Coding::Sharpen, 300),
                })
                .collect();
            let replaced = jobs[1..]
                .iter()
                .filter(|(coding, _)| matches!(coding, Coding::Key { .. }))
                .count();
            let capped = put_out(picture(64, 64, Some(Kbps::new(8000.0))), &buf, &jobs).closed;
            let uncapped = put_out(picture(64, 64, None), &buf, &jobs).closed;
            assert!(
                capped == replaced && uncapped == 0,
                "case {case}: {capped} encoders handed over under a cap and {uncapped} \
                 without, for {replaced} keyframes after the first frame"
            );
        }
    }

    /// A frame is on its viewers' lanes before its picture goes home. The capture loop,
    /// woken by the picture, asks the lanes for room next, and the room counts what they
    /// hold: a frame not yet on them would let it capture one frame past their room.
    /// Through the real encode loop and a viewer that has started, over generated runs of
    /// keyframes, motion and passes of new pictures: the way home holds no picture (a
    /// rendezvous), so the test takes each one the moment the encode loop hands it over,
    /// and then the viewer's lane has had every frame so far, and no other.
    #[test]
    fn a_frame_is_in_flight_before_its_picture_goes_home() {
        let mut g = Gen(Xorshift::new(0x2545F4914F6CDD1D));
        let (w, h) = (64usize, 64usize);
        for case in 0..20 {
            let hub = Hub::new().unwrap();
            let (viewer, queue) = viewer(&hub);
            viewer.watch();
            viewer.keyframe_planned();
            viewer.keyframe_sent(true);
            let (job_tx, rx) = mpsc::sync_channel::<Job>(1);
            let (ret, ret_rx) = mpsc::sync_channel(0);
            let (ended_tx, ended) = mpsc::channel();
            let encode = encoder_thread(hub.clone(), {
                let hub = hub.clone();
                move || {
                    let (close, _closing) = mpsc::sync_channel(1);
                    let mut coder = Coder::new(
                        picture(w, h, None),
                        Crfs {
                            configured: 23.0,
                            paintover: 18.0,
                        },
                        false,
                        close,
                        unmissed(),
                    );
                    let _ = ended_tx.send(encode_frames(
                        &mut coder,
                        Fps::new(60).unwrap(),
                        rx,
                        ret,
                        &hub,
                    ));
                }
            })
            .unwrap();
            let mut on_lanes = 0;
            let frames = 1 + g.below(12) as usize;
            for idx in 0..frames {
                let mut buf = I420::new(w, h);
                buf.y = g.bytes(w * h);
                let frame = match (idx, g.below(3)) {
                    (0, _) | (_, 0) => super::Frame::Key {
                        blocks: None,
                        starts: vec![],
                    },
                    (_, 1) => super::Frame::Motion {
                        dirty: vec![1; (w / 16) * (h / 16)],
                    },
                    _ => super::Frame::Sharpen {
                        blocks: Sharpen {
                            map: vec![1; (w / 16) * (h / 16)],
                            at: Instant::now(),
                        },
                    },
                };
                job_tx
                    .send(Job {
                        buf: Box::new(buf),
                        frame,
                        allowance: 14_600,
                        damage: Instant::now(),
                        marker_ms: None,
                    })
                    .unwrap();
                let home = ret_rx.recv().unwrap();
                on_lanes += updates(&queue).len();
                assert_eq!(
                    on_lanes,
                    idx + 1,
                    "case {case}: frame {idx}, its picture home"
                );
                drop(home);
            }
            drop(job_tx);
            drop(encode);
            ended.recv().unwrap().unwrap();
        }
    }

    /// Once the screen goes still the paint-overs show x264 forgetting the drag, so the
    /// next motion frame isn't handed the drag's correction.
    #[test]
    fn paintovers_wind_the_correction_down() {
        let crf = 24.0;
        let mut run = Run::new(true, crf, None);
        for _ in 0..40 {
            run.frame(Scene::Drag, None);
        }
        let during = crf - run.motion.crf(crf);
        for _ in 0..3 {
            run.frame(Scene::Drag, Some(16.0));
        }
        let after = crf - run.motion.crf(crf);
        assert!(
            after <= during / 2.0,
            "correction {after} after the paint-overs, {during} during the drag"
        );
    }

    /// The tests' generator, drawing headers' fields.
    struct Rng(Xorshift);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0.next()
        }
        /// A number of any size: a random one, shifted right by a random amount.
        fn any(&mut self) -> u64 {
            let shift = self.next() % 64;
            self.next() >> shift
        }
    }

    fn random_header(rng: &mut Rng) -> Header {
        Header {
            key: rng.next() & 1 == 1,
            sharpens: rng.next() & 1 == 1,
            frame: rng.any() as u32,
            pipe: Pipe(rng.any() as u32),
        }
    }

    #[test]
    fn a_header_reads_back_as_it_was_written() {
        let mut rng = Rng(Xorshift::new(0x9e37_79b9_7f4a_7c15));
        for _ in 0..100_000 {
            let header = random_header(&mut rng);
            let picture: Vec<u8> = (0..1 + rng.next() % 64).map(|_| rng.next() as u8).collect();
            let message = [&header.bytes()[..], &picture].concat();
            assert_eq!(Header::read(&message), Ok(Video::Picture(header, &picture)));
        }
    }

    #[test]
    fn a_header_alone_is_empty_and_less_is_short() {
        let mut rng = Rng(Xorshift::new(0x2545_f491_4f6c_dd1d));
        for _ in 0..100_000 {
            let header = random_header(&mut rng);
            let bytes = header.bytes();
            assert_eq!(Header::read(&bytes), Ok(Video::Empty(header)));
            let cut = (rng.next() % Header::LEN as u64) as usize;
            assert_eq!(Header::read(&bytes[..cut]), Err(Short));
        }
    }

    #[test]
    fn a_pipe_is_whole_tenths_of_a_millisecond() {
        let mut rng = Rng(Xorshift::new(0x6a09_e667_f3bc_c908));
        let tenth = Duration::from_micros(100);
        for _ in 0..100_000 {
            let d = Duration::from_nanos(rng.any());
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
        key: bool,
        sharpens: bool,
        frame: u32,
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
    /// daemon writes and reads: each is the relay's one page-type byte, then the header.
    #[test]
    fn the_header_is_what_the_fixture_says() {
        let cases: Vec<Case> =
            serde_json::from_str(include_str!("../web/src/video-header.json")).unwrap();
        for c in cases {
            let header = Header {
                key: c.key,
                sharpens: c.sharpens,
                frame: c.frame,
                pipe: Pipe(c.pipe),
            };
            let page = hex(&c.header);
            let bytes = &page[1..];
            assert_eq!(header.bytes().to_vec(), bytes, "{}", c.header);
            let picture = hex(&c.picture);
            let message = [bytes, &picture].concat();
            assert_eq!(
                Header::read(&message),
                Ok(Video::Picture(header, &picture)),
                "{}",
                c.header
            );
        }
    }

    /// A viewer that joins while the stream is rebuilt (a resize, a display switch)
    /// starts on the rebuilt stream: no frame of the old stream reaches its lanes, whose
    /// decoder would take it as the start of the stream its hello then resets. Steps, the
    /// loops' own: a page joins and the capture loop plans its keyframe and clears its
    /// request; a resize ends the capture loop, and `encode.join` lets the real encode
    /// loop send that last job while a second page has just joined; then `build`'s
    /// `restart_clients` says hello. The first page's lane has the keyframe, so the test
    /// is of the stream, not of a loop that sent nothing.
    #[test]
    fn a_viewer_that_joins_while_the_stream_is_rebuilt_starts_on_the_rebuilt_stream() {
        let hub = Hub::new().unwrap();
        let watch = || {
            let (c, queue) = viewer(&hub);
            c.watch();
            (c, queue)
        };
        let (w, h) = (64, 64);
        let ecfg = EncoderConfig {
            pictures: Pictures {
                width: w as u32,
                height: h as u32,
                fps: Fps::new(60).unwrap(),
                threads: 1,
                debug: Debugging::read(|_| None).unwrap(),
            },
            crf: 23.0,
            max_kbps: None,
        };
        let picture = Picture {
            cfg: ecfg.clone(),
            running: None,
        };
        let (job_tx, job_rx) = mpsc::sync_channel::<Job>(1);
        let (ret_tx, _ret_rx) = mpsc::sync_channel::<Returned>(1);
        let encode = std::thread::spawn({
            let hub = hub.clone();
            move || {
                encode_loop(
                    picture,
                    Fps::new(60).unwrap(),
                    job_rx,
                    ret_tx,
                    hub,
                    Crfs {
                        configured: 23.0,
                        paintover: 18.0,
                    },
                    Debugging::read(|_| None).unwrap(),
                )
            }
        });
        // A page joins; the capture loop plans its keyframe and clears its request.
        let (page, page_lane) = watch();
        let starts = [page]
            .into_iter()
            .filter(|c| c.keyframe_planned())
            .collect();
        let key = Job {
            buf: Box::new(I420::new(w, h)),
            frame: super::Frame::Key {
                blocks: None,
                starts,
            },
            allowance: 14_600,
            damage: Instant::now(),
            marker_ms: None,
        };
        // A resize ends the capture loop, and a recording joins, before the encode loop
        // has coded that keyframe: it reads the viewers once the frame is coded.
        let (_second, second_lane) = watch();
        job_tx.send(key).unwrap();
        drop(job_tx);
        encode.join().unwrap();
        // build(): the rebuilt stream's hello.
        hub.restart_clients();
        assert_eq!(
            updates(&page_lane).len(),
            1,
            "the first page had no keyframe"
        );
        assert!(
            updates(&second_lane).is_empty(),
            "the second page's lane had a frame of the old stream before its hello"
        );
    }

    /// The test binary's allocator: the system's, counting by thread what is allocated
    /// while it is armed.
    #[global_allocator]
    static COUNTING: counting::Counting = counting::Counting;

    pub(crate) mod counting {
        use std::alloc::{GlobalAlloc, Layout, System};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Mutex, MutexGuard};

        pub struct Counting;

        /// What one thread allocated while the count was armed.
        #[derive(Clone, Copy, Debug)]
        pub struct Thread {
            /// As the kernel has it: 15 bytes at most.
            pub name: [u8; 16],
            pub allocations: usize,
            /// How many of each size it allocated, which tells where they come from: the
            /// first 16 sizes it allocated, by size.
            pub sizes: [Option<(usize, usize)>; 16],
        }

        /// Every thread that allocated while armed: a fixed table, since the allocator
        /// cannot allocate to keep it.
        struct Seen {
            threads: [Option<Thread>; 32],
            /// Allocations by threads after the table's 32.
            others: usize,
        }

        static ARMED: AtomicBool = AtomicBool::new(false);
        static SEEN: Mutex<Seen> = Mutex::new(Seen {
            threads: [None; 32],
            others: 0,
        });

        fn seen() -> MutexGuard<'static, Seen> {
            SEEN.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
        }

        /// The calling thread's name, as the kernel has it.
        pub fn thread_name() -> [u8; 16] {
            let mut name = [0u8; 16];
            // SAFETY: each writes at most the 16 bytes it is given, a name and its NUL.
            #[cfg(target_os = "linux")]
            unsafe {
                libc::prctl(libc::PR_GET_NAME, name.as_mut_ptr())
            };
            #[cfg(not(target_os = "linux"))]
            unsafe {
                libc::pthread_getname_np(libc::pthread_self(), name.as_mut_ptr().cast(), 16)
            };
            name
        }

        fn note(size: usize) {
            if !ARMED.load(Ordering::Relaxed) {
                return;
            }
            let name = thread_name();
            let mut seen = seen();
            let slot = seen
                .threads
                .iter()
                .position(|t| t.is_none_or(|t| t.name == name));
            let Some(i) = slot else {
                seen.others += 1;
                return;
            };
            let t = seen.threads[i].get_or_insert(Thread {
                name,
                allocations: 0,
                sizes: [None; 16],
            });
            t.allocations += 1;
            let same = t
                .sizes
                .iter()
                .position(|s| s.is_none_or(|(s, _)| s == size));
            if let Some(s) = same {
                t.sizes[s].get_or_insert((size, 0)).1 += 1;
            }
        }

        /// Counts every allocation from now on, by every thread of the process.
        pub fn arm() {
            *seen() = Seen {
                threads: [None; 32],
                others: 0,
            };
            ARMED.store(true, Ordering::SeqCst);
        }

        /// Stops counting: what each thread allocated since `arm`, and how many
        /// allocations came from threads past the table.
        pub fn disarm() -> (Vec<Thread>, usize) {
            ARMED.store(false, Ordering::SeqCst);
            let seen = seen();
            (
                seen.threads.iter().flatten().copied().collect(),
                seen.others,
            )
        }

        unsafe impl GlobalAlloc for Counting {
            unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                note(layout.size());
                System.alloc(layout)
            }
            unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
                note(layout.size());
                System.alloc_zeroed(layout)
            }
            unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
                note(size);
                System.realloc(ptr, layout, size)
            }
            unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
                System.dealloc(ptr, layout)
            }
        }
    }

    /// The counting allocator counts the whole process, where other tests allocate as
    /// they please: so `name` runs in a process of its own, the test binary started again
    /// with only that test selected. True in that process; in the first, starts it and
    /// checks that it ran the test and the test passed.
    pub(crate) fn in_a_process_of_its_own(name: &str) -> bool {
        const ALONE: &str = "VOXL_TEST_IN_A_PROCESS_OF_ITS_OWN";
        if std::env::var_os(ALONE).is_some() {
            return true;
        }
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                name,
                "--exact",
                "--include-ignored",
                "--test-threads=1",
                "--nocapture",
            ])
            .env(ALONE, "1")
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&out.stdout);
        // What it printed (a benchmark's numbers), here, where its runner sees it.
        print!("{said}");
        assert!(
            out.status.success() && said.contains("1 passed"),
            "{said}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        false
    }

    /// The screen a test draws on, shared by the test and the capture it hands the loop.
    #[cfg(target_os = "linux")]
    struct Canvas {
        w: u32,
        h: u32,
        drawn: parking_lot::Mutex<Drawn>,
        /// Readable when something was drawn: the capture's fd.
        news: crate::hub::Wake,
        /// The test is done: the capture loop ends, as at a change of the screen's size.
        done: std::sync::atomic::AtomicBool,
    }

    #[cfg(target_os = "linux")]
    struct Drawn {
        bgrx: Vec<u8>,
        /// The rows drawn since the capture last took in what was drawn.
        rows: Option<(u32, u32)>,
    }

    #[cfg(target_os = "linux")]
    impl Canvas {
        /// Text in rows `y0..y1` as it stands `n` steps into a scroll of 3 px a step.
        fn draw(&self, y0: u32, y1: u32, n: usize) {
            let mut d = self.drawn.lock();
            let w = self.w as usize;
            for (y, row) in d
                .bgrx
                .chunks_exact_mut(w * 4)
                .enumerate()
                .take(y1 as usize)
                .skip(y0 as usize)
            {
                let (line, within) = ((y + 3 * n) / 18, (y + 3 * n) % 18);
                for (x, glyph) in row.chunks_mut(9 * 4).enumerate() {
                    let ink = within < 12 && (x * 2654435761 + line * 40503) >> 7 & 1 == 1;
                    glyph.fill(if ink { 30 } else { 235 });
                }
            }
            d.rows = Some(match d.rows {
                None => (y0, y1),
                Some((a, b)) => (a.min(y0), b.max(y1)),
            });
            drop(d);
            self.news.signal();
        }
    }

    /// The loop's capture of a `Canvas`: it keeps damage as the X11 capturer does
    /// (capture.rs: `pump`, `take_damage`, `take_mb_map`, the same lines, and the same
    /// `Damage`), over pixels the test draws, read in place as from Xvfb's framebuffer.
    #[cfg(target_os = "linux")]
    struct Drawing {
        canvas: Arc<Canvas>,
        damage: Damage,
        mb_dirty: Vec<u8>,
        mb_cols: usize,
        rows: crate::convert::Rows,
    }

    #[cfg(target_os = "linux")]
    impl Capture for Drawing {
        fn size(&self) -> (u32, u32) {
            (self.canvas.w, self.canvas.h)
        }
        fn fd(&self) -> Waitable<'_> {
            self.canvas.news.waitable()
        }
        fn pump(&mut self) -> Result<(), String> {
            self.canvas.news.drain();
            let Some((top, bot)) = self.canvas.drawn.lock().rows.take() else {
                return Ok(());
            };
            for row in (top as usize / 16)..(bot as usize).div_ceil(16) {
                self.mb_dirty[row * self.mb_cols..(row + 1) * self.mb_cols].fill(1);
            }
            self.damage.add(top, bot, Instant::now());
            Ok(())
        }
        fn needs_rebuild(&self) -> bool {
            self.canvas.done.load(Ordering::Relaxed)
        }
        fn damage(&mut self) -> &mut Damage {
            &mut self.damage
        }
        fn take_damage(&mut self) -> Result<Option<Band>, String> {
            Ok(self.damage.take(self.canvas.h))
        }
        fn take_mb_map(&mut self, fresh: Vec<u8>) -> Vec<u8> {
            std::mem::replace(&mut self.mb_dirty, fresh)
        }
        fn full(&self) -> Band {
            Band {
                y0: 0,
                y1: self.canvas.h,
            }
        }
        fn grab(&mut self, _band: Band) -> Result<(), String> {
            Ok(())
        }
        fn convert(&self, dst: &mut I420, y0: usize, y1: usize) -> Result<(), String> {
            let d = self.canvas.drawn.lock();
            let stride = self.canvas.w as usize * 4;
            self.rows.convert(&d.bgrx, stride, dst, y0, y1)
        }
        fn snapshot(&mut self) -> Result<Vec<u8>, String> {
            Ok(self.canvas.drawn.lock().bgrx.clone())
        }
        fn bgrx(&self) -> Option<(&[u8], usize)> {
            None
        }
    }

    /// What the stream's threads allocated over a steady run, by thread.
    #[cfg(target_os = "linux")]
    struct Counted {
        threads: Vec<counting::Thread>,
        /// Allocations by threads past the counting table.
        others: usize,
        /// The frames the run sent.
        frames: usize,
        /// The test's own thread: the viewer, not counted.
        viewer: [u8; 16],
    }

    #[cfg(target_os = "linux")]
    impl Counted {
        /// The stream's threads named so.
        fn named(&self, name: impl Fn(&str) -> bool) -> impl Iterator<Item = &counting::Thread> {
            self.threads
                .iter()
                .filter(move |t| t.name != self.viewer && name(&thread(t)))
        }

        /// Every thread's allocations, a frame, with their sizes.
        fn said(&self) -> String {
            let said: Vec<String> = self
                .threads
                .iter()
                .map(|t| {
                    let sizes: Vec<String> = t
                        .sizes
                        .iter()
                        .flatten()
                        .map(|(size, n)| format!("{size} B x {n}"))
                        .collect();
                    format!(
                        "{}: {:.2} a frame ({})",
                        thread(t),
                        t.allocations as f64 / self.frames as f64,
                        sizes.join(", ")
                    )
                })
                .collect();
            format!(
                "{} frames; by thread (the viewer is this test's own):\n{}\nand {} times on \
                 threads past the table",
                self.frames,
                said.join("\n"),
                self.others
            )
        }
    }

    #[cfg(target_os = "linux")]
    fn thread(t: &counting::Thread) -> String {
        String::from_utf8_lossy(&t.name)
            .trim_end_matches('\0')
            .to_owned()
    }

    /// A thread parks on a parking_lot condition variable, surely: it says it is ready under
    /// the lock and waits, and is told to go only once the lock is free, which waiting frees.
    #[cfg(target_os = "linux")]
    fn parked_once() {
        let state = Arc::new((
            parking_lot::Mutex::new((false, false)),
            parking_lot::Condvar::new(),
        ));
        let waiter = {
            let state = state.clone();
            std::thread::spawn(move || {
                let (lock, cv) = &*state;
                let mut s = lock.lock();
                s.0 = true;
                while !s.1 {
                    cv.wait(&mut s);
                }
            })
        };
        let (lock, cv) = &*state;
        loop {
            let mut s = lock.lock();
            if s.0 {
                s.1 = true;
                cv.notify_one();
                break;
            }
        }
        waiter.join().unwrap();
    }

    /// A steady run of the capture loop, its stream and one viewer, counted: in a process
    /// of its own (see `in_a_process_of_its_own`), so None in the first.
    ///
    /// The loop runs as `spawn` runs it, on its own thread, over a 1024x768 screen the test
    /// draws on (a `Canvas`, captured by a `Drawing` that keeps damage as the X11 capturer
    /// does, so no X server is needed), at the defaults, with one viewer: the stream it
    /// makes for that viewer is the one a real viewer gets. The test draws text scrolling
    /// through rows of any height, asks for keyframes, and lets the screen go still until a
    /// sharpening pass goes; and plays the viewer, taking each frame's messages, writing
    /// them as its socket task would, and acknowledging the frame. First each kind of step
    /// goes through 24 times, which brings every buffer the stream keeps to its size; then
    /// 60 generated steps are counted (a keyframe 1 in 30, sharpening 1 in 10). The test's
    /// own thread is the viewer, and the server's handling of its acks keeps 30 s of them,
    /// so it is not the stream's.
    ///
    /// What it cannot see: x264 and libyuv are C, and allocate with C's malloc, which the
    /// counting allocator does not count; nor what the X11 capturer does over its socket.
    #[cfg(target_os = "linux")]
    fn steady_state(name: &str, convert_threads: usize) -> Option<Counted> {
        if !in_a_process_of_its_own(name) {
            return None;
        }
        let (w, h) = (1024u32, 768u32);
        let cfg = VideoConfig {
            threads: 4,
            convert_threads,
            ..VideoConfig::new(
                ":0".into(),
                Debugging::read(|_| None).unwrap(),
                std::num::NonZeroUsize::MIN,
            )
        };
        let hub = Hub::new().unwrap();
        let (client, lane) = viewer(&hub);
        client.watch();
        let canvas = Arc::new(Canvas {
            w,
            h,
            drawn: parking_lot::Mutex::new(Drawn {
                bgrx: vec![0; (w * h * 4) as usize],
                rows: None,
            }),
            news: crate::hub::Wake::new().unwrap(),
            done: std::sync::atomic::AtomicBool::new(false),
        });
        let (cols, rows) = ((w as usize).div_ceil(16), (h as usize).div_ceil(16));
        let mut drawing = Drawing {
            canvas: canvas.clone(),
            damage: Damage::Unreported,
            mb_dirty: vec![0; cols * rows],
            mb_cols: cols,
            rows: crate::convert::Rows::new(cfg.convert_threads).unwrap(),
        };
        let capture = {
            let (cfg, hub) = (cfg.clone(), hub.clone());
            std::thread::Builder::new()
                .name("capture".into())
                .spawn(move || capture_loop(&mut drawing, cfg, hub, None))
                .unwrap()
        };
        // The viewer: it takes frames off its lane until one of the kind waited for,
        // reporting each decoded, as its relay does.
        struct Viewer {
            lane: Arc<Queue>,
            client: Arc<crate::hub::Client>,
            hub: Arc<Hub>,
            frames: usize,
        }
        impl Viewer {
            fn until(&mut self, what: fn(&Header) -> bool) {
                loop {
                    let frames = self.lane.frames();
                    if frames.is_empty() {
                        // Nothing yet: the stream's threads are coding it.
                        std::thread::yield_now();
                        continue;
                    }
                    let mut found = false;
                    for f in &frames {
                        let now = Instant::now();
                        let reported = f.report(Some(Outcome::Decoded), |r| {
                            crate::session::control::reported(&self.hub, &self.client, r, now)
                        });
                        assert_eq!(reported, Ok(()));
                        for (_, bytes) in &f.members {
                            let Ok(Video::Picture(header, _)) = Header::read(bytes) else {
                                continue;
                            };
                            self.frames += 1;
                            found |= what(&header);
                        }
                    }
                    if found {
                        return;
                    }
                }
            }
        }
        const ANY: fn(&Header) -> bool = |_| true;
        const KEY: fn(&Header) -> bool = |header| header.key;
        const PASS: fn(&Header) -> bool = |header| header.sharpens;
        let mut viewer = Viewer {
            lane,
            client,
            hub: hub.clone(),
            frames: 0,
        };
        let mut rng = Xorshift::new(0x2545F4914F6CDD1D);
        let mut next = move |n: u64| rng.below(n);
        // A keyframe 1 in 30 steps, sharpening 1 in 10.
        const STEPS: usize = 60;
        let kinds: [u64; STEPS] = std::array::from_fn(|_| match next(30) {
            0 => 0,
            1..=3 => 1,
            _ => 2,
        });
        // One step: what the test does, and the frame it waits for.
        let mut step = |kind: u64, n: usize, viewer: &mut Viewer| match kind {
            // The viewer asks for a keyframe.
            0 => {
                viewer.client.ask_keyframe();
                hub.wake();
                viewer.until(KEY);
            }
            // Text scrolls through rows of any height, and the screen is then still until
            // a sharpening pass goes.
            1 => {
                let y0 = next(h as u64) as u32;
                canvas.draw(y0, y0 + 1 + next((h - y0) as u64) as u32, n);
                viewer.until(ANY);
                viewer.until(PASS);
            }
            // Text scrolls: a line, a block of lines, the whole screen.
            _ => {
                let y0 = next(h as u64) as u32;
                let y1 = match next(3) {
                    0 => (y0 + 48).min(h),
                    1 => y0 + 1 + next((h - y0) as u64) as u32,
                    _ => h,
                };
                canvas.draw(y0, y1, n);
                viewer.until(ANY);
            }
        };
        viewer.until(KEY);
        let mut n = 0;
        for _ in 0..3 {
            for kind in 0..3 {
                for _ in 0..8 {
                    n += 1;
                    step(kind, n, &mut viewer);
                }
            }
        }
        // parking_lot makes its table of parked threads (1 KB) the first time any thread
        // parks: a park now, so the count does not catch the process's first contended
        // lock.
        parked_once();
        let test = counting::thread_name();
        let before = viewer.frames;
        counting::arm();
        for kind in kinds {
            n += 1;
            step(kind, n, &mut viewer);
        }
        let (threads, others) = counting::disarm();
        let counted = viewer.frames - before;
        canvas.done.store(true, Ordering::Relaxed);
        hub.wake();
        capture.join().unwrap().unwrap();
        Some(Counted {
            threads,
            others,
            frames: counted,
            viewer: test,
        })
    }

    /// A steady-state frame allocates nothing in the capture loop or its conversion
    /// threads: its block maps and viewer lists come back from the encoder to be used
    /// again, a pass's scratch is the refiner's, and conversion runs on threads started
    /// with the capturer.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_steady_state_frame_allocates_nothing_capturing() {
        let Some(c) = steady_state(
            "video::tests::a_steady_state_frame_allocates_nothing_capturing",
            4,
        ) else {
            return;
        };
        let capturing = |t: &str| t == "capture" || t == "convert";
        let all: usize = c.named(capturing).map(|t| t.allocations).sum();
        assert_eq!(all, 0, "{}", c.said());
    }

    /// A steady-state frame allocates nothing in the encode loop, its lanes included.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_steady_state_frame_allocates_nothing_encoding() {
        let Some(c) = steady_state(
            "video::tests::a_steady_state_frame_allocates_nothing_encoding",
            4,
        ) else {
            return;
        };
        let all: usize = c.named(|t| t == "encode").map(|t| t.allocations).sum();
        assert_eq!(all, 0, "{}", c.said());
    }

    /// The room an open gate finds in a look, and what a keyframe may take while one is
    /// wanted: any two numbers whose frames differ, so a capture says which it took.
    const ROOM: usize = 9_000;
    const KEY_BYTES: usize = 30_000;
    /// The room past the next capture's need a look finds when there is some.
    const SPARE: usize = 4_000;
    /// The blocks `each_case`'s pending refiner holds.
    const PENDING: [u8; 12] = [0, 0, 0, 0, 0, 1, 1, 0, 0, 0, 0, 1];

    /// One look, as the capture loop reads it: the capturer's damage, whether a viewer
    /// wants a keyframe, the last capture, the pacing, the gate, the room to spare,
    /// whether the buffer is home, the area the damage comes to when taken, and the
    /// blocks pending.
    #[derive(Clone, Copy)]
    struct StepCase<'a> {
        damage: Damage,
        want_key: bool,
        now: Instant,
        captured: Captured,
        min_interval: Duration,
        gate: Gated,
        spare: Gated,
        home: bool,
        band: Option<Band>,
        refiner: &'a Refiner,
    }

    impl StepCase<'_> {
        fn key(&self) -> Key {
            match self.want_key {
                true => Key::Wanted { bytes: KEY_BYTES },
                false => Key::NotWanted,
            }
        }
        fn buffer(&self) -> Buffer<()> {
            match self.home {
                true => Buffer::Home(()),
                false => Buffer::Away,
            }
        }
        fn look(&self) -> Look<()> {
            Look {
                key: self.key(),
                damage: self.damage,
                pending: self.refiner.pending(),
                now: self.now,
                captured: self.captured,
                min_interval: self.min_interval,
                gate: self.gate,
                spare: self.spare,
                buffer: self.buffer(),
            }
        }
    }

    /// How many looks `each_case` makes: four frame rates, three damages, 28 pairs of
    /// times since the last change and the last capture, two answers of the gate, and 32
    /// ways for the rest.
    const CASES: usize = 4 * 3 * 28 * 2 * 32;

    /// Every look over a discretised domain: each state the capturer's damage takes; four
    /// frame rates, at seven times since the last change and since the last capture (no
    /// longer ago than the change), either side of the least interval and its rounding to
    /// whole milliseconds; a keyframe wanted or not; the gate open or held; room to spare
    /// or none; the buffer home or out; damage that came to an area or to none; and blocks
    /// pending or none.
    fn each_case(mut check: impl FnMut(StepCase)) -> usize {
        let ns = Duration::from_nanos;
        let us = Duration::from_micros;
        let ms = Duration::from_millis;
        let now = Instant::now() + Duration::from_secs(10);
        let sharp = Refiner::new(PENDING.len());
        let mut pending = Refiner::new(PENDING.len());
        pending.mark(&PENDING, now - ms(500));
        let gates = [Gated::Open(ROOM), Gated::Held];
        let damages = [
            Damage::None,
            Damage::Unreported,
            Damage::Rows {
                first: now - ms(3),
                y0: 16,
                y1: 48,
            },
        ];
        let mut cases = 0;
        for fps in [1, 30, 60, 144] {
            let interval = Duration::from_secs_f64(1.0 / fps as f64);
            let min_interval = interval.mul_f64(MIN_INTERVAL_SHARE);
            let sinces = [
                ns(0),
                min_interval - us(1000),
                min_interval - us(999),
                min_interval - ns(1),
                min_interval,
                min_interval + ns(1),
                Duration::from_secs(5),
            ];
            // The last capture of any kind is the last change or later.
            let captures = sinces.iter().enumerate().flat_map(|(i, &change)| {
                sinces[..=i].iter().map(move |&any| Captured {
                    any: now - any,
                    change: now - change,
                })
            });
            for (damage, captured) in damages
                .into_iter()
                .flat_map(|d| captures.clone().map(move |c| (d, c)))
            {
                for (case, gate) in (0..32).flat_map(|case| gates.map(move |g| (case, g))) {
                    let bit = |i: u32| case >> i & 1 == 1;
                    check(StepCase {
                        damage,
                        want_key: bit(0),
                        now,
                        captured,
                        min_interval,
                        gate,
                        spare: match bit(4) {
                            false => Gated::Held,
                            true => Gated::Open(SPARE),
                        },
                        home: bit(1),
                        band: bit(2).then_some(Band { y0: 16, y1: 48 }),
                        refiner: if bit(3) { &pending } else { &sharp },
                    });
                    cases += 1;
                }
            }
        }
        cases
    }

    /// A look waits for the first of these that holds it, and only for that, else it
    /// captures if the encoder is idle, or waits for the buffer to come home: a still
    /// screen (nothing damaged, no keyframe wanted, and nothing pending, or blocks pending
    /// while the encoder holds the buffer: for a change; or blocks pending with no room to
    /// spare: for the reports, with no time); the rest of the least interval since the last
    /// capture; the lanes' room, with no time. A pass takes exactly the spare room; a change, the gate's; a keyframe, what
    /// it may of the slowest budget. Checked over every look of `each_case`.
    #[test]
    fn a_capture_waits_only_for_what_holds_it() {
        let cases = each_case(|c| {
            let got = step(c.look());
            let moved = c.damage != Damage::None || c.want_key;
            let pending = c.refiner.pending() == Pending::Blocks;
            // A pass goes only to an idle encoder, and into room to spare.
            let pass_goes = !moved && pending && c.home && c.spare != Gated::Held;
            let still = !moved && !pass_goes;
            // A change waits a frame after the last change, a pass after any capture.
            let (next, after) = match moved {
                true => (Next::Change, c.captured.change),
                false => (Next::Pass, c.captured.any),
            };
            let since = c.now - after;
            let early = (since < c.min_interval).then(|| c.min_interval - since);
            // A wait leaves the buffer where it was; a capture takes it.
            let wait = |w| Step::Wait(w, c.buffer());
            let allowance = match (c.want_key, next, c.gate, c.spare) {
                (true, _, _, _) => Some(KEY_BYTES),
                (false, Next::Change, Gated::Open(room), _) => Some(room),
                (false, Next::Pass, _, Gated::Open(room)) => Some(room),
                (false, Next::Change, Gated::Held, _) | (false, Next::Pass, _, Gated::Held) => None,
            };
            let want = match (still, early) {
                (true, _) => wait(match (pending, c.home, c.spare) {
                    (true, true, Gated::Held) => Wait::Gate,
                    (true, true, Gated::Open(_)) => unreachable!("a pass that goes"),
                    (true, false, _) | (false, _, _) => Wait::Change,
                }),
                (false, Some(rest)) => wait(Wait::Interval { rest, next }),
                (false, None) => match (allowance, c.home) {
                    (None, _) => wait(Wait::Gate),
                    (Some(_), false) => wait(Wait::Buffer),
                    (Some(bytes), true) => Step::Capture(next, (), bytes),
                },
            };
            assert_eq!(
                got, want,
                "still {still}, early {early:?}, allowance \
                 {allowance:?}, buffer home {}",
                c.home
            );
        });
        assert_eq!(cases, CASES);
    }

    /// Each wait wakes on what it waits for, and ends on its time: a change on damage or
    /// a wake, with no limit; the interval before
    /// a change only on a wake, before a pass on damage too, or once over; the gate on
    /// damage or a wake (a report), with no limit: no time opens it; a buffer only on a
    /// wake, with no limit. Over
    /// every wait `step` makes in `each_case`.
    #[test]
    fn a_wait_wakes_on_what_it_waits_for() {
        let mut waits = 0;
        each_case(|c| {
            let Step::Wait(wait, _) = step(c.look()) else {
                return;
            };
            waits += 1;
            let mut polled = None;
            wait.through(|on, t| polled = Some((on, t)));
            let want = match wait {
                Wait::Change => (On::Event, None),
                Wait::Interval {
                    rest,
                    next: Next::Change,
                } => (On::Wake, Some(rest)),
                Wait::Interval {
                    rest,
                    next: Next::Pass,
                } => (On::Event, Some(rest)),
                Wait::Gate => (On::Event, None),
                Wait::Buffer => (On::Wake, None),
            };
            assert_eq!(polled, Some(want), "{wait:?}");
        });
        assert!(waits > 0);
    }

    /// A capture is a keyframe while one is wanted, at paint-over quality in place of a
    /// pass when nothing moved and blocks pend; else what moved, if anything did; else a
    /// pass takes every pending block, or finds the screen sharp and sends nothing; else
    /// nothing is sent. Over every look of `each_case`, as a change and as a pass.
    #[test]
    fn a_capture_is_sent_as_what_it_holds() {
        let mut cases = 0;
        each_case(|c| {
            let pending = c.refiner.pending() == Pending::Blocks;
            for next in [Next::Change, Next::Pass] {
                let shown = format!("key {}, band {:?}, {next:?}", c.want_key, c.band);
                match plan(c.key(), c.band, next, c.refiner) {
                    Planned::Send(Plan::Key(Keyframe::Pass)) => {
                        assert!(c.want_key && c.band.is_none() && pending, "{shown}")
                    }
                    Planned::Send(Plan::Key(Keyframe::Fresh)) => {
                        assert!(c.want_key && (c.band.is_some() || !pending), "{shown}")
                    }
                    Planned::Send(Plan::Motion) => {
                        assert!(!c.want_key && c.band.is_some(), "{shown}")
                    }
                    Planned::Send(Plan::Sharpen) => {
                        assert!(!c.want_key && c.band.is_none() && next == Next::Pass);
                        assert!(pending, "{shown}");
                    }
                    Planned::Empty => assert!(
                        !c.want_key && c.band.is_none() && (next == Next::Change || !pending),
                        "{shown}"
                    ),
                }
                cases += 1;
            }
        });
        assert_eq!(cases, 2 * CASES);
    }

    /// The capturer's damage, against what was drawn: over every sequence of five events
    /// from a new capturer (rows drawn at three places, the screen marked to capture, a
    /// take), damage is due exactly when something was drawn or marked since the last
    /// take; it has rows exactly when rows were drawn, stamped with the first of them,
    /// spanning them all; and a take returns those rows widened to even rows
    /// within the picture, and leaves nothing.
    #[test]
    fn damage_is_what_was_drawn_since_the_take() {
        let t0 = Instant::now();
        let height = 101;
        let rows = [(0, 1), (17, 48), (90, 101)];
        for seq in 0..5u32.pow(5) {
            let mut damage = Damage::Unreported;
            // Since the last take: when rows were first drawn and which, and
            // whether anything at all is due (a new capturer is).
            let mut drawn: Option<(Instant, u32, u32)> = None;
            let mut due: bool;
            for i in 0..5 {
                let now = t0 + Duration::from_millis(i as u64);
                match seq / 5u32.pow(i) % 5 {
                    r @ 0..=2 => {
                        let (top, bot) = rows[r as usize];
                        damage.add(top, bot, now);
                        drawn = Some(match drawn {
                            Some((first, a, b)) => (first, a.min(top), b.max(bot)),
                            None => (now, top, bot),
                        });
                        due = true;
                    }
                    3 => {
                        damage.mark();
                        due = true;
                    }
                    _ => {
                        let band = damage.take(height).map(|b| (b.y0, b.y1));
                        let want = drawn.map(|(_, a, b)| (a & !1, ((b + 1) & !1).min(height)));
                        assert_eq!(band, want, "sequence {seq}, event {i}");
                        assert_eq!(damage, Damage::None);
                        (drawn, due) = (None, false);
                    }
                }
                let want = match (drawn, due) {
                    (Some((first, y0, y1)), _) => Damage::Rows { first, y0, y1 },
                    (None, true) => Damage::Unreported,
                    (None, false) => Damage::None,
                };
                assert_eq!(damage, want, "sequence {seq}, event {i}");
            }
        }
    }

    /// Damage that comes while the loop waits out the frame interval for a pass ends the
    /// wait: it makes the capture a change, which the pass does not hold back. Waiting out
    /// the interval for a change, more damage changes nothing, and only a wake ends it
    /// early. Over every interval wait `step` makes in `each_case`.
    #[test]
    fn damage_ends_the_wait_before_a_pass() {
        let mut waits = [0; 2];
        each_case(|c| {
            let Step::Wait(wait @ Wait::Interval { .. }, _) = step(c.look()) else {
                return;
            };
            // What the capture held back would be, as the look says.
            let (next, on) = match c.damage != Damage::None || c.want_key {
                true => (Next::Change, On::Wake),
                false => (Next::Pass, On::Event),
            };
            waits[usize::from(next == Next::Pass)] += 1;
            let mut polled = None;
            wait.through(|on, _| polled = Some(on));
            assert_eq!(polled, Some(on), "waiting out the interval for {next:?}");
        });
        assert!(waits.iter().all(|&n| n > 0), "unreached: {waits:?}");
    }

    /// The hot path at production's shape, as a benchmark (`--ignored`, by name; wall
    /// clock, not a test): one 3024x1898 picture, coded and sent onto a session's lane as
    /// the encode loop does, a capture a frame interval apart; the relay reports each block
    /// decoded once it is on the lane. Typing-sized updates (one macroblock of glyphs
    /// changed) and scroll-sized ones (the page moved up 48 rows: every macroblock), 600
    /// captures each. Prints the CPU each second of screen takes, and percentiles of each
    /// capture's latency from its job to its picture home, its frame on the lane.
    #[test]
    #[ignore = "a benchmark: run by name with --ignored --nocapture"]
    fn bench_hot_path() {
        const W: usize = 3024;
        const H: usize = 1898;
        const CAPTURES: usize = 600;
        /// Captures before allocations are counted: the stream's buffers grow to size.
        const WARM: usize = 120;
        // The process's CPU time.
        let cpu = || {
            // SAFETY: getrusage writes the struct it is given.
            let mut u: libc::rusage = unsafe { std::mem::zeroed() };
            unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
            let t = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
            t(u.ru_utime) + t(u.ru_stime)
        };
        let (cols, rows) = (W.div_ceil(16), H.div_ceil(16));
        if !in_a_process_of_its_own("video::tests::bench_hot_path") {
            return;
        }
        let workloads = [1u32, 2, 3, 4, 6]
            .into_iter()
            .flat_map(|t| [("typing", false, t), ("scroll", true, t)]);
        for (name, scroll, threads) in workloads {
            let hub = Hub::new().unwrap();
            let (client, lane) = viewer(&hub);
            client.watch();
            client.keyframe_planned();
            let (job_tx, job_rx) = mpsc::sync_channel::<Job>(1);
            let (ret_tx, ret_rx) = mpsc::sync_channel::<Returned>(1);
            let encode = {
                let hub = hub.clone();
                std::thread::Builder::new()
                    .name("encode".into())
                    .spawn(move || {
                        let (close, _closing) = mpsc::sync_channel(1);
                        let mut pic = picture(W, H, Some(Kbps::new(30_000.0)));
                        pic.cfg.pictures.threads = threads;
                        let crfs = Crfs {
                            configured: 24.0,
                            paintover: 16.0,
                        };
                        let mut coder = Coder::new(pic, crfs, false, close, unmissed());
                        encode_frames(&mut coder, Fps::new(60).unwrap(), job_rx, ret_tx, &hub)
                    })
                    .unwrap()
            };
            let mut g = Gen(Xorshift::new(0x0123_4567_89ab_cdef));
            let mut buf = Box::new(I420::new(W, H));
            buf.y = g.bytes(W * H);
            let mut latencies = Vec::with_capacity(CAPTURES);
            // The test's relay reading the lane back: not the daemon's work.
            let mut relay_us = 0;
            let (wall, cpu0) = (Instant::now(), cpu());
            for i in 0..CAPTURES {
                // Allocations counted past the warm-up: x264's own (C's malloc) are not.
                if i == WARM {
                    counting::arm();
                }
                let mut dirty = vec![0u8; cols * rows];
                match (i, scroll) {
                    (0, _) => {}
                    (_, true) => {
                        buf.y.copy_within(48 * W.., 0);
                        dirty.fill(1);
                    }
                    (_, false) => {
                        let (bx, by) = (i % cols, (i / cols) % rows);
                        for y in by * 16..(by * 16 + 16).min(H) {
                            for x in bx * 16..(bx * 16 + 16).min(W) {
                                buf.y[y * W + x] = buf.y[y * W + x].wrapping_add(97);
                            }
                        }
                        dirty[by * cols + bx] = 1;
                    }
                }
                let frame = match i {
                    0 => super::Frame::Key {
                        blocks: None,
                        starts: vec![client.clone()],
                    },
                    _ => super::Frame::Motion { dirty },
                };
                let t0 = Instant::now();
                job_tx
                    .send(Job {
                        buf,
                        frame,
                        allowance: 4_000_000,
                        damage: t0,
                        marker_ms: None,
                    })
                    .unwrap();
                buf = ret_rx.recv().unwrap().buf;
                latencies.push(t0.elapsed());
                let now = Instant::now();
                let relay0 = thread_cpu_us().unwrap();
                for block in lane.frames() {
                    block
                        .report(Some(crate::lanes::wire::Outcome::Decoded), |r| {
                            crate::session::control::reported(&hub, &client, r, now)
                        })
                        .unwrap();
                }
                relay_us += thread_cpu_us().unwrap() - relay0;
                let next = t0 + Duration::from_micros(16_667);
                std::thread::sleep(next.saturating_duration_since(Instant::now()));
            }
            let (counted, _) = counting::disarm();
            for t in counted.iter().filter(|t| t.name.starts_with(b"encode")) {
                println!(
                    "{name}, {threads} threads: encode thread's sizes {:?}",
                    t.sizes
                );
            }
            let encoding = counted
                .iter()
                .filter(|t| t.name.starts_with(b"encode"))
                .map(|t| t.allocations)
                .sum::<usize>();
            let (wall, used) = (wall.elapsed(), cpu() - cpu0);
            let used = used - Duration::from_micros(relay_us as u64);
            drop(job_tx);
            encode.join().unwrap().unwrap();
            latencies.sort();
            let at = |p: usize| latencies[(latencies.len() - 1) * p / 100];
            println!(
                "{name}, {threads} threads: {:.3} CPU s per screen s; latency p50 {:?} p90 {:?} p99 {:?} max {:?}; \
                 {:.2} allocations a capture on the encode thread",
                used.as_secs_f64() / wall.as_secs_f64(),
                at(50),
                at(90),
                at(99),
                at(100),
                encoding as f64 / (CAPTURES - WARM) as f64,
            );
        }
    }
}
