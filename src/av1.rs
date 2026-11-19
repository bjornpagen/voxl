//! The desktop as AV1, through libaom's one-pass realtime encoder tuned for the screen: one
//! stream, every frame at one quantiser, predicted from the frames before it, and a
//! keyframe exactly when a viewer starts. The lane is a stream that carries every frame, in
//! order, so a viewer has every picture a frame refers to. The rate follows the capture gate:
//! the frame rate falls when the link does, never the quality.
//!
//! Why no rate control: every frame goes whole and sharp, at one quality, and what is in
//! flight is bounded where it can be, at the capture gate (`session::link`). A frame held
//! to a budget of bytes was built and measured under the previous codec (2026-11-18): a
//! picture coded coarse and refined ended 4.4 to 6.4 dB under the same screen coded once,
//! and no budget held under the coarsest whole-screen frame.
//!
//! The stream's shape fixes the rest: its level (AV1's table), and from the level and the
//! width the tile columns and the threads.

mod ffi {
    include!(concat!(env!("OUT_DIR"), "/aom.rs"));
}

use crate::wire::Coding;
use std::ffi::c_int;

/// A stream's pictures: their size. Every stream runs at most at `video::FRAMERATE`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shape {
    pub width: u32,
    pub height: u32,
}

/// An AV1 level: its `seq_level_idx`, as the sequence header and the codec string carry it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Level(u8);

/// One row of AV1's level table: the bounds a stream's shape and rate meet.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Row {
    level: Level,
    /// Luma samples a picture.
    picture: u64,
    /// The widest picture, in luma samples.
    width: u32,
    /// The tallest picture, in luma samples.
    height: u32,
    /// Luma samples shown a second.
    display_rate: u64,
    /// Tile columns.
    tile_columns: u32,
}

/// The AV1 levels a desktop reaches, lowest first, each with the bounds it sets on a stream
/// (main tier): from `screen::MIN`, 640x480, whose 18,432,000 samples a second at
/// `video::FRAMERATE` need 3.0, through 6.0, which holds every picture over 5.1's 8,912,896
/// samples up to 6.0's display rate (17,825,792 samples at 60 a second; 3840x2400 is
/// 9,216,000). 2.0 and 2.1 hold no desktop of `screen::MIN`; 5.2 and 5.3 differ from 5.1 only
/// in a display rate no picture within 5.1's size reaches at 60 a second; 6.1 and above hold
/// only pictures over 6.0's rate. A picture under 640x480 is within 3.0 too.
///
/// @limitKind published
/// @owner the AV1 Bitstream and Decoding Process Specification, Annex A (levels)
/// @unit per column: seq_level_idx; luma samples; luma samples; luma samples; luma
/// samples/s; tile columns
/// @scope the level a stream declares (the codec string, a recording's `av1C`) and the tile
/// columns it may use under that level
/// @constraint a conformant stream of a level has "UpscaledWidth * FrameHeight is less than
/// or equal to MaxPicSize", "UpscaledWidth is less than or equal to MaxHSize", "FrameHeight
/// is less than or equal to MaxVSize", "TotalDisplayLumaSampleRate is less than or equal to
/// MaxDisplayRate", "TileCols is less than or equal to MaxTileCols"
/// @source https://github.com/AOMediaCodec/av1-spec/blob/master/annex.a.levels.md, the level
/// variables tables, the rows kept quoted as they are written there (Level, MaxPicSize,
/// MaxHSize, MaxVSize, MaxDisplayRate; MaxTileCols):
/// "3.0 | 665856 | 4352 | 2448 | 19,975,680" and 6 tile columns;
/// "3.1 | 1065024 | 5504 | 3096 | 31,950,720" and 6;
/// "4.0 | 2359296 | 6144 | 3456 | 70,778,880" and 8;
/// "4.1 | 2359296 | 6144 | 3456 | 141,557,760" and 8;
/// "5.0 | 8912896 | 8192 | 4352 | 267,386,880" and 8;
/// "5.1 | 8912896 | 8192 | 4352 | 534,773,760" and 8;
/// "6.0 | 35651584 | 16384 | 8704 | 1,069,547,520" and 16.
/// seq_level_idx is the level's (major - 2) * 4 + minor: 13 is 5.1, as the codec report's
/// `av01.0.13M.08` ("Main, level 5.1"). The decode rate, header rate, bitrate and
/// compression ratio columns are left out: every frame is shown, so the decode rate is the
/// display rate, under MaxDecodeRate in every row; the stream is coded at one quantiser, to
/// no bitrate.
/// @verified 2026-11-19
const LEVELS: [Row; 7] = [
    row(4, 665_856, 4_352, 2_448, 19_975_680, 6),
    row(5, 1_065_024, 5_504, 3_096, 31_950_720, 6),
    row(8, 2_359_296, 6_144, 3_456, 70_778_880, 8),
    row(9, 2_359_296, 6_144, 3_456, 141_557_760, 8),
    row(12, 8_912_896, 8_192, 4_352, 267_386_880, 8),
    row(13, 8_912_896, 8_192, 4_352, 534_773_760, 8),
    row(16, 35_651_584, 16_384, 8_704, 1_069_547_520, 16),
];

