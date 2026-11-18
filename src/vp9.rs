//! The desktop as VP9, through libvpx, with references the daemon chooses for each frame.
//!
//! libvpx's encoder keeps VP9's eight reference slots and, in its SVC mode with the temporal
//! layering left to the caller (`VP9E_TEMPORAL_LAYERING_MODE_BYPASS`), takes per frame the
//! slot it predicts from and the slots it stores the frame in
//! (`VP9E_SET_SVC_REF_FRAME_CONFIG`, vpx/vp8cx.h). That is what makes "a frame refers only
//! to a picture every viewer has" a choice made before the frame is coded, not a repair after
//! a loss: `Slots` holds which picture each slot keeps and whether the viewers have it.
//!
//! Outside SVC mode libvpx names three references (last, golden, altref) by encode flags, but
//! its realtime mode predicts only from last: a frame told to predict from golden or altref
//! came out intra-coded (measured: 298-300 KB a typed character at 3024x1898 against 0-1 KB
//! from last). In SVC mode the predicted-from slot is always "last".
//!
//! The stream's shape fixes the rest: its VP9 level (the published table), and from the
//! level and the width the tile columns, the reference slots used and the threads.

#[allow(
    non_upper_case_globals,
    non_camel_case_types,
    non_snake_case,
    dead_code,
    unused_imports,
    clippy::all
)]
mod ffi {
    include!(concat!(env!("OUT_DIR"), "/vpx.rs"));
}

use std::ffi::c_int;

/// The reference slots a VP9 stream has (the VP9 bitstream specification's `NUM_REF_FRAMES`,
/// 8; a keyframe refreshes all of them, §8.10).
const SLOTS: usize = 8;

/// The temporal layers libvpx is configured with: two, every frame coded in the first.
///
/// One cannot work in SVC mode, whatever targets are set. With SVC on, one pass, libvpx
/// replaces its rate control state before every frame with the layer's
/// (vp9/encoder/vp9_encoder.c `vp9_get_compressed_data`: "if (is_one_pass_svc(cpi)) {"
/// ... "vp9_restore_layer_context(cpi);", and vp9_svc_layercontext.c
/// `vp9_restore_layer_context`: "cpi->rc = lc->rc;"), but sets that layer state up, and
/// takes the layers' targets into it, only for more than one layer (vp9_encoder.c
/// `init_config`, before "vp9_init_layer_context(cpi);", and `vp9_change_config`, before
/// "vp9_update_layer_context_change_config(...)": both under "if
/// ((cpi->svc.number_temporal_layers > 1) || ((cpi->svc.number_temporal_layers > 1 ||
/// cpi->svc.number_spatial_layers > 1) && cpi->oxcf.pass != 1))"), libvpx v1.15.0. With one
/// layer every frame starts from a zeroed rate state: measured, with `rc_target_bitrate`,
/// `ts_target_bitrate[0]`, `layer_target_bitrate[0]` and `ss_target_bitrate[0]` all set and
/// set again through `vpx_codec_enc_config_set`, a page of text came out at QP 62-63 at 2, 8
/// and 30 Mbit/s alike (630 B a scrolled frame against budgets of 4.2 to 62.5 KB); with two,
/// QP 20 for the keyframe and the scroll settling to 20. The second layer is never coded,
/// and VP9's bitstream carries no layer: a decoder sees one stream.
const LAYERS: usize = 2;

/// One row of the VP9 level table, the columns a stream's shape and rate decide.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Row {
    /// The level, times ten (51 is level 5.1), as the codec string writes it.
    level: u8,
    /// Luma samples a second.
    sample_rate: u64,
    /// Luma samples a picture.
    picture: u64,
    /// The longer side, in luma samples.
    breadth: u32,
    /// Average bitrate, kbit/s.
    kbps: u32,
    /// Tile columns.
    tiles: u32,
    /// Reference frame buffers a stream refreshes.
    buffers: u8,
}

/// The VP9 levels, lowest first, each with the bounds it sets on a stream.
///
/// @limitKind published
/// @owner the WebM project's VP9 levels (libvpx's encoder-side copy of the table)
/// @unit per column: luma samples/s, luma samples, luma samples, kbit/s, tile columns,
/// reference frame buffers
/// @scope the level a stream declares in its codec string, and the tiles and reference
/// buffers it may use under that level
/// @constraint a decoder of a level decodes every stream within that row's bounds
/// @source https://github.com/webmproject/libvpx/blob/v1.15.0/vp9/encoder/vp9_encoder.c
/// `vp9_level_defs`, e.g. "{ LEVEL_5_1, 588251136, 8912896, 8384, 120000, 46000, 8, 8, 10,
/// 4 }" (sample rate, size, breadth, bitrate, cpb, compression ratio, column tiles, altref
/// distance, reference buffers); the CPB, compression ratio and altref distance are left
/// out: the first two bound the bits actually sent, under the bitrate's bound, and the
/// stream has no altref frames.
/// @verified 2026-11-18
const LEVELS: [Row; 14] = [
    row(10, 829_440, 36_864, 512, 200, 1, 8),
    row(11, 2_764_800, 73_728, 768, 800, 1, 8),
    row(20, 4_608_000, 122_880, 960, 1_800, 1, 8),
    row(21, 9_216_000, 245_760, 1_344, 3_600, 2, 8),
    row(30, 20_736_000, 552_960, 2_048, 7_200, 4, 8),
    row(31, 36_864_000, 983_040, 2_752, 12_000, 4, 8),
    row(40, 83_558_400, 2_228_224, 4_160, 18_000, 4, 8),
    row(41, 160_432_128, 2_228_224, 4_160, 30_000, 4, 6),
    row(50, 311_951_360, 8_912_896, 8_384, 60_000, 8, 4),
    row(51, 588_251_136, 8_912_896, 8_384, 120_000, 8, 4),
    row(52, 1_176_502_272, 8_912_896, 8_384, 180_000, 8, 4),
    row(60, 1_176_502_272, 35_651_584, 16_832, 180_000, 16, 4),
    row(61, 2_353_004_544, 35_651_584, 16_832, 240_000, 16, 4),
    row(62, 4_706_009_088, 35_651_584, 16_832, 480_000, 16, 4),
];

