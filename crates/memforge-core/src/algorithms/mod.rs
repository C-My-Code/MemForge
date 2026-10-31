//! Test algorithm implementations.
//!
//! Progress accounting: every counted sweep reports exactly
//! `region_bytes` over its chunks, so the sum over one `run` is
//! `region_bytes * sweeps()` (block moves are counted as one sweep each).
//! All accesses to the region under test are volatile or SIMD-intrinsic so
//! writes cannot be fused with the verifying reads.

mod common;
mod simd;
mod simple;
mod stress;
#[cfg(test)]
mod tests;

use common::Checker;
use simd::Level;

use crate::{MemTest, Reporter, TestContext, TestId, Word};

pub(crate) fn description(id: TestId) -> &'static str {
    match id {
        TestId::OwnAddress => {
            "Writes each word's own address (then its complement) and verifies; catches addressing faults and aliasing between locations."
        }
        TestId::WalkingOnes => {
            "A single set bit walks through all 64 data lines, rotating per word and per round; exposes stuck or coupled data lines."
        }
        TestId::WalkingZeros => {
            "A single cleared bit walks through all 64 data lines, rotating per word and per round; the inverse of walking ones."
        }
        TestId::SolidBits => {
            "All-zero, all-one and alternating 0x5555/0xAAAA per-word checkerboard fills in both polarities; maximises simultaneous switching and neighbour coupling."
        }
        TestId::MovingInversions => {
            "Memtest86-style moving inversions with several fixed and 8-bit walking patterns: fill, ascending check+invert, descending check+restore."
        }
        TestId::MovingInversionsRandom => {
            "Moving inversions using random per-word data, so every cell and its neighbours see uncorrelated values."
        }
        TestId::RandomPattern => {
            "Fills memory with a seeded xoshiro256** stream and verifies by regenerating it; stresses the bus with high-entropy data."
        }
        TestId::BlockMove => {
            "Fills with random data, then memmoves large blocks between halves and by one misaligned word, verifying the moved data; tests sustained read+write traffic."
        }
        TestId::Modulo20 => {
            "Memtest86 test 9: every 20th word is written with a pattern, then all other words are overwritten with its complement over repeated whole-region sweeps, and the pattern cells are verified to catch write-disturb coupling."
        }
        TestId::SimdStream => {
            "AVX-512/AVX2 non-temporal 64-byte stores streamed to DRAM and read back with wide loads; maximum memory bandwidth."
        }
        TestId::RowStress => {
            "Hammers a few aggressor address pairs per MiB (8 KiB / 64 KiB apart) with thousands of read+clflush iterations each, then verifies checkerboard data for disturbance errors. User mode cannot see physical addresses, so rows and banks are only guessed from virtual addresses and hammering may miss the real adjacent rows."
        }
        TestId::BitFade => {
            "Fills a checkerboard, idles for the configured delay, then verifies (and repeats inverted); exposes refresh and retention faults such as marginal tREFI."
        }
    }
}

pub(crate) struct Algo {
    id: TestId,
    /// SIMD level override (tests); `None` = runtime detection.
    level: Option<Level>,
}

pub(crate) fn create(id: TestId) -> Box<dyn MemTest> {
    Box::new(Algo { id, level: None })
}

impl MemTest for Algo {
    fn id(&self) -> TestId {
        self.id
    }

    fn sweeps(&self) -> u32 {
        match self.id {
            TestId::OwnAddress => simple::OWN_ADDRESS_SWEEPS,
            TestId::WalkingOnes | TestId::WalkingZeros => simple::WALKING_SWEEPS,
            TestId::SolidBits => simple::SOLID_SWEEPS,
            TestId::MovingInversions => simple::MI_SWEEPS,
            TestId::MovingInversionsRandom => simple::MIR_SWEEPS,
            TestId::RandomPattern => simple::RANDOM_SWEEPS,
            TestId::BlockMove => stress::BLOCK_MOVE_SWEEPS,
            TestId::Modulo20 => simple::MODULO_SWEEPS,
            TestId::SimdStream => simd::SIMD_SWEEPS,
            TestId::RowStress => stress::ROW_STRESS_SWEEPS,
            TestId::BitFade => stress::BIT_FADE_SWEEPS,
        }
    }

    fn run(&self, region: &mut [Word], ctx: &TestContext, rep: &dyn Reporter) {
        let mut chk = Checker::new(self.id, ctx, rep);
        let c = &mut chk;
        // `None` means "stopped"; nothing else to do either way.
        let _ = match self.id {
            TestId::OwnAddress => simple::own_address(region, c),
            TestId::WalkingOnes => simple::walking(region, c, false),
            TestId::WalkingZeros => simple::walking(region, c, true),
            TestId::SolidBits => simple::solid_bits(region, c),
            TestId::MovingInversions => simple::moving_inversions(region, c),
            TestId::MovingInversionsRandom => simple::moving_inversions_random(region, c, ctx),
            TestId::RandomPattern => simple::random_pattern(region, c, ctx),
            TestId::BlockMove => stress::block_move(region, c, ctx),
            TestId::Modulo20 => simple::modulo20(region, c, ctx),
            TestId::SimdStream => {
                simd::simd_stream(region, c, ctx, self.level.unwrap_or_else(Level::detect))
            }
            TestId::RowStress => stress::row_stress(region, c),
            TestId::BitFade => stress::bit_fade(region, c, ctx),
        };
    }
}
