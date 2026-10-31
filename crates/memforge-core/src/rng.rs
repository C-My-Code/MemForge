//! Deterministic, fast PRNG: xoshiro256** seeded through splitmix64.
//!
//! Besides the sequential generator, [`mix64`] / [`hash_at`] provide a
//! stateless, random-access "random" word per index. Tests that must walk
//! memory in both directions (or regenerate a value for a single address)
//! use that instead of the sequential stream.

const GOLDEN: u64 = 0x9E37_79B9_7F4A_7C15;

/// splitmix64 finalizer (a strong 64-bit bijective mixer).
#[inline(always)]
pub fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Stateless pseudo-random word for position `index` under `key`.
/// Random-access, so patterns can be regenerated in any order.
#[inline(always)]
pub fn hash_at(key: u64, index: usize) -> u64 {
    mix64(key.wrapping_add((index as u64).wrapping_mul(GOLDEN)))
}

/// Derive a per-(pass, thread) seed from the run seed.
pub fn derive_seed(seed: u64, pass: u64, thread: usize) -> u64 {
    let a = mix64(seed ^ GOLDEN);
    let b = mix64(a ^ pass.wrapping_mul(0xD6E8_FEB8_6659_FD93));
    mix64(b ^ (thread as u64).wrapping_mul(0xA076_1D64_78BD_642F) ^ 0x1234_5678_9ABC_DEF1)
}

/// xoshiro256** generator.
#[derive(Clone, Debug)]
pub struct Rng {
    s: [u64; 4],
}

impl Rng {
    /// Seed via splitmix64 (guarantees a non-zero state).
    pub fn new(seed: u64) -> Self {
        let mut x = seed;
        let mut s = [0u64; 4];
        for w in &mut s {
            x = x.wrapping_add(GOLDEN);
            *w = mix64(x);
        }
        Rng { s }
    }

    #[inline(always)]
    pub fn next_u64(&mut self) -> u64 {
        let s = &mut self.s;
        let result = s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = s[1] << 17;
        s[2] ^= s[0];
        s[3] ^= s[1];
        s[1] ^= s[2];
        s[0] ^= s[3];
        s[2] ^= t;
        s[3] = s[3].rotate_left(45);
        result
    }

    /// Uniform-ish value in `0..bound` (`bound` must be non-zero; slight modulo bias is fine).
    #[inline(always)]
    pub fn next_below(&mut self, bound: u64) -> u64 {
        ((self.next_u64() as u128 * bound as u128) >> 64) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_distinct() {
        let mut a = Rng::new(1);
        let mut b = Rng::new(1);
        let mut c = Rng::new(2);
        let va: Vec<u64> = (0..16).map(|_| a.next_u64()).collect();
        let vb: Vec<u64> = (0..16).map(|_| b.next_u64()).collect();
        let vc: Vec<u64> = (0..16).map(|_| c.next_u64()).collect();
        assert_eq!(va, vb);
        assert_ne!(va, vc);
    }

    #[test]
    fn zero_seed_not_degenerate() {
        let mut r = Rng::new(0);
        assert_ne!(r.next_u64(), r.next_u64());
    }

    #[test]
    fn derive_seed_mixes_all_inputs() {
        let base = derive_seed(1, 2, 3);
        assert_ne!(base, derive_seed(2, 2, 3));
        assert_ne!(base, derive_seed(1, 3, 3));
        assert_ne!(base, derive_seed(1, 2, 4));
        assert_eq!(base, derive_seed(1, 2, 3));
    }

    #[test]
    fn bits_roughly_balanced() {
        let mut r = Rng::new(42);
        let ones: u32 = (0..10_000).map(|_| r.next_u64().count_ones()).sum();
        let avg = ones as f64 / 10_000.0;
        assert!((31.0..33.0).contains(&avg), "avg {avg}");
    }

    #[test]
    fn next_below_in_range() {
        let mut r = Rng::new(7);
        for _ in 0..1000 {
            assert!(r.next_below(10) < 10);
        }
    }
}
