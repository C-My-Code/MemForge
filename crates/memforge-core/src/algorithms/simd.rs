//! SIMD non-temporal streaming test (AVX-512 / AVX2 with scalar fallback).
//!
//! The wide kernels are `#[inline(never)]` so the compiler cannot forward a
//! stored vector into a later verification load: the write and the verify
//! are separated by an opaque call that receives the raw pointer.

use super::common::{fill_fn, verify_fn, Checker};
use crate::rng::{derive_seed, Rng};
use crate::{TestContext, Word};

pub(crate) const SIMD_ROUNDS: usize = 4;
pub(crate) const SIMD_SWEEPS: u32 = SIMD_ROUNDS as u32 * 2;

/// Instruction-set level used by the stream kernels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Level {
    Avx512,
    Avx2,
    Scalar,
}

impl Level {
    pub(crate) fn detect() -> Level {
        #[cfg(target_arch = "x86_64")]
        {
            if is_x86_feature_detected!("avx512f") {
                return Level::Avx512;
            }
            if is_x86_feature_detected!("avx2") {
                return Level::Avx2;
            }
        }
        Level::Scalar
    }

    /// Whether this level can run on the current CPU.
    #[allow(dead_code)] // used by tests
    pub(crate) fn supported(self) -> bool {
        match self {
            Level::Scalar => true,
            Level::Avx2 => Level::detect() != Level::Scalar,
            Level::Avx512 => Level::detect() == Level::Avx512,
        }
    }
}

#[cfg(target_arch = "x86_64")]
mod x86 {
    use super::Checker;
    use core::arch::x86_64::*;

    /// Non-temporal fill of `nvec` 64-byte blocks. `p` must be 64-byte aligned.
    ///
    /// # Safety
    /// CPU must support AVX-512F; `p` valid for `nvec * 64` bytes of writes.
    #[target_feature(enable = "avx512f")]
    #[inline(never)]
    pub(super) unsafe fn fill512(p: *mut u64, nvec: usize, pat: &[u64; 8]) {
        let v = _mm512_loadu_si512(pat.as_ptr().cast());
        let q = p.cast::<__m512i>();
        let mut i = 0;
        while i + 4 <= nvec {
            _mm512_stream_si512(q.add(i), v);
            _mm512_stream_si512(q.add(i + 1), v);
            _mm512_stream_si512(q.add(i + 2), v);
            _mm512_stream_si512(q.add(i + 3), v);
            i += 4;
        }
        while i < nvec {
            _mm512_stream_si512(q.add(i), v);
            i += 1;
        }
        _mm_sfence();
    }

    /// Wide-load verify of `nvec` 64-byte blocks. `p` must be 64-byte aligned.
    ///
    /// # Safety
    /// CPU must support AVX-512F; `p` valid for `nvec * 64` bytes of reads.
    #[target_feature(enable = "avx512f")]
    #[inline(never)]
    pub(super) unsafe fn verify512(p: *const u64, nvec: usize, pat: &[u64; 8], chk: &mut Checker) {
        let v = _mm512_loadu_si512(pat.as_ptr().cast());
        let q = p.cast::<__m512i>();
        for i in 0..nvec {
            let a = _mm512_load_si512(q.add(i).cast());
            let m = _mm512_cmpeq_epi64_mask(a, v);
            if chk.inject || m != 0xFF {
                let w: [u64; 8] = core::mem::transmute(a);
                for (l, &x) in w.iter().enumerate() {
                    chk.check(p.add(i * 8 + l), pat[l], x);
                }
            }
        }
    }

    /// # Safety
    /// CPU must support AVX2; `p` 64-byte aligned, valid for `nvec * 64` bytes.
    #[target_feature(enable = "avx2")]
    #[inline(never)]
    pub(super) unsafe fn fill256(p: *mut u64, nvec: usize, pat: &[u64; 8]) {
        let lo = _mm256_loadu_si256(pat.as_ptr().cast());
        let hi = _mm256_loadu_si256(pat.as_ptr().add(4).cast());
        let q = p.cast::<__m256i>();
        for i in 0..nvec {
            _mm256_stream_si256(q.add(2 * i), lo);
            _mm256_stream_si256(q.add(2 * i + 1), hi);
        }
        _mm_sfence();
    }

