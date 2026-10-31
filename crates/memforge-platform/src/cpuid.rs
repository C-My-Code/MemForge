//! CPU brand string and feature flags (shared by all targets).

use crate::CpuFeatures;

#[cfg(target_arch = "x86_64")]
#[allow(unused_unsafe)]
pub fn brand() -> String {
    use std::arch::x86_64::__cpuid;
    // SAFETY: CPUID is available on every x86_64 CPU.
    let max_ext = unsafe { __cpuid(0x8000_0000) }.eax;
    if max_ext < 0x8000_0004 {
        return "Unknown x86_64 CPU".to_string();
    }
    let mut bytes = Vec::with_capacity(48);
    for leaf in 0x8000_0002u32..=0x8000_0004 {
        // SAFETY: leaf checked against the maximum extended leaf above.
        let r = unsafe { __cpuid(leaf) };
        for v in [r.eax, r.ebx, r.ecx, r.edx] {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
    }
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

#[cfg(not(target_arch = "x86_64"))]
#[allow(unused_unsafe)]
pub fn brand() -> String {
    "Unknown CPU".to_string()
}

#[cfg(target_arch = "x86_64")]
pub fn features() -> CpuFeatures {
    CpuFeatures {
        sse2: is_x86_feature_detected!("sse2"),
        avx2: is_x86_feature_detected!("avx2"),
        avx512f: is_x86_feature_detected!("avx512f"),
    }
}

#[cfg(not(target_arch = "x86_64"))]
pub fn features() -> CpuFeatures {
    CpuFeatures::default()
}
