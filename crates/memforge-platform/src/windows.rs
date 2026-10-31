//! Windows implementation (windows-sys).

use crate::{
    cpuid, smbios, timefmt, AllocOptions, CpuInfo, DimmInfo, LogicalCpu, MemoryStatus,
    PlatformError, Result, TestRegion, ThreadPriority, WheaEvent,
};
use std::collections::BTreeMap;
use std::ptr::{null, null_mut, NonNull};
use std::sync::Mutex;
use std::time::SystemTime;
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_INSUFFICIENT_BUFFER, ERROR_NOT_ALL_ASSIGNED, HANDLE, LUID,
};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, GetTokenInformation, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES,
    SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_ELEVATION, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows_sys::Win32::System::EventLog::{
    EvtClose, EvtFormatMessage, EvtNext, EvtOpenPublisherMetadata, EvtQuery, EvtRender, EVT_HANDLE,
};
use windows_sys::Win32::System::Memory::{
    GetLargePageMinimum, SetProcessWorkingSetSizeEx, VirtualAlloc, VirtualFree, VirtualLock,
    VirtualUnlock, MEM_COMMIT, MEM_LARGE_PAGES, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
};
use windows_sys::Win32::System::Power::{
    SetThreadExecutionState, ES_AWAYMODE_REQUIRED, ES_CONTINUOUS, ES_SYSTEM_REQUIRED,
};
use windows_sys::Win32::System::SystemInformation::{
    GetLogicalProcessorInformationEx, GetSystemFirmwareTable, GetSystemInfo, GlobalMemoryStatusEx,
    GROUP_AFFINITY, MEMORYSTATUSEX, SYSTEM_INFO, SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, GetProcessWorkingSetSize, OpenProcessToken,
    SetThreadGroupAffinity, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
    THREAD_PRIORITY_HIGHEST, THREAD_PRIORITY_NORMAL,
};

fn os_err(op: &'static str) -> PlatformError {
    // SAFETY: GetLastError has no preconditions.
    let code = unsafe { GetLastError() };
    os_err_code(op, code)
}

fn os_err_code(op: &'static str, code: u32) -> PlatformError {
    PlatformError::Os {
        op,
        code,
        message: std::io::Error::from_raw_os_error(code as i32).to_string(),
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// RAII wrapper closing a kernel HANDLE.
struct OwnedHandle(HANDLE);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: handle was returned by a successful open call and is closed once.
        unsafe { CloseHandle(self.0) };
    }
}

fn open_process_token(access: u32) -> Result<OwnedHandle> {
    let mut h: HANDLE = null_mut();
    // SAFETY: GetCurrentProcess returns a pseudo-handle; h is a valid out pointer.
    if unsafe { OpenProcessToken(GetCurrentProcess(), access, &mut h) } == 0 {
        return Err(os_err("OpenProcessToken"));
    }
    Ok(OwnedHandle(h))
}

// ---------------------------------------------------------------- memory

fn system_info() -> SYSTEM_INFO {
    // SAFETY: SYSTEM_INFO is plain data; GetSystemInfo fills it.
    unsafe {
        let mut si: SYSTEM_INFO = std::mem::zeroed();
        GetSystemInfo(&mut si);
        si
    }
}

fn large_page_min() -> Option<usize> {
    // SAFETY: no preconditions.
    let m = unsafe { GetLargePageMinimum() };
    (m != 0).then_some(m)
}

pub fn memory_status() -> MemoryStatus {
    // SAFETY: MEMORYSTATUSEX is plain data; dwLength set as required.
    let ms = unsafe {
        let mut ms: MEMORYSTATUSEX = std::mem::zeroed();
        ms.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        GlobalMemoryStatusEx(&mut ms);
        ms
    };
    MemoryStatus {
        total_physical: ms.ullTotalPhys,
        available_physical: ms.ullAvailPhys,
        page_size: system_info().dwPageSize as usize,
        large_page_size: large_page_min(),
    }
}

/// Extra working-set headroom beyond the region itself.
const WS_MARGIN: usize = 64 << 20;

