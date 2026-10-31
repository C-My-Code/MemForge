//! Shared machinery: chunking, progress, verification with fault injection,
//! and the volatile fill/verify primitives all tests are built from.

use crate::rng::{derive_seed, Rng};
use crate::{MemError, Reporter, TestContext, TestId, Word};

/// Words per processing chunk (1 MiB).
pub(crate) const CHUNK_WORDS: usize = 128 * 1024;

/// Verification helper: compares, applies optional fault injection, reports.
pub(crate) struct Checker<'a> {
    test: TestId,
    thread: usize,
    pass: u64,
    pub(crate) rep: &'a dyn Reporter,
    /// Hoisted so the hot path is one predictable branch.
    pub(crate) inject: bool,
    one_in: u64,
    countdown: u64,
    rng: Rng,
}

impl<'a> Checker<'a> {
    pub(crate) fn new(test: TestId, ctx: &TestContext, rep: &'a dyn Reporter) -> Self {
        let one_in = ctx.inject_fault_one_in.unwrap_or(0).max(1);
        let mut c = Checker {
            test,
            thread: ctx.thread,
            pass: ctx.pass,
            rep,
            inject: ctx.inject_fault_one_in.is_some(),
            one_in,
            countdown: 1,
            rng: Rng::new(derive_seed(
                ctx.seed ^ 0xFA17_FA17_FA17_FA17,
                ctx.pass,
                ctx.thread,
            )),
        };
        c.countdown = c.next_interval();
        c
    }

    /// Random gap (mean `one_in`) until the next injected flip.
    fn next_interval(&mut self) -> u64 {
        1 + self
            .rng
            .next_below(self.one_in.saturating_mul(2).saturating_sub(1).max(1))
    }

    #[cold]
    #[inline(never)]
    fn flip(&mut self, actual: Word) -> Word {
        self.countdown = self.next_interval();
        actual ^ (1u64 << (self.rng.next_u64() & 63))
    }

    /// Compare one word (`actual` was read from `ptr`); report on mismatch.
    #[inline(always)]
    pub(crate) fn check(&mut self, ptr: *const Word, expected: Word, actual: Word) {
        let mut actual = actual;
        if self.inject {
            self.countdown -= 1;
            if self.countdown == 0 {
                actual = self.flip(actual);
            }
        }
        if actual != expected {
            self.report(ptr, expected, actual);
        }
    }

    #[cold]
    #[inline(never)]
    fn report(&self, ptr: *const Word, expected: Word, actual: Word) {
        self.rep.error(MemError {
            test: self.test,
            thread: self.thread,
            pass: self.pass,
            address: ptr as usize,
            expected,
            actual,
        });
    }

    /// Run `f(start, n, self)` over blocks of at most [`CHUNK_WORDS`] covering
    /// `0..total` words (descending when `rev`). Polls `should_stop` before
    /// each block and reports `n * 8 * mult` bytes of progress after it
    /// (nothing when `mult` is 0). Returns `None` if stopped.
    pub(crate) fn blocks(
        &mut self,
        total: usize,
        rev: bool,
        mult: u64,
        mut f: impl FnMut(usize, usize, &mut Self),
    ) -> Option<()> {
        let n = total.div_ceil(CHUNK_WORDS);
        let mut one = |i: usize, this: &mut Self| -> Option<()> {
            if this.rep.should_stop() {
                return None;
            }
            let start = i * CHUNK_WORDS;
            let len = CHUNK_WORDS.min(total - start);
            f(start, len, this);
            if mult != 0 {
                this.rep.progress(len as u64 * 8 * mult);
            }
            Some(())
        };
        if rev {
            for i in (0..n).rev() {
                one(i, self)?;
            }
        } else {
            for i in 0..n {
                one(i, self)?;
            }
        }
        Some(())
    }

    /// One counted sweep over `region`: `f(chunk, chunk_start_index, self)`.
    pub(crate) fn sweep(
        &mut self,
        region: &mut [Word],
        rev: bool,
        f: impl FnMut(&mut [Word], usize, &mut Self),
    ) -> Option<()> {
        self.sweep_weighted(region, rev, 1, f)
    }

    /// Like [`sweep`](Self::sweep) with an explicit progress multiplier
    /// (0 = not counted).
    pub(crate) fn sweep_weighted(
        &mut self,
        region: &mut [Word],
        rev: bool,
        mult: u64,
        mut f: impl FnMut(&mut [Word], usize, &mut Self),
    ) -> Option<()> {
        let len = region.len();
        let p = region.as_mut_ptr();
        self.blocks(len, rev, mult, |start, n, this| {
            // SAFETY: start + n <= len, blocks are disjoint, and `region` is
            // exclusively borrowed for the duration of this call.
            let s = unsafe { core::slice::from_raw_parts_mut(p.add(start), n) };
            f(s, start, this);
        })
    }
}

/// Volatile ascending fill: `s[i] = f(i)`.
#[inline(always)]
pub(crate) fn fill_fn(s: &mut [Word], mut f: impl FnMut(usize) -> Word) {
    let p = s.as_mut_ptr();
    let n = s.len();
    let mut i = 0;
    while i + 4 <= n {
        // SAFETY: i + 3 < n, p is valid for n writes.
        unsafe {
            p.add(i).write_volatile(f(i));
            p.add(i + 1).write_volatile(f(i + 1));
            p.add(i + 2).write_volatile(f(i + 2));
            p.add(i + 3).write_volatile(f(i + 3));
        }
        i += 4;
    }
    while i < n {
        // SAFETY: i < n.
        unsafe { p.add(i).write_volatile(f(i)) };
        i += 1;
    }
}