const fn row(
    level: u8,
    sample_rate: u64,
    picture: u64,
    breadth: u32,
    kbps: u32,
    tiles: u32,
    buffers: u8,
) -> Row {
    Row {
        level,
        sample_rate,
        picture,
        breadth,
        kbps,
        tiles,
        buffers,
    }
}

/// The narrowest a tile column may be, in 64x64 superblocks, and so the most tile columns a
/// width allows.
///
/// @limitKind published
/// @owner the VP9 bitstream specification
/// @unit 64x64 superblocks
/// @scope the tile columns the encoder is configured with
/// @constraint protocol constant: `MIN_TILE_WIDTH_B64`, the bound of `maxLog2TileCols`
/// @source https://storage.googleapis.com/downloads.webmproject.org/docs/vp9/vp9-bitstream-specification-v0.6-20160331-draft.pdf
/// §3 "MIN_TILE_WIDTH_B64 4 Minimum width of a tile in units of superblocks", and
/// `calc_max_log2_tile_cols` (§7.2: "while ( (sb64Cols >> maxLog2) >= MIN_TILE_WIDTH_B64 )")
/// @verified 2026-11-18
const MIN_TILE_WIDTH_B64: u32 = 4;

/// The most threads libvpx takes.
///
/// @limitKind published
/// @owner libvpx
/// @unit threads
/// @scope `g_threads`
/// @constraint hard limit: a larger value fails the encoder's configuration check
/// @source https://github.com/webmproject/libvpx/blob/v1.15.0/vpx_util/vpx_thread.h
/// "#define MAX_NUM_THREADS 64", checked in vp9/vp9_cx_iface.c `validate_config`:
/// "RANGE_CHECK_HI(cfg, g_threads, MAX_NUM_THREADS)"
/// @verified 2026-11-18
const MAX_THREADS: u32 = 64;

/// A stream's pictures: their size and how many a second at most.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shape {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

/// A stream of a shape no VP9 level holds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Unleveled(pub Shape);

/// How a stream is coded, all of it fixed by its shape, the most it is sent at and the
/// cores it may use: its level, tile columns, reference buffers and threads.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stream {
    pub shape: Shape,
    level: Row,
    /// log2 of the tile columns.
    log2_tiles: u32,
    pub threads: u32,
}

impl Stream {
    /// A stream of `shape`, coded on up to `cores` cores.
    ///
    /// - The level is the lowest whose sample rate, picture size and breadth hold it; its
    ///   bitrate is the most the stream is ever sent at (`ceiling`).
    /// - The tile columns are the most the width allows (each at least `MIN_TILE_WIDTH_B64`
    ///   superblocks wide) and the level takes.
    /// - The threads: libvpx's row-based threading codes a tile's superblock rows at once,
    ///   each a superblock behind the row above (`vp9_row_mt_sync_read`, sync range 1), so a
    ///   tile codes at most as many rows at once as it is superblocks wide, and no more than
    ///   it has rows; the threads are what all tiles code at once, at most the cores and
    ///   `MAX_THREADS`.
    pub fn new(shape: Shape, cores: std::num::NonZeroUsize) -> Result<Stream, Unleveled> {
        let breadth = shape.width.max(shape.height);
        let picture = u64::from(shape.width) * u64::from(shape.height);
        let level = *LEVELS
            .iter()
            .find(|r| {
                picture * u64::from(shape.fps) <= r.sample_rate
                    && picture <= r.picture
                    && breadth <= r.breadth
            })
            .ok_or(Unleveled(shape))?;
        let sb_cols = shape.width.div_ceil(64);
        let sb_rows = shape.height.div_ceil(64);
        let by_width = (0..)
            .take_while(|&k| k == 0 || sb_cols >> k >= MIN_TILE_WIDTH_B64)
            .last()
            .unwrap_or(0);
        let log2_tiles = by_width.min(level.tiles.ilog2());
        // A tile's superblock columns, as VP9 splits them (§6.4.3 `get_tile_offset`).
        let at = |i: u32| (i * sb_cols) >> log2_tiles;
        let rows_at_once: u32 = (0..1 << log2_tiles)
            .map(|i| (at(i + 1) - at(i)).min(sb_rows))
            .sum();
        // At most 64 (`MAX_THREADS`), so the cores fit a u32.
        let usable = cores.get().min(MAX_THREADS as usize) as u32;
        let threads = rows_at_once.clamp(1, usable);
        Ok(Stream {
            shape,
            level,
            log2_tiles,
            threads,
        })
    }

    /// The same stream on `threads` threads (a benchmark's sweep).
    #[cfg(test)]
    pub fn on(self, threads: u32) -> Stream {
        Stream { threads, ..self }
    }

    /// The stream as WebCodecs names its codec: `vp09.PP.LL.DD`, profile 0 (4:2:0, 8-bit),
    /// its level, 8-bit, as the VP Codec ISO Media File Format Binding's codecs parameter
    /// writes them (https://www.webmproject.org/vp9/mp4/, "Codecs Parameter String").
    pub fn codec(&self) -> String {
        format!("vp09.00.{:02}.08", self.level.level)
    }