/// Process-wide working-set bookkeeping so concurrent / successive regions
/// compute limits from a consistent baseline and the original limits are
/// restored once the last locked region is freed.
struct WsState {
    /// Original (pre-raise) min/max working set; valid while `active > 0`.
    baseline: (usize, usize),
    active: usize,
    total_bytes: usize,
}

static WS_STATE: Mutex<WsState> = Mutex::new(WsState {
    baseline: (0, 0),
    active: 0,
    total_bytes: 0,
});

fn ws_state() -> std::sync::MutexGuard<'static, WsState> {
    WS_STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Apply working-set limits for the given state (restores baseline when idle).
fn ws_apply(st: &WsState) -> bool {
    let (bmin, bmax) = st.baseline;
    let (min, max) = if st.active == 0 {
        (bmin, bmax)
    } else {
        let new_min = bmin
            .saturating_add(st.total_bytes)
            .saturating_add(WS_MARGIN);
        (new_min, bmax.max(new_min).saturating_add(WS_MARGIN))
    };
    // SAFETY: plain call with valid handle; flags 0 = soft limits.
    unsafe { SetProcessWorkingSetSizeEx(GetCurrentProcess(), min, max, 0) != 0 }
}

fn try_lock(base: *mut u8, bytes: usize) -> bool {
    let mut st = ws_state();
    if st.active == 0 {
        let (mut min, mut max) = (0usize, 0usize);
        // SAFETY: valid pseudo-handle and out pointers.
        if unsafe { GetProcessWorkingSetSize(GetCurrentProcess(), &mut min, &mut max) } == 0 {
            return false;
        }
        st.baseline = (min, max);
    }
    st.active += 1;
    st.total_bytes = st.total_bytes.saturating_add(bytes);
    // SAFETY: [base, base+bytes) is a committed region owned by the caller.
    if ws_apply(&st) && unsafe { VirtualLock(base as *const _, bytes) != 0 } {
        return true;
    }
    // Roll back our contribution.
    st.active -= 1;
    st.total_bytes = st.total_bytes.saturating_sub(bytes);
    ws_apply(&st);
    false
}

/// Undo the working-set raise made by a successful `try_lock`.
fn unlock(base: *mut core::ffi::c_void, bytes: usize) {
    let mut st = ws_state();
    // SAFETY: region was locked by try_lock and is still committed.
    unsafe { VirtualUnlock(base, bytes) };
    st.active = st.active.saturating_sub(1);
    st.total_bytes = st.total_bytes.saturating_sub(bytes);
    ws_apply(&st);
}

pub fn allocate(bytes: usize, opts: &AllocOptions) -> Result<TestRegion> {
    if bytes == 0 {
        return Err(PlatformError::InvalidArgument(
            "allocation size is 0".into(),
        ));
    }
    let page = system_info().dwPageSize as usize;
    let mut ptr: *mut u8 = null_mut();
    let mut size = 0usize;
    let mut large = false;

    if opts.large_pages {
        if let Some(lp) = large_page_min() {
            let sz = bytes / lp * lp;
            if sz > 0 && matches!(crate::enable_privilege("SeLockMemoryPrivilege"), Ok(true)) {
                // SAFETY: fresh reservation at an OS-chosen address.
                let p = unsafe {
                    VirtualAlloc(
                        null(),
                        sz,
                        MEM_RESERVE | MEM_COMMIT | MEM_LARGE_PAGES,
                        PAGE_READWRITE,
                    )
                };
                if !p.is_null() {
                    ptr = p as *mut u8;
                    size = sz;
                    large = true;
                }
            }
        }
    }
    if ptr.is_null() {
        let sz = bytes / page * page;
        if sz == 0 {
            return Err(PlatformError::InvalidArgument(format!(
                "{bytes} bytes is less than one page ({page})"
            )));
        }
        // SAFETY: fresh reservation at an OS-chosen address.
        let p = unsafe { VirtualAlloc(null(), sz, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) };
        if p.is_null() {
            return Err(os_err("VirtualAlloc"));
        }
        ptr = p as *mut u8;
        size = sz;
    }

    // Large-page memory is non-pageable, so it counts as locked.
    let locked = if large {
        true
    } else if opts.lock {
        try_lock(ptr, size)
    } else {
        false
    };

    // Pre-fault: write one word per page.
    let step = page.max(8);
    let mut off = 0;
    while off < size {
        // SAFETY: off < size, within the committed, writable region.
        unsafe { (ptr.add(off) as *mut u64).write_volatile(0) };
        off += step;
    }

    Ok(TestRegion {
        // SAFETY: checked non-null above.
        ptr: unsafe { NonNull::new_unchecked(ptr as *mut u64) },
        len_words: size / 8,
        large_pages: large,
        locked,
        alloc_bytes: size,
    })
}

