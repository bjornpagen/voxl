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

/// Smallest desktop a client may ask for: VGA, the least screen desktop toolkits lay their
/// dialogs out for. A choice, not measured; what would measure it is the smallest size at
/// which XFCE's settings dialogs still fit.
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
    pub fn open(display: &str) -> Result<Self, String> {
        let (conn, n) = RustConnection::connect(Some(display)).map_err(err)?;
        let s = &conn.setup().roots[n];
        let (root, w, h) = (
            s.root,
            u32::from(s.width_in_pixels),
            u32::from(s.height_in_pixels),
        );
        let mm_per_px = (
            f64::from(s.width_in_millimeters) / f64::from(w),
            f64::from(s.height_in_millimeters) / f64::from(h),
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
            max: (u32::from(range.max_width), u32::from(range.max_height)),
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

    /// Sets the screen to `w`x`h` (fitted): the size it has now.
    pub fn resize(&mut self, w: u32, h: u32) -> Result<(u32, u32), String> {
        let (w, h) = self.fit(w, h);
        let geometry = self
            .conn
            .get_geometry(self.root)
            .map_err(err)?
            .reply()
            .map_err(err)?;
        self.current = (u32::from(geometry.width), u32::from(geometry.height));
        if (w, h) == self.current {
            return Ok(self.current);
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
        // A side's millimetres, at least one: a whole number well under 2^32 for any screen.
        let mm = |px: u32, per: f64| ((f64::from(px) * per).round() as u32).max(1);
        // Every size here is fitted, so within the screen's maximum, which RandR gives in
        // 16 bits.
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
        Ok(self.current)
    }

    /// A new mode of `w`x`h`, named for its size. Called only when the screen has no mode
    /// of that size, so a name of ours is taken only by a mode of another size someone
    /// else named so, and then making it fails. The size is fitted, so within RandR's 16
    /// bits, and its name a few bytes.
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

    /// A size asked is fitted to one the screen takes: each side the largest it can be
    /// that is no more than the ask held between `MIN` (or the maximum, if smaller) and
    /// the maximum, the width a multiple of 8 and the height even. Over generated asks and
    /// maxima, small and large, of either side of `MIN`.
    #[test]
    fn a_size_is_fitted_to_one_the_screen_takes() {
        let mut rng = crate::xorshift::Xorshift::new(0x3c6e_f372_a54f_f53a);
        let mut draw = |n: u64| rng.below(n) as u32;
        for _ in 0..100_000 {
            let max = (8 + draw(4000), 2 + draw(3000));
            let (w, h) = (draw(5000), draw(4000));
            let (fw, fh) = fit(w, h, max);
            let side = |ask: u32, min: u32, most: u32, step: u32, got: u32| {
                let held = ask.clamp(min.min(most), most);
                got.is_multiple_of(step) && got <= held && held - got < step
            };
            assert!(
                side(w, MIN.0, max.0, 8, fw),
                "{w}x{h} in {max:?}: {fw}x{fh}"
            );
            assert!(
                side(h, MIN.1, max.1, 2, fh),
                "{w}x{h} in {max:?}: {fw}x{fh}"
            );
        }
    }
}
