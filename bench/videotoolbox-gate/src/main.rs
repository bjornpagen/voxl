//! The VideoToolbox gate: this Mac's hardware VP9 decoder, the one the page's WebCodecs
//! decoder uses, decodes the daemon's stream as libavcodec's software decoder does, whatever
//! a viewer lost. `cargo test` proves the stream in software everywhere; this measures the
//! one decoder it cannot reach, so it runs here, by hand, before a deploy.
//!
//! Over generated runs of typing and scrolling on a page of text, at a small size and at
//! the production size (3024x1898), each frame but the first lost with a quarter's chance
//! (the live link's measured loss before repair), each loss and each decode reported a
//! generated round trip of 1 to 30 frames later: the frames that arrived, decoded by FFmpeg
//! through VideoToolbox with every error fatal, come out as exactly the pictures the whole
//! stream decodes to in software. Exits 0 with one line per run, or 1 at the first that
//! does not.

#![allow(dead_code)]

#[path = "../../../src/vp9.rs"]
mod vp9;
#[path = "../../../src/xorshift.rs"]
mod xorshift;

use vp9::{Coding, Encoder, Fineness, Planes, Shape, Stream};
use xorshift::Xorshift;

/// A page of text-like lines, `rows` tall: dark marks on a light page.
fn page(w: usize, rows: usize, rng: &mut Xorshift) -> Vec<u8> {
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

/// What FFmpeg decodes `file` to as I420, every error fatal, in software or through
/// VideoToolbox; or what it said instead.
fn decode(file: &std::path::Path, videotoolbox: bool) -> Result<Vec<u8>, String> {
    let mut cmd = std::process::Command::new("ffmpeg");
    cmd.args(["-v", "error", "-err_detect", "explode", "-xerror"]);
    if videotoolbox {
        cmd.args(["-hwaccel", "videotoolbox"]);
    }
    cmd.arg("-i").arg(file);
    cmd.args([
        "-f",
        "rawvideo",
        "-pix_fmt",
        "yuv420p",
        "-fps_mode",
        "passthrough",
        "-",
    ]);
    let out = cmd.output().map_err(|e| e.to_string())?;
    let said = String::from_utf8_lossy(&out.stderr).into_owned();
    match out.status.success() && said.is_empty() {
        true => Ok(out.stdout),
        false => Err(said),
    }
}

/// An IVF file's header for VP9 at `w`x`h` (the WebM project's IVF layout).
fn ivf_header(w: usize, h: usize) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(b"DKIF");
    b.extend_from_slice(&0u16.to_le_bytes());
    b.extend_from_slice(&32u16.to_le_bytes());
    b.extend_from_slice(b"VP90");
    b.extend_from_slice(&(w as u16).to_le_bytes());
    b.extend_from_slice(&(h as u16).to_le_bytes());
    b.extend_from_slice(&1000u32.to_le_bytes());
    b.extend_from_slice(&1u32.to_le_bytes());
    b.extend_from_slice(&0u64.to_le_bytes());
    b
}

fn ivf_frame(b: &mut Vec<u8>, pts: u64, frame: &[u8]) {
    b.extend_from_slice(&(frame.len() as u32).to_le_bytes());
    b.extend_from_slice(&pts.to_le_bytes());
    b.extend_from_slice(frame);
}

/// One run: whether VideoToolbox decoded every frame that arrived as software decodes it.
fn run(
    case: u64,
    w: usize,
    h: usize,
    rng: &mut Xorshift,
    dir: &std::path::Path,
) -> Result<String, String> {
    const FRAMES: u64 = 120;
    let text = page(w, h * 4, rng);
    let lag = 1 + rng.below(30);
    let shape = Shape {
        width: w as u32,
        height: h as u32,
        fps: 60,
    };
    let cores = std::thread::available_parallelism().map_err(|e| e.to_string())?;
    let stream = Stream::new(shape, cores).map_err(|e| format!("{e:?}"))?;
    let mut enc = Encoder::new(&stream, 8_000).map_err(|e| e.to_string())?;
    let (mut slots, key) = stream.start(0);
    let mut key = Some(key);
    let (u, v) = (vec![128u8; w * h / 4], vec![128u8; w * h / 4]);
    let mut due: Vec<(u64, u64, bool)> = Vec::new();
    let (mut whole, mut arrived_ivf) = (ivf_header(w, h), ivf_header(w, h));
    let mut arrived = Vec::new();
    let mut top = 0usize;
    let mut screen = text[..w * h].to_vec();
    for f in 0..FRAMES {
        for &(_, n, came) in due.iter().filter(|d| d.0 <= f) {
            match came {
                true => slots.decoded(n),
                false => slots.lost(n),
            }
        }
        due.retain(|d| d.0 > f);
        match rng.below(3) {
            0 => {
                top = (top + 1 + rng.below(8) as usize).min(h * 3);
                screen.copy_from_slice(&text[top * w..(top + h) * w]);
            }
            _ => {
                let at = rng.below((w * h) as u64) as usize;
                screen[at] = screen[at].wrapping_add(128);
            }
        }
        let coding: Coding = match key.take() {
            Some(k) => k.into(),
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
            .map_err(|e| e.to_string())?;
        slots.sent(f, 1);
        ivf_frame(&mut whole, f, &out);
        let came = f == 0 || rng.below(4) > 0;
        due.push((f + lag, f, came));
        if came {
            ivf_frame(&mut arrived_ivf, f, &out);
            arrived.push(f as usize);
        }
    }
    let (whole_file, arrived_file) = (
        dir.join(format!("whole{case}.ivf")),
        dir.join(format!("arrived{case}.ivf")),
    );
    std::fs::write(&whole_file, &whole).map_err(|e| e.to_string())?;
    std::fs::write(&arrived_file, &arrived_ivf).map_err(|e| e.to_string())?;
    let frame = w * h * 3 / 2;
    let truth = decode(&whole_file, false)?;
    let got = decode(&arrived_file, true)?;
    if got.len() != arrived.len() * frame || truth.len() != FRAMES as usize * frame {
        return Err(format!(
            "{} pictures for {} arrived",
            got.len() / frame,
            arrived.len()
        ));
    }
    for (i, &f) in arrived.iter().enumerate() {
        if got[i * frame..][..frame] != truth[f * frame..][..frame] {
            return Err(format!("frame {f} differs"));
        }
    }
    Ok(format!(
        "{w}x{h} ({}, {} threads), round trip {lag} frames: {} of {FRAMES} frames arrived, all as software decodes them",
        stream.codec(),
        stream.threads,
        arrived.len()
    ))
}

fn main() {
    let dir = std::env::temp_dir().join(format!("voxl-videotoolbox-gate-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a folder for the streams");
    let mut rng = Xorshift::new(0x6a09_e667_f3bc_c908);
    let sizes = [
        (640, 384),
        (640, 384),
        (640, 384),
        (640, 384),
        (3024, 1898),
        (3024, 1898),
    ];
    let mut failed = false;
    for (case, &(w, h)) in sizes.iter().enumerate() {
        match run(case as u64, w, h, &mut rng, &dir) {
            Ok(said) => println!("ok: {said}"),
            Err(e) => {
                println!("FAILED: {w}x{h}, run {case}: {e}");
                failed = true;
                break;
            }
        }
    }
    std::fs::remove_dir_all(&dir).expect("the streams' folder removed");
    std::process::exit(i32::from(failed));
}