pub fn free(r: &mut TestRegion) {
    let base = r.ptr.as_ptr() as *mut core::ffi::c_void;
    // SAFETY: base/alloc_bytes describe the region allocated by VirtualAlloc; freed exactly once in Drop.
    unsafe {
        if r.locked && !r.large_pages {
            unlock(base, r.alloc_bytes);
        }
        VirtualFree(base, 0, MEM_RELEASE);
    }
}

// ---------------------------------------------------------------- CPU

fn group_affinities(ga: *const GROUP_AFFINITY, count: usize) -> Vec<(u16, u8)> {
    let mut v = Vec::new();
    for i in 0..count {
        // SAFETY: caller guarantees `count` GROUP_AFFINITY entries live at `ga`
        // (inside the buffer returned by the OS; bounds checked by the caller).
        let g = unsafe { ga.add(i).read_unaligned() };
        for bit in 0..usize::BITS as u8 {
            if g.Mask & (1usize << bit) != 0 {
                v.push((g.Group, bit));
            }
        }
    }
    v
}

pub fn cpu_info() -> CpuInfo {
    let brand = cpuid::brand();
    let features = cpuid::features();

    let mut len: u32 = 0;
    // SAFETY: size query with null buffer.
    unsafe { GetLogicalProcessorInformationEx(0xFFFF, null_mut(), &mut len) };
    let mut buf = vec![0u8; len as usize];
    let ok = len > 0
        // SAFETY: buf has `len` writable bytes.
        && unsafe { GetLogicalProcessorInformationEx(0xFFFF, buf.as_mut_ptr() as *mut _, &mut len) } != 0;

    // (group, number) -> (core id, efficiency class)
    let mut cores: BTreeMap<(u16, u8), (u32, u8)> = BTreeMap::new();
    // (level, group masks)
    let mut caches: Vec<(u8, Vec<(u16, u8)>)> = Vec::new();
    let mut n_cores = 0u32;

    if ok {
        let hdr = 8usize; // Relationship + Size
        let mut off = 0usize;
        while off + hdr <= len as usize {
            let rel = u32::from_ne_bytes(buf[off..off + 4].try_into().unwrap());
            let size = u32::from_ne_bytes(buf[off + 4..off + 8].try_into().unwrap()) as usize;
            if size < hdr || off + size > len as usize {
                break;
            }
            // Copy at most size_of bytes into a zeroed struct so we never read past the record.
            // SAFETY: plain-data struct; the copy is bounded by both the record and the struct size.
            let rec: SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX = unsafe {
                let mut r: SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX = std::mem::zeroed();
                let n = size.min(std::mem::size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>());
                std::ptr::copy_nonoverlapping(
                    buf.as_ptr().add(off),
                    &mut r as *mut _ as *mut u8,
                    n,
                );
                r
            };
            let base = off;
            off += size;
            // Offset of GroupMask arrays within the record; keep everything inside [base, base+size).
            let in_rec = |p: usize, count: usize| {
                p + count * std::mem::size_of::<GROUP_AFFINITY>() <= base + size
            };
            match rel {
                0 => {
                    // RelationProcessorCore
                    // SAFETY: union variant selected by Relationship.
                    let pr = unsafe { rec.Anonymous.Processor };
                    let gm_off = base
                        + std::mem::offset_of!(SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX, Anonymous)
                        + std::mem::offset_of!(
                            windows_sys::Win32::System::SystemInformation::PROCESSOR_RELATIONSHIP,
                            GroupMask
                        );
                    let n = pr.GroupCount as usize;
                    if !in_rec(gm_off, n) {
                        continue;
                    }
                    // SAFETY: bounds checked above.
                    let list = group_affinities(
                        unsafe { buf.as_ptr().add(gm_off) } as *const GROUP_AFFINITY,
                        n,
                    );
                    for k in list {
                        cores.insert(k, (n_cores, pr.EfficiencyClass));
                    }
                    n_cores += 1;
                }
                2 => {
                    // RelationCache
                    // SAFETY: union variant selected by Relationship.
                    let c = unsafe { rec.Anonymous.Cache };
                    let gm_off = base
                        + std::mem::offset_of!(SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX, Anonymous)
                        + std::mem::offset_of!(
                            windows_sys::Win32::System::SystemInformation::CACHE_RELATIONSHIP,
                            Anonymous
                        );
                    let n = (c.GroupCount as usize).max(1);
                    if !in_rec(gm_off, n) {
                        continue;
                    }
                    // SAFETY: bounds checked above.
                    let list = group_affinities(
                        unsafe { buf.as_ptr().add(gm_off) } as *const GROUP_AFFINITY,
                        n,
                    );
                    caches.push((c.Level, list));
                }
                _ => {}
            }
        }
    }

    // Last-level = highest cache level reported.
    let llc_level = caches.iter().map(|c| c.0).max().unwrap_or(0);
    let mut llc_of: BTreeMap<(u16, u8), u32> = BTreeMap::new();
    let mut next_llc = 0u32;
    for (lvl, list) in &caches {
        if *lvl == llc_level {
            for k in list {
                llc_of.insert(*k, next_llc);
            }
            next_llc += 1;
        }
    }

    let logical: Vec<LogicalCpu> = if cores.is_empty() {
        // Fallback: topology query failed.
        let n = std::thread::available_parallelism().map_or(1, |n| n.get());
        (0..n)
            .map(|i| LogicalCpu {
                index: i,
                group: (i / 64) as u16,
                number: (i % 64) as u8,
                core: i as u32,
                llc: 0,
                efficiency_class: 0,
            })
            .collect()
    } else {
        cores
            .iter()
            .enumerate()
            .map(|(i, (&(group, number), &(core, eff)))| LogicalCpu {
                index: i,
                group,
                number,
                core,
                llc: llc_of.get(&(group, number)).copied().unwrap_or(0),
                efficiency_class: eff,
            })
            .collect()
    };
    let physical_cores = if n_cores > 0 {
        n_cores as usize
    } else {
        logical.len()
    };
    CpuInfo {
        brand,
        logical,
        physical_cores,
        features,
    }
}