const fn row(
    level: u8,
    picture: u64,
    width: u32,
    height: u32,
    display_rate: u64,
    tile_columns: u32,
) -> Row {
    Row {
        level: Level(level),
        picture,
        width,
        height,
        display_rate,
        tile_columns,
    }
}

/// The superblock, in luma samples a side: 64x64, set (`AV1E_SET_SUPERBLOCK_SIZE`), so the
/// tile columns derived from it are the ones a stream can have. Left to itself, libaom's
/// realtime mode picks by its threads: 64x64 at 8 threads, 128x128 at 2, at every size
/// from 1280x800 to 3024x1898 (`use_128x128_superblock` in the stream, read with ffmpeg's
/// `trace_headers`, `aomenc --rt --cpu-used=10` with the daemon's settings, 2026-11-19); the
/// codec report measured the daemon's 8 threads, so 64x64.
///
/// @limitKind published
/// @owner the AV1 Bitstream and Decoding Process Specification (tile_info), and libaom
/// @unit luma samples
/// @scope the superblock the encoder uses, and so the most tile columns a width allows
/// @constraint a tile column is whole superblocks: "sbCols = use_128x128_superblock ? ( (
/// MiCols + 31 ) >> 5 ) : ( ( MiCols + 15 ) >> 4 )", "maxLog2TileCols = tile_log2(1,
/// Min(sbCols, MAX_TILE_COLS))" (06.bitstream.syntax.md, tile_info)
/// @source aom_codec.h "AOM_SUPERBLOCK_SIZE_64X64, /**< Always use 64x64 superblocks. */"
/// @verified 2026-11-19
const SUPERBLOCK: u32 = 64;
const SUPERBLOCK_SIZE: ffi::AomSuperblockSize = ffi::AOM_SUPERBLOCK_SIZE_64X64;

/// A stream of a shape no AV1 level of `LEVELS` holds at the frame rate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Unleveled(pub Shape);

/// How a stream is coded, all of it fixed by its shape and the cores it may use: its level,
/// tile columns and threads.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stream {
    pub shape: Shape,
    level: Level,
    /// log2 of the tile columns.
    log2_tiles: u32,
    pub threads: u32,
}

impl Stream {
    /// A stream of `shape`, coded on up to `cores` cores.
    ///
    /// - The level is the lowest of `LEVELS` whose picture size, width, height and display
    ///   rate at `video::FRAMERATE` hold it.
    /// - The tile columns are the most the level takes (a power of two, as libaom spaces
    ///   them evenly) and the width allows, a whole superblock each: 8 at levels 4 and 5,
    ///   the codec report's measured 8 tile columns at 3024x1898; one tile row, since a
    ///   second cost 50% more CPU for nothing there (report §3).
    /// - The threads: one a tile column, at most the cores. 8 threads at 8 tile columns was
    ///   the measured knee (report §3: 16, 32 and 64 threads used the same CPU and were no
    ///   faster).
    pub fn new(shape: Shape, cores: std::num::NonZeroUsize) -> Result<Stream, Unleveled> {
        let row = Row::of(shape)?;
        let columns = row.tile_columns.min(shape.width.div_ceil(SUPERBLOCK));
        let log2_tiles = columns.ilog2();
        // At most 16 (6.0's tile columns), so the threads fit a u32.
        let threads = (1usize << log2_tiles).min(cores.get()) as u32;
        Ok(Stream {
            shape,
            level: row.level,
            log2_tiles,
            threads,
        })
    }

