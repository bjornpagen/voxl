//! Thin safe wrapper around libx264 tuned for low-latency desktop streaming.

#![allow(
    non_camel_case_types,
    non_upper_case_globals,
    non_snake_case,
    dead_code
)]
mod ffi {
    include!(concat!(env!("OUT_DIR"), "/x264.rs"));
}

use crate::env::{Debugging, X264Log};
use crate::path::Kbps;
use std::ffi::{c_char, c_int, CStr, CString};
use std::num::NonZeroU32;
use std::ptr;

/// What every encoder is tuned for: no frame held back to look ahead.
const TUNE: &CStr = c"zerolatency";

/// An x264 preset (`--preset`): a name x264 has, asked of x264 when voxl starts, so no
/// encoder refuses it later, at a viewer's join.
#[derive(Clone, Debug, PartialEq)]
pub struct Preset(CString);

impl Preset {
    /// The preset named `name`, if x264 has one by that name.
    pub fn parse(name: &str) -> Option<Preset> {
        let name = CString::new(name).ok()?;
        // x264 fills in the parameters itself before it applies the preset.
        let mut param: ffi::x264_param_t = unsafe { std::mem::zeroed() };
        let known =
            unsafe { ffi::x264_param_default_preset(&mut param, name.as_ptr(), TUNE.as_ptr()) }
                >= 0;
        known.then_some(Preset(name))
    }
}

impl std::fmt::Display for Preset {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}", self.0.to_string_lossy())
    }
}

/// A frame rate: 1 to `Fps::MAX` frames a second, so that every use of it holds it: a
/// frame's time, x264's rate, and the four frames' worth of it under a VBV buffer
/// (`vbv_buffer_kbit`), which leaves a u32 past 2^30 frames a second.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Fps(NonZeroU32);

impl Fps {
    /// The most frames a second. Upstream's bound for the same flag on macOS (its macOS
    /// support, 4fdf528, clamps `--framerate` to it), unmeasured: the refresh of the
    /// fastest common displays. At 120 a 144 or 240 Hz display could not be streamed at
    /// its rate; at 480 nothing here breaks. It bounds what may be asked, so that every
    /// use above holds it.
    pub const MAX: u32 = 240;

    /// `fps`, if it is 1 to `MAX`.
    pub const fn new(fps: u32) -> Option<Fps> {
        match NonZeroU32::new(fps) {
            Some(rate) if fps <= Fps::MAX => Some(Fps(rate)),
            _ => None,
        }
    }

    pub fn get(self) -> u32 {
        self.0.get()
    }
}

/// What every encoder is opened with, whatever decides its bits: the pictures' size and
/// rate, the preset and threads that code them, and the debugging variables voxl was
/// started with (x264's log and hints, and a line per picture).
#[derive(Clone)]
pub struct Pictures {
    pub width: u32,
    pub height: u32,
    pub fps: Fps,
    pub preset: Preset,
    pub threads: u32,
    pub debug: Debugging,
}

/// The settings of an encoder that codes at a quality, under a cap.
#[derive(Clone)]
pub struct EncoderConfig {
    pub pictures: Pictures,
    pub crf: f32,
    /// The VBV cap. None: uncapped for good, since x264 turns VBV on only when it opens
    /// and `set_rate` cannot add a cap later.
    pub max_kbps: Option<Kbps>,
}

/// How far x264's motion search looks, in pixels. A trackpad flick through a terminal
/// moves the text up to ~140 px between frames at 60 fps, and its speed changes every
/// frame, so the vectors the search starts from are tens of pixels off. At 64 the frames
/// that moved more than 32 px came out 19-57% bigger than at 128, for the same picture.
const ME_RANGE: u32 = 128;

pub struct Encoder {
    h: *mut ffi::x264_t,
    param: ffi::x264_param_t,
    pic: ffi::x264_picture_t,
    pts: i64,
    pub width: u32,
    pub height: u32,
    debug: Debugging,
    /// The frame rate it was opened with, which sizes its VBV buffer.
    fps: Fps,
}

unsafe impl Send for Encoder {}

pub struct Planes<'a> {
    pub y: &'a [u8],
    pub u: &'a [u8],
    pub v: &'a [u8],
    pub stride_y: usize,
    pub stride_uv: usize,
}