pub fn pin_current_thread(cpu: &LogicalCpu) -> Result<()> {
    if cpu.number >= 64 {
        return Err(PlatformError::InvalidArgument(format!(
            "processor number {} out of range",
            cpu.number
        )));
    }
    let ga = GROUP_AFFINITY {
        Mask: 1usize << cpu.number,
        Group: cpu.group,
        Reserved: [0; 3],
    };
    // SAFETY: valid pseudo-handle and affinity struct; previous-affinity out ptr is optional.
    if unsafe { SetThreadGroupAffinity(GetCurrentThread(), &ga, null_mut()) } == 0 {
        return Err(os_err("SetThreadGroupAffinity"));
    }
    Ok(())
}

pub fn set_current_thread_priority(p: ThreadPriority) -> Result<()> {
    let v = match p {
        ThreadPriority::Normal => THREAD_PRIORITY_NORMAL,
        ThreadPriority::AboveNormal => THREAD_PRIORITY_ABOVE_NORMAL,
        ThreadPriority::Highest => THREAD_PRIORITY_HIGHEST,
    };
    // SAFETY: valid pseudo-handle.
    if unsafe { SetThreadPriority(GetCurrentThread(), v) } == 0 {
        return Err(os_err("SetThreadPriority"));
    }
    Ok(())
}

// ---------------------------------------------------------------- process

pub fn is_elevated() -> bool {
    let Ok(tok) = open_process_token(TOKEN_QUERY) else {
        return false;
    };
    let mut el = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut ret = 0u32;
    // SAFETY: out buffer is a TOKEN_ELEVATION of the stated size; 20 = TokenElevation.
    let ok = unsafe {
        GetTokenInformation(
            tok.0,
            20,
            &mut el as *mut _ as *mut _,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret,
        )
    };
    ok != 0 && el.TokenIsElevated != 0
}

