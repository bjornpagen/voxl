//! Dynamic resolution: resizes the X screen with RandR so the desktop matches the
//! viewer's window instead of being scaled down into it.
//!
//! Xvfb only offers the mode it started with, and that size is its maximum, so start
//! it at the largest size to allow. A mode outlives the client that made it: when voxl
//! exits, Xvfb keeps every mode it made, still on the output, and the screen at the size
//! it last set. So the mode for a size is found among the screen's modes, whoever made
//! it, and made only when the screen has none of that size: a mode left behind by an
//! earlier voxl, or made by another tool, is the mode for its size, and none is ever made
//! twice. Xvfb keeps the same `-shmem` framebuffer (and stride) across sizes; the screen
//! is its top-left.

use x11rb::connection::Connection;
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::xproto::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

/// Smallest desktop a client may ask for.
pub const MIN: (u32, u32) = (640, 480);

/// A size the screen can take: within [MIN, max], width a multiple of 8 and height
/// even (4:2:0 video), rounding down. Where the screen's maximum is below MIN (an
/// Xvfb started at 800x450), that side stays at the maximum.
pub fn fit(w: u32, h: u32, max: (u32, u32)) -> (u32, u32) {
    // clamp panics when min > max, and with panic = "abort" any viewer's resize
    // request would take the whole server down.
    (
        w.clamp(MIN.0.min(max.0), max.0) & !7,
        h.clamp(MIN.1.min(max.1), max.1) & !1,
    )
}

pub struct Screen {
    conn: RustConnection,
    root: u32,
    output: randr::Output,
    crtc: randr::Crtc,
    max: (u32, u32),
    current: (u32, u32),
    /// Millimetres per pixel, kept constant so the DPI doesn't change with the size.
    mm_per_px: (f64, f64),
}

fn err(e: impl std::fmt::Display) -> String {
    format!("RandR: {e}")
}

impl Screen {
    pub fn open(display: Option<&str>) -> Result<Self, String> {
        let (conn, n) = RustConnection::connect(display).map_err(err)?;
        let s = &conn.setup().roots[n];
        let (root, w, h) = (s.root, s.width_in_pixels as u32, s.height_in_pixels as u32);
        let mm_per_px = (
            s.width_in_millimeters as f64 / w as f64,
            s.height_in_millimeters as f64 / h as f64,
        );
        conn.randr_query_version(1, 3)
            .map_err(err)?
            .reply()
            .map_err(err)?;
        let res = conn
            .randr_get_screen_resources(root)
            .map_err(err)?
            .reply()
            .map_err(err)?;
        let (&output, &crtc) = res
            .outputs
            .first()
            .zip(res.crtcs.first())
            .ok_or("RandR: no output")?;
        let range = conn
            .randr_get_screen_size_range(root)
            .map_err(err)?
            .reply()
            .map_err(err)?;
        Ok(Screen {
            conn,
            root,
            output,
            crtc,
            max: (range.max_width as u32, range.max_height as u32),
            current: (w, h),
            mm_per_px,
        })
    }

    pub fn max(&self) -> (u32, u32) {
        self.max
    }

    pub fn fit(&self, w: u32, h: u32) -> (u32, u32) {
        fit(w, h, self.max)
    }

