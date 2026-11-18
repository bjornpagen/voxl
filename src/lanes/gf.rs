//! GF(2^8), the field the code computes in: bytes, added by XOR and multiplied modulo
//! x^8 + x^4 + x^3 + x^2 + 1 (0x11d), the primitive polynomial Reed-Solomon codes have used
//! since CCSDS and that x = 2 generates.
//!
//! Every product is one lookup in [`MUL`], built at compile time. The region operations, the
//! code's whole cost (`out += c·src` over a symbol, and `out = c·out`), run by the split-nibble
//! method of ISA-L and Leopard (Plank, Greenan and Miller, "Screaming Fast Galois Field
//! Arithmetic Using Intel SIMD Instructions", FAST 2013): c·b = c·(b & 0xf) ⊕ c·(b & 0xf0),
//! each term one lookup in a 16-entry table of c's products, sixteen bytes at a time by a byte
//! shuffle (NEON `tbl`, SSSE3 `pshufb`). The kernels are written once, generic over
//! fearless_simd's [`Simd`], and run at the level it detects ([`dispatch!`]).

use fearless_simd::{dispatch, prelude::*, u8x16, Level};

/// The primitive polynomial, with its x^8 term.
const POLY: u16 = 0x11d;

/// `EXP[i]` is 2^i, for i from 0 to 509, so that `EXP[LOG[a] + LOG[b]]` needs no reduction
/// modulo 255.
const EXP: [u8; 510] = {
    let mut t = [0u8; 510];
    let mut x: u16 = 1;
    let mut i = 0;
    while i < 510 {
        t[i] = x as u8;
        x <<= 1;
        if x & 0x100 != 0 {
            x ^= POLY;
        }
        i += 1;
    }
    t
};

/// `LOG[a]` is the i with 2^i = a, for every a but 0 (whose entry is unused).
const LOG: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 255 {
        t[EXP[i] as usize] = i as u8;
        i += 1;
    }
    t
};

/// `MUL[a][b]` is a·b.
pub static MUL: [[u8; 256]; 256] = {
    let mut t = [[0u8; 256]; 256];
    let mut a = 1;
    while a < 256 {
        let mut b = 1;
        while b < 256 {
            t[a][b] = EXP[LOG[a] as usize + LOG[b] as usize];
            b += 1;
        }
        a += 1;
    }
    t
};

/// a·b.
pub fn mul(a: u8, b: u8) -> u8 {
    MUL[a as usize][b as usize]
}

/// The b with a·b = 1. `a` is not 0: 0 has no inverse, and no caller holds one (a pivot is
/// non-zero by its definition).
pub fn inv(a: u8) -> u8 {
    debug_assert_ne!(a, 0);
    EXP[255 - LOG[a as usize] as usize]
}

/// c's products with the sixteen low nibbles and the sixteen high ones.
fn nibbles(c: u8) -> ([u8; 16], [u8; 16]) {
    let row = &MUL[c as usize];
    (
        core::array::from_fn(|i| row[i]),
        core::array::from_fn(|i| row[i << 4]),
    )
}

/// `out += c·src`, over the shorter of the two: a shorter symbol is the same symbol with
/// zeros after it.
pub fn mul_add(out: &mut [u8], c: u8, src: &[u8]) {
    let n = out.len().min(src.len());
    dispatch!(Level::new(), simd => mul_add_in(simd, &mut out[..n], c, &src[..n]));
}

/// `out = c·out`.
pub fn scale(out: &mut [u8], c: u8) {
    dispatch!(Level::new(), simd => scale_in(simd, out, c));
}

/// [`mul_add`] at one level, over slices of one length.
#[inline(always)]
fn mul_add_in<S: Simd>(simd: S, out: &mut [u8], c: u8, src: &[u8]) {
    let (lo, hi) = nibbles(c);
    let (lo, hi) = (u8x16::from_slice(simd, &lo), u8x16::from_slice(simd, &hi));
    let low = u8x16::splat(simd, 0x0f);
    let (outs, out_rest) = out.as_chunks_mut::<16>();
    let (srcs, src_rest) = src.as_chunks::<16>();
    for (o, s) in outs.iter_mut().zip(srcs) {
        let b = u8x16::from_slice(simd, s);
        let product = lo.swizzle_dyn(b & low) ^ hi.swizzle_dyn(b >> 4);
        (u8x16::from_slice(simd, o) ^ product).store_slice(o);
    }
    let row = &MUL[c as usize];
    for (o, s) in out_rest.iter_mut().zip(src_rest) {
        *o ^= row[*s as usize];
    }
}