pub fn enable_privilege(name: &str) -> Result<bool> {
    let tok = open_process_token(TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY)?;
    let wname = wide(name);
    let mut luid = LUID {
        LowPart: 0,
        HighPart: 0,
    };
    // SAFETY: wname is NUL-terminated; luid is a valid out pointer.
    if unsafe { LookupPrivilegeValueW(null(), wname.as_ptr(), &mut luid) } == 0 {
        return Err(os_err("LookupPrivilegeValueW"));
    }
    let tp = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES {
            Luid: luid,
            Attributes: SE_PRIVILEGE_ENABLED,
        }],
    };
    // SAFETY: tok has TOKEN_ADJUST_PRIVILEGES; tp is a valid one-entry TOKEN_PRIVILEGES.
    if unsafe { AdjustTokenPrivileges(tok.0, 0, &tp, 0, null_mut(), null_mut()) } == 0 {
        return Err(os_err("AdjustTokenPrivileges"));
    }
    // SAFETY: no preconditions; must be read immediately after AdjustTokenPrivileges.
    Ok(unsafe { GetLastError() } != ERROR_NOT_ALL_ASSIGNED)
}

/// Note: execution state is per-thread; call from a long-lived thread and
/// clear from the same thread.
pub fn keep_system_awake(enable: bool) {
    let flags = if enable {
        ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_AWAYMODE_REQUIRED
    } else {
        ES_CONTINUOUS
    };
    // SAFETY: no preconditions.
    unsafe { SetThreadExecutionState(flags) };
}

// ---------------------------------------------------------------- SMBIOS

pub fn dimm_info() -> Result<Vec<DimmInfo>> {
    const RSMB: u32 = u32::from_be_bytes(*b"RSMB");
    for _ in 0..4 {
        // SAFETY: size query with null buffer.
        let n = unsafe { GetSystemFirmwareTable(RSMB, 0, null_mut(), 0) };
        if n == 0 {
            return Err(os_err("GetSystemFirmwareTable"));
        }
        let mut buf = vec![0u8; n as usize];
        // SAFETY: buf has n writable bytes.
        let got = unsafe { GetSystemFirmwareTable(RSMB, 0, buf.as_mut_ptr() as *mut _, n) };
        if got == 0 {
            return Err(os_err("GetSystemFirmwareTable"));
        }
        if got > n {
            continue; // table grew between the size query and the fetch; retry
        }
        buf.truncate(got as usize);
        return Ok(smbios::parse_raw_smbios(&buf));
    }
    Err(PlatformError::InvalidArgument(
        "SMBIOS table size kept changing".into(),
    ))
}

// ---------------------------------------------------------------- WHEA

struct EvtH(EVT_HANDLE);
impl Drop for EvtH {
    fn drop(&mut self) {
        if self.0 != 0 {
            // SAFETY: handle came from an Evt* call and is closed once.
            unsafe { EvtClose(self.0) };
        }
    }
}

/// Grow-and-retry helper for UTF-16 returning Evt calls.
fn evt_wide_call(
    op: &'static str,
    mut f: impl FnMut(u32, *mut u16, &mut u32) -> bool,
) -> Result<String> {
    let mut buf: Vec<u16> = vec![0; 4096];
    loop {
        let mut used = 0u32;
        if f(buf.len() as u32, buf.as_mut_ptr(), &mut used) {
            let n = (used as usize).min(buf.len());
            let s = String::from_utf16_lossy(&buf[..n]);
            return Ok(s.trim_end_matches('\0').to_string());
        }
        // SAFETY: no preconditions.
        let e = unsafe { GetLastError() };
        if e == ERROR_INSUFFICIENT_BUFFER && used as usize > buf.len() {
            buf.resize(used as usize, 0);
        } else {
            return Err(os_err_code(op, e));
        }
    }
}