    /// The stream as WebCodecs names its codec: `av01.P.LLT.DD`, the Main profile (4:2:0,
    /// 8-bit), its level, the main tier, 8-bit, as AV1's ISO media binding writes them (AV1
    /// Codec ISO Media File Format Binding, "Codecs Parameter String").
    pub fn codec(&self) -> String {
        format!("av01.0.{:02}M.08", self.level.0)
    }
}

impl Level {
    /// Its `seq_level_idx`, as `av1C` carries it.
    pub fn index(self) -> u8 {
        self.0
    }
}

impl Row {
    /// The least level that holds a stream of `shape` at the frame rate: the table's first
    /// row whose picture size, width, height and display rate it is within.
    fn of(shape: Shape) -> Result<Row, Unleveled> {
        let picture = u64::from(shape.width) * u64::from(shape.height);
        let rate = picture * u64::from(crate::video::FRAMERATE);
        LEVELS
            .iter()
            .find(|r| {
                picture <= r.picture
                    && shape.width <= r.width
                    && shape.height <= r.height
                    && rate <= r.display_rate
            })
            .copied()
            .ok_or(Unleveled(shape))
    }
}

/// libaom's speed, realtime's 0 to 12 (aomcx.h `AOME_SET_CPUUSED`).
///
/// @limitKind measured
/// @owner the daemon's AV1 encoder
/// @unit libaom cpu-used
/// @scope every frame
/// @evidence the codec report (measured 2026-11-19, not in this repository), §3, tables
/// for the server and for the Mac
/// @observed 2026-11-19
/// @environment libaom 3.13.1 on the 64-core Graviton server, 3.15.1 on an M2 Max laptop;
/// screen tune, palette on, quantiser 20, lag 0
/// @workload a full-screen scroll of a dense terminal at 3024x1896, 48 px a frame, keyframes
/// forced mid-stream
/// @result speed 10 at 8 threads: deltas 15.1 to 18.7 ms median, a forced keyframe 34.7 to
/// 37.0 ms; speeds 7 to 9 took 154 to 214 ms a keyframe; 11 and 12 gave larger keyframes
/// and no faster deltas
/// @decision 10, the first speed whose keyframes are fast
/// @revalidate a libaom release, or keyframes slower than a delta's 3x on the server
const SPEED: c_int = 10;

/// The quantiser (libaom's 0 to 63 scale; `base_q_idx` 80) every frame is coded at: its
/// least and its most, keyframes too. A slow link lowers the frame rate (the capture gate),
/// never this.
///
/// @limitKind measured
/// @owner the daemon's AV1 encoder
/// @unit libaom quantiser, 0 (finest) to 63
/// @scope every frame
/// @evidence the codec report, §1 ("Keyframes on five screens") and §2 ("Delta frames",
/// "Legibility")
/// @observed 2026-11-19
/// @environment libaom 3.15.1, speed 10, screen tune, palette only, on an M2 Max laptop
/// @workload five real screens' keyframes at 3024x1898 and 1512x944; real-size scrolls of
/// two terminals; a 132x96 text window of a scroll frame, 12x nearest-neighbour
/// @result at 20, keyframes 49.2 to 58.6 dB, every one at or above the previous codec's at its same
/// quantiser; the text window decoded with 0 of 12,672 pixels off its source. At 36
/// the terminal keyframes were only 1.2 times smaller: palette-coded text barely shrinks
/// with the quantiser
/// @decision 20: pixel-exact text, and a coarser one buys little
/// @revalidate a viewer's judgement of text; a libaom or speed change
const QUANTISER: u32 = 20;

