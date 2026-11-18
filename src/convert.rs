//! The I420 picture the encoder codes, and the conversion of captured BGRX frames to it
//! with libyuv, split across threads by row bands.

#[cfg(target_os = "linux")]
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

/// Threads that convert bands of rows of a capture, started once with their capturer and
/// kept: a capture starts no thread. The thread that asks converts a band itself.
#[cfg(target_os = "linux")]
pub struct Rows {
    workers: Vec<Worker>,
}

#[cfg(target_os = "linux")]
struct Worker {
    task: std::sync::mpsc::SyncSender<Band>,
    done: std::sync::mpsc::Receiver<()>,
}

/// Rows `a..b` of a BGRX picture at `src` (`src_stride` bytes a row) into an I420 one of
/// width `w` (chroma rows `suv` bytes): raw pointers, valid while `Rows::convert`, which
/// holds the two pictures borrowed, waits for every band it handed out.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
struct Band {
    src: *const u8,
    src_stride: usize,
    y: *mut u8,
    u: *mut u8,
    v: *mut u8,
    w: usize,
    suv: usize,
    a: usize,
    b: usize,
}

// SAFETY: a band's pointers are to pictures `Rows::convert` keeps borrowed until the band
// is done, and bands of one call write disjoint rows.
#[cfg(target_os = "linux")]
unsafe impl Send for Band {}

#[cfg(target_os = "linux")]
impl Band {
    /// SAFETY: the pointers are valid for the band's rows, which nothing else writes.
    unsafe fn run(self) {
        let Band {
            src,
            src_stride,
            y,
            u,
            v,
            w,
            suv,
            a,
            b,
        } = self;
        unsafe {
            ARGBToI420(
                src.add(a * src_stride),
                src_stride as i32,
                y.add(a * w),
                w as i32,
                u.add((a / 2) * suv),
                suv as i32,
                v.add((a / 2) * suv),
                suv as i32,
                w as i32,
                (b - a) as i32,
            );
        }
    }
}

#[cfg(target_os = "linux")]
impl Rows {
    /// `threads` in all: the caller's own and `threads - 1` workers.
    pub fn new(threads: usize) -> Result<Rows, String> {
        let workers = (1..threads.max(1))
            .map(|_| {
                let (task, tasks) = std::sync::mpsc::sync_channel::<Band>(1);
                let (finished, done) = std::sync::mpsc::sync_channel::<()>(1);
                std::thread::Builder::new()
                    .name("convert".into())
                    .spawn(move || {
                        while let Ok(band) = tasks.recv() {
                            // SAFETY: as `Band` says; the asker waits for `finished`.
                            unsafe { band.run() };
                            if finished.send(()).is_err() {
                                return;
                            }
                        }
                    })
                    .map_err(|e| format!("no conversion thread: {e}"))?;
                Ok(Worker { task, done })
            })
            .collect::<Result<_, String>>()?;
        Ok(Rows { workers })
    }

    /// Converts rows [y0, y1) (even-aligned) of `src` into `dst`, in bands of at least 64
    /// rows over the threads, each band's edges even for 4:2:0. A worker gone is the error.
    pub fn convert(
        &self,
        src: &[u8],
        src_stride: usize,
        dst: &mut I420,
        y0: usize,
        y1: usize,
    ) -> Result<(), String> {
        let rows = y1 - y0;
        if rows == 0 {
            return Ok(());
        }
        let n = (self.workers.len() + 1).clamp(1, (rows / 64).max(1));
        let chunk = (rows.div_ceil(n) + 1) & !1;
        let (y, u, v) = (dst.y.as_mut_ptr(), dst.u.as_mut_ptr(), dst.v.as_mut_ptr());
        let (w, suv) = (dst.width, dst.stride_uv());
        let band = |a: usize, b: usize| Band {
            src: src.as_ptr(),
            src_stride,
            y,
            u,
            v,
            w,
            suv,
            a,
            b,
        };
        let mut handed = 0;
        let mut a = y0;
        let mut failed = None;
        while a < y1 {
            let b = (a + chunk).min(y1);
            match (b == y1, self.workers.get(handed)) {
                (false, Some(w)) => match w.task.send(band(a, b)) {
                    Ok(()) => handed += 1,
                    Err(_) => {
                        failed = Some("a conversion thread has ended");
                        break;
                    }
                },
                // The last band, or one past the workers: this thread's own.
                (true, _) | (false, None) => {
                    // SAFETY: `src` and `dst` are borrowed for the call, and no worker
                    // writes these rows.
                    unsafe { band(a, b).run() };
                }
            }
            a = b;
        }
        // Every band handed out is done before the pictures are let go.
        for w in &self.workers[..handed] {
            if w.done.recv().is_err() {
                failed = Some("a conversion thread has ended");
            }
        }
        match failed {
            None => Ok(()),
            Some(why) => Err(why.into()),
        }
    }
}