pub fn whea_events_since(since: SystemTime) -> Result<Vec<WheaEvent>> {
    const NO_MORE_ITEMS: u32 = 259;
    // EvtQueryChannelPath | EvtQueryForwardDirection
    const QUERY_FLAGS: u32 = 0x1 | 0x100;
    let xpath = format!(
        "*[System[Provider[@Name='Microsoft-Windows-WHEA-Logger'] and TimeCreated[@SystemTime>='{}']]]",
        timefmt::format_iso8601(since)
    );
    let (wchan, wxpath) = (wide("System"), wide(&xpath));
    // SAFETY: both strings are NUL-terminated UTF-16.
    let q = EvtH(unsafe { EvtQuery(0, wchan.as_ptr(), wxpath.as_ptr(), QUERY_FLAGS) });
    if q.0 == 0 {
        return Err(os_err("EvtQuery"));
    }
    let wpub = wide("Microsoft-Windows-WHEA-Logger");
    // Prefer en-US (0x0409) so rendered messages are English regardless of the OS
    // locale; fall back to the default locale. Classification does not depend on
    // the text (see `is_memory_related`), this only keeps messages readable/stable.
    // SAFETY: wpub is NUL-terminated; a failure yields 0 which we handle (raw fallback).
    let mut meta = EvtH(unsafe { EvtOpenPublisherMetadata(0, wpub.as_ptr(), null(), 0x0409, 0) });
    if meta.0 == 0 {
        // SAFETY: as above.
        meta = EvtH(unsafe { EvtOpenPublisherMetadata(0, wpub.as_ptr(), null(), 0, 0) });
    }

    let mut out = Vec::new();
    loop {
        let mut handles = [0 as EVT_HANDLE; 16];
        let mut returned = 0u32;
        // SAFETY: handles has 16 slots; q is a live query handle.
        let ok = unsafe { EvtNext(q.0, 16, handles.as_mut_ptr(), 2000, 0, &mut returned) };
        if ok == 0 {
            // SAFETY: no preconditions.
            let e = unsafe { GetLastError() };
            if e == NO_MORE_ITEMS {
                break;
            }
            return Err(os_err_code("EvtNext", e));
        }
        for &h in &handles[..returned as usize] {
            let ev = EvtH(h);
            if let Some(w) = read_event(&ev, &meta) {
                out.push(w);
            }
        }
    }
    Ok(out)
}

fn read_event(ev: &EvtH, meta: &EvtH) -> Option<WheaEvent> {
    // EvtRenderEventXml = 1
    let xml = evt_wide_call("EvtRender", |size, buf, used| {
        let mut props = 0u32;
        let mut bytes = 0u32;
        // SAFETY: buf has `size` u16s => size*2 bytes; ev is a live event handle.
        let ok =
            unsafe { EvtRender(0, ev.0, 1, size * 2, buf as *mut _, &mut bytes, &mut props) } != 0;
        *used = bytes / 2;
        ok
    });
    let Ok(xml) = xml else {
        // Keep the event visible even though it could not be rendered.
        return Some(WheaEvent {
            time: SystemTime::UNIX_EPOCH,
            event_id: 0,
            level: level_name(0).to_string(),
            message: "unrenderable WHEA event".to_string(),
            memory_related: false,
        });
    };

    let event_id = tag_text(&xml, "EventID")
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    let level_num: u32 = tag_text(&xml, "Level")
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    let time = attr_value(&xml, "TimeCreated", "SystemTime")
        .and_then(|s| timefmt::parse_iso8601(&s))
        .unwrap_or(SystemTime::UNIX_EPOCH);

    // EvtFormatMessageEvent = 1
    let formatted = if meta.0 != 0 {
        evt_wide_call("EvtFormatMessage", |size, buf, used| {
            // SAFETY: meta/ev live handles; buf has `size` u16s.
            unsafe { EvtFormatMessage(meta.0, ev.0, 0, 0, null(), 1, size, buf, used) != 0 }
        })
        .ok()
        .filter(|s| !s.trim().is_empty())
    } else {
        None
    };
    let message = formatted.unwrap_or_else(|| event_data_summary(&xml));
    let memory_related = is_memory_related(event_id, &message, &xml);
    Some(WheaEvent {
        time,
        event_id,
        level: level_name(level_num).to_string(),
        message,
        memory_related,
    })
}

fn level_name(l: u32) -> &'static str {
    match l {
        1 => "Critical",
        2 => "Error",
        3 => "Warning",
        4 => "Information",
        5 => "Verbose",
        _ => "Unknown",
    }
}