/// Volatile ascending verify: `s[i] == f(i)`.
#[inline(always)]
pub(crate) fn verify_fn(s: &[Word], chk: &mut Checker, mut f: impl FnMut(usize) -> Word) {
    let p = s.as_ptr();
    let n = s.len();
    let mut i = 0;
    while i + 4 <= n {
        // SAFETY: i + 3 < n, p is valid for n reads.
        unsafe {
            let a0 = p.add(i).read_volatile();
            let a1 = p.add(i + 1).read_volatile();
            let a2 = p.add(i + 2).read_volatile();
            let a3 = p.add(i + 3).read_volatile();
            chk.check(p.add(i), f(i), a0);
            chk.check(p.add(i + 1), f(i + 1), a1);
            chk.check(p.add(i + 2), f(i + 2), a2);
            chk.check(p.add(i + 3), f(i + 3), a3);
        }
        i += 4;
    }
    while i < n {
        // SAFETY: i < n.
        unsafe {
            let a = p.add(i).read_volatile();
            chk.check(p.add(i), f(i), a);
        }
        i += 1;
    }
}

/// Ascending (or descending) verify-then-write: check `s[i] == exp(i)` then
/// store `new(i)`.
#[inline(always)]
pub(crate) fn verify_write_fn(
    s: &mut [Word],
    chk: &mut Checker,
    rev: bool,
    mut exp: impl FnMut(usize) -> Word,
    mut new: impl FnMut(usize) -> Word,
) {
    let p = s.as_mut_ptr();
    let n = s.len();
    let mut step = |i: usize| {
        // SAFETY: i < n.
        unsafe {
            let a = p.add(i).read_volatile();
            chk.check(p.add(i), exp(i), a);
            p.add(i).write_volatile(new(i));
        }
    };
    if rev {
        for i in (0..n).rev() {
            step(i);
        }
    } else {
        for i in 0..n {
            step(i);
        }
    }
}

/// Flush the cache lines covering `len` words at `ptr` out to DRAM
/// (clflushopt when available, else clflush), then fence. No-op off x86_64.
pub(crate) fn flush_range(ptr: *const Word, len: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        use core::arch::x86_64::{_mm_clflush, _mm_sfence};
        let start = ptr as usize & !63;
        let end = ptr as usize + len * 8;
        if len == 0 {
            return;
        }
        if has_clflushopt() {
            // SAFETY: feature detected at runtime; the lines overlap memory
            // that is mapped (the caller's region).
            unsafe { flush_opt(start, end) };
        } else {
            let mut a = start;
            while a < end {
                // SAFETY: line overlaps the caller's mapped region.
                unsafe { _mm_clflush(a as *const u8) };
                a += 64;
            }
            // SAFETY: sfence has no preconditions.
            unsafe { _mm_sfence() };
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = (ptr, len);
}

/// CPUID.(EAX=7,ECX=0):EBX bit 23. (The intrinsic and the feature name are
/// unstable, so use cpuid plus inline asm.)
#[cfg(target_arch = "x86_64")]
fn has_clflushopt() -> bool {
    use std::sync::OnceLock;
    static HAS: OnceLock<bool> = OnceLock::new();
    *HAS.get_or_init(|| {
        // cpuid is available on every x86_64 CPU.
        use core::arch::x86_64::{__cpuid, __cpuid_count};
        __cpuid(0).eax >= 7 && (__cpuid_count(7, 0).ebx >> 23) & 1 == 1
    })
}

/// # Safety
/// CPU must support clflushopt; lines in `[start, end)` must overlap mapped memory.
#[cfg(target_arch = "x86_64")]
unsafe fn flush_opt(start: usize, end: usize) {
    let mut a = start;
    while a < end {
        // SAFETY: per the contract; clflushopt does not touch flags or the stack.
        unsafe {
            core::arch::asm!("clflushopt [{0}]", in(reg) a, options(nostack, preserves_flags));
        }
        a += 64;
    }
    // SAFETY: sfence has no preconditions.
    unsafe { core::arch::x86_64::_mm_sfence() };
}

/// Words flushed from the tail of a just-written region before it is re-read
/// (32 MiB, enough to cover most large L3 caches).
pub(crate) const FLUSH_TAIL_WORDS: usize = 4 * 1024 * 1024;

/// Flush the last [`FLUSH_TAIL_WORDS`] words of `region` (or all of it).
pub(crate) fn flush_tail(region: &[Word]) {
    let n = region.len().min(FLUSH_TAIL_WORDS);
    flush_range(region[region.len() - n..].as_ptr(), n);
}

/// Flush the first [`FLUSH_TAIL_WORDS`] words of `region` (or all of it);
/// the most recently written part after a descending sweep.
pub(crate) fn flush_head(region: &[Word]) {
    let n = region.len().min(FLUSH_TAIL_WORDS);
    flush_range(region.as_ptr(), n);
}
