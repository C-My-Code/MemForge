//! memforge-platform: OS abstraction for MemForge.
//!
//! PUBLIC API CONTRACT — depended upon by `memforge-engine`. Signatures must
//! not change without coordination. Windows implementation uses `windows-sys`;
//! on other targets a portable fallback (std::alloc, no-op affinity, empty
//! SMBIOS/WHEA) keeps the workspace buildable and testable.

use serde::{Deserialize, Serialize};
use std::ptr::NonNull;
use std::time::SystemTime;

mod cpuid;
pub mod smbios;
pub mod timefmt;

#[cfg(not(windows))]
mod fallback;
#[cfg(windows)]
mod windows;
#[cfg(not(windows))]
use fallback as imp;
#[cfg(windows)]
use windows as imp;

#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    #[error("{op} failed: OS error {code}: {message}")]
    Os {
        op: &'static str,
        code: u32,
        message: String,
    },
    #[error("{0}")]
    Unsupported(&'static str),
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
}

pub type Result<T> = std::result::Result<T, PlatformError>;

// ---------------------------------------------------------------- memory

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemoryStatus {
    pub total_physical: u64,
    pub available_physical: u64,
    pub page_size: usize,
    /// `GetLargePageMinimum()`; `None` if unsupported.
    pub large_page_size: Option<usize>,
}

pub fn memory_status() -> MemoryStatus {
    imp::memory_status()
}

#[derive(Clone, Debug, Default)]
pub struct AllocOptions {
    /// Try `MEM_LARGE_PAGES` (needs SeLockMemoryPrivilege); falls back to
    /// normal pages on failure.
    pub large_pages: bool,
    /// Try to grow the working set and `VirtualLock` the region; falls back
    /// to unlocked on failure.
    pub lock: bool,
}

/// An owned, page-aligned allocation of physical-backed memory under test.
/// Freed on drop. Contents are committed and touched (pre-faulted).
pub struct TestRegion {
    ptr: NonNull<u64>,
    len_words: usize,
    /// Whether large pages were actually obtained.
    pub large_pages: bool,
    /// Whether the region was successfully locked into RAM.
    pub locked: bool,
    /// Bytes of the underlying allocation (for release).
    alloc_bytes: usize,
}

// SAFETY: TestRegion uniquely owns its allocation.
unsafe impl Send for TestRegion {}
unsafe impl Sync for TestRegion {}

impl Drop for TestRegion {
    fn drop(&mut self) {
        imp::free(self);
    }
}

impl TestRegion {
    pub fn as_mut_slice(&mut self) -> &mut [u64] {
        // SAFETY: ptr/len describe a live allocation uniquely owned by self.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len_words) }
    }
    pub fn as_ptr(&self) -> *mut u64 {
        self.ptr.as_ptr()
    }
    pub fn len_words(&self) -> usize {
        self.len_words
    }
    pub fn len_bytes(&self) -> usize {
        self.len_words * 8
    }
}

/// Allocate `bytes` (rounded down to the page / large-page size) of memory
/// to test. Pre-faults every page.
pub fn allocate(bytes: usize, opts: &AllocOptions) -> Result<TestRegion> {
    imp::allocate(bytes, opts)
}

// ---------------------------------------------------------------- CPU

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CpuFeatures {
    pub sse2: bool,
    pub avx2: bool,
    pub avx512f: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogicalCpu {
    /// Dense index 0..n across all processor groups.
    pub index: usize,
    /// Windows processor group.
    pub group: u16,
    /// Processor number within the group (0..64).
    pub number: u8,
    /// Physical core id (SMT siblings share it).
    pub core: u32,
    /// Last-level cache domain id (e.g. AMD CCD / CCX).
    pub llc: u32,
    /// Higher = more performant (P-core vs E-core). 0 on homogeneous CPUs.
    pub efficiency_class: u8,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CpuInfo {
    pub brand: String,
    pub logical: Vec<LogicalCpu>,
    pub physical_cores: usize,
    pub features: CpuFeatures,
}

pub fn cpu_info() -> CpuInfo {
    imp::cpu_info()
}

/// Pin the calling thread to one logical CPU (uses group affinity).
pub fn pin_current_thread(cpu: &LogicalCpu) -> Result<()> {
    imp::pin_current_thread(cpu)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThreadPriority {
    Normal,
    #[default]
    AboveNormal,
    Highest,
}

pub fn set_current_thread_priority(p: ThreadPriority) -> Result<()> {
    imp::set_current_thread_priority(p)
}

// ---------------------------------------------------------------- process

/// Is the process running elevated (Administrator)?
pub fn is_elevated() -> bool {
    imp::is_elevated()
}

/// Enable a named privilege on the process token, e.g. "SeLockMemoryPrivilege".
/// Returns Ok(true) if enabled, Ok(false) if the account does not hold it.
pub fn enable_privilege(name: &str) -> Result<bool> {
    imp::enable_privilege(name)
}

/// Prevent (true) / allow (false) system sleep and display-off while testing.
/// NOTE: on Windows the execution state is per-thread; the engine must call
/// this from a long-lived thread and clear it from the same thread.
pub fn keep_system_awake(enable: bool) {
    imp::keep_system_awake(enable);
}

// ---------------------------------------------------------------- SMBIOS

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DimmInfo {
    pub locator: String,
    pub bank_locator: String,
    pub manufacturer: String,
    pub part_number: String,
    pub serial_number: String,
    pub size_bytes: u64,
    /// e.g. "DDR5"
    pub memory_type: String,
    /// Rated speed (MT/s).
    pub speed_mts: u32,
    /// Configured speed (MT/s), what it's actually running at.
    pub configured_speed_mts: u32,
    /// Configured voltage in millivolts (0 = unknown).
    pub configured_voltage_mv: u32,
    pub rank: u8,
}

/// Installed DIMMs from SMBIOS type 17 (empty slots omitted).
pub fn dimm_info() -> Result<Vec<DimmInfo>> {
    imp::dimm_info()
}

// ---------------------------------------------------------------- WHEA

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WheaEvent {
    pub time: SystemTime,
    pub event_id: u32,
    /// e.g. "Warning"/"Error".
    pub level: String,
    /// Rendered message or best-effort summary of the event XML.
    pub message: String,
    /// True if the event appears memory-related (e.g. "Memory" / "Cache
    /// Hierarchy" error source / event ids 47, 19 with memory component).
    pub memory_related: bool,
}

/// WHEA-Logger events in the System log with time >= `since`.
pub fn whea_events_since(since: SystemTime) -> Result<Vec<WheaEvent>> {
    imp::whea_events_since(since)
}