impl Encoder {
    pub fn new(cfg: &EncoderConfig) -> Result<Self, String> {
        let EncoderConfig {
            ref pictures,
            crf,
            max_kbps,
        } = *cfg;
        unsafe {
            let mut param: ffi::x264_param_t = std::mem::zeroed();
            let preset = &pictures.preset;
            let accepted =
                ffi::x264_param_default_preset(&mut param, preset.0.as_ptr(), TUNE.as_ptr()) >= 0;
            assert!(
                accepted,
                "x264 refused the preset {preset} it had when voxl started"
            );
            param.i_width = pictures.width as i32;
            param.i_height = pictures.height as i32;
            param.i_csp = ffi::X264_CSP_I420 as i32;
            param.i_fps_num = pictures.fps.get();
            param.i_fps_den = 1;
            param.i_timebase_num = 1;
            param.i_timebase_den = 1000;
            // VFR input costs a frame of encoder delay (duration estimation).
            param.b_vfr_input = 0;
            param.i_threads = pictures.threads as i32;
            param.b_sliced_threads = (pictures.threads > 1) as i32;
            param.i_slice_count = pictures.threads as i32;
            param.i_keyint_max = ffi::X264_KEYINT_MAX_INFINITE as i32;
            param.i_keyint_min = 1;
            param.i_scenecut_threshold = 0;
            param.b_intra_refresh = 0;
            param.i_bframe = 0;
            param.b_annexb = 1;
            param.b_repeat_headers = 1;
            param.b_aud = 0;
            param.i_log_level = match pictures.debug.x264_log {
                X264Log::Warning => ffi::X264_LOG_WARNING,
                X264Log::Info => ffi::X264_LOG_INFO,
                // Per-frame QP, macroblock types and size.
                X264Log::Debug => ffi::X264_LOG_DEBUG,
            } as i32;
            // Into voxl's log, where x264's own would write to stderr from the encoding thread.
            param.pf_log = Some(ffi::voxl_x264_log);
            param.rc.i_rc_method = ffi::X264_RC_CRF as i32;
            param.rc.f_rf_constant = crf;
            param.rc.i_lookahead = 0;
            // ultrafast's diamond search only follows motion of a few pixels, so a dragged
            // window or a fast scroll would be coded as new pixels every frame. UMH finds
            // those shifts (a third of the bits for a window drag) for about the same CPU.
            param.analyse.i_me_method = ffi::X264_ME_UMH as i32;
            param.analyse.i_me_range = ME_RANGE as i32;
            // What a scroll or a page of new text costs is the pixels that weren't on the
            // screen before, and ultrafast codes those the dearest way H.264 has: whole
            // 16x16 blocks, with the simple entropy coder. Text is edges a few pixels wide:
            // 4x4 intra blocks follow them, and CABAC packs what is left. Together a
            // scrolling page of text costs 30% fewer bits at the same quantizer for about
            // 2 ms more a frame at 3024x1898 (`scroll_cost` below); unchanged areas are
            // skipped as before, so a desktop at rest costs what it did (`encode_cost`).
            param.b_cabac = 1;
            param.analyse.inter = ffi::X264_ANALYSE_I4x4;
            // The same blocks in keyframes: `inter` only covers intra blocks inside P
            // frames, and ultrafast leaves `intra` empty, so a keyframe was still all
            // 16x16. And ultrafast picks each block's mode by SAD on whole pixels only;
            // one step finer (SATD, a half-pixel pass) picks better ones for no more
            // time. A keyframe of text is a third smaller, a page of new text 6%.
            param.analyse.intra = ffi::X264_ANALYSE_I4x4;
            param.analyse.i_subpel_refine = 1;
            // Per-macroblock "unchanged" hints from X damage let x264 skip analysis.
            param.analyse.b_mb_info = pictures.debug.mb_info as i32;
            param.i_sync_lookahead = 0;
            param.rc.b_mb_tree = 0;
            if let Some(cap) = max_kbps {
                let kbps = cap.get() as u32;
                param.rc.i_vbv_max_bitrate = kbps as i32;
                // ~1.25 frames of buffer keeps single-frame spikes bounded.
                param.rc.i_vbv_buffer_size = vbv_buffer_kbit(kbps, pictures.fps) as i32;
            }
            // sRGB content (BT.709 primaries, sRGB transfer) in BT.601 limited range,
            // matching libyuv ARGBToI420 and the macOS NV12 capture.
            param.vui.i_colorprim = 1;
            param.vui.i_transfer = 13; // IEC 61966-2-1: every capture path delivers sRGB
            param.vui.i_colmatrix = 6;
            param.vui.b_fullrange = 0;
            let profile = c"high";
            ffi::x264_param_apply_profile(&mut param, profile.as_ptr());

            let h = ffi::x264_encoder_open(&mut param);
            if h.is_null() {
                return Err("x264_encoder_open failed".into());
            }
            let mut pic: ffi::x264_picture_t = std::mem::zeroed();
            ffi::x264_picture_init(&mut pic);
            pic.img.i_csp = ffi::X264_CSP_I420 as i32;
            pic.img.i_plane = 3;
            Ok(Encoder {
                h,
                param,
                pic,
                pts: 0,
                width: pictures.width,
                height: pictures.height,
                debug: pictures.debug,
                fps: pictures.fps,
            })
        }
    }

    /// As `new`, with the motion search looking `me_range` pixels instead of `ME_RANGE`,
    /// so a test can show what the range buys. Set by reconfiguration before the first
    /// picture, it gives the stream x264 gives when opened at that range, byte for byte.
    #[cfg(test)]
    fn with_me_range(cfg: &EncoderConfig, me_range: u32) -> Result<Self, String> {
        let mut enc = Self::new(cfg)?;
        enc.param.analyse.i_me_range = me_range as i32;
        match unsafe { ffi::x264_encoder_reconfig(enc.h, &mut enc.param) } {
            0.. => Ok(enc),
            _ => Err(format!("x264 refused a motion search range of {me_range}")),
        }
    }