    /// The level, times ten, as `vpcC` and the codec string carry it.
    pub fn level(&self) -> u8 {
        self.level.level
    }

    /// The most the stream is sent at, kbit/s: its level's bitrate, the most a decoder of
    /// the level is promised to take.
    pub fn ceiling(&self) -> u32 {
        self.level.kbps
    }

    /// A viewer starts the stream: its first frame, a keyframe numbered `number`, and the
    /// slots it leaves, the level's reference buffers.
    pub fn start(&self, number: u64) -> (Slots, Key) {
        let mut slots = Slots {
            slots: [Slot::Empty; SLOTS],
            used: usize::from(self.level.buffers),
            every: 1,
            since_store: 0,
            newest: number,
        };
        let key = slots.start(number);
        (slots, key)
    }
}

/// A keyframe: coded from nothing, stored in every slot (VP9 §8.10, a key frame refreshes
/// all reference slots). Only a viewer's start makes one (`Stream::start`, `Slots::start`).
#[derive(Debug, PartialEq)]
pub struct Key(());

/// A frame coded from a picture every viewer has, and stored in a slot or not. Only
/// `Slots::code` makes one, and only from a slot whose picture is had.
#[derive(Debug, PartialEq)]
pub struct Inter {
    from: usize,
    store: Option<usize>,
}

/// What a frame is coded as.
#[derive(Debug, PartialEq)]
pub enum Coding {
    Key(Key),
    Inter(Inter),
}

impl From<Key> for Coding {
    fn from(k: Key) -> Coding {
        Coding::Key(k)
    }
}

impl From<Inter> for Coding {
    fn from(i: Inter) -> Coding {
        Coding::Inter(i)
    }
}

/// The pictures of an encode, as the viewers have them: which picture each slot holds, and
/// whether every viewer it was sent to has it. A frame is coded only from a slot whose
/// picture every viewer has, so no frame a viewer receives refers to a picture it lacks.
///
/// A viewer has a picture once it is sent, on a lane that delivers everything in order (a
/// stream lane, a recording); on a datagram lane once its relay reports it decoded. A
/// keyframe is the picture viewers start on: it is had as it is coded. A viewer that loses
/// it starts again (the hub's), and the slots go on from it for the viewers that have it.
///
/// There is always a picture had: the slots begin with a keyframe in each, a had picture
/// leaves its slot only when a newer had one is kept (the one predicted from is never
/// stored over), and a loss empties only a slot not yet had.
#[derive(Clone, Debug)]
pub struct Slots {
    slots: [Slot; SLOTS],
    /// The slots the stream refreshes (the level's reference buffers): the first so many.
    used: usize,
    /// Frames between stores: the slots beside the one predicted from must span a report's
    /// round trip.
    every: u64,
    since_store: u64,
    /// The newest picture coded.
    newest: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Slot {
    Empty,
    /// Picture `number`, coded and not yet sent.
    Coded(u64),
    /// Picture `number`, sent, and this many reports of it decoded still to come.
    Sent {
        number: u64,
        awaiting: u32,
    },
    /// Picture `number`, which every viewer it was sent to has.
    Had(u64),
}

impl Slot {
    fn number(self) -> Option<u64> {
        match self {
            Slot::Empty => None,
            Slot::Coded(n) | Slot::Sent { number: n, .. } | Slot::Had(n) => Some(n),
        }
    }
}

impl Slots {
    /// A viewer starts: picture `number` is a keyframe, in every slot.
    pub fn start(&mut self, number: u64) -> Key {
        self.newest = number;
        self.slots = [Slot::Had(number); SLOTS];
        self.since_store = 0;
        Key(())
    }

    /// How picture `number` is coded: from the newest picture every viewer has, storing it
    /// every `every` frames in a slot whose picture is of no more use.
    pub fn code(&mut self, number: u64) -> Inter {
        self.newest = number;
        let (had, from) = (0..self.used)
            .filter_map(|i| match self.slots[i] {
                Slot::Had(n) => Some((n, i)),
                Slot::Empty | Slot::Coded(_) | Slot::Sent { .. } => None,
            })
            .max()
            .expect("a picture is always had (see Slots)");
        self.since_store += 1;
        let store = match self.since_store >= self.every {
            false => None,
            // The slot to give up: an empty one, else a picture older than the one
            // predicted from (of no use once a newer is had), else the oldest sent.
            true => (0..self.used).filter(|&i| i != from).min_by_key(|&i| {
                match self.slots[i].number() {
                    None => (0, 0),
                    Some(n) if n < had => (1, n),
                    Some(n) => (2, n),
                }
            }),
        };
        if let Some(s) = store {
            self.slots[s] = Slot::Coded(number);
            self.since_store = 0;
        }
        Inter { from, store }
    }

    /// Picture `number` was sent to every viewer, `reporting` of them on datagram lanes,
    /// whose relays report it: the others have it.
    pub fn sent(&mut self, number: u64, reporting: u32) {
        for s in &mut self.slots {
            if *s == Slot::Coded(number) {
                *s = match reporting {
                    0 => Slot::Had(number),
                    awaiting => Slot::Sent { number, awaiting },
                };
            }
        }
    }

    /// A viewer's relay reported picture `number` decoded: the report's round trip, in
    /// frames, sets how often a picture is stored, so the slots beside the one predicted
    /// from span it.
    pub fn decoded(&mut self, number: u64) {
        let lag = self.newest.saturating_sub(number);
        self.every = lag.div_ceil(self.used as u64 - 1).max(1);
        for s in &mut self.slots {
            if let Slot::Sent {
                number: n,
                awaiting,
            } = *s
            {
                if n == number {
                    *s = match awaiting {
                        1 => Slot::Had(n),
                        _ => Slot::Sent {
                            number: n,
                            awaiting: awaiting - 1,
                        },
                    };
                }
            }
        }
    }

