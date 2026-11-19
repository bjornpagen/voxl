//! The desktop as VP9, through libvpx's one-pass realtime encoder as it comes: one stream,
//! every frame at one quantiser, predicted as libvpx chooses from the frames before it, and a
//! keyframe only when a viewer starts. The lane is a stream that carries every frame, in
//! order, so a viewer has every picture a frame refers to. The rate follows the capture gate:
//! the frame rate falls when the link does, never the quality.
//!
//! The stream's shape fixes the rest: its VP9 level (the published table), and from the
//! level and the width the tile columns and the threads.

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
    /// Tile columns.
    tiles: u32,
}

/// The VP9 levels, lowest first, each with the bounds it sets on a stream.
///
/// @limitKind published
/// @owner the WebM project's VP9 levels (libvpx's encoder-side copy of the table)
/// @unit per column: luma samples/s, luma samples, luma samples, tile columns
/// @scope the level a stream declares in its codec string, and the tiles it may use under
/// that level
/// @constraint a decoder of a level decodes every stream within that row's bounds
/// @source https://github.com/webmproject/libvpx/blob/v1.15.0/vp9/encoder/vp9_encoder.c
/// `vp9_level_defs`, e.g. "{ LEVEL_5_1, 588251136, 8912896, 8384, 120000, 46000, 8, 8, 10,
/// 4 }" (sample rate, size, breadth, bitrate, cpb, compression ratio, column tiles, altref
/// distance, reference buffers); the bitrate, CPB, compression ratio, altref distance and
/// reference buffers are left out: the stream is coded at one quantiser, to no bitrate, so
/// the level it declares bounds its pictures' size and rate, not its bits.
/// @verified 2026-11-18
const LEVELS: [Row; 14] = [
    row(10, 829_440, 36_864, 512, 1),
    row(11, 2_764_800, 73_728, 768, 1),
    row(20, 4_608_000, 122_880, 960, 1),
    row(21, 9_216_000, 245_760, 1_344, 2),
    row(30, 20_736_000, 552_960, 2_048, 4),
    row(31, 36_864_000, 983_040, 2_752, 4),
    row(40, 83_558_400, 2_228_224, 4_160, 4),
    row(41, 160_432_128, 2_228_224, 4_160, 4),
    row(50, 311_951_360, 8_912_896, 8_384, 8),
    row(51, 588_251_136, 8_912_896, 8_384, 8),
    row(52, 1_176_502_272, 8_912_896, 8_384, 8),
    row(60, 1_176_502_272, 35_651_584, 16_832, 16),
    row(61, 2_353_004_544, 35_651_584, 16_832, 16),
    row(62, 4_706_009_088, 35_651_584, 16_832, 16),
];

const fn row(level: u8, sample_rate: u64, picture: u64, breadth: u32, tiles: u32) -> Row {
    Row {
        level,
        sample_rate,
        picture,
        breadth,
        tiles,
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

/// How a stream is coded, all of it fixed by its shape and the cores it may use: its level,
/// tile columns and threads.
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
    /// - The level is the lowest whose sample rate, picture size and breadth hold it.
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
}

/// A keyframe's picture size, as its uncompressed header says it (the VP9 bitstream
/// specification §6.2, `uncompressed_header`, through `frame_size`); none for a frame that is
/// no keyframe, or too short to say. The size rides the keyframe: nothing else carries it.
pub fn keyframe_size(frame: &[u8]) -> Option<(u32, u32)> {
    let mut at = 0usize;
    let mut bits = |n: usize| -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = frame.get(at / 8)?;
            v = v << 1 | u32::from(byte >> (7 - at % 8) & 1);
            at += 1;
        }
        Some(v)
    };
    // frame_marker, then the profile's low and high bits.
    if bits(2)? != 2 {
        return None;
    }
    let profile = bits(1)? | bits(1)? << 1;
    if profile == 3 {
        bits(1)?;
    }
    // show_existing_frame, then frame_type (0, a keyframe), show_frame, error_resilient_mode.
    if bits(1)? == 1 || bits(1)? != 0 {
        return None;
    }
    bits(2)?;
    // frame_sync_code.
    if bits(24)? != 0x49_83_42 {
        return None;
    }
    // color_config: the bit depth past profile 1, the colour space, and its range and
    // subsampling (or, for RGB, a reserved bit) as the profile has them.
    if profile >= 2 {
        bits(1)?;
    }
    const CS_RGB: u32 = 7;
    match (bits(3)? == CS_RGB, profile) {
        (false, 1 | 3) => drop(bits(4)?),
        (false, _) => drop(bits(1)?),
        (true, 1 | 3) => drop(bits(1)?),
        (true, _) => {}
    }
    Some((bits(16)? + 1, bits(16)? + 1))
}

/// What a frame is coded as.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Coding {
    /// From nothing: a viewer starts on it.
    Key,
    /// From the frames before it, as libvpx predicts.
    Delta,
}