    /// Reconfigure rate control on the fly (buffer of ~1.25 frames at `max_kbps`).
    pub fn set_rate(&mut self, crf: f32, max_kbps: u32) {
        self.set_rate_buf(crf, max_kbps, vbv_buffer_kbit(max_kbps, self.fps));
    }

    /// Change the CRF alone; the cap stays what it was.
    #[cfg(test)]
    pub fn set_crf(&mut self, crf: f32) {
        self.param.rc.f_rf_constant = crf;
        unsafe { ffi::x264_encoder_reconfig(self.h, &mut self.param) };
    }

    /// Reconfigure rate control with an explicit VBV buffer (kbit).
    pub fn set_rate_buf(&mut self, crf: f32, max_kbps: u32, buf_kbit: u32) {
        unsafe {
            self.param.rc.f_rf_constant = crf;
            if max_kbps > 0 {
                self.param.rc.i_vbv_max_bitrate = max_kbps as i32;
                self.param.rc.i_vbv_buffer_size = buf_kbit.max(1) as i32;
            }
            ffi::x264_encoder_reconfig(self.h, &mut self.param);
        }
    }

    /// Encode one picture. `force_idr` requests a keyframe, `qp` (if set) forces the frame QP.
    /// Appends Annex-B output to `out`.
    /// `mb_info` (one byte per macroblock, X264_MBINFO_CONSTANT for unchanged ones) lets
    /// x264 skip analysis of static areas; it is consumed before this returns.
    pub fn encode(
        &mut self,
        p: &Planes,
        ts_ms: i64,
        force_idr: bool,
        qp: Option<i32>,
        mb_info: Option<&mut [u8]>,
        out: &mut Vec<u8>,
    ) -> Result<Encoded, String> {
        unsafe {
            self.pic.img.plane[0] = p.y.as_ptr() as *mut u8;
            self.pic.img.plane[1] = p.u.as_ptr() as *mut u8;
            self.pic.img.plane[2] = p.v.as_ptr() as *mut u8;
            self.pic.img.i_stride[0] = p.stride_y as i32;
            self.pic.img.i_stride[1] = p.stride_uv as i32;
            self.pic.img.i_stride[2] = p.stride_uv as i32;
            self.pic.i_type = if force_idr {
                ffi::X264_TYPE_IDR as i32
            } else {
                ffi::X264_TYPE_AUTO as i32
            };
            self.pic.i_qpplus1 = qp.map(|q| q + 1).unwrap_or(0);
            self.pic.prop.mb_info = mb_info.map(|m| m.as_mut_ptr()).unwrap_or(ptr::null_mut());
            self.pic.prop.mb_info_free = None;
            // Strictly increasing pts even if the wall clock stalls.
            self.pts = ts_ms.max(self.pts + 1);
            self.pic.i_pts = self.pts;
            let mut nals: *mut ffi::x264_nal_t = ptr::null_mut();
            let mut n: i32 = 0;
            let mut pic_out: ffi::x264_picture_t = std::mem::zeroed();
            let size =
                ffi::x264_encoder_encode(self.h, &mut nals, &mut n, &mut self.pic, &mut pic_out);
            if size < 0 {
                return Err("x264_encoder_encode failed".into());
            }
            if size > 0 && !nals.is_null() {
                // NAL payloads are contiguous in memory.
                let first = &*nals;
                out.extend_from_slice(std::slice::from_raw_parts(first.p_payload, size as usize));
            }
            if self.debug.marker.is_some() {
                log::info!(
                    "x264 out {} bytes, delayed {}, pts in {} out {}",
                    size,
                    ffi::x264_encoder_delayed_frames(self.h),
                    self.pts,
                    pic_out.i_pts
                );
            }
            Ok(match size {
                0 => Encoded::Nothing,
                _ => Encoded::Picture {
                    keyframe: pic_out.b_keyframe != 0,
                    // Undocumented as an output, but x264 sets it on every frame it puts
                    // out (encoder.c, `pic_out->i_qpplus1 = i_global_qp + 1`), and
                    // FFmpeg's libx264 wrapper reads it the same way.
                    qp: (pic_out.i_qpplus1 - 1) as f32,
                    crf_avg: pic_out.prop.f_crf_avg as f32,
                },
            })
        }
    }
}

/// A VBV buffer of 1.25 frames at `max_kbps`, rounded up: x264 treats a buffer under
/// 1.1 frames as a single-frame VBV, which may lower rows of a frame below its starting
/// QP (video.rs's MotionQuality relies on it not doing that).
fn vbv_buffer_kbit(max_kbps: u32, fps: Fps) -> u32 {
    (max_kbps * 5).div_ceil(4 * fps.get())
}