/// [`scale`] at one level.
#[inline(always)]
fn scale_in<S: Simd>(simd: S, out: &mut [u8], c: u8) {
    let (lo, hi) = nibbles(c);
    let (lo, hi) = (u8x16::from_slice(simd, &lo), u8x16::from_slice(simd, &hi));
    let low = u8x16::splat(simd, 0x0f);
    let (outs, rest) = out.as_chunks_mut::<16>();
    for o in outs {
        let b = u8x16::from_slice(simd, o);
        (lo.swizzle_dyn(b & low) ^ hi.swizzle_dyn(b >> 4)).store_slice(o);
    }
    let row = &MUL[c as usize];
    for o in rest {
        *o = row[*o as usize];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The region kernels are their scalar definitions, `out[i] ^= c·src[i]` over the shorter
    /// of the two and `out[i] = c·out[i]`, for coefficients drawn at random (0 and 1 among
    /// them) and lengths from 0 to three vectors and a symbol past it, most not a multiple of
    /// the vector width, at any offset.
    #[test]
    fn the_kernels_are_their_definitions() {
        use crate::lanes::wire::SYMBOL;
        use crate::xorshift::Xorshift;
        let mut rng = Xorshift::new(0x2545_F491_4F6C_DD1D);
        for case in 0..4000 {
            let c = match case % 50 {
                0 => 0,
                1 => 1,
                _ => rng.below(256) as u8,
            };
            let len = |rng: &mut Xorshift| match rng.below(2) {
                0 => rng.below(49) as usize,
                _ => rng.below(SYMBOL as u64 + 1) as usize,
            };
            let (n_out, n_src, at) = (len(&mut rng), len(&mut rng), rng.below(16) as usize);
            let bytes = |rng: &mut Xorshift, n: usize| -> Vec<u8> {
                (0..n).map(|_| rng.below(256) as u8).collect()
            };
            let out = bytes(&mut rng, n_out + at);
            let src = bytes(&mut rng, n_src + at);
            let mut want = out.clone();
            for (o, s) in want[at..].iter_mut().zip(&src[at..]) {
                *o ^= mul(c, *s);
            }
            let mut got = out.clone();
            mul_add(&mut got[at..], c, &src[at..]);
            assert_eq!(got, want, "mul_add c {c} out {n_out} src {n_src} at {at}");
            let mut want = out.clone();
            for o in &mut want[at..] {
                *o = mul(c, *o);
            }
            let mut got = out;
            scale(&mut got[at..], c);
            assert_eq!(got, want, "scale c {c} len {n_out} at {at}");
        }
    }

    /// Multiplication by the tables is the carry-less product reduced modulo 0x11d, for
    /// every pair; every non-zero element's inverse is its inverse; 2 generates the field.
    #[test]
    fn the_tables_are_the_field() {
        fn slow(a: u8, b: u8) -> u8 {
            let mut p: u16 = 0;
            for i in 0..8 {
                if b >> i & 1 == 1 {
                    p ^= (a as u16) << i;
                }
            }
            for i in (8..16).rev() {
                if p >> i & 1 == 1 {
                    p ^= POLY << (i - 8);
                }
            }
            p as u8
        }
        for a in 0..=255u8 {
            for b in 0..=255u8 {
                assert_eq!(mul(a, b), slow(a, b), "{a}·{b}");
            }
            if a != 0 {
                assert_eq!(mul(a, inv(a)), 1, "{a}");
            }
        }
        let mut seen = [false; 256];
        for &x in &EXP[..255] {
            assert!(!seen[x as usize], "2 repeats {x} before its order");
            seen[x as usize] = true;
        }
    }
}
