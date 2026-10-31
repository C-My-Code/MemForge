//! Pattern tests built on the volatile primitives: own-address, walking bits,
//! solid/checkerboard, moving inversions (fixed and random), random pattern
//! and modulo-20.

use super::common::{fill_fn, flush_head, flush_tail, verify_fn, verify_write_fn, Checker};
use crate::rng::{derive_seed, hash_at, Rng};
use crate::{TestContext, Word};

pub(crate) const OWN_ADDRESS_SWEEPS: u32 = 4;
pub(crate) const WALKING_SWEEPS: u32 = 128;
pub(crate) const SOLID_SWEEPS: u32 = 8;
pub(crate) const MI_PATTERNS: [u64; 12] = [
    0x0000_0000_0000_0000,
    0x5555_5555_5555_5555,
    0x3333_3333_3333_3333,
    0x0F0F_0F0F_0F0F_0F0F,
    // 8-bit walking one, replicated across the word.
    0x0101_0101_0101_0101,
    0x0202_0202_0202_0202,
    0x0404_0404_0404_0404,
    0x0808_0808_0808_0808,
    0x1010_1010_1010_1010,
    0x2020_2020_2020_2020,
    0x4040_4040_4040_4040,
    0x8080_8080_8080_8080,
];
pub(crate) const MI_SWEEPS: u32 = MI_PATTERNS.len() as u32 * 4;
pub(crate) const MIR_KEYS: usize = 4;
pub(crate) const MIR_SWEEPS: u32 = MIR_KEYS as u32 * 4;
pub(crate) const RANDOM_ROUNDS: u64 = 2;
pub(crate) const RANDOM_SWEEPS: u32 = RANDOM_ROUNDS as u32 * 2;
pub(crate) const MODULO: usize = 20;
pub(crate) const MODULO_SWEEPS: u32 = MODULO as u32 * (MODULO_WRITE_REPEATS as u32 + 1);
/// Full-region `!p` write sweeps per modulo-20 offset (memtest86 repeats this
/// to give the neighbours time to disturb the `p` cells; 2 keeps runtime sane).
pub(crate) const MODULO_WRITE_REPEATS: usize = 2;

/// Address-in-address, then its complement.
pub(crate) fn own_address(region: &mut [Word], chk: &mut Checker) -> Option<()> {
    for inv in [0u64, !0u64] {
        chk.sweep(region, false, |s, _, _| {
            let base = s.as_ptr() as usize;
            fill_fn(s, |i| (base + i * 8) as u64 ^ inv);
        })?;
        flush_tail(region);
        chk.sweep(region, false, |s, _, c| {
            let base = s.as_ptr() as usize;
            verify_fn(s, c, |i| (base + i * 8) as u64 ^ inv);
        })?;
    }
    Some(())
}

/// A single set bit (or single clear bit when `invert`) rotating by one
/// position per word, shifted by one more position each round, so every cell
/// sees all 64 single-bit patterns and neighbours differ.
pub(crate) fn walking(region: &mut [Word], chk: &mut Checker, invert: bool) -> Option<()> {
    let inv = if invert { !0u64 } else { 0 };
    for k in 0..64usize {
        chk.sweep(region, false, |s, base, _| {
            let mut v = 1u64.rotate_left(((base + k) % 64) as u32);
            fill_fn(s, |_| {
                let r = v;
                v = v.rotate_left(1);
                r ^ inv
            });
        })?;
        flush_tail(region);
        chk.sweep(region, false, |s, base, c| {
            let mut v = 1u64.rotate_left(((base + k) % 64) as u32);
            verify_fn(s, c, |_| {
                let r = v;
                v = v.rotate_left(1);
                r ^ inv
            });
        })?;
    }
    Some(())
}

/// All-zero, all-one, then 0x55../0xAA.. alternating per word in both polarities.
pub(crate) fn solid_bits(region: &mut [Word], chk: &mut Checker) -> Option<()> {
    const A: u64 = 0x5555_5555_5555_5555;
    const B: u64 = 0xAAAA_AAAA_AAAA_AAAA;
    for (even, odd) in [(0, 0), (!0, !0), (A, B), (B, A)] {
        chk.sweep(region, false, |s, base, _| {
            fill_fn(s, |i| if (base + i) & 1 == 0 { even } else { odd });
        })?;
        flush_tail(region);
        chk.sweep(region, false, |s, base, c| {
            verify_fn(s, c, |i| if (base + i) & 1 == 0 { even } else { odd });
        })?;
    }
    Some(())
}

