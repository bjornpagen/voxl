//! The rates the flags bound the stream by: what a viewer's path is taken to carry, and the
//! rate the encoders open capped at.

use crate::path::Kbps;

/// What a viewer's path is taken to carry: never under a floor, and never over a ceiling if
/// there is one. Where the two cross, the ceiling wins: it is the most a person lets the
/// stream send (Selkies' `--video-bitrate` sets it alone), where the floor only keeps a
/// window from starving the picture.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Bounds {
    /// Never over the ceiling.
    floor: Kbps,
    ceiling: Option<Kbps>,
}

impl Bounds {
    /// A floor over the ceiling is the ceiling.
    pub fn new(floor: Kbps, ceiling: Option<Kbps>) -> Self {
        let floor = ceiling.into_iter().fold(floor, Kbps::min);
        Bounds { floor, ceiling }
    }

    /// None: no ceiling. Encoders open without a rate cap, so nothing applies what a frame
    /// may take: a viewer on a slow path gets every frame at its CRF.
    pub fn ceiling(self) -> Option<Kbps> {
        self.ceiling
    }

    /// What a viewer's path is taken to carry in `LATENESS` (window.rs): at least what the
    /// floor carries in it, and at most what the ceiling does, or with none as much as
    /// there is.
    pub fn paths(self) -> crate::hub::Paths {
        // kbit/s times ms is bits, exactly for whole numbers of each.
        let ms = crate::window::LATENESS.as_millis() as f64;
        let bytes = |rate: Kbps| (rate.get() * ms / 8.0) as usize;
        crate::hub::Paths {
            least: bytes(self.floor),
            most: self.ceiling.map_or(usize::MAX, bytes),
        }
    }
}
