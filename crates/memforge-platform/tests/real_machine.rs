//! Integration tests exercising the real OS. Run with `-- --nocapture` to see output.
#![cfg(windows)]

use memforge_platform::*;
use std::time::{Duration, SystemTime};

fn exercise(bytes: usize, opts: &AllocOptions) -> TestRegion {
    let mut r = allocate(bytes, opts).expect("allocate");
    println!(
        "alloc {} MiB opts={:?}: large_pages={} locked={} len_bytes={}",
        bytes >> 20,
        opts,
        r.large_pages,
        r.locked,
        r.len_bytes()
    );
    assert_eq!(r.as_ptr() as usize % 4096, 0, "page aligned");
    assert!(r.len_bytes() <= bytes && r.len_bytes() > 0);
    let s = r.as_mut_slice();
    for (i, w) in s.iter_mut().enumerate() {
        *w = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
    for (i, w) in s.iter().enumerate() {
        assert_eq!(*w, (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    }
    r
}

#[test]
fn memory_status_sane() {
    let m = memory_status();
    println!("{m:?}");
    assert!(m.total_physical > 0 && m.available_physical <= m.total_physical);
    assert!(m.page_size >= 4096);
}

#[test]
fn allocate_normal_and_locked() {
    assert!(matches!(
        allocate(0, &AllocOptions::default()),
        Err(PlatformError::InvalidArgument(_))
    ));
    drop(exercise(64 << 20, &AllocOptions::default()));
    let r = exercise(
        64 << 20,
        &AllocOptions {
            large_pages: false,
            lock: true,
        },
    );
    println!("64 MiB lock result: {}", r.locked);
}

#[test]
fn allocate_locked_large_sizes() {
    for gib in [1usize, 4] {
        let r = allocate(
            gib << 30,
            &AllocOptions {
                large_pages: false,
                lock: true,
            },
        );
        match r {
            Ok(r) => println!("{gib} GiB lock=true -> locked={}", r.locked),
            Err(e) => println!("{gib} GiB alloc failed: {e}"),
        }
    }
}

#[test]
fn allocate_large_pages_graceful() {
    let opts = AllocOptions {
        large_pages: true,
        lock: true,
    };
    let r = exercise(64 << 20, &opts);
    println!(
        "large_pages obtained: {} (elevated={})",
        r.large_pages,
        is_elevated()
    );
}

#[test]
fn cpu_info_sanity_and_pinning() {
    let c = cpu_info();
    println!("brand: {}", c.brand);
    println!(
        "logical={} physical={} features={:?}",
        c.logical.len(),
        c.physical_cores,
        c.features
    );
    let mut llcs: Vec<u32> = c.logical.iter().map(|l| l.llc).collect();
    llcs.sort();
    llcs.dedup();
    println!("llc domains: {llcs:?}; efficiency classes: {:?}", {
        let mut e: Vec<u8> = c.logical.iter().map(|l| l.efficiency_class).collect();
        e.sort();
        e.dedup();
        e
    });
    assert!(!c.brand.is_empty());
    assert!(!c.logical.is_empty());
    assert!(c.physical_cores >= 1 && c.physical_cores <= c.logical.len());
    assert!(c.features.sse2);
    for (i, l) in c.logical.iter().enumerate() {
        assert_eq!(l.index, i);
    }
    let cpus = c.logical.clone();
    // Pin on a scratch thread so the test harness thread keeps its affinity.
    std::thread::spawn(move || {
        for l in &cpus {
            pin_current_thread(l).unwrap_or_else(|e| panic!("pin {l:?}: {e}"));
        }
    })
    .join()
    .unwrap();
}

#[test]
fn priority_and_process() {
    std::thread::spawn(|| {
        for p in [
            ThreadPriority::AboveNormal,
            ThreadPriority::Highest,
            ThreadPriority::Normal,
        ] {
            set_current_thread_priority(p).unwrap();
        }
        keep_system_awake(true);
        keep_system_awake(false);
    })
    .join()
    .unwrap();
    println!("elevated: {}", is_elevated());
    println!(
        "SeLockMemoryPrivilege enabled: {:?}",
        enable_privilege("SeLockMemoryPrivilege")
    );
    assert!(enable_privilege("SeNoSuchPrivilegeXyz").is_err());
}

#[test]
fn dimm_info_real() {
    let d = dimm_info().expect("dimm_info");
    for x in &d {
        println!("{x:?}");
    }
    assert!(!d.is_empty(), "expected at least one DIMM");
    assert!(d.iter().all(|x| x.size_bytes > 0));
}

#[test]
fn whea_query_runs() {
    let since = SystemTime::now() - Duration::from_secs(30 * 86400);
    let ev = whea_events_since(since).expect("whea query");
    println!("WHEA events in last 30 days: {}", ev.len());
    for e in ev.iter().take(5) {
        println!("{e:?}");
    }
    // Future start must work and return nothing.
    assert!(
        whea_events_since(SystemTime::now() + Duration::from_secs(3600))
            .unwrap()
            .is_empty()
    );
}