    /// # Safety
    /// CPU must support AVX2; `p` 64-byte aligned, valid for `nvec * 64` bytes.
    #[target_feature(enable = "avx2")]
    #[inline(never)]
    pub(super) unsafe fn verify256(p: *const u64, nvec: usize, pat: &[u64; 8], chk: &mut Checker) {
        let lo = _mm256_loadu_si256(pat.as_ptr().cast());
        let hi = _mm256_loadu_si256(pat.as_ptr().add(4).cast());
        let q = p.cast::<__m256i>();
        for i in 0..nvec {
            let a = _mm256_load_si256(q.add(2 * i));
            let b = _mm256_load_si256(q.add(2 * i + 1));
            let ok = _mm256_movemask_epi8(_mm256_cmpeq_epi64(a, lo)) == -1
                && _mm256_movemask_epi8(_mm256_cmpeq_epi64(b, hi)) == -1;
            if chk.inject || !ok {
                let wa: [u64; 4] = core::mem::transmute(a);
                let wb: [u64; 4] = core::mem::transmute(b);
                for l in 0..4 {
                    chk.check(p.add(i * 8 + l), pat[l], wa[l]);
                    chk.check(p.add(i * 8 + 4 + l), pat[4 + l], wb[l]);
                }
            }
        }
    }
}

/// Number of scalar words before `ptr` reaches 64-byte alignment.
fn head_words(ptr: *const Word, len: usize) -> usize {
    (((64 - (ptr as usize & 63)) & 63) / 8).min(len)
}

/// Pattern lane of an absolute word address (lane = address / 8 mod 8).
#[inline(always)]
fn lane(ptr: *const Word, i: usize) -> usize {
    ((ptr as usize >> 3) + i) & 7
}

fn stream_fill(level: Level, s: &mut [Word], pat: &[u64; 8]) {
    let head = head_words(s.as_ptr(), s.len());
    let (h, rest) = s.split_at_mut(head);
    let hp = h.as_ptr();
    fill_fn(h, |i| pat[lane(hp, i)]);
    let nvec = rest.len() / 8;
    let (body, tail) = rest.split_at_mut(nvec * 8);
    if nvec > 0 {
        match level {
            // SAFETY: level was chosen from runtime detection (or checked in
            // tests); body is 64-byte aligned and holds nvec * 64 bytes.
            #[cfg(target_arch = "x86_64")]
            Level::Avx512 => unsafe { x86::fill512(body.as_mut_ptr(), nvec, pat) },
            #[cfg(target_arch = "x86_64")]
            Level::Avx2 => unsafe { x86::fill256(body.as_mut_ptr(), nvec, pat) },
            _ => fill_fn(body, |i| pat[i & 7]),
        }
    }
    fill_fn(tail, |i| pat[i & 7]);
}

fn stream_verify(level: Level, s: &[Word], pat: &[u64; 8], chk: &mut Checker) {
    let head = head_words(s.as_ptr(), s.len());
    let (h, rest) = s.split_at(head);
    let hp = h.as_ptr();
    verify_fn(h, chk, |i| pat[lane(hp, i)]);
    let nvec = rest.len() / 8;
    let (body, tail) = rest.split_at(nvec * 8);
    if nvec > 0 {
        match level {
            // SAFETY: as in `stream_fill`.
            #[cfg(target_arch = "x86_64")]
            Level::Avx512 => unsafe { x86::verify512(body.as_ptr(), nvec, pat, chk) },
            #[cfg(target_arch = "x86_64")]
            Level::Avx2 => unsafe { x86::verify256(body.as_ptr(), nvec, pat, chk) },
            _ => verify_fn(body, chk, |i| pat[i & 7]),
        }
    }
    verify_fn(tail, chk, |i| pat[i & 7]);
}

/// Four rounds: 0xAA/0x55 lane checker and its complement, then random lanes
/// and their complement; each is a non-temporal write sweep + wide verify sweep.
pub(crate) fn simd_stream(
    region: &mut [Word],
    chk: &mut Checker,
    ctx: &TestContext,
    level: Level,
) -> Option<()> {
    let mut rng = Rng::new(derive_seed(ctx.seed ^ 0x5349_4D44, ctx.pass, ctx.thread));
    let mut rand = [0u64; 8];
    for w in &mut rand {
        *w = rng.next_u64();
    }
    let a = 0xAAAA_AAAA_AAAA_AAAAu64;
    let checker = [a, !a, a, !a, a, !a, a, !a];
    let pats: [[u64; 8]; SIMD_ROUNDS] = [checker, checker.map(|x| !x), rand, rand.map(|x| !x)];
    for pat in &pats {
        chk.sweep(region, false, |s, _, _| stream_fill(level, s, pat))?;
        chk.sweep(region, false, |s, _, c| stream_verify(level, s, pat, c))?;
    }
    Some(())
}