    /// A viewer's relay reported picture `number` missed: a slot of it not yet had holds
    /// nothing usable. A had picture missed is a keyframe, which restarts that viewer alone.
    pub fn lost(&mut self, number: u64) {
        for s in &mut self.slots {
            if let Slot::Coded(n) | Slot::Sent { number: n, .. } = *s {
                if n == number {
                    *s = Slot::Empty;
                }
            }
        }
    }
}

/// A libvpx VP9 encoder, the references each frame's caller's.
pub struct Encoder {
    ctx: ffi::vpx_codec_ctx_t,
    cfg: ffi::vpx_codec_enc_cfg_t,
    shape: Shape,
    pts: i64,
}

// SAFETY: the codec context is used by one thread at a time: the encoder's owner.
unsafe impl Send for Encoder {}

/// The planes of an I420 picture.
pub struct Planes<'a> {
    pub y: &'a [u8],
    pub u: &'a [u8],
    pub v: &'a [u8],
    pub stride_y: usize,
    pub stride_uv: usize,
}

/// The quantiser (libvpx's 0 to 63 scale) a frame is coded at when the link allows: the
/// finest the encoder goes (its floor, so spare rate is not spent past it), and what a
/// still screen's pass is held to (its ceiling), whatever the rate.
///
/// @limitKind measured
/// @owner the daemon's VP9 encoder
/// @unit libvpx quantiser, 0 (finest) to 63
/// @scope the floor of every frame; the ceiling of a pass
/// @evidence scratchpad run output vp9h/examples/sharp.rs (uncommitted), page1280.y: voxl's
/// own source rendered by ffmpeg drawtext in Menlo at 26 px on 1280x5760
/// @observed 2026-11-18, the measuring machine's local day
/// @environment libvpx 1.15.0 (MacPorts), speed 9, screen tune, CBR, error resilient, SVC
/// bypass as `Encoder::new` configures it; an M2 Max laptop
/// @workload 1280x800 of text scrolled 8 px a frame for 30 frames at 2 and 8 Mbit/s (2 is the
/// live link's data share, 257 KB/s), then the still picture coded again; references the
/// previous frame (a stream lane) and 22 frames old (a datagram lane's report round trip,
/// 370 ms at 60 a second); one run per floor of 12, 20, 28
/// @result references the previous frame: the last scroll frame 50.9, 47.0, 43.4 dB luma PSNR
/// at floors 12, 20, 28, at 1.9 KB a frame; a pass adds nothing. References 22 frames old:
/// the last scroll frame 30.3 dB (2 Mbit/s) and 37.8 dB (8 Mbit/s); frames at the rate after
/// it stay there or fall (28.0 dB after four); one pass held to 12, 20, 28: 50.4, 46.4,
/// 42.7 dB at 79, 67, 57 KB. No viewer judgement recorded
/// @decision 20, the coarsest of the three that brings the page past 45 dB, taken
/// provisionally as the line of legible text
/// @revalidate a viewer's judgement of text at 12, 20 and 28; a libvpx or speed change
const TARGET_Q: u32 = 20;

/// The coarsest quantiser libvpx has (vpx_encoder.h `rc_max_quantizer`, 0 to 63).
const COARSEST_Q: u32 = 63;

/// How fine a frame is coded.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Fineness {
    /// As fine as the rate allows, down to `TARGET_Q`.
    Rate,
    /// At `TARGET_Q` or finer, whatever it takes: a still screen's pass.
    Target,
}

/// The quantiser libvpx coded a frame at: at the target, or coarser (the rate held it back).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Came {
    AtTarget,
    Coarser,
}

/// libvpx's realtime speed, 0 to 9 (vp8cx.h `VP8E_SET_CPUUSED`): 9, the fastest, for the
/// least latency a frame adds; screen content keeps its detail through the screen tune.
const SPEED: c_int = 9;

/// A libvpx call that failed: which, and its error.
#[derive(Debug)]
pub struct Failed {
    what: &'static str,
    error: ffi::vpx_codec_err_t,
}

impl std::fmt::Display for Failed {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "libvpx {}: error {}", self.what, self.error)
    }
}

/// `kbps` as the config's rate, the stream's and its layers' (each layer's target counts the
/// layers below it; only the first is coded).
fn rate(cfg: &mut ffi::vpx_codec_enc_cfg_t, kbps: u32) {
    cfg.rc_target_bitrate = kbps;
    for l in 0..LAYERS {
        cfg.ts_target_bitrate[l] = kbps;
        cfg.layer_target_bitrate[l] = kbps;
    }
}

fn ok(what: &'static str, error: ffi::vpx_codec_err_t) -> Result<(), Failed> {
    match error {
        ffi::vpx_codec_err_t_VPX_CODEC_OK => Ok(()),
        error => Err(Failed { what, error }),
    }
}