/// Classify a WHEA-Logger event as memory-related.
///
/// Primarily structural (locale independent), using the event XML:
/// * Event 47 is the corrected *memory* error event.
/// * Events 19 (corrected hardware error), 18 (fatal), 20, 46 etc. carry
///   `ErrorSource`/`ErrorType`/`MCABank`/component data; they count when the
///   error type/component is Memory, Cache Hierarchy or Bus/Interconnect, or
///   when MCA memory-transaction fields (`MemorIO`, `MemHierarchyLvl`) are present.
/// * English text in the message/XML is kept as an extra signal.
///
/// Cache-hierarchy and bus/interconnect errors are deliberately counted: with a
/// memory overclock they indicate memory-subsystem instability (on AMD the
/// Infinity Fabric FCLK/UCLK/MCLK ratio is the classic source of event-19
/// "Bus/Interconnect" errors), even though no DIMM is named.
pub(crate) fn is_memory_related(event_id: u32, message: &str, xml: &str) -> bool {
    if event_id == 47 {
        return true;
    }
    for (name, value) in event_data_fields(xml) {
        let n = name.to_ascii_lowercase();
        if n == "memorio" || n == "memhierarchylvl" || n == "memoryio" {
            return true;
        }
        if matches!(
            n.as_str(),
            "errortype" | "errorsource" | "component" | "componentname" | "errortypestring"
        ) {
            let v = value.to_ascii_lowercase();
            if KEYWORDS.iter().any(|k| v.contains(k)) {
                return true;
            }
        }
    }
    let hay = format!(
        "{message}
{xml}"
    )
    .to_ascii_lowercase();
    KEYWORDS.iter().any(|k| hay.contains(k))
}

const KEYWORDS: [&str; 5] = [
    "memory",
    "dimm",
    "cache hierarchy",
    "bus/interconnect",
    "interconnect",
];

/// `(Name, value)` pairs of `<Data Name='x'>v</Data>` elements.
pub(crate) fn event_data_fields(xml: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = xml[from..].find("<Data") {
        let s = from + i;
        let Some(gt) = xml[s..].find('>').map(|g| g + s) else {
            break;
        };
        let name = attr_value(&xml[s..], "Data", "Name").unwrap_or_default();
        let (val, next) = if xml[..gt].ends_with('/') {
            (String::new(), gt + 1)
        } else if let Some(end) = xml[gt..].find("</Data>").map(|e| e + gt) {
            (xml[gt + 1..end].to_string(), end + 7)
        } else {
            break;
        };
        out.push((name, val));
        from = next;
    }
    out
}

/// Text between `<tag ...>` and `</tag>`.
pub(crate) fn tag_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let mut from = 0;
    while let Some(i) = xml[from..].find(&open) {
        let start = from + i + open.len();
        match xml[start..].chars().next() {
            Some('>') | Some(' ') | Some('/') => {
                let gt = xml[start..].find('>')? + start;
                if xml[..gt].ends_with('/') {
                    return Some(String::new());
                }
                let end = xml[gt + 1..].find(&format!("</{tag}>"))? + gt + 1;
                return Some(xml[gt + 1..end].to_string());
            }
            _ => from = start,
        }
    }
    None
}

/// Value of `attr='...'` or `attr="..."` within the first `<tag ...>`.
pub(crate) fn attr_value(xml: &str, tag: &str, attr: &str) -> Option<String> {
    let open = format!("<{tag} ");
    let s = xml.find(&open)? + open.len();
    let e = xml[s..].find('>')? + s;
    let elem = &xml[s..e];
    let key = format!("{attr}=");
    let k = elem.find(&key)? + key.len();
    let q = elem[k..].chars().next()?;
    if q != '\'' && q != '"' {
        return None;
    }
    let rest = &elem[k + 1..];
    Some(rest[..rest.find(q)?].to_string())
}