/// Every frame is coded to the screen's tools: libaom's screen tune with its palette, and
/// intra block copy off.
///
/// @limitKind measured
/// @owner the daemon's AV1 encoder
/// @unit libaom controls (`AV1E_SET_TUNE_CONTENT`, `AV1E_SET_ENABLE_PALETTE`,
/// `AV1E_SET_ENABLE_INTRABC`)
/// @scope every frame
/// @evidence the codec report, §1 and "Unknown unknowns"
/// @observed 2026-11-19
/// @environment libaom 3.15.1 (MacPorts) and 3.13.1 (the server's distro)
/// @workload the five screens and scrolls above, with each tool on and off
/// @result with palette and block copy both off, AV1 realtime keyframes were larger and
/// worse than the previous codec's (S1 296,461 B at 48.0 dB against 248,041 B at 51.5);
/// with palette they were 4.5 to 1.4 times smaller. Intra block copy on more than one
/// thread corrupts the picture in libaom 3.15.0 to 3.15.3 ("Encode/decode mismatch on frame
/// 2"), applies to keyframes only, and cost 1.75 times a keyframe's time for 12% fewer
/// bytes
/// @decision the screen tune, palette 1, intra block copy 0, set explicitly whatever the
/// library's default
/// @revalidate a libaom release with the fix ("Fixed RTC IntraBC with multiple threads")
const PALETTE: c_int = 1;
const INTRA_BLOCK_COPY: c_int = 0;

/// The sequence header's colour description: what the capture is. The X framebuffer is
/// sRGB (BT.709 primaries, the sRGB transfer), converted to I420 by libyuv's ARGBToI420,
/// BT.601's matrix at its studio range; the chroma's position, a 2x2 average, is none AV1
/// names.
///
/// @limitKind published
/// @owner libaom's aom_image.h (the CICP values)
/// @unit CICP codes
/// @scope the sequence header, and so a decoder's conversion to RGB
/// @constraint the stream says how its samples were made
/// @source aom_image.h "AOM_CICP_CP_BT_709 = 1", "AOM_CICP_TC_SRGB = 13", "AOM_CICP_MC_BT_601
/// = 6", "AOM_CR_STUDIO_RANGE = 0"
/// @verified 2026-11-19
const PRIMARIES: ffi::AomColorPrimaries = ffi::AOM_CICP_CP_BT_709;
const TRANSFER: ffi::AomTransferCharacteristics = ffi::AOM_CICP_TC_SRGB;
const MATRIX: ffi::AomMatrixCoefficients = ffi::AOM_CICP_MC_BT_601;
const RANGE: ffi::AomColorRange = ffi::AOM_CR_STUDIO_RANGE;

/// A stream as a decoder is configured for it, what a recording's sample entry and `av1C`
/// say: its shape, its level, and the sequence header OBU (with its size field) every one of
/// its keyframes carries.
#[derive(Clone, Debug, PartialEq)]
pub struct Configuration {
    pub shape: Shape,
    pub level: Level,
    pub sequence_header: Vec<u8>,
}

/// The temporal delimiter OBU every temporal unit libaom puts out begins with: its header,
/// type 2 with its size field (0 forbidden, 0010 type, 0 extension, 1 size field, 0
/// reserved: `0x12`), then that size, 0. A recording's samples leave it out; the recorder
/// refuses a unit that does not begin with it (`undelimited`).
///
/// @limitKind published
/// @owner the AV1 Bitstream and Decoding Process Specification (OBU syntax), and AV1 in
/// ISOBMFF
/// @unit bytes
/// @scope a recording's samples
/// @constraint an ISOBMFF sample's "OBUs of type OBU_TEMPORAL_DELIMITER, OBU_PADDING, or
/// OBU_REDUNDANT_FRAME_HEADER SHOULD NOT be used" (aomediacodec.github.io/av1-isobmff)
/// @source 06.bitstream.syntax.md: obu_header "obu_forbidden_bit f(1)", "obu_type f(4)",
/// "obu_extension_flag f(1)", "obu_has_size_field f(1)", "obu_reserved_1bit f(1)";
/// temporal_delimiter_obu "The temporal delimiter has an empty payload"; and
/// 07.bitstream.semantics.md: obu_type 2, "OBU_TEMPORAL_DELIMITER"
/// @verified 2026-11-19
pub const TEMPORAL_DELIMITER: [u8; 2] = [0x12, 0x00];

/// Bytes that are no temporal unit of the encoder's: they do not begin with
/// `TEMPORAL_DELIMITER`.
#[derive(Debug)]
pub struct Undelimited;

/// The temporal unit `unit` without its temporal delimiter, as a recording's sample holds
/// it.
pub fn undelimited(unit: &bytes::Bytes) -> Result<bytes::Bytes, Undelimited> {
    match unit.starts_with(&TEMPORAL_DELIMITER) {
        true => Ok(unit.slice(TEMPORAL_DELIMITER.len()..)),
        false => Err(Undelimited),
    }
}