impl Encoder {
    /// An encoder of `stream` at `kbps`.
    pub fn new(stream: &Stream, kbps: u32) -> Result<Encoder, Failed> {
        let shape = stream.shape;
        // SAFETY: plain C calls on structs this function owns, each checked.
        unsafe {
            let iface = ffi::vpx_codec_vp9_cx();
            let mut cfg: ffi::vpx_codec_enc_cfg_t = std::mem::zeroed();
            ok(
                "default config",
                ffi::vpx_codec_enc_config_default(iface, &mut cfg, 0),
            )?;
            cfg.g_w = shape.width;
            cfg.g_h = shape.height;
            cfg.g_timebase = ffi::vpx_rational { num: 1, den: 1000 };
            cfg.g_threads = stream.threads;
            cfg.g_lag_in_frames = 0;
            // Every frame's entropy coding starts afresh: a frame decodes without the frames
            // before it, only its reference.
            cfg.g_error_resilient = ffi::VPX_ERROR_RESILIENT_DEFAULT;
            cfg.rc_end_usage = ffi::vpx_rc_mode_VPX_CBR;
            // A frame is never dropped: every capture sent is coded.
            cfg.rc_dropframe_thresh = 0;
            cfg.rc_min_quantizer = TARGET_Q;
            cfg.rc_max_quantizer = COARSEST_Q;
            cfg.kf_mode = ffi::vpx_kf_mode_VPX_KF_DISABLED;
            cfg.ss_number_layers = 1;
            cfg.ts_number_layers = LAYERS as u32;
            cfg.ts_periodicity = 1;
            cfg.temporal_layering_mode =
                ffi::vp9e_temporal_layering_mode_VP9E_TEMPORAL_LAYERING_MODE_BYPASS as c_int;
            cfg.ts_rate_decimator[..LAYERS].fill(1);
            rate(&mut cfg, kbps);
            let mut ctx: ffi::vpx_codec_ctx_t = std::mem::zeroed();
            ok(
                "init",
                ffi::vpx_codec_enc_init_ver(
                    &mut ctx,
                    iface,
                    &cfg,
                    0,
                    ffi::VPX_ENCODER_ABI_VERSION as c_int,
                ),
            )?;
            let mut e = Encoder {
                ctx,
                cfg,
                shape,
                pts: 0,
            };
            e.control("speed", ffi::vp8e_enc_control_id_VP8E_SET_CPUUSED, SPEED)?;
            e.control(
                "screen content",
                ffi::vp8e_enc_control_id_VP9E_SET_TUNE_CONTENT,
                ffi::vp9e_tune_content_VP9E_CONTENT_SCREEN as c_int,
            )?;
            // libyuv's ARGBToI420, which converts the capture, uses BT.601's matrix at its
            // studio range: the stream says so (and so does a recording's `vpcC`).
            e.control(
                "colour space",
                ffi::vp8e_enc_control_id_VP9E_SET_COLOR_SPACE,
                ffi::vpx_color_space_VPX_CS_BT_601 as c_int,
            )?;
            e.control("row threads", ffi::vp8e_enc_control_id_VP9E_SET_ROW_MT, 1)?;
            e.control(
                "tile columns",
                ffi::vp8e_enc_control_id_VP9E_SET_TILE_COLUMNS,
                stream.log2_tiles as c_int,
            )?;
            e.control("SVC", ffi::vp8e_enc_control_id_VP9E_SET_SVC, 1)?;
            e.quantisers(COARSEST_Q)?;
            Ok(e)
        }
    }

    fn control(
        &mut self,
        what: &'static str,
        id: ffi::vp8e_enc_control_id,
        value: c_int,
    ) -> Result<(), Failed> {
        // SAFETY: an int control on a live context.
        ok(what, unsafe {
            ffi::vpx_codec_control_(&mut self.ctx, id as c_int, value)
        })
    }

    /// The layers' quantisers from the next frame on: `TARGET_Q` to `coarsest`. In SVC mode
    /// these are the layers' own, not the config's.
    fn quantisers(&mut self, coarsest: u32) -> Result<(), Failed> {
        self.cfg.rc_max_quantizer = coarsest;
        // SAFETY: a plain struct, read during the call.
        unsafe {
            let mut svc: ffi::vpx_svc_extra_cfg_t = std::mem::zeroed();
            for l in 0..LAYERS {
                svc.max_quantizers[l] = coarsest as c_int;
                svc.min_quantizers[l] = TARGET_Q as c_int;
            }
            svc.scaling_factor_num[0] = 1;
            svc.scaling_factor_den[0] = 1;
            svc.speed_per_layer[0] = SPEED;
            svc.temporal_layering_mode = self.cfg.temporal_layering_mode;
            ok(
                "SVC parameters",
                ffi::vpx_codec_control_(
                    &mut self.ctx,
                    ffi::vp8e_enc_control_id_VP9E_SET_SVC_PARAMETERS as c_int,
                    &mut svc as *mut ffi::vpx_svc_extra_cfg_t,
                ),
            )
        }
    }

    /// The target, `kbps`, from the next frame on.
    pub fn set_rate(&mut self, kbps: u32) -> Result<(), Failed> {
        rate(&mut self.cfg, kbps);
        // SAFETY: the config this encoder was opened with, changed in its rate.
        ok("rate", unsafe {
            ffi::vpx_codec_enc_config_set(&mut self.ctx, &self.cfg)
        })
    }