/// Best-effort summary of `<Data Name='x'>v</Data>` elements.
pub(crate) fn event_data_summary(xml: &str) -> String {
    let parts: Vec<String> = event_data_fields(xml)
        .into_iter()
        .map(|(name, val)| {
            if name.is_empty() {
                val
            } else {
                format!("{name}={val}")
            }
        })
        .collect();
    if parts.is_empty() {
        "(no event data)".to_string()
    } else {
        parts.join("; ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_helpers() {
        let xml =
            "<Event><System><Provider Name='Microsoft-Windows-WHEA-Logger'/><EventID>47</EventID>\
<Level>3</Level><TimeCreated SystemTime='2026-10-31T01:02:03.4567890Z'/></System>\
<EventData><Data Name='ErrorSource'>3</Data><Data Name='X'/></EventData></Event>";
        assert_eq!(tag_text(xml, "EventID").as_deref(), Some("47"));
        assert_eq!(tag_text(xml, "Level").as_deref(), Some("3"));
        assert_eq!(
            attr_value(xml, "TimeCreated", "SystemTime").as_deref(),
            Some("2026-10-31T01:02:03.4567890Z")
        );
        assert_eq!(event_data_summary(xml), "ErrorSource=3; X=");
        assert!(is_memory_related(47, "", xml));
        assert!(is_memory_related(19, "A corrected memory error", ""));
        assert!(!is_memory_related(19, "PCI Express Root Port", xml));
    }

    fn ev(id: u32, data: &str) -> String {
        format!(
            "<Event><System><Provider Name='Microsoft-Windows-WHEA-Logger'/><EventID>{id}</EventID></System><EventData>{data}</EventData></Event>"
        )
    }

    #[test]
    fn classify_event_47_always_memory() {
        assert!(is_memory_related(
            47,
            "",
            &ev(47, "<Data Name='X'>1</Data>")
        ));
    }

    #[test]
    fn classify_event_19_by_error_type_text() {
        let mem = ev(
            19,
            "<Data Name='ErrorSource'>1</Data><Data Name='ErrorType'>Memory</Data>",
        );
        assert!(is_memory_related(
            19,
            "localized message ohne keywords",
            &mem
        ));
        let cache = ev(19, "<Data Name='ErrorType'>Cache Hierarchy Error</Data>");
        assert!(is_memory_related(19, "", &cache));
        let bus = ev(19, "<Data Name='ErrorType'>Bus/Interconnect Error</Data>");
        assert!(is_memory_related(19, "", &bus));
    }

    #[test]
    fn classify_by_mca_memory_fields() {
        let x = ev(
            19,
            "<Data Name='MCABank'>0</Data><Data Name='MemorIO'>1</Data>",
        );
        assert!(is_memory_related(19, "", &x));
        let y = ev(18, "<Data Name='MemHierarchyLvl'>2</Data>");
        assert!(is_memory_related(18, "", &y));
    }

    #[test]
    fn classify_english_text_signal() {
        assert!(is_memory_related(1, "A corrected DIMM error", &ev(1, "")));
    }

    #[test]
    fn classify_non_memory_negative() {
        let x = ev(
            19,
            "<Data Name='ErrorSource'>1</Data><Data Name='ErrorType'>TLB Error</Data>",
        );
        assert!(!is_memory_related(
            19,
            "A corrected hardware error has occurred.",
            &x
        ));
        let pcie = ev(17, "<Data Name='ErrorSource'>PCI Express Root Port</Data>");
        assert!(!is_memory_related(17, "", &pcie));
    }
}

#[cfg(test)]
mod ws_tests {
    use super::*;

    fn ws() -> (usize, usize) {
        let (mut min, mut max) = (0usize, 0usize);
        // SAFETY: valid pseudo-handle and out pointers.
        unsafe { GetProcessWorkingSetSize(GetCurrentProcess(), &mut min, &mut max) };
        (min, max)
    }

    #[test]
    fn lock_twice_restores_working_set() {
        let opts = AllocOptions {
            large_pages: false,
            lock: true,
        };
        let gib = 1usize << 30;
        let before = ws();
        let r1 = allocate(gib, &opts).expect("alloc 1");
        assert!(r1.locked, "first lock failed");
        drop(r1);
        assert_eq!(ws(), before, "working set not restored after drop");
        let r2 = allocate(gib, &opts).expect("alloc 2");
        assert!(r2.locked, "second lock failed");
        drop(r2);
        assert_eq!(ws(), before);
    }
}