/// A libaom AV1 encoder, and the configuration of its stream.
pub struct Encoder {
    ctx: Context,
    /// The next frame's number, its timestamp in frames.
    frame: i64,
    configuration: std::sync::Arc<Configuration>,
}

/// An initialised libaom codec context, destroyed when dropped.
struct Context(ffi::AomCodecCtx);

// SAFETY: the codec context is used by one thread at a time: the encoder's owner.
unsafe impl Send for Context {}

impl Drop for Context {
    fn drop(&mut self) {
        // SAFETY: the context was initialised and is destroyed once.
        unsafe { ffi::aom_codec_destroy(&mut self.0) };
    }
}

impl Context {
    fn control(
        &mut self,
        what: &'static str,
        id: ffi::AomeEncControlId,
        value: c_int,
    ) -> Result<(), Failed> {
        // SAFETY: an int control on a live context.
        ok(what, unsafe {
            ffi::aom_codec_control(&mut self.0, id as c_int, value)
        })
    }
}

/// The planes of an I420 picture.
pub struct Planes<'a> {
    pub y: &'a [u8],
    pub u: &'a [u8],
    pub v: &'a [u8],
    pub stride_y: usize,
    pub stride_uv: usize,
}

/// A libaom call that failed, or an encode that broke what the stream promises: which,
/// and libaom's error.
#[derive(Debug)]
pub struct Failed {
    what: &'static str,
    error: ffi::AomCodecErr,
}

impl std::fmt::Display for Failed {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "libaom {}: error {}", self.what, self.error)
    }
}

fn ok(what: &'static str, error: ffi::AomCodecErr) -> Result<(), Failed> {
    match error {
        ffi::AOM_CODEC_OK => Ok(()),
        error => Err(Failed { what, error }),
    }
}

/// A promise of the stream's broken: the error that says which.
fn broken(what: &'static str) -> Failed {
    Failed {
        what,
        error: ffi::AOM_CODEC_ERROR,
    }
}

/// What one picture given to libaom came out as: the frames it put out for it.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Out {
    Nothing,
    One(Coding),
    More,
}

impl Context {
    /// An encoder of `stream`, configured, before its first frame.
    fn open(stream: &Stream) -> Result<Context, Failed> {
        let shape = stream.shape;
        // SAFETY: plain C calls on structs this function owns, each checked.
        unsafe {
            let iface = ffi::aom_codec_av1_cx();
            let mut cfg: ffi::AomCodecEncCfg = std::mem::zeroed();
            ok(
                "default config",
                ffi::aom_codec_enc_config_default(iface, &mut cfg, ffi::AOM_USAGE_REALTIME),
            )?;
            cfg.g_w = shape.width;
            cfg.g_h = shape.height;
            // A frame's timestamp is its number: the rate libaom reads off the time base is
            // the stream's, which sets the level it writes.
            cfg.g_timebase = ffi::AomRational {
                num: 1,
                den: crate::video::FRAMERATE as c_int,
            };
            cfg.g_threads = stream.threads;
            // Every picture given comes out as one frame that shows one picture, at once:
            // a viewer's page holds one update at a time and says it took it when its
            // decoder puts out that update's picture, so an update that showed none would
            // stall its session for good. With no lag libaom has no later picture to code a
            // hidden reference from; and it drops no frame (its dropper is for a rate
            // target, and off). Both are set: libaom 3.15 no longer forces the lag to 0 in
            // realtime. `encode` holds it to this.
            cfg.g_lag_in_frames = 0;
            cfg.rc_dropframe_thresh = 0;
            cfg.rc_min_quantizer = QUANTISER;
            cfg.rc_max_quantizer = QUANTISER;
            // A keyframe only when a viewer starts (`Coding::Key`): none of libaom's own,
            // on a schedule or at a change of scene.
            cfg.kf_mode = ffi::AOM_KF_DISABLED;
            let mut ctx: ffi::AomCodecCtx = std::mem::zeroed();
            ok(
                "init",
                ffi::aom_codec_enc_init_ver(
                    &mut ctx,
                    iface,
                    &cfg,
                    0,
                    ffi::AOM_ENCODER_ABI_VERSION as c_int,
                ),
            )?;
            let mut e = Context(ctx);
            e.control("speed", ffi::AOME_SET_CPUUSED, SPEED)?;
            e.control(
                "screen content",
                ffi::AV1E_SET_TUNE_CONTENT,
                ffi::AOM_CONTENT_SCREEN as c_int,
            )?;
            e.control("palette", ffi::AV1E_SET_ENABLE_PALETTE, PALETTE)?;
            e.control(
                "intra block copy",
                ffi::AV1E_SET_ENABLE_INTRABC,
                INTRA_BLOCK_COPY,
            )?;
            e.control(
                "primaries",
                ffi::AV1E_SET_COLOR_PRIMARIES,
                PRIMARIES as c_int,
            )?;
            e.control(
                "transfer",
                ffi::AV1E_SET_TRANSFER_CHARACTERISTICS,
                TRANSFER as c_int,
            )?;
            e.control("matrix", ffi::AV1E_SET_MATRIX_COEFFICIENTS, MATRIX as c_int)?;
            e.control("range", ffi::AV1E_SET_COLOR_RANGE, RANGE as c_int)?;
            e.control(
                "superblock",
                ffi::AV1E_SET_SUPERBLOCK_SIZE,
                SUPERBLOCK_SIZE as c_int,
            )?;
            e.control("row threads", ffi::AV1E_SET_ROW_MT, 1)?;
            e.control(
                "tile columns",
                ffi::AV1E_SET_TILE_COLUMNS,
                stream.log2_tiles as c_int,
            )?;
            Ok(e)
        }
    }

