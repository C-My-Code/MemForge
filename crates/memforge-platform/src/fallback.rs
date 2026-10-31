//! Portable fallback for non-Windows targets (std only).

use crate::{
    cpuid, AllocOptions, CpuInfo, DimmInfo, LogicalCpu, MemoryStatus, PlatformError, Result,
    TestRegion, ThreadPriority, WheaEvent,
};
use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::ptr::NonNull;
use std::time::SystemTime;

const PAGE: usize = 4096;

pub fn memory_status() -> MemoryStatus {
    let mut total = 0u64;
    let mut avail = 0u64;
    if let Ok(s) = std::fs::read_to_string("/proc/meminfo") {
        for line in s.lines() {
            let kb = |l: &str| {
                l.split_whitespace()
                    .nth(1)
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0)
                    * 1024
            };
            if line.starts_with("MemTotal:") {
                total = kb(line);
            } else if line.starts_with("MemAvailable:") {
                avail = kb(line);
            }
        }
    }
    MemoryStatus {
        total_physical: total,
        available_physical: avail,
        page_size: PAGE,
        large_page_size: None,
    }
}

pub fn allocate(bytes: usize, _opts: &AllocOptions) -> Result<TestRegion> {
    if bytes == 0 {
        return Err(PlatformError::InvalidArgument(
            "allocation size is 0".into(),
        ));
    }
    let size = bytes / PAGE * PAGE;
    if size == 0 {
        return Err(PlatformError::InvalidArgument(format!(
            "{bytes} bytes is less than one page"
        )));
    }
    let layout = Layout::from_size_align(size, PAGE)
        .map_err(|e| PlatformError::InvalidArgument(e.to_string()))?;
    // SAFETY: layout has non-zero size.
    let p = unsafe { alloc_zeroed(layout) };
    let ptr = NonNull::new(p as *mut u64).ok_or(PlatformError::Os {
        op: "alloc",
        code: 0,
        message: "out of memory".into(),
    })?;
    Ok(TestRegion {
        ptr,
        len_words: size / 8,
        large_pages: false,
        locked: false,
        alloc_bytes: size,
    })
}

pub fn free(r: &mut TestRegion) {
    if let Ok(layout) = Layout::from_size_align(r.alloc_bytes, PAGE) {
        // SAFETY: allocated in `allocate` with this exact layout; freed once.
        unsafe { dealloc(r.ptr.as_ptr() as *mut u8, layout) };
    }
}

pub fn cpu_info() -> CpuInfo {
    let n = std::thread::available_parallelism().map_or(1, |n| n.get());
    let logical = (0..n)
        .map(|i| LogicalCpu {
            index: i,
            group: 0,
            number: (i % 64) as u8,
            core: i as u32,
            llc: 0,
            efficiency_class: 0,
        })
        .collect();
    CpuInfo {
        brand: cpuid::brand(),
        logical,
        physical_cores: n,
        features: cpuid::features(),
    }
}

pub fn pin_current_thread(_cpu: &LogicalCpu) -> Result<()> {
    Ok(())
}

pub fn set_current_thread_priority(_p: ThreadPriority) -> Result<()> {
    Ok(())
}

pub fn is_elevated() -> bool {
    false
}

pub fn enable_privilege(_name: &str) -> Result<bool> {
    Ok(false)
}

pub fn keep_system_awake(_enable: bool) {}

pub fn dimm_info() -> Result<Vec<DimmInfo>> {
    Ok(Vec::new())
}

pub fn whea_events_since(_since: SystemTime) -> Result<Vec<WheaEvent>> {
    Ok(Vec::new())
}
