//! Rates and amounts of data, as the flags and x264 speak of them.

/// A rate, in kbit/s.
#[derive(Clone, Copy, PartialEq, PartialOrd, Debug)]
pub struct Kbps(f64);

impl Kbps {
    pub const fn new(kbps: f64) -> Self {
        Kbps(kbps)
    }
    pub fn get(self) -> f64 {
        self.0
    }
    pub fn min(self, other: Self) -> Self {
        Kbps(self.0.min(other.0))
    }
}

/// An amount of data, in bits.
#[derive(Clone, Copy, PartialEq, PartialOrd, Debug)]
pub struct Bits(f64);

impl Bits {
    pub fn of_bytes(bytes: usize) -> Self {
        Bits(bytes as f64 * 8.0)
    }
    /// The rate that sends this much in every frame at `fps`: how x264's VBV is told
    /// what one frame may take.
    pub fn a_frame_at(self, fps: u32) -> Kbps {
        Kbps(self.0 * fps as f64 / 1000.0)
    }
}