    /// Codes `p`, `shape`, as frame number `frame`, appending its temporal unit to `out`: a
    /// keyframe if `coding` asks for one, else a delta. What libaom coded it as, by the
    /// frame's own flags, which is what was asked: a picture that made no frame, or more
    /// than one, and a frame coded otherwise than asked, are errors.
    fn encode(
        &mut self,
        (shape, frame): (Shape, i64),
        p: &Planes,
        coding: Coding,
        out: &mut Vec<u8>,
    ) -> Result<Coding, Failed> {
        let flags = match coding {
            Coding::Key => ffi::AOM_EFLAG_FORCE_KF as ffi::AomEncFrameFlags,
            Coding::Delta => 0,
        };
        let mut got = Out::Nothing;
        // SAFETY: the image points at `p`'s planes, read during the call; each packet is
        // copied before the next call.
        unsafe {
            let mut img: ffi::AomImage = std::mem::zeroed();
            ffi::aom_img_wrap(
                &mut img,
                ffi::AOM_IMG_FMT_I420,
                shape.width,
                shape.height,
                1,
                p.y.as_ptr() as *mut u8,
            );
            img.planes[0] = p.y.as_ptr() as *mut u8;
            img.planes[1] = p.u.as_ptr() as *mut u8;
            img.planes[2] = p.v.as_ptr() as *mut u8;
            img.stride[0] = p.stride_y as c_int;
            img.stride[1] = p.stride_uv as c_int;
            img.stride[2] = p.stride_uv as c_int;
            ok(
                "encode",
                ffi::aom_codec_encode(&mut self.0, &img, frame, 1, flags),
            )?;
            let mut iter: ffi::AomCodecIter = std::ptr::null();
            loop {
                let pkt = ffi::aom_codec_get_cx_data(&mut self.0, &mut iter);
                if pkt.is_null() {
                    break;
                }
                if (*pkt).kind == ffi::AOM_CODEC_CX_FRAME_PKT {
                    let f = &(*pkt).data.frame;
                    out.extend_from_slice(std::slice::from_raw_parts(f.buf as *const u8, f.sz));
                    let is = match f.flags & ffi::AOM_FRAME_IS_KEY {
                        0 => Coding::Delta,
                        _ => Coding::Key,
                    };
                    got = match got {
                        Out::Nothing => Out::One(is),
                        Out::One(_) | Out::More => Out::More,
                    };
                }
            }
        }
        match got {
            Out::Nothing => Err(broken("a frame for the picture")),
            Out::One(one) if one == coding => Ok(one),
            Out::One(_) => Err(broken("a frame coded as asked")),
            Out::More => Err(broken("one frame for the picture")),
        }
    }

