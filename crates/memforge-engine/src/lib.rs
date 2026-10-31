//! memforge-engine: orchestrates a memory stability test run.
//!
//! PUBLIC API CONTRACT — depended upon by `memforge-cli` and `memforge-gui`.
//! Signatures must not change without coordination.

use memforge_core::{MemError, TestId};
use memforge_platform::{CpuInfo, DimmInfo, MemoryStatus, ThreadPriority, WheaEvent};
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime};

pub use crossbeam_channel::Receiver;
pub use memforge_core;
pub use memforge_platform;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
    #[error("allocation failed: {0}")]
    Allocation(#[from] memforge_platform::PlatformError),
    #[error("{0}")]
    Other(String),
}

/// How much memory to test.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum MemorySize {
    Bytes(u64),
    /// Percent (0..=100) of *currently available* physical memory.
    PercentAvailable(f32),
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StopConditions {
    /// Stop after this many full passes (all selected tests). None = unlimited.
    pub passes: Option<u64>,
    /// Stop after this wall-clock duration. None = unlimited.
    pub duration: Option<Duration>,
    /// Stop as soon as this many errors have been found. None = never.
    pub max_errors: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunConfig {
    pub memory: MemorySize,
    /// Number of worker threads; None = one per logical CPU.
    pub threads: Option<usize>,
    pub tests: Vec<TestId>,
    pub stop: StopConditions,
    pub large_pages: bool,
    pub lock_memory: bool,
    pub pin_threads: bool,
    pub priority: ThreadPriority,
    /// None = random seed chosen at start (recorded in report).
    pub seed: Option<u64>,
    pub bit_fade_delay: Duration,
    /// Self-test fault injection (see `memforge_core::TestContext`).
    pub inject_fault_one_in: Option<u64>,
    /// Poll the WHEA event log during the run.
    pub monitor_whea: bool,
    pub keep_awake: bool,
}

impl Default for RunConfig {
    fn default() -> Self {
        RunConfig {
            memory: MemorySize::PercentAvailable(90.0),
            threads: None,
            tests: memforge_core::default_test_suite(),
            stop: StopConditions {
                passes: Some(1),
                duration: None,
                max_errors: None,
            },
            large_pages: false,
            lock_memory: true,
            pin_threads: true,
            priority: ThreadPriority::AboveNormal,
            seed: None,
            bit_fade_delay: Duration::from_secs(300),
            inject_fault_one_in: None,
            monitor_whea: true,
            keep_awake: true,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SystemSnapshot {
    pub cpu: CpuInfo,
    pub memory: MemoryStatus,
    pub dimms: Vec<DimmInfo>,
    pub elevated: bool,
    pub os: String,
}

/// Gather system information (CPU, memory, DIMMs). Never fails; missing
/// pieces are left empty.
pub fn system_snapshot() -> SystemSnapshot {
    use memforge_platform as p;
    SystemSnapshot {
        cpu: p::cpu_info(),
        memory: p::memory_status(),
        dimms: p::dimm_info().unwrap_or_default(),
        elevated: p::is_elevated(),
        os: format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Allocating,
    Running,
    Stopping,
    Finished,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    /// At least one full pass completed, no errors, no memory WHEA events.
    Pass,
    /// Errors (or memory-related WHEA events) detected.
    Fail,
    /// Stopped before completing a full pass, with no errors.
    Incomplete,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TestStats {
    pub test: Option<TestId>,
    pub runs: u64,
    pub errors: u64,
    pub time: Duration,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThreadStatus {
    pub index: usize,
    /// Logical CPU index the worker is pinned to, if pinned.
    pub cpu: Option<usize>,
    pub current_test: Option<TestId>,
    pub bytes_processed: u64,
    pub errors: u64,
}

/// Cheap snapshot of a running test, for UIs.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Status {
    pub phase: Phase,
    pub elapsed: Duration,
    pub bytes_under_test: u64,
    pub large_pages: bool,
    pub locked: bool,
    /// Completed full passes.
    pub passes_completed: u64,
    /// The test currently running (all workers run the same test).
    pub current_test: Option<TestId>,
    /// Index of `current_test` within the configured test list.
    pub current_test_index: usize,
    /// Progress through the current pass, 0.0..=1.0.
    pub pass_progress: f64,
    pub bytes_processed: u64,
    /// Recent throughput (bytes/s), smoothed.
    pub throughput: f64,
    pub errors: u64,
    /// errors_by_bit[i] = number of errors where bit i flipped.
    pub errors_by_bit: Vec<u64>,
    pub per_test: Vec<TestStats>,
    pub threads: Vec<ThreadStatus>,
    pub whea_events: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Event {
    Log {
        level: LogLevel,
        message: String,
    },
    /// Allocation finished; testing begins.
    Started {
        bytes: u64,
        threads: usize,
        large_pages: bool,
        locked: bool,
        seed: u64,
    },
    TestStarted {
        pass: u64,
        test: TestId,
    },
    PassCompleted {
        pass: u64,
        errors_so_far: u64,
    },
    /// Individual errors (rate-limited; the counters in `Status` are exact).
    MemoryError(MemError),
    Whea(WheaEvent),
    Finished(Box<RunReport>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunReport {
    pub tool_version: String,
    pub config: RunConfig,
    pub system: SystemSnapshot,
    pub seed: u64,
    pub started_at: SystemTime,
    pub duration: Duration,
    pub bytes_tested: u64,
    pub threads: usize,
    pub large_pages: bool,
    pub locked: bool,
    pub passes_completed: u64,
    pub bytes_processed: u64,
    pub errors_total: u64,
    pub errors_by_bit: Vec<u64>,
    /// Bounded sample of errors (first N).
    pub error_sample: Vec<MemError>,
    pub per_test: Vec<TestStats>,
    pub whea_events: Vec<WheaEvent>,
    pub verdict: Verdict,
    /// Why the run ended, e.g. "passes reached", "user stop", "max errors".
    ///
    /// Convention: setup failures (allocation failed, too little memory, region
    /// too small, worker spawn failure) start with `"setup failed: "` and have
    /// `bytes_tested == 0`; front-ends may map these to a setup error.
    pub stop_reason: String,
}

impl RunReport {
    pub fn to_json_pretty(&self) -> String {
        serde_json::to_string_pretty(self).expect("report serializes")
    }
}

/// A running test. Dropping the handle requests stop and joins.
pub struct RunHandle {
    inner: run::Inner,
}

impl RunHandle {
    /// Request a cooperative stop. Returns immediately.
    pub fn stop(&self) {
        self.inner.stop()
    }
    pub fn status(&self) -> Status {
        self.inner.status()
    }
    /// Event stream. Cloneable receiver; events are delivered once total
    /// across all clones.
    pub fn events(&self) -> Receiver<Event> {
        self.inner.events()
    }
    pub fn is_finished(&self) -> bool {
        self.inner.is_finished()
    }
    /// Block until the run ends and return the report.
    pub fn wait(self) -> RunReport {
        self.inner.wait()
    }
}

/// Validate the config, allocate memory (on a background thread) and start
/// testing. Returns immediately; allocation progress shows as
/// `Phase::Allocating`.
pub fn start(config: RunConfig) -> Result<RunHandle, EngineError> {
    Ok(RunHandle {
        inner: run::Inner::start(config)?,
    })
}

mod run;