/// A libvpx VP9 encoder.
pub struct Encoder {
    ctx: ffi::vpx_codec_ctx_t,
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

/// The quantiser (libvpx's 0 to 63 scale) every frame is coded at: its least and its most,
/// whatever the link. A slow link lowers the frame rate (the capture gate), never this.
///
/// @limitKind measured
/// @owner the daemon's VP9 encoder
/// @unit libvpx quantiser, 0 (finest) to 63
/// @scope every frame
/// @evidence scratchpad run output vp9h/examples/sharp.rs (uncommitted), page1280.y: voxl's
/// own source rendered by ffmpeg drawtext in Menlo at 26 px on 1280x5760
/// @observed 2026-11-18, the measuring machine's local day
/// @environment libvpx 1.15.0 (MacPorts), speed 9, screen tune, CBR, error resilient, SVC
/// bypass as `Encoder::new` then configured it (before the plain encoder); an M2 Max laptop
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
/// @revalidate a viewer's judgement of text at 12, 20 and 28; a libvpx or speed change; the
/// move to the plain encoder (no SVC, no error resilience), which this was not measured on
const TARGET_Q: u32 = 20;

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

fn ok(what: &'static str, error: ffi::vpx_codec_err_t) -> Result<(), Failed> {
    match error {
        ffi::vpx_codec_err_t_VPX_CODEC_OK => Ok(()),
        error => Err(Failed { what, error }),
    }
}

impl Encoder {
    /// An encoder of `stream`.
    pub fn new(stream: &Stream) -> Result<Encoder, Failed> {
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
            cfg.rc_min_quantizer = TARGET_Q;
            cfg.rc_max_quantizer = TARGET_Q;
            // A keyframe only when a viewer starts (`Coding::Key`).
            cfg.kf_mode = ffi::vpx_kf_mode_VPX_KF_DISABLED;
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
            let mut e = Encoder { ctx, shape, pts: 0 };
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

    /// Codes `p` at `ms` as `coding`, appending the frame to `out`. A picture libvpx put out
    /// nothing for is the error.
    pub fn encode(
        &mut self,
        p: &Planes,
        ms: i64,
        coding: Coding,
        out: &mut Vec<u8>,
    ) -> Result<(), Failed> {
        let flags = match coding {
            Coding::Key => ffi::VPX_EFLAG_FORCE_KF as i64,
            Coding::Delta => 0,
        };
        let before = out.len();
        // SAFETY: the image points at `p`'s planes, read during the call; each packet is
        // copied before the next call.
        unsafe {
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
        Ok(())
    }

    /// The quantiser the last frame was coded at (`VP8E_GET_LAST_QUANTIZER_64`).
    #[cfg(test)]
    fn last_quantiser(&mut self) -> Result<u32, Failed> {
        let mut q: c_int = 0;
        // SAFETY: an int out-parameter on a live context.
        ok("the frame's quantiser", unsafe {
            ffi::vpx_codec_control_(
                &mut self.ctx,
                ffi::vp8e_enc_control_id_VP8E_GET_LAST_QUANTIZER_64 as c_int,
                &mut q as *mut c_int,
            )
        })?;
        Ok(q as u32)
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

    /// Pictures of typing and scrolling on a page of text, `n` of them, `w` x `h`: the luma of
    /// each, from a page four screens tall.
    fn screens(w: usize, h: usize, n: usize, rng: &mut Xorshift) -> Vec<Vec<u8>> {
        let text = page(w, h * 4, rng);
        let mut top = 0usize;
        let mut screen = text[..w * h].to_vec();
        (0..n)
            .map(|_| {
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
                screen.clone()
            })
            .collect()
    }

    /// Codes `y` as the frame at `f`, `coding`, appending it to `out`.
    fn code(
        enc: &mut Encoder,
        (w, h): (usize, usize),
        y: &[u8],
        f: u64,
        coding: Coding,
        out: &mut Vec<u8>,
    ) {
        let (u, v) = (vec![128u8; w * h / 4], vec![128u8; w * h / 4]);
        let p = Planes {
            y,
            u: &u,
            v: &v,
            stride_y: w,
            stride_uv: w / 2,
        };
        let ms = (f * 1000 / u64::from(enc.shape.fps)) as i64;
        enc.encode(&p, ms, coding, out).unwrap();
        assert_eq!(
            enc.last_quantiser().unwrap(),
            TARGET_Q,
            "a frame off the quantiser"
        );
    }

    /// The codec gate. A viewer decodes the stream from the keyframe it starts on: over
    /// generated runs of typing and scrolling on a page of text, with keyframes forced where
    /// viewers start (the first frame, and now and then later), the whole stream and the
    /// stream from each keyframe on are decoded by ffmpeg in software, every error fatal, and
    /// each frame from a keyframe comes out exactly as the whole stream decodes it. A delta
    /// that referred past the keyframe before it would decode otherwise, or not at all. (This
    /// Mac's VideoToolbox decoder is gated by bench/videotoolbox-gate.)
    #[test]
    fn a_viewer_decodes_the_stream_from_the_keyframe_it_starts_on() {
        let (w, h) = (640usize, 384usize);
        let dir = std::env::temp_dir().join(format!("voxl-vp9-gate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut rng = Xorshift::new(0x6a09_e667_f3bc_c908);
        let shape = Shape {
            width: w as u32,
            height: h as u32,
            fps: 60,
        };
        let stream = Stream::new(shape, std::num::NonZeroUsize::MIN.saturating_add(1)).unwrap();
        let frame = w * h * 3 / 2;
        let mut later_keys = 0;
        for case in 0..6 {
            let mut enc = Encoder::new(&stream).unwrap();
            let mut frames: Vec<(bool, Vec<u8>)> = Vec::new();
            for (f, y) in screens(w, h, 120, &mut rng).iter().enumerate() {
                let key = f == 0 || rng.below(30) == 0;
                later_keys += usize::from(key && f > 0);
                let coding = match key {
                    true => Coding::Key,
                    false => Coding::Delta,
                };
                let mut out = Vec::new();
                code(&mut enc, (w, h), y, f as u64, coding, &mut out);
                frames.push((key, out));
            }
            let decode_from = |from: usize| {
                let mut ivf = ivf_header(w, h);
                for (f, (_, b)) in frames.iter().enumerate().skip(from) {
                    ivf_frame(&mut ivf, f as u64, b);
                }
                let file = dir.join(format!("from{case}-{from}.ivf"));
                std::fs::write(&file, ivf).unwrap();
                decode(&file).unwrap_or_else(|e| panic!("case {case}, from {from}: {e}"))
            };
            let truth = decode_from(0);
            assert_eq!(truth.len(), 120 * frame, "case {case}");
            for (k, _) in frames.iter().enumerate().filter(|(_, (key, _))| *key) {
                let got = decode_from(k);
                assert!(
                    got[..] == truth[k * frame..],
                    "case {case}: from keyframe {k}"
                );
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(later_keys > 5, "{later_keys} keyframes after the first");
    }

    /// Every keyframe says its size, and no other frame says one: over generated shapes,
    /// libvpx's keyframes read back as their stream's size, its deltas as none; and any frame
    /// cut short reads as no size or its own, never another, and never panics.
    #[test]
    fn a_keyframe_says_its_size() {
        let mut rng = Xorshift::new(0xbb67_ae85_84ca_a73b);
        for _ in 0..12 {
            let (w, h) = (
                2 * rng.range(32, 700) as usize,
                2 * rng.range(32, 500) as usize,
            );
            let shape = Shape {
                width: w as u32,
                height: h as u32,
                fps: 60,
            };
            let stream = Stream::new(shape, std::num::NonZeroUsize::MIN).unwrap();
            let mut enc = Encoder::new(&stream).unwrap();
            for (f, y) in screens(w, h, 6, &mut rng).iter().enumerate() {
                let key = f % 3 == 0;
                let coding = match key {
                    true => Coding::Key,
                    false => Coding::Delta,
                };
                let mut out = Vec::new();
                code(&mut enc, (w, h), y, f as u64, coding, &mut out);
                let want = key.then_some((w as u32, h as u32));
                assert_eq!(keyframe_size(&out), want, "{w}x{h}, frame {f}");
                for cut in 0..out.len().min(16) {
                    let got = keyframe_size(&out[..cut]);
                    assert!(got.is_none() || got == want, "{w}x{h} cut at {cut}");
                }
            }
        }
    }

    /// A stream declares the level libvpx measures it at: over generated shapes from 64x64
    /// to 4096x2304 at 30 or 60 frames a second, a keyframe and a run of frames after it,
    /// timed at the stream's rate and sent at a rate under every level's bitrate, is
    /// measured by libvpx's own level statistics (`VP9E_GET_LEVEL`) at exactly the level the
    /// codec string names; and the tile columns are within the level's and the width's
    /// bounds.
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
            let mut enc = Encoder::new(&stream).unwrap();
            enc.keep_level().unwrap();
            let (w, h) = (shape.width as usize, shape.height as usize);
            let y = page(w, h, &mut rng);
            for f in 0..24u64 {
                let coding = match f {
                    0 => Coding::Key,
                    _ => Coding::Delta,
                };
                code(&mut enc, (w, h), &y, f, coding, &mut Vec::new());
            }
            let measured = enc.measured_level().unwrap();
            assert_eq!(measured, c_int::from(stream.level()), "{shape:?}");
            levels.insert(stream.level());
        }
        assert!(levels.len() >= 4, "levels reached: {levels:?}");
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