    /// The stream's sequence header OBU, with its size field, as libaom has settled it: read
    /// once the first frame is coded, since libaom settles some of its tools at that frame
    /// (a header read before it differed from the stream's at 1210x1408).
    fn sequence_header(&mut self) -> Result<Vec<u8>, Failed> {
        // SAFETY: a live context; the buffer it hands over is ours, read and then freed as
        // aom_encoder.h says ("must be freed via call to free()").
        unsafe {
            let headers = ffi::aom_codec_get_global_headers(&mut self.0);
            if headers.is_null() {
                return Err(broken("the sequence header"));
            }
            let h = &*headers;
            let header = std::slice::from_raw_parts(h.buf as *const u8, h.sz).to_vec();
            libc::free(h.buf);
            libc::free(headers.cast());
            Ok(header)
        }
    }
}

impl Encoder {
    /// An encoder of `stream`, which starts it: `first` coded as its first frame, a
    /// keyframe, its temporal unit appended to `out`.
    pub fn start(stream: &Stream, first: &Planes, out: &mut Vec<u8>) -> Result<Encoder, Failed> {
        let mut ctx = Context::open(stream)?;
        ctx.encode((stream.shape, 0), first, Coding::Key, out)?;
        let sequence_header = ctx.sequence_header()?;
        Ok(Encoder {
            ctx,
            frame: 1,
            configuration: std::sync::Arc::new(Configuration {
                shape: stream.shape,
                level: stream.level,
                sequence_header,
            }),
        })
    }

    /// The configuration of the stream it codes.
    pub fn configuration(&self) -> &std::sync::Arc<Configuration> {
        &self.configuration
    }