/// Classic moving inversions for one pattern function: fill `p`; ascending
/// verify `p` + write `!p`; descending verify `!p` + write `p`; final verify `p`.
#[inline(always)]
fn moving_inversions_one(
    region: &mut [Word],
    chk: &mut Checker,
    pat: impl Fn(usize) -> Word + Copy,
) -> Option<()> {
    chk.sweep(region, false, |s, base, _| fill_fn(s, |i| pat(base + i)))?;
    chk.sweep(region, false, |s, base, c| {
        verify_write_fn(s, c, false, |i| pat(base + i), |i| !pat(base + i));
    })?;
    // The tail was just written and is the first thing the descending sweep
    // reads; push it to DRAM so it is not verified from cache.
    flush_tail(region);
    chk.sweep(region, true, |s, base, c| {
        verify_write_fn(s, c, true, |i| !pat(base + i), |i| pat(base + i));
    })?;
    flush_head(region);
    chk.sweep(region, false, |s, base, c| {
        verify_fn(s, c, |i| pat(base + i))
    })
}

pub(crate) fn moving_inversions(region: &mut [Word], chk: &mut Checker) -> Option<()> {
    for p in MI_PATTERNS {
        moving_inversions_one(region, chk, move |_| p)?;
    }
    Some(())
}

pub(crate) fn moving_inversions_random(
    region: &mut [Word],
    chk: &mut Checker,
    ctx: &TestContext,
) -> Option<()> {
    let mut rng = Rng::new(derive_seed(
        ctx.seed ^ 0x4D49_5241_4E44,
        ctx.pass,
        ctx.thread,
    ));
    for _ in 0..MIR_KEYS {
        let key = rng.next_u64();
        moving_inversions_one(region, chk, move |i| hash_at(key, i))?;
    }
    Some(())
}

/// Seeded sequential random fill, verified by regenerating the same stream.
pub(crate) fn random_pattern(
    region: &mut [Word],
    chk: &mut Checker,
    ctx: &TestContext,
) -> Option<()> {
    for round in 0..RANDOM_ROUNDS {
        let seed = derive_seed(
            ctx.seed ^ round.wrapping_mul(0x5EED_0000_0001),
            ctx.pass,
            ctx.thread,
        );
        let mut rng = Rng::new(seed);
        chk.sweep(region, false, |s, _, _| fill_fn(s, |_| rng.next_u64()))?;
        flush_tail(region);
        let mut rng = Rng::new(seed);
        chk.sweep(region, false, |s, _, c| verify_fn(s, c, |_| rng.next_u64()))?;
    }
    Some(())
}

/// Memtest86 test 9: for each offset k, write `p` to every 20th word
/// (offset k); then `MODULO_WRITE_REPEATS` times write `!p` to all the other
/// words, each repeat sweeping the whole region so the writes are evicted to
/// DRAM between repeats; finally verify the `p` words (and the `!p` words).
/// A neighbour write that disturbs a `p` cell is therefore caught.
pub(crate) fn modulo20(region: &mut [Word], chk: &mut Checker, ctx: &TestContext) -> Option<()> {
    let key = derive_seed(ctx.seed ^ 0x4D4F_4455_4C4F, ctx.pass, ctx.thread);
    for k in 0..MODULO {
        let p = hash_at(key, k);
        // Sparse pass over 1/20 of the words: not counted as a sweep.
        chk.sweep_weighted(region, false, 0, |s, base, _| {
            let first = (k + MODULO - base % MODULO) % MODULO;
            let ptr = s.as_mut_ptr();
            let mut i = first;
            while i < s.len() {
                // SAFETY: i < s.len().
                unsafe { ptr.add(i).write_volatile(p) };
                i += MODULO;
            }
        })?;
        for _ in 0..MODULO_WRITE_REPEATS {
            chk.sweep(region, false, |s, base, _| {
                let ptr = s.as_mut_ptr();
                let first = (k + MODULO - base % MODULO) % MODULO;
                // Words [0, first) and then runs between p-cells get !p.
                let mut start = 0;
                let mut cell = first;
                while start < s.len() {
                    let end = cell.min(s.len());
                    for i in start..end {
                        // SAFETY: i < s.len().
                        unsafe { ptr.add(i).write_volatile(!p) };
                    }
                    start = cell + 1;
                    cell += MODULO;
                }
            })?;
            flush_tail(region);
        }
        chk.sweep(region, false, |s, base, c| {
            let mut m = base % MODULO;
            verify_fn(s, c, |_| {
                let e = if m == k { p } else { !p };
                m += 1;
                if m == MODULO {
                    m = 0;
                }
                e
            });
        })?;
    }
    Some(())
}