/// What x264 made of one picture.
pub enum Encoded {
    /// x264 put nothing out for it.
    Nothing,
    /// A frame.
    Picture {
        keyframe: bool,
        /// The QP x264 started the frame at, rounded. VBV may have raised rows of the
        /// frame above it; x264 doesn't report the average.
        qp: f32,
        /// x264's "average effective CRF": the CRF the frame was encoded with plus what
        /// VBV added to its average QP, before the frame and within it.
        crf_avg: f32,
    },
}

/// Whether x264 coded a frame at the CRF it was asked for.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Quality {
    AsAsked,
    /// Coarser than asked: the rate cap raised its quantizers, or x264 was still coming
    /// down from the frames before it.
    Coarser,
}

/// Where a stripe's last frame left x264's QP, for judging the next one.
///
/// x264 starts a predicted frame at the CRF plus a term from the picture's complexity,
/// which it blurs over the frames before, halving their weight each frame. After a
/// keyframe or motion, passes over a still picture start well above where x264 comes to
/// rest and fall about 2.4 QP a frame: on a 1280x768 page of text, uncapped, after a
/// keyframe at CRF 24 (QP 21) passes at CRF 16 started at QP 24, 21, 19, 16, 14, 12, 11,
/// 10, 9, 9 and sent 38, 64, 1,812, 19,283, then fewer bytes, 285 and 74 by the last; after
/// twelve frames of scrolling, at 24, 22, 19, 17, 15, 13, 11, 10, 9, 9. A pass x264 started
/// lower than the frame before it has not caught up with the picture it codes.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Settling {
    /// Nothing to go by: the encoder is new, the last frame was a keyframe (whose own
    /// intra cost x264 blurs into the frames after it), or x264 put nothing out.
    Unknown,
    /// The last frame was predicted, and x264 started it this many QP over its CRF.
    Over(f32),
}

impl Encoded {
    /// Whether x264 coded the frame at `crf`: a predicted frame, started no lower above
    /// `crf` than the frame before it (`before`), which the rate cap left at `crf` but for
    /// a small squeeze.
    ///
    /// The start: judged by the cap alone, the first pass after a keyframe the cap left
    /// alone (38 bytes, above) sharpened every block. Judged by its QP against `crf` alone,
    /// the pass at QP 16 did (49.9 dB on a 640x384 page, where x264 went on to 56.1 at QP
    /// 9; live, after a keyframe from an encoder opened for it, 57.5-57.6 dB at 40 Mbit/s
    /// against 60.5-60.6 judged this way). Started no lower than the frame before, a pass
    /// comes where x264 rests. No number: the bound is the frame before. A keyframe never
    /// counts, nor the frame after one: x264 plans those from its own rules.
    ///
    /// The squeeze: `crf_avg` is `crf` plus what the cap added to the frame's average QP,
    /// before the frame and within it, so a frame the cap left alone reads `crf` exactly. A
    /// sharpening pass over a slow link almost never does: the cap raises a few of its
    /// rows. So a raise of under half a step on average (half the rows one step coarser, or
    /// fewer rows by more) counts as asked. It is a tolerance, not rounding: at 6 Mbit/s, 2
    /// of 2,503 sharpening frames read `crf` exactly, and a quarter of a step or a whole one
    /// sharpened a page, a flick and a scroll in the same time as half a step.
    pub fn quality(&self, crf: f32, before: Settling) -> Quality {
        match (self, before) {
            (
                Encoded::Picture {
                    keyframe: false,
                    qp,
                    crf_avg,
                },
                Settling::Over(over),
            ) if qp - crf >= over && *crf_avg < crf + 0.5 => Quality::AsAsked,
            (Encoded::Nothing | Encoded::Picture { .. }, Settling::Unknown | Settling::Over(_)) => {
                Quality::Coarser
            }
        }
    }

    /// Where this frame, coded at `crf`, leaves x264's QP for the next one.
    pub fn settling(&self, crf: f32) -> Settling {
        match self {
            Encoded::Picture {
                keyframe: false,
                qp,
                ..
            } => Settling::Over(qp - crf),
            Encoded::Picture { keyframe: true, .. } | Encoded::Nothing => Settling::Unknown,
        }
    }

    /// Whether it is a keyframe.
    pub fn keyframe(&self) -> bool {
        match self {
            Encoded::Picture { keyframe, .. } => *keyframe,
            Encoded::Nothing => false,
        }
    }
}

