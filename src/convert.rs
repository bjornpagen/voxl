//! Conversion of captured frames to I420, split across threads by row bands: BGRX via
//! libyuv on Linux and Windows, NV12 (already YUV, from ScreenCaptureKit) on macOS.

#[cfg(not(target_os = "macos"))]
extern "C" {
    fn ARGBToI420(
        src_argb: *const u8,
        src_stride_argb: i32,
        dst_y: *mut u8,
        dst_stride_y: i32,
        dst_u: *mut u8,
        dst_stride_u: i32,
        dst_v: *mut u8,
        dst_stride_v: i32,
        width: i32,
        height: i32,
    ) -> i32;
}

pub struct I420 {
    pub width: usize,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

impl I420 {
    pub fn new(width: usize, height: usize) -> Self {
        let cw = width.div_ceil(2);
        let ch = height.div_ceil(2);
        I420 {
            width,
            y: vec![16; width * height],
            u: vec![128; cw * ch],
            v: vec![128; cw * ch],
        }
    }
    pub fn stride_uv(&self) -> usize {
        self.width.div_ceil(2)
    }
}

use std::sync::mpsc;

struct SendPtr(*mut u8);
unsafe impl Send for SendPtr {}
unsafe impl Sync for SendPtr {}
impl SendPtr {
    fn get(&self) -> *mut u8 {
        self.0
    }
}

/// The fewest rows a conversion hands to a thread of its own: upstream's guess,
/// unmeasured. A band of fewer than twice as many is converted on the capture thread
/// alone.
const LEAST_ROWS: usize = 64;

/// Threads that share out ranges of rows with the thread that asks. They are made once,
/// with the stream, so that splitting a band starts no thread: a thread started per
/// capture waits to be scheduled, and allocates.
pub struct Rows {
    /// Each helper's next range.
    helpers: Vec<mpsc::SyncSender<Range>>,
    /// One message per range a helper has finished.
    done: mpsc::Receiver<()>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

/// A range of rows for a helper, and what to run over it. `run` points into the frame
/// of the `split` that sent it, which waits for the helper to finish before returning.
struct Range {
    run: *const (dyn Fn(usize, usize) + Sync),
    a: usize,
    b: usize,
}

unsafe impl Send for Range {}

impl Rows {
    /// `threads` threads in all, the caller's among them: so `threads - 1` helpers.
    pub fn new(threads: usize) -> Rows {
        let (done_tx, done) = mpsc::sync_channel(threads);
        let (helpers, threads) = (1..threads)
            .map(|i| {
                let (tx, rx) = mpsc::sync_channel::<Range>(1);
                let done = done_tx.clone();
                let thread = std::thread::Builder::new()
                    .name(format!("rows-{i}"))
                    .spawn(move || {
                        while let Ok(r) = rx.recv() {
                            unsafe { (*r.run)(r.a, r.b) };
                            if done.send(()).is_err() {
                                return;
                            }
                        }
                    })
                    .expect("a thread for converting rows");
                (tx, thread)
            })
            .unzip();
        Rows {
            helpers,
            done,
            threads,
        }
    }

