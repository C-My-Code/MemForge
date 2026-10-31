//! memforge-core: platform-independent memory test algorithms.
//!
//! PUBLIC API CONTRACT — the items in this file are depended upon by
//! `memforge-engine`. Signatures must not change without coordination.
//! Implementations of individual algorithms live in `algorithms/`.

use serde::{Deserialize, Serialize};
use std::time::Duration;

pub mod algorithms;
pub mod rng;

/// The unit of memory access for all tests.
pub type Word = u64;

/// Identifies a test algorithm.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum TestId {
    OwnAddress,
    WalkingOnes,
    WalkingZeros,
    SolidBits,
    MovingInversions,
    MovingInversionsRandom,
    RandomPattern,
    BlockMove,
    Modulo20,
    SimdStream,
    RowStress,
    BitFade,
}

impl TestId {
    /// Every test, in recommended execution order.
    pub const ALL: &'static [TestId] = &[
        TestId::OwnAddress,
        TestId::WalkingOnes,
        TestId::WalkingZeros,
        TestId::SolidBits,
        TestId::MovingInversions,
        TestId::MovingInversionsRandom,
        TestId::RandomPattern,
        TestId::BlockMove,
        TestId::Modulo20,
        TestId::SimdStream,
        TestId::RowStress,
        TestId::BitFade,
    ];

    /// Stable kebab-case machine name (used by CLI `--tests` and JSON).
    pub fn name(self) -> &'static str {
        match self {
            TestId::OwnAddress => "own-address",
            TestId::WalkingOnes => "walking-ones",
            TestId::WalkingZeros => "walking-zeros",
            TestId::SolidBits => "solid-bits",
            TestId::MovingInversions => "moving-inversions",
            TestId::MovingInversionsRandom => "moving-inversions-random",
            TestId::RandomPattern => "random-pattern",
            TestId::BlockMove => "block-move",
            TestId::Modulo20 => "modulo-20",
            TestId::SimdStream => "simd-stream",
            TestId::RowStress => "row-stress",
            TestId::BitFade => "bit-fade",
        }
    }

    /// Human-readable title.
    pub fn title(self) -> &'static str {
        match self {
            TestId::OwnAddress => "Own address",
            TestId::WalkingOnes => "Walking ones",
            TestId::WalkingZeros => "Walking zeros",
            TestId::SolidBits => "Solid bits / checkerboard",
            TestId::MovingInversions => "Moving inversions",
            TestId::MovingInversionsRandom => "Moving inversions (random)",
            TestId::RandomPattern => "Random pattern",
            TestId::BlockMove => "Block move",
            TestId::Modulo20 => "Modulo 20",
            TestId::SimdStream => "SIMD non-temporal stream",
            TestId::RowStress => "Row stress",
            TestId::BitFade => "Bit fade (retention)",
        }
    }

    /// One-sentence description of what the test targets.
    pub fn description(self) -> &'static str {
        algorithms::description(self)
    }

    /// Parse from [`TestId::name`] (case-insensitive).
    pub fn from_name(s: &str) -> Option<TestId> {
        TestId::ALL
            .iter()
            .copied()
            .find(|t| t.name().eq_ignore_ascii_case(s.trim()))
    }
}

/// The default suite: everything except the slow `BitFade`.
pub fn default_test_suite() -> Vec<TestId> {
    TestId::ALL
        .iter()
        .copied()
        .filter(|t| *t != TestId::BitFade)
        .collect()
}

/// A single detected mismatch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemError {
    pub test: TestId,
    /// Worker thread index that detected the error.
    pub thread: usize,
    /// Pass number (0-based).
    pub pass: u64,
    /// Virtual address of the failing word.
    pub address: usize,
    pub expected: Word,
    pub actual: Word,
}

impl MemError {
    /// Bits that differ between expected and actual.
    pub fn xor_mask(&self) -> Word {
        self.expected ^ self.actual
    }
    /// Number of flipped bits.
    pub fn flipped_bits(&self) -> u32 {
        self.xor_mask().count_ones()
    }
}

/// Per-invocation parameters handed to a test.
#[derive(Clone, Debug)]
pub struct TestContext {
    /// Base seed for this run; tests derive their RNG from
    /// `rng::derive_seed(seed, pass, thread)`.
    pub seed: u64,
    pub pass: u64,
    pub thread: usize,
    pub thread_count: usize,
    /// Idle time for [`TestId::BitFade`].
    pub bit_fade_delay: Duration,
    /// Self-test: when `Some(n)`, the verification helper simulates a single
    /// random bit flip in roughly one out of every `n` verified words.
    /// Used to prove the detection/reporting pipeline works.
    pub inject_fault_one_in: Option<u64>,
}

impl Default for TestContext {
    fn default() -> Self {
        TestContext {
            seed: 0x4D45_4D46_4F52_4745, // "MEMFORGE"
            pass: 0,
            thread: 0,
            thread_count: 1,
            bit_fade_delay: Duration::from_secs(300),
            inject_fault_one_in: None,
        }
    }
}

/// Sink for results. Implemented by the engine; must be cheap and thread-safe
/// (each worker thread holds its own reporter, but implementations may share
/// state across threads).
pub trait Reporter: Sync {
    /// Report a mismatch. May be called at very high rates on bad hardware.
    fn error(&self, err: MemError);
    /// Report `bytes` of memory traffic completed (reads + writes counted
    /// as bytes of the region touched per sweep; see `algorithms` docs).
    fn progress(&self, bytes: u64);
    /// Polled between chunks; when true the test must return promptly.
    fn should_stop(&self) -> bool;
}

/// A memory test algorithm.
pub trait MemTest: Send + Sync {
    fn id(&self) -> TestId;
    /// Number of full sweeps over the region this test performs per run.
    /// Used by the engine to estimate progress (`progress` bytes / (len * sweeps)).
    fn sweeps(&self) -> u32;
    /// Write and verify `region` in place. Must report every mismatch via
    /// `rep.error`, call `rep.progress` periodically, and honor `rep.should_stop`.
    fn run(&self, region: &mut [Word], ctx: &TestContext, rep: &dyn Reporter);
}

/// Factory for all tests.
pub fn create_test(id: TestId) -> Box<dyn MemTest> {
    algorithms::create(id)
}
