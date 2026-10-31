//! Block move, row stress and bit fade.

use std::time::{Duration, Instant};

use super::common::{fill_fn, verify_fn, Checker};
use crate::rng::{derive_seed, hash_at};
use crate::{Reporter, TestContext, Word};

pub(crate) const BLOCK_MOVE_SWEEPS: u32 = 5;
/// Row strides (bytes) hammered, double-sided (`b` and `b + 2 * stride`).
const ROW_STRIDES: [usize; 2] = [8 * 1024, 64 * 1024];
/// Words per modelled DRAM row for the checker polarity (8 KiB).
const ROW_WORDS_LOG2: u32 = 10;
/// Aggressor pairs hammered per 1 MiB block and activations (read+flush
/// iterations) per pair; thousands of activations are needed for disturbance
/// errors, so this is a tight loop per pair rather than a broad sweep.
const PAIRS_PER_BLOCK: usize = 2;
const HAMMER_ITERS: usize = 1500;
pub(crate) const ROW_STRESS_SWEEPS: u32 = (ROW_STRIDES.len() * 2 * 3) as u32;
pub(crate) const BIT_FADE_SWEEPS: u32 = 4;

/// Fill with random words, move the lower half onto the upper half with
/// `memmove`, verify, then shift the whole region up by one word (overlapping,
/// misaligned memmove), verify again.
pub(crate) fn block_move(region: &mut [Word], chk: &mut Checker, ctx: &TestContext) -> Option<()> {
    let len = region.len();
    let h = len / 2;
    let key = derive_seed(ctx.seed ^ 0x424C_4B4D, ctx.pass, ctx.thread);
    let pat = move |i: usize| hash_at(key, i);
    // State after the half copy.
    let s_val = move |i: usize| {
        if i >= h && i < 2 * h {
            pat(i - h)
        } else {
            pat(i)
        }
    };

    chk.sweep(region, false, |s, base, _| fill_fn(s, |i| pat(base + i)))?;

    let p = region.as_mut_ptr();
    chk.blocks(h, false, 2, |start, n, _| {
        // SAFETY: source [start, start + n) lies in the lower half, the
        // destination [h + start, h + start + n) in the upper half (h + start + n <= 2h <= len);
        // the ranges are disjoint. `region` is exclusively borrowed.
        unsafe { core::ptr::copy_nonoverlapping(p.add(start), p.add(h + start), n) };
    })?;
    chk.sweep(region, false, |s, base, c| {
        verify_fn(s, c, |i| s_val(base + i))
    })?;

    // Shift up by one word, highest block first so unread sources are never clobbered.
    let p = region.as_mut_ptr();
    chk.blocks(len.saturating_sub(1), true, 1, |start, n, _| {
        // SAFETY: start + n <= len - 1, so the destination end start + n + 1 <= len;
        // `ptr::copy` handles the overlap inside the block.
        unsafe { core::ptr::copy(p.add(start), p.add(start + 1), n) };
    })?;
    chk.sweep(region, false, |s, base, c| {
        verify_fn(s, c, |i| {
            let g = base + i;
            if g == 0 {
                s_val(0)
            } else {
                s_val(g - 1)
            }
        });
    })
}

#[inline(always)]
fn fence() {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: mfence has no preconditions.
    unsafe {
        core::arch::x86_64::_mm_mfence();
    }
}

/// Hammer one aggressor pair: `ITERS` times read `a1` and `a2`, flush both
/// lines and fence, so every read is a DRAM row activation.
fn hammer_pair(base: *const Word, a1: usize, a2: usize) {
    let mut sink = 0u64;
    // SAFETY: the caller guarantees a1 < a2 < region length and that `base`
    // is valid for that many reads.
    let (p1, p2) = unsafe { (base.add(a1), base.add(a2)) };
    for _ in 0..HAMMER_ITERS {
        // SAFETY: see above.
        unsafe {
            sink ^= p1.read_volatile();
            sink ^= p2.read_volatile();
        }
        flush_lines(p1, p2);
    }
    core::hint::black_box(sink);
}

#[inline(always)]
fn flush_lines(p1: *const Word, p2: *const Word) {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: clflush is baseline SSE2 on x86_64 and the pointers lie in the
    // mapped region under test; mfence has no preconditions.
    unsafe {
        core::arch::x86_64::_mm_clflush(p1.cast());
        core::arch::x86_64::_mm_clflush(p2.cast());
        core::arch::x86_64::_mm_mfence();
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = (p1, p2);
}

/// Hammer a few sparse aggressor pairs inside the block `[start, start + n)`.
/// Each pair is `a1` and `a2 = a1 + 2 * stride`, i.e. one victim row between
/// them as far as virtual addresses allow us to guess.
fn hammer_block(base: *const Word, len: usize, start: usize, n: usize, stride_words: usize) {
    let span = n / PAIRS_PER_BLOCK;
    for j in 0..PAIRS_PER_BLOCK {
        // Row-aligned (virtually) start, with a per-pair column offset so
        // pairs touch different cache lines.
        let a1 = (start + j * span) / stride_words * stride_words + (j * 8) % stride_words;
        let a2 = a1 + 2 * stride_words;
        if a2 < len {
            hammer_pair(base, a1, a2);
        }
    }
    fence();
}

/// Checker data (0xAA../0x55.. per 8 KiB row, optionally inverted), hammer
/// a limited number of aggressor pairs per 1 MiB block, verify. Two strides,
/// both polarities.
pub(crate) fn row_stress(region: &mut [Word], chk: &mut Checker) -> Option<()> {
    let row = |g: usize| -> Word {
        if (g >> ROW_WORDS_LOG2) & 1 == 0 {
            0xAAAA_AAAA_AAAA_AAAA
        } else {
            0x5555_5555_5555_5555
        }
    };
    let len = region.len();
    for inv in [0u64, !0u64] {
        for stride in ROW_STRIDES {
            let sw = stride / 8;
            chk.sweep(region, false, |s, base, _| {
                fill_fn(s, |i| row(base + i) ^ inv)
            })?;
            let bp = region.as_ptr();
            chk.blocks(len, false, 1, |start, n, _| {
                hammer_block(bp, len, start, n, sw)
            })?;
            chk.sweep(region, false, |s, base, c| {
                verify_fn(s, c, |i| row(base + i) ^ inv)
            })?;
        }
    }
    Some(())
}

/// Sleep up to `delay`, polling `should_stop`. `None` if stopped.
fn wait(delay: Duration, rep: &dyn Reporter) -> Option<()> {
    // An overflowing deadline means "effectively forever": wait until stopped.
    let end = Instant::now().checked_add(delay);
    loop {
        if rep.should_stop() {
            return None;
        }
        let step = match end {
            Some(end) => {
                let now = Instant::now();
                if now >= end {
                    return Some(());
                }
                (end - now).min(Duration::from_millis(50))
            }
            None => Duration::from_millis(50),
        };
        std::thread::sleep(step);
    }
}

/// Fill checkerboard, idle, verify; then the complement.
pub(crate) fn bit_fade(region: &mut [Word], chk: &mut Checker, ctx: &TestContext) -> Option<()> {
    const A: u64 = 0x5555_5555_5555_5555;
    for (even, odd) in [(A, !A), (!A, A)] {
        chk.sweep(region, false, |s, base, _| {
            fill_fn(s, |i| if (base + i) & 1 == 0 { even } else { odd });
        })?;
        wait(ctx.bit_fade_delay, chk.rep)?;
        chk.sweep(region, false, |s, base, c| {
            verify_fn(s, c, |i| if (base + i) & 1 == 0 { even } else { odd });
        })?;
    }
    Some(())
}