    /// Codes `p` at `ms` as `coding`, as fine as `fineness` says, appending the frame to
    /// `out`: whether it came out at the target. A picture libvpx put out nothing for is the
    /// error.
    pub fn encode(
        &mut self,
        p: &Planes,
        ms: i64,
        coding: Coding,
        fineness: Fineness,
        out: &mut Vec<u8>,
    ) -> Result<Came, Failed> {
        let coarsest = match fineness {
            Fineness::Rate => COARSEST_Q,
            Fineness::Target => TARGET_Q,
        };
        if self.cfg.rc_max_quantizer != coarsest {
            self.quantisers(coarsest)?;
        }
        let (from, store, flags) = match coding {
            Coding::Key(Key(())) => (0, (1 << SLOTS) - 1, ffi::VPX_EFLAG_FORCE_KF as i64),
            Coding::Inter(Inter { from, store }) => (from, store.map_or(0, |s| 1 << s), 0),
        };
        let before = out.len();
        // SAFETY: the layer and reference configs are plain structs, read during the call;
        // the image points at `p`'s planes, read during the call; each packet is copied
        // before the next call.
        unsafe {
            let mut layer: ffi::vpx_svc_layer_id_t = std::mem::zeroed();
            ok(
                "layer",
                ffi::vpx_codec_control_(
                    &mut self.ctx,
                    ffi::vp8e_enc_control_id_VP9E_SET_SVC_LAYER_ID as c_int,
                    &mut layer as *mut ffi::vpx_svc_layer_id_t,
                ),
            )?;
            let mut refs: ffi::vpx_svc_ref_frame_config_t = std::mem::zeroed();
            refs.lst_fb_idx[0] = from as c_int;
            refs.gld_fb_idx[0] = from as c_int;
            refs.alt_fb_idx[0] = from as c_int;
            refs.reference_last[0] = 1;
            refs.update_buffer_slot[0] = store;
            ok(
                "references",
                ffi::vpx_codec_control_(
                    &mut self.ctx,
                    ffi::vp8e_enc_control_id_VP9E_SET_SVC_REF_FRAME_CONFIG as c_int,
                    &mut refs as *mut ffi::vpx_svc_ref_frame_config_t,
                ),
            )?;
            let mut img: ffi::vpx_image_t = std::mem::zeroed();
            ffi::vpx_img_wrap(
                &mut img,
                ffi::vpx_img_fmt_VPX_IMG_FMT_I420,
                self.shape.width,
                self.shape.height,
                1,
                p.y.as_ptr() as *mut u8,
            );
            img.planes[0] = p.y.as_ptr() as *mut u8;
            img.planes[1] = p.u.as_ptr() as *mut u8;
            img.planes[2] = p.v.as_ptr() as *mut u8;
            img.stride[0] = p.stride_y as c_int;
            img.stride[1] = p.stride_uv as c_int;
            img.stride[2] = p.stride_uv as c_int;
            self.pts = ms.max(self.pts + 1);
            let duration = 1000 / self.shape.fps.max(1);
            ok(
                "encode",
                ffi::vpx_codec_encode(
                    &mut self.ctx,
                    &img,
                    self.pts,
                    duration.into(),
                    flags,
                    ffi::VPX_DL_REALTIME.into(),
                ),
            )?;
            let mut iter: ffi::vpx_codec_iter_t = std::ptr::null();
            loop {
                let pkt = ffi::vpx_codec_get_cx_data(&mut self.ctx, &mut iter);
                if pkt.is_null() {
                    break;
                }
                if (*pkt).kind == ffi::vpx_codec_cx_pkt_kind_VPX_CODEC_CX_FRAME_PKT {
                    let f = (*pkt).data.frame;
                    out.extend_from_slice(std::slice::from_raw_parts(f.buf as *const u8, f.sz));
                }
            }
        }
        if out.len() == before {
            return Err(Failed {
                what: "a frame for the picture",
                error: ffi::vpx_codec_err_t_VPX_CODEC_ERROR,
            });
        }
        let mut q: c_int = 0;
        // SAFETY: an int out-parameter on a live context.
        ok("the frame's quantiser", unsafe {
            ffi::vpx_codec_control_(
                &mut self.ctx,
                ffi::vp8e_enc_control_id_VP8E_GET_LAST_QUANTIZER_64 as c_int,
                &mut q as *mut c_int,
            )
        })?;
        Ok(match q as u32 <= TARGET_Q {
            true => Came::AtTarget,
            false => Came::Coarser,
        })
    }

    /// libvpx keeps the stream's level statistics from now on (`VP9E_SET_TARGET_LEVEL` 0).
    #[cfg(test)]
    fn keep_level(&mut self) -> Result<(), Failed> {
        self.control(
            "level statistics",
            ffi::vp8e_enc_control_id_VP9E_SET_TARGET_LEVEL,
            0,
        )
    }