    /// Runs `run(a, b)` over ranges that cover rows [y0, y1): at most one a thread, each
    /// starting `step` rows apart from y0 and, but the last, at least `least` rows long.
    /// The calling thread takes the last range. Returns once every range has run.
    pub fn split(
        &self,
        y0: usize,
        y1: usize,
        step: usize,
        least: usize,
        run: impl Fn(usize, usize) + Sync,
    ) {
        let rows = y1.saturating_sub(y0);
        let n = (self.helpers.len() + 1).min(rows / least).max(1);
        let chunk = rows.div_ceil(n).next_multiple_of(step);
        let run: &(dyn Fn(usize, usize) + Sync) = &run;
        // The helpers run it only while this call waits for them (`Wait`, below).
        let shared: *const (dyn Fn(usize, usize) + Sync + 'static) =
            unsafe { std::mem::transmute(run) };
        let mut wait = Wait {
            done: &self.done,
            left: 0,
        };
        let mut a = y0;
        for helper in &self.helpers {
            let b = (a + chunk).min(y1);
            if b == y1 {
                break;
            }
            helper
                .send(Range { run: shared, a, b })
                .expect("a thread for converting rows ended");
            wait.left += 1;
            a = b;
        }
        if a < y1 {
            run(a, y1);
        }
    }
}

/// Waits for the helpers' ranges still out, also if the caller's own range panics: they
/// run a closure that lives in the caller's frame.
struct Wait<'a> {
    done: &'a mpsc::Receiver<()>,
    left: usize,
}

impl Drop for Wait<'_> {
    fn drop(&mut self) {
        for _ in 0..self.left {
            self.done
                .recv()
                .expect("a thread for converting rows ended");
        }
    }
}

impl Drop for Rows {
    fn drop(&mut self) {
        self.helpers.clear();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// Convert rows [y0, y1) (even-aligned) of `src` into `dst`.
#[cfg(not(target_os = "macos"))]
pub fn convert_band(
    src: &[u8],
    src_stride: usize,
    dst: &mut I420,
    y0: usize,
    y1: usize,
    rows: &Rows,
) {
    if y1 <= y0 {
        return;
    }
    let w = dst.width;
    let suv = dst.stride_uv();
    let (yp, up, vp) = (
        SendPtr(dst.y.as_mut_ptr()),
        SendPtr(dst.u.as_mut_ptr()),
        SendPtr(dst.v.as_mut_ptr()),
    );
    let src_p = SendPtr(src.as_ptr() as *mut u8);
    let run = |a: usize, b: usize| unsafe {
        ARGBToI420(
            src_p.get().add(a * src_stride),
            src_stride as i32,
            yp.get().add(a * w),
            w as i32,
            up.get().add((a / 2) * suv),
            suv as i32,
            vp.get().add((a / 2) * suv),
            suv as i32,
            w as i32,
            (b - a) as i32,
        );
    };
    // Chunk edges stay even, for 4:2:0.
    rows.split(y0, y1, 2, LEAST_ROWS, run);
}

/// Copy rows [y0, y1) (even-aligned) of an NV12 frame into `dst`: the luma rows as
/// they are, the interleaved chroma split into U and V.
#[cfg(target_os = "macos")]
#[allow(clippy::too_many_arguments)]
pub fn convert_nv12_band(
    src_y: *const u8,
    stride_y: usize,
    src_uv: *const u8,
    stride_uv: usize,
    dst: &mut I420,
    y0: usize,
    y1: usize,
    rows: &Rows,
) {
    let w = dst.width;
    let cw = w.div_ceil(2);
    let suv = dst.stride_uv();
    let (yp, up, vp) = (
        SendPtr(dst.y.as_mut_ptr()),
        SendPtr(dst.u.as_mut_ptr()),
        SendPtr(dst.v.as_mut_ptr()),
    );
    let (sy, suv_src) = (SendPtr(src_y as *mut u8), SendPtr(src_uv as *mut u8));
    rows.split(y0, y1, 2, LEAST_ROWS, |a, b| unsafe {
        for r in a..b {
            std::ptr::copy_nonoverlapping(sy.get().add(r * stride_y), yp.get().add(r * w), w);
        }
        for r in a / 2..b / 2 {
            let src = std::slice::from_raw_parts(suv_src.get().add(r * stride_uv), cw * 2);
            let u = std::slice::from_raw_parts_mut(up.get().add(r * suv), cw);
            let v = std::slice::from_raw_parts_mut(vp.get().add(r * suv), cw);
            for ((uv, u), v) in src.chunks_exact(2).zip(u.iter_mut()).zip(v.iter_mut()) {
                *u = uv[0];
                *v = uv[1];
            }
        }
    });
}

/// An NV12 frame (BT.601 limited range, as ScreenCaptureKit delivers it) as packed BGRX.
#[cfg(target_os = "macos")]
pub fn nv12_to_bgrx(
    y: &[u8],
    y_stride: usize,
    uv: &[u8],
    uv_stride: usize,
    w: usize,
    h: usize,
    out: &mut [u8],
) {
    let clamp = |v: i32| (v >> 8).clamp(0, 255) as u8;
    for (row, line) in out.chunks_exact_mut(w * 4).take(h).enumerate() {
        let (yr, cr) = (&y[row * y_stride..][..w], &uv[(row / 2) * uv_stride..]);
        for (x, px) in line.chunks_exact_mut(4).enumerate() {
            let c = 298 * (yr[x] as i32 - 16);
            let d = cr[x / 2 * 2] as i32 - 128;
            let e = cr[x / 2 * 2 + 1] as i32 - 128;
            px[0] = clamp(c + 516 * d + 128);
            px[1] = clamp(c - 100 * d - 208 * e + 128);
            px[2] = clamp(c + 409 * e + 128);
            px[3] = 255;
        }
    }
}

#[cfg(all(test, not(target_os = "macos")))]
mod tests {
    use super::*;

    struct Rng(u64);
    impl Rng {
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 as usize % n
        }
    }

    /// Converting a band on any number of threads gives the bytes one thread gives.
    #[test]
    fn a_band_converts_on_many_threads_as_on_one() {
        let (w, h) = (333usize, 700usize);
        let mut rng = Rng(0x9E3779B97F4A7C15);
        let src: Vec<u8> = (0..w * 4 * h).map(|_| rng.below(256) as u8).collect();
        let one = Rows::new(1);
        let crews: Vec<Rows> = (2..=6).map(Rows::new).collect();
        for _ in 0..200 {
            let y0 = rng.below(h) & !1;
            let y1 = (y0 + rng.below(h - y0 + 1)) & !1;
            let mut want = I420::new(w, h);
            convert_band(&src, w * 4, &mut want, y0, y1, &one);
            for rows in &crews {
                let mut got = I420::new(w, h);
                convert_band(&src, w * 4, &mut got, y0, y1, rows);
                assert!(
                    got.y == want.y && got.u == want.u && got.v == want.v,
                    "rows {y0}..{y1} on {} threads",
                    rows.helpers.len() + 1
                );
            }
        }
    }
}