    pub fn resize(&mut self, w: u32, h: u32) -> Result<(), String> {
        let (w, h) = self.fit(w, h);
        let geometry = self
            .conn
            .get_geometry(self.root)
            .map_err(err)?
            .reply()
            .map_err(err)?;
        self.current = (u32::from(geometry.width), u32::from(geometry.height));
        if (w, h) == self.current {
            return Ok(());
        }
        let c = &self.conn;
        let res = c
            .randr_get_screen_resources(self.root)
            .map_err(err)?
            .reply()
            .map_err(err)?;
        // The screen's mode of this size, or one made for it: by size, not by name, as the
        // CRTC shows a size and a mode of it under any name shows it.
        let mode = match res
            .modes
            .iter()
            .find(|m| (u32::from(m.width), u32::from(m.height)) == (w, h))
        {
            Some(m) => m.id,
            None => self.make_mode(w, h)?,
        };
        // A mode the output lacks (made by a voxl that ended before it gave it, or by a
        // tool that never did) is given to it: a CRTC shows only its output's modes.
        let on_output = c
            .randr_get_output_info(self.output, res.config_timestamp)
            .map_err(err)?
            .reply()
            .map_err(err)?
            .modes;
        if !on_output.contains(&mode) {
            c.randr_add_output_mode(self.output, mode)
                .map_err(err)?
                .check()
                .map_err(err)?;
        }
        let mm = |px: u32, per: f64| ((px as f64 * per).round() as u32).max(1);
        let set_size = |w: u32, h: u32| {
            c.randr_set_screen_size(
                self.root,
                w as u16,
                h as u16,
                mm(w, self.mm_per_px.0),
                mm(h, self.mm_per_px.1),
            )
            .map_err(err)?
            .check()
            .map_err(err)
        };
        // The screen must contain the CRTC at every step: grow it first, switch the
        // mode, then shrink it to the final size.
        let (cw, ch) = self.current;
        set_size(w.max(cw), h.max(ch))?;
        let reply = c
            .randr_set_crtc_config(
                self.crtc,
                x11rb::CURRENT_TIME,
                res.config_timestamp,
                0,
                0,
                mode,
                randr::Rotation::ROTATE0,
                &[self.output],
            )
            .map_err(err)?
            .reply()
            .map_err(err)?;
        if reply.status != randr::SetConfig::SUCCESS {
            return Err(format!("RandR: set mode {w}x{h}: {:?}", reply.status));
        }
        set_size(w, h)?;
        c.flush().map_err(err)?;
        self.current = (w, h);
        log::info!("desktop resized to {w}x{h}");
        Ok(())
    }