    /// The level libvpx measured the stream at, times ten (`VP9E_GET_LEVEL`).
    #[cfg(test)]
    fn measured_level(&mut self) -> Result<c_int, Failed> {
        let mut level: c_int = 0;
        // SAFETY: an int out-parameter on a live context.
        ok("level", unsafe {
            ffi::vpx_codec_control_(
                &mut self.ctx,
                ffi::vp8e_enc_control_id_VP9E_GET_LEVEL as c_int,
                &mut level as *mut c_int,
            )
        })?;
        Ok(level)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: the context was initialised and is destroyed once.
        unsafe { ffi::vpx_codec_destroy(&mut self.ctx) };
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::xorshift::Xorshift;

    /// A screen of text lines, `rows` tall: dark glyph-like marks on a light page.
    pub(crate) fn page(w: usize, rows: usize, rng: &mut Xorshift) -> Vec<u8> {
        let mut y = vec![235u8; w * rows];
        for line in 0..rows / 20 {
            for _ in 0..w / 9 {
                let (x0, len) = (rng.below(w as u64 - 8) as usize, 1 + rng.below(6) as usize);
                for r in line * 20 + 4..line * 20 + 15 {
                    for x in x0..x0 + len {
                        y[r * w + x] = 30;
                    }
                }
            }
        }
        y
    }

    /// What ffmpeg (from `PATH`) decodes `file` to, in software (libavcodec's own VP9
    /// decoder), its pictures as I420, with every error fatal (`-err_detect explode
    /// -xerror`); or what it said instead.
    pub(crate) fn decode(file: &std::path::Path) -> Result<Vec<u8>, String> {
        let out = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-err_detect", "explode", "-xerror", "-i"])
            .arg(file)
            .args([
                "-f",
                "rawvideo",
                "-pix_fmt",
                "yuv420p",
                "-fps_mode",
                "passthrough",
                "-",
            ])
            .output()
            .map_err(|e| e.to_string())?;
        let said = String::from_utf8_lossy(&out.stderr).into_owned();
        match out.status.success() && said.is_empty() {
            true => Ok(out.stdout),
            false => Err(said),
        }
    }

    /// The codec gate. A stream coded with references only to pictures every viewer has is
    /// decodable whatever is lost: over generated runs of typing and scrolling on a page of
    /// text, with one to three viewers on datagram lanes (each frame but the first lost to
    /// each with a quarter's chance, the live link's measured loss before repair, each loss
    /// and each decode reported a generated round trip of 1 to 30 frames later) and one on
    /// a stream lane or none, the frames each viewer received are decoded by ffmpeg in
    /// software, every error fatal: each comes out as exactly the picture the whole stream
    /// decodes it to. (This Mac's VideoToolbox decoder is gated by bench/videotoolbox-gate.)
    #[test]
    fn every_frame_that_arrives_decodes_whatever_was_lost() {
        let (w, h) = (640usize, 384usize);
        let dir = std::env::temp_dir().join(format!("voxl-vp9-gate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut rng = Xorshift::new(0x6a09_e667_f3bc_c908);
        let mut lost_total = 0;
        let shape = Shape {
            width: w as u32,
            height: h as u32,
            fps: 60,
        };
        let stream = Stream::new(shape, std::num::NonZeroUsize::MIN.saturating_add(1)).unwrap();
        for case in 0..6 {
            let text = page(w, h * 4, &mut rng);
            let datagram = 1 + rng.below(3) as usize;
            let lags: Vec<u64> = (0..datagram).map(|_| 1 + rng.below(30)).collect();
            let mut enc = Encoder::new(&stream, 8_000).unwrap();
            let (mut slots, key) = stream.start(0);
            let mut key = Some(key);
            let (u, v) = (vec![128u8; w * h / 4], vec![128u8; w * h / 4]);
            // Reports due: (at frame, number, arrived).
            let mut due: Vec<(u64, u64, bool)> = Vec::new();
            let mut whole = ivf_header(w, h);
            let mut arrived: Vec<(Vec<u8>, Vec<usize>)> = (0..datagram)
                .map(|_| (ivf_header(w, h), Vec::new()))
                .collect();
            let mut top = 0usize;
            let mut screen = text[..w * h].to_vec();
            for f in 0..120u64 {
                for &(_, n, came) in due.iter().filter(|d| d.0 <= f) {
                    match came {
                        true => slots.decoded(n),
                        false => slots.lost(n),
                    }
                }
                due.retain(|d| d.0 > f);
                match rng.below(3) {
                    // A scroll of a few rows.
                    0 => {
                        top = (top + 1 + rng.below(8) as usize).min(h * 3);
                        screen.copy_from_slice(&text[top * w..(top + h) * w]);
                    }
                    // Typing: a mark changes.
                    _ => {
                        let at = rng.below((w * h) as u64) as usize;
                        screen[at] = screen[at].wrapping_add(128);
                    }
                }
                let coding = match key.take() {
                    Some(key) => Coding::from(key),
                    None => slots.code(f).into(),
                };
                let mut out = Vec::new();
                let p = Planes {
                    y: &screen,
                    u: &u,
                    v: &v,
                    stride_y: w,
                    stride_uv: w / 2,
                };
                enc.encode(&p, f as i64 * 16, coding, Fineness::Rate, &mut out)
                    .unwrap();
                slots.sent(f, datagram as u32);
                ivf_frame(&mut whole, f, &out);
                for (viewer, (ivf, got)) in arrived.iter_mut().enumerate() {
                    let came = f == 0 || rng.below(4) > 0;
                    due.push((f + lags[viewer], f, came));
                    match came {
                        true => {
                            ivf_frame(ivf, f, &out);
                            got.push(f as usize);
                        }
                        false => lost_total += 1,
                    }
                }
            }
            let whole_file = dir.join(format!("whole{case}.ivf"));
            std::fs::write(&whole_file, &whole).unwrap();
            let frame = w * h * 3 / 2;
            let truth = decode(&whole_file).unwrap();
            assert_eq!(truth.len(), 120 * frame, "case {case}");
            for (viewer, (ivf, got_frames)) in arrived.iter().enumerate() {
                let file = dir.join(format!("arrived{case}-{viewer}.ivf"));
                std::fs::write(&file, ivf).unwrap();
                let got =
                    decode(&file).unwrap_or_else(|e| panic!("case {case}, viewer {viewer}: {e}"));
                assert_eq!(got.len(), got_frames.len() * frame, "case {case}");
                for (i, &f) in got_frames.iter().enumerate() {
                    let (got, want) = (&got[i * frame..][..frame], &truth[f * frame..][..frame]);
                    assert!(got == want, "case {case}, viewer {viewer}: frame {f}");
                }
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(lost_total > 100, "{lost_total} lost");
    }

    /// A stream declares the level libvpx measures it at: over generated shapes from 64x64
    /// to 4096x2304 at 30 or 60 frames a second, a run of frames coded as `Slots` codes
    /// them (keyframe, then pictures stored round the level's reference buffers), timed at
    /// the stream's rate and sent at a rate under every level's bitrate, is measured by
    /// libvpx's own level statistics (`VP9E_GET_LEVEL`) at exactly the level the codec
    /// string names; and the tile columns are within the level's and the width's bounds.
    #[test]
    fn a_stream_is_at_the_level_it_declares() {
        let mut rng = Xorshift::new(0x3c6e_f372_fe94_f82b);
        let mut levels = std::collections::BTreeSet::new();
        for _ in 0..24 {
            let shape = Shape {
                width: 2 * rng.range(32, 2049) as u32,
                height: 2 * rng.range(32, 1153) as u32,
                fps: [30, 60][rng.below(2) as usize],
            };
            let stream = Stream::new(shape, std::num::NonZeroUsize::MIN).unwrap();
            assert!(1 << stream.log2_tiles <= stream.level.tiles);
            assert!(
                shape.width.div_ceil(64) >> stream.log2_tiles >= MIN_TILE_WIDTH_B64
                    || stream.log2_tiles == 0
            );
            let mut enc = Encoder::new(&stream, 150).unwrap();
            enc.keep_level().unwrap();
            let (w, h) = (shape.width as usize, shape.height as usize);
            let y = page(w, h, &mut rng);
            let (u, v) = (vec![128u8; w * h / 4], vec![128u8; w * h / 4]);
            let (mut slots, key) = stream.start(0);
            let mut key = Some(key);
            for f in 0..3 * SLOTS as u64 {
                let p = Planes {
                    y: &y,
                    u: &u,
                    v: &v,
                    stride_y: w,
                    stride_uv: w / 2,
                };
                let ms = (f * 1000 / u64::from(shape.fps)) as i64;
                let coding = match key.take() {
                    Some(key) => Coding::from(key),
                    None => slots.code(f).into(),
                };
                enc.encode(&p, ms, coding, Fineness::Rate, &mut Vec::new())
                    .unwrap();
                slots.sent(f, 0);
            }
            let measured = enc.measured_level().unwrap();
            assert_eq!(measured, c_int::from(stream.level()), "{shape:?}");
            levels.insert(stream.level());
        }
        assert!(levels.len() >= 4, "levels reached: {levels:?}");
    }

    /// A frame is coded only from a picture every viewer it was sent to has: over generated
    /// runs of up to four viewers, each on a stream lane (it has what it is sent) or a
    /// datagram lane (it has a picture once its report says decoded, each frame lost with a
    /// chance of one in four and reported 0 to 40 frames later), viewers starting at random
    /// (a keyframe), the picture each `Inter` predicts from, followed through the slots'
    /// stores, is one every viewer had been sent and has. `code` can make no keyframe (its
    /// type is `Inter`), so a run never falls back to one.
    #[test]
    fn a_frame_refers_only_to_a_picture_every_viewer_has() {
        let mut rng = Xorshift::new(0x9b05_688c_2b3e_6c1f);
        let stream = Stream::new(
            Shape {
                width: 3024,
                height: 1898,
                fps: 60,
            },
            std::num::NonZeroUsize::MIN,
        )
        .unwrap();
        let (mut stores, mut stale) = (0, 0);
        for _ in 0..400 {
            let viewers = 1 + rng.below(4) as usize;
            let datagram: Vec<bool> = (0..viewers).map(|_| rng.below(2) == 0).collect();
            let lags: Vec<u64> = (0..viewers).map(|_| rng.below(41)).collect();
            let (mut slots, _key) = stream.start(0);
            // What each slot holds, as the stores say, and what each viewer has.
            let mut held = [0u64; SLOTS];
            let mut has: Vec<std::collections::HashSet<u64>> =
                (0..viewers).map(|_| [0].into()).collect();
            let mut due: Vec<(u64, usize, u64, bool)> = Vec::new();
            for f in 1..300u64 {
                for &(_, v, n, came) in due.iter().filter(|d| d.0 <= f) {
                    match came {
                        true => {
                            has[v].insert(n);
                            slots.decoded(n);
                        }
                        false => slots.lost(n),
                    }
                }
                due.retain(|d| d.0 > f);
                if rng.below(100) == 0 {
                    let _key = slots.start(f);
                    held = [f; SLOTS];
                    has.iter_mut().for_each(|h| *h = [f].into());
                    continue;
                }
                let Inter { from, store } = slots.code(f);
                let picture = held[from];
                for (v, h) in has.iter().enumerate() {
                    assert!(h.contains(&picture), "viewer {v} lacks {picture} at {f}");
                }
                stale += (f - picture > 1) as usize;
                if let Some(s) = store {
                    held[s] = f;
                    stores += 1;
                }
                let mut reporting = 0;
                for v in 0..viewers {
                    match datagram[v] {
                        false => {
                            has[v].insert(f);
                        }
                        true => {
                            reporting += 1;
                            due.push((f + lags[v], v, f, rng.below(4) > 0));
                        }
                    }
                }
                slots.sent(f, reporting);
            }
        }
        assert!(
            stores > 10_000 && stale > 10_000,
            "{stores} stores, {stale} stale"
        );
    }

    /// An IVF file's header for VP9 at `w`x`h` (the WebM project's IVF layout).
    pub(crate) fn ivf_header(w: usize, h: usize) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(b"DKIF");
        b.extend_from_slice(&0u16.to_le_bytes());
        b.extend_from_slice(&32u16.to_le_bytes());
        b.extend_from_slice(b"VP90");
        b.extend_from_slice(&(w as u16).to_le_bytes());
        b.extend_from_slice(&(h as u16).to_le_bytes());
        b.extend_from_slice(&1000u32.to_le_bytes());
        b.extend_from_slice(&1u32.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b
    }

    pub(crate) fn ivf_frame(b: &mut Vec<u8>, pts: u64, frame: &[u8]) {
        b.extend_from_slice(&(frame.len() as u32).to_le_bytes());
        b.extend_from_slice(&pts.to_le_bytes());
        b.extend_from_slice(frame);
    }
}
