//! The tests' one generator of cases.

/// Marsaglia's xorshift64, shifts 13, 7 and 17. Each test seeds its own, so it draws the
/// same cases on every run; a seed of 0 draws only 0.
#[derive(Clone, Debug)]
pub(crate) struct Xorshift(u64);

impl Xorshift {
    pub(crate) const fn new(seed: u64) -> Xorshift {
        Xorshift(seed)
    }

    /// The next 64 bits.
    pub(crate) fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// A number from 0 to `n` less one.
    pub(crate) fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    /// A number from `lo` to `hi` less one.
    pub(crate) fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.below(hi - lo)
    }
}