    /// Codes `p`, appending its temporal unit to `out`: a keyframe if `coding` asks for one,
    /// else a delta; what libaom coded it as, which is what was asked (`Context::encode`).
    pub fn encode(
        &mut self,
        p: &Planes,
        coding: Coding,
        out: &mut Vec<u8>,
    ) -> Result<Coding, Failed> {
        let at = (self.configuration.shape, self.frame);
        self.frame += 1;
        self.ctx.encode(at, p, coding, out)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::xorshift::Xorshift;

    /// A stream's configuration as the encoder makes one: a generated shape within the
    /// levels, its level, and generated bytes standing for its sequence header.
    pub(crate) fn configuration(rng: &mut Xorshift) -> Configuration {
        let shape = Shape {
            width: 2 * rng.range(8, 1920) as u32,
            height: 2 * rng.range(8, 1200) as u32,
        };
        let stream = Stream::new(shape, std::num::NonZeroUsize::MIN).unwrap();
        Configuration {
            shape,
            level: stream.level,
            sequence_header: (0..rng.range(8, 40))
                .map(|_| rng.below(256) as u8)
                .collect(),
        }
    }

    /// Whether a stream of `shape` at the frame rate is within `row`'s bounds, as Annex A
    /// words them for a conformant stream.
    fn within(shape: Shape, row: &Row) -> bool {
        let samples = u64::from(shape.width) * u64::from(shape.height);
        samples <= row.picture
            && shape.width <= row.width
            && shape.height <= row.height
            && samples * u64::from(crate::video::FRAMERATE) <= row.display_rate
    }

    /// Shapes on both sides of every bound where the level changes: for each row's picture
    /// size, display rate at the frame rate, width and height, the largest even shape of a
    /// generated other side within it and the next even one past it.
    fn shapes_at_the_bounds(rng: &mut Xorshift) -> Vec<Shape> {
        let rate = u64::from(crate::video::FRAMERATE);
        let mut shapes = Vec::new();
        for row in &LEVELS {
            for samples in [row.picture, row.display_rate / rate] {
                let width = 2 * rng.range(320, 4096) as u32;
                let height = (samples / u64::from(width)) as u32 & !1;
                shapes.extend([0, 2].map(|more| Shape {
                    width,
                    height: height + more,
                }));
            }
            let side = 2 * rng.range(8, 240) as u32;
            shapes.extend([0, 2].map(|more| Shape {
                width: row.width + more,
                height: side,
            }));
            shapes.extend([0, 2].map(|more| Shape {
                width: side,
                height: row.height + more,
            }));
        }
        shapes
    }

    /// A stream is coded at the lowest level that holds it, with the most tile columns that
    /// level and its width allow, and a thread a tile column within the cores. Over shapes on
    /// both sides of every bound of every level and generated cores: the level is one whose
    /// bounds the shape is within (Annex A's constraints on a conformant stream) and no lower
    /// one's; a shape no level holds is refused; the codec string names the level; the tile
    /// columns are a power of two within the level's MaxTileCols and the width's 64x64
    /// superblocks, and twice as many would not be; the threads are the tile columns, at
    /// most the cores. Annex A's examples at 60 frames a second are at their levels (1920x1080
    /// at 4.1, 3840x2160 at 5.1). At the codec report's measured shape, 3024x1898 on 8 cores, the
    /// stream is level 5.1 (`av01.0.13M.08`), 8 tile columns, 8 threads.
    #[test]
    fn a_stream_is_the_lowest_level_that_holds_it() {
        let knee = Stream::new(
            Shape {
                width: 3024,
                height: 1898,
            },
            std::num::NonZeroUsize::new(8).unwrap(),
        )
        .unwrap();
        assert_eq!(
            (knee.codec(), 1 << knee.log2_tiles, knee.threads),
            ("av01.0.13M.08".to_string(), 8, 8)
        );
        // Annex A's own examples at 60 a second: "4.1 ... 1920x1080@60fps", "5.1 ...
        // 3840x2160@60fps".
        for ((width, height), level) in [((1920, 1080), 9), ((3840, 2160), 13)] {
            let one = std::num::NonZeroUsize::MIN;
            let example = Stream::new(Shape { width, height }, one).unwrap();
            assert_eq!(example.level, Level(level), "{width}x{height}");
        }
        let mut rng = Xorshift::new(0x3c6e_f372_fe94_f82b);
        let (mut levels, mut refused) = (std::collections::BTreeSet::new(), 0);
        for _ in 0..50 {
            for shape in shapes_at_the_bounds(&mut rng) {
                let cores = std::num::NonZeroUsize::new(rng.range(1, 65) as usize).unwrap();
                let lowest = LEVELS.iter().find(|r| within(shape, r));
                let row = match (Stream::new(shape, cores), lowest) {
                    (Ok(stream), Some(row)) => {
                        assert_eq!(stream.level, row.level, "{shape:?}");
                        assert_eq!(stream.codec(), format!("av01.0.{:02}M.08", row.level.0));
                        let room = row.tile_columns.min(shape.width.div_ceil(64));
                        let tiles = 1 << stream.log2_tiles;
                        assert!(tiles <= room && 2 * tiles > room, "{shape:?}");
                        assert_eq!(stream.threads as usize, (tiles as usize).min(cores.get()));
                        row
                    }
                    (Err(Unleveled(s)), None) => {
                        assert_eq!(s, shape);
                        refused += 1;
                        continue;
                    }
                    (got, want) => panic!("{shape:?}: {got:?}, the lowest holding {want:?}"),
                };
                levels.insert(row.level);
            }
        }
        assert!(
            levels.len() == LEVELS.len() && refused > 0,
            "{levels:?}, {refused} refused"
        );
    }

    /// A recording's sample is its temporal unit without the temporal delimiter: for
    /// generated units, the delimiter then any bytes is those bytes, and any bytes that do
    /// not begin with the delimiter are refused.
    #[test]
    fn a_sample_is_its_unit_without_the_delimiter() {
        let mut rng = Xorshift::new(0x9216_d5d9_8979_fb1b);
        let mut refused = 0;
        for _ in 0..10_000 {
            let rest: Vec<u8> = (0..rng.below(40)).map(|_| rng.below(256) as u8).collect();
            let delimited = [&TEMPORAL_DELIMITER[..], &rest].concat();
            let got = undelimited(&bytes::Bytes::from(delimited)).unwrap();
            assert_eq!(got[..], rest[..]);
            let other = bytes::Bytes::from(rest.clone());
            match (undelimited(&other), rest.starts_with(&TEMPORAL_DELIMITER)) {
                (Ok(got), true) => assert_eq!(got[..], rest[2..]),
                (Err(Undelimited), false) => refused += 1,
                (Ok(_), false) | (Err(Undelimited), true) => panic!("{rest:02x?}"),
            }
        }
        assert!(refused > 9_000);
    }
}