/// A line x264 logged (src/x264_log.c), as voxl's under the target `x264`. x264's level
/// already chose what it says.
#[no_mangle]
extern "C" fn voxl_x264_line(level: c_int, line: *const c_char, len: usize) {
    // SAFETY: x264_log.c hands over `len` bytes of the line it formatted, alive for this call.
    let line = unsafe { std::slice::from_raw_parts(line.cast::<u8>(), len) };
    let level = match level as u32 {
        ffi::X264_LOG_ERROR => log::Level::Error,
        ffi::X264_LOG_WARNING => log::Level::Warn,
        ffi::X264_LOG_INFO => log::Level::Info,
        ffi::X264_LOG_DEBUG.. => log::Level::Debug,
    };
    log::log!(target: "x264", level, "{}", String::from_utf8_lossy(line).trim_end());
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { ffi::x264_encoder_close(self.h) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A page of text `rows` tall, light on dark: words of 5x7 glyphs drawn 3x in 18x30
    /// cells, from a 16-letter alphabet, so letters repeat as they do in real text.
    fn text_page(w: usize, rows: usize) -> Vec<u8> {
        let mut page = vec![12u8; w * rows];
        let hash = |n: usize| (n as u64).wrapping_mul(0x9E3779B97F4A7C15) >> 29;
        let glyphs: Vec<u64> = (1..=16).map(|i| hash(i) & ((1 << 35) - 1)).collect();
        for line in 0..rows / 30 {
            let len = (hash(line + 99) % 60) as usize;
            for col in 0..len.min(w / 18) {
                if col % 6 == 5 || hash(line * 977 + col / 6) % 9 == 0 {
                    continue; // a space, or a blank word
                }
                let glyph = glyphs[(hash(line * 31 + col) % 16) as usize];
                for gy in 0..7 {
                    for gx in (0..5).filter(|gx| (glyph >> (gy * 5 + gx)) & 1 == 1) {
                        for py in 0..3 {
                            let at = (line * 30 + 4 + gy * 3 + py) * w + col * 18 + 2 + gx * 3;
                            page[at..at + 3].fill(220);
                        }
                    }
                }
            }
        }
        page
    }

    /// Bytes of the frames that moved 32 px or more, scrolling text as a trackpad flick
    /// does: from 140 px a frame, slowing every frame.
    fn flick_bytes(me_range: u32) -> usize {
        let (w, h) = (1280, 768);
        let steps: Vec<usize> = (0..40)
            .map(|i| (140.0 * (-(i as f64) / 20.0).exp()) as usize)
            .collect();
        let page = text_page(w, h + steps.iter().sum::<usize>() + 30);
        let (u, v) = (vec![128u8; w * h / 4], vec![128u8; w * h / 4]);
        let mut enc = Encoder::with_me_range(
            &EncoderConfig {
                pictures: Pictures {
                    width: w as u32,
                    height: h as u32,
                    fps: Fps::new(60).unwrap(),
                    preset: Preset::parse("ultrafast").unwrap(),
                    threads: 1,
                    debug: Debugging::read(|_| None).0,
                },
                crf: 24.0,
                max_kbps: None,
            },
            me_range,
        )
        .unwrap();
        let (mut top, mut bytes, mut out) = (0, 0, Vec::new());
        for (f, step) in std::iter::once(0).chain(steps).enumerate() {
            top += step;
            let planes = Planes {
                y: &page[top * w..(top + h) * w],
                u: &u,
                v: &v,
                stride_y: w,
                stride_uv: w / 2,
            };
            out.clear();
            enc.encode(&planes, f as i64 * 16, f == 0, None, None, &mut out)
                .unwrap();
            if step >= 32 {
                bytes += out.len();
            }
        }
        bytes
    }

    /// The frames of a flick move further than the search used to look, by different
    /// amounts each frame: found, they are coded as motion, not as new text.
    #[test]
    fn a_flick_is_found() {
        let (short, default) = (flick_bytes(64), flick_bytes(ME_RANGE));
        assert!(
            (default as f64) < short as f64 * 0.8,
            "{default} bytes at the default search range, {short} at 64"
        );
    }

    /// Without a cap at open (`--max-kbps 0`), the caps `set_rate` gives each frame change
    /// nothing: x264 turns VBV on only when it opens. With one, they hold the stream down.
    #[test]
    fn an_encoder_opened_uncapped_stays_uncapped() {
        let (w, h) = (640usize, 384usize);
        let page = text_page(w, h + 20 * 30 + 30);
        let (u, v) = (vec![128u8; w * h / 4], vec![128u8; w * h / 4]);
        let stream = |max_kbps, cap| {
            let mut enc = Encoder::new(&EncoderConfig {
                pictures: Pictures {
                    width: w as u32,
                    height: h as u32,
                    fps: Fps::new(60).unwrap(),
                    preset: Preset::parse("ultrafast").unwrap(),
                    threads: 1,
                    debug: Debugging::read(|_| None).0,
                },
                crf: 24.0,
                max_kbps,
            })
            .unwrap();
            let mut out = Vec::new();
            for f in 0..20 {
                enc.set_rate(24.0, cap);
                let planes = Planes {
                    y: &page[f * 30 * w..(f * 30 + h) * w],
                    u: &u,
                    v: &v,
                    stride_y: w,
                    stride_uv: w / 2,
                };
                enc.encode(&planes, f as i64 * 16, f == 0, None, None, &mut out)
                    .unwrap();
            }
            out
        };
        let uncapped = stream(None, 300);
        assert!(
            uncapped == stream(None, 60_000),
            "a later cap changed the stream"
        );
        let capped = stream(Some(Kbps::new(300.0)), 300);
        assert!(
            capped.len() * 2 < uncapped.len(),
            "{} bytes capped, {} uncapped",
            capped.len(),
            uncapped.len()
        );
    }

    /// What x264 logs while this test runs, as voxl's log would get it.
    struct Logged;

    static LOGGED: std::sync::Mutex<Vec<(log::Level, String, String)>> =
        std::sync::Mutex::new(Vec::new());

    impl log::Log for Logged {
        fn enabled(&self, _: &log::Metadata) -> bool {
            true
        }
        fn log(&self, r: &log::Record) {
            let line = (r.level(), r.target().to_string(), r.args().to_string());
            LOGGED.lock().unwrap().push(line);
        }
        fn flush(&self) {}
    }

    /// x264's log reaches voxl's, with the arguments x264 formatted into it: a size x264
    /// refuses (an odd side, none, or past its limit) is said once, as an error under
    /// `x264`, ending in the size, where x264's own log would have written it to stderr.
    #[test]
    fn x264_says_what_it_refuses_in_voxls_log() {
        log::set_logger(&Logged).unwrap();
        log::set_max_level(log::LevelFilter::Trace);
        let mut rng = crate::xorshift::Xorshift::new(0x264_u64 << 40 | 0x1065);
        let mut range = |lo: u32, hi: u32| rng.range(u64::from(lo), u64::from(hi)) as u32;
        for i in 0..500 {
            // Odd or nothing on one side or both; past x264's 16384 x 16384 once in a while.
            let side = |range: &mut dyn FnMut(u32, u32) -> u32, refused: bool| match refused {
                true => [0, range(0, 2048) * 2 + 1, range(8193, 40000) * 2][range(0, 3) as usize],
                false => range(1, 2048) * 2,
            };
            let refused = range(0, 3);
            let w = side(&mut range, refused != 1);
            let h = side(&mut range, refused != 0);
            let open = Encoder::new(&EncoderConfig {
                pictures: Pictures {
                    width: w,
                    height: h,
                    fps: Fps::new(60).unwrap(),
                    preset: Preset::parse("ultrafast").unwrap(),
                    threads: 1,
                    debug: Debugging::read(|_| None).0,
                },
                crf: 24.0,
                max_kbps: None,
            });
            assert!(open.is_err(), "{w}x{h} opened");
            let said = format!("({w}x{h})");
            let lines: Vec<_> = std::mem::take(&mut *LOGGED.lock().unwrap())
                .into_iter()
                .filter(|(_, target, _)| target == "x264")
                .collect();
            assert!(
                matches!(&lines[..], [(log::Level::Error, _, m)] if m.ends_with(&said)),
                "{i}: {w}x{h}: {lines:?}"
            );
        }
    }

    /// The judgement as it was while `Encoded` held `qp: Option<f32>`: the oracle for
    /// giving it two cases, deleted in the commit after that one.
    struct Old {
        keyframe: bool,
        qp: Option<f32>,
        crf_avg: f32,
    }

    impl Old {
        fn quality(&self, crf: f32, before: Settling) -> Quality {
            match (self.keyframe, self.qp, before) {
                (false, Some(qp), Settling::Over(over))
                    if qp - crf >= over && self.crf_avg < crf + 0.5 =>
                {
                    Quality::AsAsked
                }
                (false | true, Some(_) | None, Settling::Unknown | Settling::Over(_)) => {
                    Quality::Coarser
                }
            }
        }

        fn settling(&self, crf: f32) -> Settling {
            match (self.keyframe, self.qp) {
                (false, Some(qp)) => Settling::Over(qp - crf),
                (true, Some(_) | None) | (false, None) => Settling::Unknown,
            }
        }
    }

    /// Over 200,000 frames as x264 can put them out, a frame with its QP or nothing (which
    /// read as a zeroed picture: no keyframe, no QP, a crf_avg of 0), the judgement, where
    /// it leaves x264 and whether it is a keyframe are what they were.
    #[test]
    fn encoded_in_two_cases_judges_as_before() {
        let mut rng = 0x9E3779B97F4A7C15u64;
        let mut next = |n: u64| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % n
        };
        for _ in 0..200_000 {
            let crf = 10.0 + next(20) as f32;
            let before = match next(3) {
                0 => Settling::Unknown,
                _ => Settling::Over(next(40) as f32 - 20.0),
            };
            let (old, new) = match next(10) {
                0 => (
                    Old {
                        keyframe: false,
                        qp: None,
                        crf_avg: 0.0,
                    },
                    Encoded::Nothing,
                ),
                _ => {
                    let (keyframe, qp) = (next(4) == 0, next(52) as f32);
                    let crf_avg = crf + next(40) as f32 / 10.0 - 1.0;
                    (
                        Old {
                            keyframe,
                            qp: Some(qp),
                            crf_avg,
                        },
                        Encoded::Picture {
                            keyframe,
                            qp,
                            crf_avg,
                        },
                    )
                }
            };
            assert_eq!(new.quality(crf, before), old.quality(crf, before));
            assert_eq!(new.settling(crf), old.settling(crf));
            assert_eq!(new.keyframe(), old.keyframe);
        }
    }

    /// Passes over a still picture after a keyframe, or a keyframe and a scroll, that the
    /// cap leaves alone (uncapped: `crf_avg` reads the CRF on every one, so the cap cannot
    /// tell them apart). The frame after a keyframe never counts; a pass that counts is one
    /// x264 has come to rest at, which the oracle checks by going on: six more passes of
    /// the same picture start at most one QP lower. And one counts within 40, so
    /// sharpening ends. Over 40 cases: pages of 2 to 13 rows of 30 px text, a keyframe at
    /// CRF 18-30, 0 to 12 frames scrolling a row at a time at that CRF, passes at 10-17.
    #[test]
    fn a_pass_counts_once_x264_has_come_to_rest() {
        let mut rng = crate::xorshift::Xorshift::new(0x2545F4914F6CDD1D);
        let mut next = |n: u64| rng.below(n);
        let mut coming_down = 0;
        for _ in 0..40 {
            let (w, h) = (320, 64 + 32 * next(11) as usize);
            let (key_crf, crf) = (18.0 + next(13) as f32, 10.0 + next(8) as f32);
            let scroll = next(13) as usize;
            let page = text_page(w, h + 30 * scroll);
            let (u, v) = (vec![128u8; w * h / 4], vec![128u8; w * h / 4]);
            let at = |top: usize| Planes {
                y: &page[top * w..(top + h) * w],
                u: &u,
                v: &v,
                stride_y: w,
                stride_uv: w / 2,
            };
            let mut enc = Encoder::new(&EncoderConfig {
                pictures: Pictures {
                    width: w as u32,
                    height: h as u32,
                    fps: Fps::new(60).unwrap(),
                    preset: Preset::parse("ultrafast").unwrap(),
                    threads: 1,
                    debug: Debugging::read(|_| None).0,
                },
                crf: key_crf,
                max_kbps: None,
            })
            .unwrap();
            let mut out = Vec::new();
            let mut f = 0;
            let mut code = |enc: &mut Encoder, top: usize, crf: f32, before: Settling| {
                out.clear();
                let e = enc
                    .encode(&at(top), f * 16, f == 0, None, None, &mut out)
                    .unwrap();
                f += 1;
                (e.quality(crf, before), e.settling(crf), e)
            };
            let (_, mut settling, key) = code(&mut enc, 0, key_crf, Settling::Unknown);
            assert!(key.keyframe());
            // The frame after the keyframe, a row of scrolling or the first pass.
            let mut after_key = None;
            for row in 1..=scroll {
                let (quality, now, _) = code(&mut enc, row * 30, key_crf, settling);
                after_key.get_or_insert(quality);
                settling = now;
            }
            enc.set_crf(crf);
            let (first, now, _) = code(&mut enc, scroll * 30, crf, settling);
            settling = now;
            let after_key = after_key.unwrap_or(first);
            assert_eq!(
                after_key,
                Quality::Coarser,
                "the frame after a keyframe counted"
            );
            let mut rest = None;
            for _ in 0..40 {
                let (quality, now, e) = code(&mut enc, scroll * 30, crf, settling);
                let Encoded::Picture { qp, crf_avg, .. } = e else {
                    panic!("x264 put nothing out")
                };
                assert!((crf_avg - crf).abs() < 0.01, "uncapped, crf_avg {crf_avg}");
                settling = now;
                match quality {
                    Quality::AsAsked => {
                        rest = Some(qp);
                        break;
                    }
                    Quality::Coarser => coming_down += 1,
                }
            }
            let rest = rest.unwrap_or_else(|| {
                panic!("{w}x{h}, keyframe at CRF {key_crf}, {scroll} scrolled: no pass counted")
            });
            for _ in 0..6 {
                let (_, now, e) = code(&mut enc, scroll * 30, crf, settling);
                settling = now;
                let Encoded::Picture { qp, .. } = e else {
                    panic!("x264 put nothing out")
                };
                assert!(
                    qp >= rest - 1.0,
                    "counted at QP {rest}, x264 went on to {qp}"
                );
            }
        }
        assert!(
            coming_down > 80,
            "{coming_down} passes were still coming down"
        );
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    /// A full-screen page of text-like lines scrolled `STEP` px a frame: motion the search
    /// finds, plus `STEP` rows of new text every frame.
    /// cargo test --release scroll_cost -- --ignored --nocapture
    #[test]
    #[ignore]
    fn scroll_cost() {
        /// Three of the page's 44 px lines a frame, near the fastest a trackpad flick moves
        /// a terminal (see `ME_RANGE`). The bench's default since it was written; it was
        /// also run at one line and at five.
        const STEP: usize = 132;
        let (w, h) = (3024usize, 1898usize);
        let step = STEP;
        let range = ME_RANGE;
        let mut enc = Encoder::new(&EncoderConfig {
            pictures: Pictures {
                width: w as u32,
                height: h as u32,
                fps: Fps::new(60).unwrap(),
                preset: Preset::parse("ultrafast").unwrap(),
                threads: 4,
                debug: Debugging::read(|_| None).0,
            },
            crf: 24.0,
            max_kbps: None,
        })
        .unwrap();
        let mut y = vec![0u8; w * h];
        let (u, v) = (vec![128u8; w * h / 4], vec![128u8; w * h / 4]);
        let mut out = Vec::new();
        let (mut bytes, mut total) = (0usize, std::time::Duration::ZERO);
        let (frames, warm) = (40, 5);
        for f in 0..frames {
            for row in 0..h {
                let r = row + f * step;
                let line = (r / 44) as u64;
                for x in 0..w {
                    // 22x44 cells, each a "glyph" of light strokes on dark, its own per
                    // line and column.
                    let cell = (x / 22) as u64;
                    let g = (cell.wrapping_mul(2654435761) ^ line.wrapping_mul(0x9E3779B97F4A7C15))
                        >> 11;
                    let (gx, gy) = (x % 22, r % 44);
                    let ink = gy > 6
                        && gy < 36
                        && gx > 2
                        && gx < 19
                        && (g >> (((gx / 3) + (gy / 6) * 5) % 40)) & 1 == 1;
                    y[row * w + x] = if ink { 200 } else { 12 };
                }
            }
            let planes = Planes {
                y: &y,
                u: &u,
                v: &v,
                stride_y: w,
                stride_uv: w / 2,
            };
            out.clear();
            let t = std::time::Instant::now();
            enc.encode(&planes, f as i64 * 16, f == 0, None, None, &mut out)
                .unwrap();
            if f >= warm {
                total += t.elapsed();
                bytes += out.len();
            }
        }
        let n = (frames - warm) as f64;
        println!(
            "scroll {step} px/frame, search range {range}: {:.1} KB/frame, {:.1} ms/frame",
            bytes as f64 / n / 1024.0,
            total.as_secs_f64() * 1000.0 / n
        );
    }

    /// cargo test --release encode_cost -- --ignored --nocapture
    #[test]
    #[ignore]
    fn encode_cost() {
        /// One thread: what the thread governor (`--adaptive-threads`, since removed)
        /// encoded on until a frame would miss its budget. The bench's default since it
        /// was written; it was also run at four, the default.
        const THREADS: u32 = 1;
        let (w, h) = (2560usize, 1600usize);
        let mbs = (w / 16) * (h / 16);
        let threads = THREADS;
        #[cfg(unix)]
        let cpu = || unsafe {
            let mut r: libc::rusage = std::mem::zeroed();
            libc::getrusage(libc::RUSAGE_SELF, &mut r);
            (r.ru_utime.tv_sec + r.ru_stime.tv_sec) as f64 * 1e3
                + (r.ru_utime.tv_usec + r.ru_stime.tv_usec) as f64 / 1e3
        };
        // Process CPU time isn't wired up on Windows; wall times still print.
        #[cfg(windows)]
        let cpu = || 0.0;
        for (label, hint) in [("no hints", false), ("hints", true)] {
            let mut enc = Encoder::new(&EncoderConfig {
                pictures: Pictures {
                    width: w as u32,
                    height: h as u32,
                    fps: Fps::new(60).unwrap(),
                    preset: Preset::parse("ultrafast").unwrap(),
                    threads,
                    debug: Debugging::read(|_| None).0,
                },
                crf: 24.0,
                max_kbps: Some(Kbps::new(30000.0)),
            })
            .unwrap();
            let mut y = vec![0u8; w * h];
            let (u, v) = (vec![128u8; w * h / 4], vec![128u8; w * h / 4]);
            for (i, p) in y.iter_mut().enumerate() {
                *p = ((i % w) ^ (i / w)) as u8;
            }
            let mut out = Vec::new();
            let mut total = std::time::Duration::ZERO;
            let mut cpu_ms = 0.0;
            for f in 0..120 {
                // A 640x360 region changes every frame; the rest stays still.
                for row in 0..360 {
                    for x in 0..640 {
                        y[row * w + x] = (x + row + f * 7) as u8;
                    }
                }
                let mut info: Vec<u8> = (0..mbs)
                    .map(|i| ((i % (w / 16)) >= 40 || (i / (w / 16)) >= 23) as u8)
                    .collect();
                let planes = Planes {
                    y: &y,
                    u: &u,
                    v: &v,
                    stride_y: w,
                    stride_uv: w / 2,
                };
                out.clear();
                let t = std::time::Instant::now();
                let c0 = cpu();
                enc.encode(
                    &planes,
                    f as i64 * 16,
                    f == 0,
                    None,
                    if hint { Some(&mut info) } else { None },
                    &mut out,
                )
                .unwrap();
                if f >= 20 {
                    total += t.elapsed();
                    cpu_ms += cpu() - c0;
                }
            }
            println!(
                "{threads}t {label}: {:.2} ms/frame wall, {:.2} ms CPU, {} bytes last",
                total.as_secs_f64() * 1000.0 / 100.0,
                cpu_ms / 100.0,
                out.len()
            );
        }
    }
}