    /// A new mode of `w`x`h`, named for its size. Called only when the screen has no mode
    /// of that size, so a name of ours is taken only by a mode of another size someone
    /// else named so, and then making it fails.
    fn make_mode(&self, w: u32, h: u32) -> Result<randr::Mode, String> {
        let name = format!("voxl-{w}x{h}");
        let info = randr::ModeInfo {
            id: 0,
            width: w as u16,
            height: h as u16,
            dot_clock: w * h * 60,
            hsync_start: w as u16 + 1,
            hsync_end: w as u16 + 2,
            htotal: w as u16 + 3,
            hskew: 0,
            vsync_start: h as u16 + 1,
            vsync_end: h as u16 + 2,
            vtotal: h as u16 + 3,
            name_len: name.len() as u16,
            mode_flags: randr::ModeFlag::from(0u32),
        };
        Ok(self
            .conn
            .randr_create_mode(self.root, info, name.as_bytes())
            .map_err(err)?
            .reply()
            .map_err(err)?
            .mode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_stays_within_min_and_max() {
        let max = (2560, 1600);
        assert_eq!(fit(1921, 1081, max), (1920, 1080));
        assert_eq!(fit(100, 100, max), MIN);
        assert_eq!(fit(9999, 9999, max), max);
    }

    #[test]
    fn fit_handles_a_screen_smaller_than_min() {
        assert_eq!(fit(1920, 1080, (800, 450)), (800, 450));
        assert_eq!(fit(100, 100, (800, 450)), (640, 450));
        assert_eq!(fit(1920, 1080, (600, 400)), (600, 400));
    }

    /// An Xvfb of the test's own, on a display it picks itself, kept as the desktop's is
    /// (`-noreset`: it does not start over when its last client leaves); stopped when dropped.
    struct Xvfb {
        child: std::process::Child,
        display: String,
    }

    impl Xvfb {
        fn start((w, h): (u32, u32)) -> Xvfb {
            use std::io::BufRead;
            let mut child = std::process::Command::new("Xvfb")
                .args([
                    "-displayfd",
                    "1",
                    "-noreset",
                    "-nolisten",
                    "tcp",
                    "-screen",
                    "0",
                ])
                .arg(format!("{w}x{h}x24"))
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("Xvfb on PATH");
            let mut n = String::new();
            std::io::BufReader::new(child.stdout.take().unwrap())
                .read_line(&mut n)
                .unwrap();
            Xvfb {
                child,
                display: format!(":{}", n.trim()),
            }
        }
    }

    impl Drop for Xvfb {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// The X server's own answer: the screen's size, and each size's modes.
    fn seen(display: &str) -> ((u32, u32), Vec<(u32, u32)>) {
        let (conn, n) = RustConnection::connect(Some(display)).unwrap();
        let root = conn.setup().roots[n].root;
        let g = conn.get_geometry(root).unwrap().reply().unwrap();
        let res = conn
            .randr_get_screen_resources(root)
            .unwrap()
            .reply()
            .unwrap();
        let sizes = res
            .modes
            .iter()
            .map(|m| (u32::from(m.width), u32::from(m.height)))
            .collect();
        ((u32::from(g.width), u32::from(g.height)), sizes)
    }

    /// A mode another tool made, `on_output` or not, as a voxl that ended between making a
    /// mode and giving it to the output leaves it.
    fn foreign_mode(display: &str, (w, h): (u32, u32), name: &str, on_output: bool) {
        let (conn, n) = RustConnection::connect(Some(display)).unwrap();
        let root = conn.setup().roots[n].root;
        let res = conn
            .randr_get_screen_resources(root)
            .unwrap()
            .reply()
            .unwrap();
        let info = randr::ModeInfo {
            id: 0,
            width: w as u16,
            height: h as u16,
            dot_clock: w * h * 60,
            hsync_start: w as u16 + 1,
            hsync_end: w as u16 + 2,
            htotal: w as u16 + 3,
            hskew: 0,
            vsync_start: h as u16 + 1,
            vsync_end: h as u16 + 2,
            vtotal: h as u16 + 3,
            name_len: name.len() as u16,
            mode_flags: randr::ModeFlag::from(0u32),
        };
        let mode = conn
            .randr_create_mode(root, info, name.as_bytes())
            .unwrap()
            .reply()
            .unwrap()
            .mode;
        if on_output {
            conn.randr_add_output_mode(res.outputs[0], mode)
                .unwrap()
                .check()
                .unwrap();
        }
        conn.flush().unwrap();
    }

    /// A mode outlives the voxl that made it, and a later voxl still gets every size: over
    /// 200 generated asks among sizes that another tool left a mode of (on the output, and
    /// not), sizes earlier connections made, and the screen's start size, with a restart
    /// (a new connection, as a new voxl process has) before one ask in four, the X server
    /// says after each ask that the screen has the size asked for, and at the end that no
    /// size has two modes. Each kind of ask is counted, so none passes unasked. Needs Xvfb
    /// on PATH: `cargo test -- --ignored modes_left`.
    #[test]
    #[ignore = "needs Xvfb on PATH"]
    fn modes_left_behind_are_found_and_no_size_is_made_twice() {
        let max = (1600, 1000);
        let x = Xvfb::start(max);
        let sizes = [
            (640, 480),
            (800, 600),
            (960, 600),
            (1024, 640),
            (1200, 800),
            (1456, 868),
            max,
        ];
        let foreign = [(1200, 800), (1024, 640)];
        foreign_mode(&x.display, foreign[0], "other-1200x800", true);
        foreign_mode(&x.display, foreign[1], "other-1024x640", false);
        let mut rng = crate::xorshift::Xorshift::new(0x2545_F491_4F6C_DD1D);
        let mut screen = Screen::open(Some(&x.display)).unwrap();
        // Sizes a connection before this one took the screen to, and the asks counted by
        // kind: another tool's mode on the output, one not on it, the start size, and a
        // size an earlier connection made, asked after a restart.
        let (mut earlier, mut made_here) = (Vec::new(), Vec::new());
        let mut reach = [0usize; 4];
        for _ in 0..200 {
            if rng.below(4) == 0 {
                drop(screen);
                screen = Screen::open(Some(&x.display)).unwrap();
                earlier.append(&mut made_here);
            }
            let want = sizes[rng.below(sizes.len() as u64) as usize];
            reach[0] += usize::from(want == foreign[0]);
            reach[1] += usize::from(want == foreign[1]);
            reach[2] += usize::from(want == max);
            reach[3] += usize::from(earlier.contains(&want) && !foreign.contains(&want));
            made_here.push(want);
            if let Err(e) = screen.resize(want.0, want.1) {
                panic!("{}x{} refused: {e}", want.0, want.1);
            }
            assert_eq!(seen(&x.display).0, want);
        }
        assert!(reach.iter().all(|&n| n > 0), "unreached: {reach:?}");
        let (_, modes) = seen(&x.display);
        for size in sizes {
            let n = modes.iter().filter(|&&m| m == size).count();
            assert_eq!(n, 1, "{}x{} has {n} modes", size.0, size.1);
        }
    }
}
