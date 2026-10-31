use super::simd::Level;
use super::Algo;
use crate::algorithms::common::CHUNK_WORDS;
use crate::{create_test, MemError, MemTest, Reporter, TestContext, TestId};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Reporter with optional stop and a per-progress hook.
#[derive(Default)]
struct Rec {
    errors: Mutex<Vec<MemError>>,
    bytes: AtomicU64,
    calls: AtomicU64,
    stop_always: AtomicBool,
    stop_after_calls: Option<u64>,
    /// Called with the 1-based progress call number.
    hook: Option<Box<dyn Fn(u64) + Send + Sync>>,
}

impl Reporter for Rec {
    fn error(&self, err: MemError) {
        self.errors.lock().unwrap().push(err);
    }
    fn progress(&self, bytes: u64) {
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
        let n = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
        if let Some(h) = &self.hook {
            h(n);
        }
    }
    fn should_stop(&self) -> bool {
        self.stop_always.load(Ordering::Relaxed)
            || self
                .stop_after_calls
                .is_some_and(|n| self.calls.load(Ordering::Relaxed) >= n)
    }
}

fn ctx() -> TestContext {
    TestContext {
        bit_fade_delay: Duration::from_millis(5),
        ..TestContext::default()
    }
}

/// ~1.25 MiB: two chunks, odd length.
const LEN: usize = CHUNK_WORDS + 32 * 1024 + 13;

fn buf(len: usize) -> Vec<u64> {
    vec![0xDEAD_BEEF_0BAD_F00D; len]
}

#[test]
fn clean_runs_report_no_errors_and_exact_progress() {
    for &id in TestId::ALL {
        let t = create_test(id);
        assert_eq!(t.id(), id);
        for len in [0usize, 1, 7, 100, 4099, LEN] {
            let mut b = buf(len);
            let rec = Rec::default();
            t.run(&mut b, &ctx(), &rec);
            assert!(
                rec.errors.lock().unwrap().is_empty(),
                "{id:?} len {len}: {:?}",
                rec.errors
            );
            let want = len as u64 * 8 * t.sweeps() as u64;
            let got = rec.bytes.load(Ordering::Relaxed);
            let tol = 64 * t.sweeps() as u64;
            assert!(
                got.abs_diff(want) <= tol,
                "{id:?} len {len}: progress {got} vs {want}"
            );
        }
    }
}

#[test]
fn fault_injection_detected_in_every_test() {
    for &id in TestId::ALL {
        let t = create_test(id);
        let mut b = buf(LEN);
        let (lo, hi) = (b.as_ptr() as usize, b.as_ptr() as usize + b.len() * 8);
        let rec = Rec::default();
        let c = TestContext {
            inject_fault_one_in: Some(1000),
            ..ctx()
        };
        t.run(&mut b, &c, &rec);
        let errs = rec.errors.lock().unwrap();
        assert!(!errs.is_empty(), "{id:?}: no errors with injection");
        for e in errs.iter() {
            assert_eq!(e.test, id);
            assert_ne!(e.expected, e.actual, "{id:?}");
            assert_eq!(e.flipped_bits(), 1, "{id:?}");
            assert!(
                e.address >= lo && e.address < hi && e.address % 8 == 0,
                "{id:?}"
            );
        }
    }
}

#[test]
fn injection_rate_is_about_one_in_n() {
    let t = create_test(TestId::RandomPattern);
    let mut b = buf(1 << 20);
    let rec = Rec::default();
    let c = TestContext {
        inject_fault_one_in: Some(100),
        ..ctx()
    };
    t.run(&mut b, &c, &rec);
    // 2 rounds * 2^20 verified words / 100
    let n = rec.errors.lock().unwrap().len() as f64;
    let want = 2.0 * (1 << 20) as f64 / 100.0;
    assert!((0.8 * want..1.2 * want).contains(&n), "{n} vs {want}");
}

#[test]
fn should_stop_is_honored() {
    for &id in TestId::ALL {
        let t = create_test(id);
        let mut b = buf(LEN);
        let rec = Rec {
            stop_always: AtomicBool::new(true),
            ..Rec::default()
        };
        t.run(
            &mut b,
            &TestContext {
                bit_fade_delay: Duration::from_secs(60),
                ..ctx()
            },
            &rec,
        );
        assert_eq!(rec.bytes.load(Ordering::Relaxed), 0, "{id:?}");
        // Stop mid-run: at most one more chunk after the stop request.
        let rec = Rec {
            stop_after_calls: Some(1),
            ..Rec::default()
        };
        let start = Instant::now();
        t.run(
            &mut b,
            &TestContext {
                bit_fade_delay: Duration::from_secs(60),
                ..ctx()
            },
            &rec,
        );
        assert!(start.elapsed() < Duration::from_secs(5), "{id:?} slow stop");
        assert!(rec.calls.load(Ordering::Relaxed) <= 2, "{id:?}");
        assert!(rec.errors.lock().unwrap().is_empty(), "{id:?}");
    }
}

#[test]
fn simd_levels_all_work() {
    for level in [Level::Avx512, Level::Avx2, Level::Scalar] {
        if !level.supported() {
            continue;
        }
        let t = Algo {
            id: TestId::SimdStream,
            level: Some(level),
        };
        // Misaligned starting offsets and odd lengths.
        for off in 0..9usize {
            for len in [0usize, 5, 8, 63, 64, 1000, CHUNK_WORDS + 77] {
                let mut b = buf(len + off);
                let rec = Rec::default();
                t.run(&mut b[off..], &ctx(), &rec);
                assert!(
                    rec.errors.lock().unwrap().is_empty(),
                    "{level:?} off {off} len {len}"
                );
                // Injection must work in the SIMD verify path too.
                let rec = Rec::default();
                let c = TestContext {
                    inject_fault_one_in: Some(7),
                    ..ctx()
                };
                t.run(&mut b[off..], &c, &rec);
                let n = rec.errors.lock().unwrap().len();
                assert!(len < 64 || n > 0, "{level:?} off {off} len {len} n {n}");
            }
        }
    }
}

/// Flip one bit of the buffer from inside the first `progress` callback,
/// i.e. between the first write sweep and its verification.
fn corruption_detected(id: TestId) {
    // Modulo20 rewrites the region repeatedly; corrupt after the last rewrite.
    let call = if id == TestId::Modulo20 {
        super::simple::MODULO_WRITE_REPEATS as u64
    } else {
        1
    };
    let t = create_test(id);
    let mut b = buf(4096 + 5);
    let target = b.as_mut_ptr() as usize + 101 * 8;
    let rec = Rec {
        hook: Some(Box::new(move |n| {
            if n == call {
                // SAFETY: test-only; address is inside the live buffer and the
                // caller (`run`) is suspended in this callback.
                unsafe {
                    let p = target as *mut u64;
                    p.write_volatile(p.read_volatile() ^ (1 << 33));
                }
            }
        })),
        ..Rec::default()
    };
    t.run(&mut b, &ctx(), &rec);
    let errs = rec.errors.lock().unwrap();
    assert!(!errs.is_empty(), "{id:?}: real corruption missed");
    assert!(
        errs.iter()
            .any(|e| e.address == target && e.xor_mask() == 1 << 33),
        "{id:?}: {errs:?}"
    );
}

#[test]
fn real_corruption_is_detected() {
    for id in [
        TestId::RandomPattern,
        TestId::MovingInversions,
        TestId::MovingInversionsRandom,
        TestId::OwnAddress,
        TestId::SolidBits,
        TestId::WalkingOnes,
        TestId::WalkingZeros,
        TestId::BlockMove,
        TestId::SimdStream,
        TestId::BitFade,
        TestId::Modulo20,
        TestId::RowStress,
    ] {
        corruption_detected(id);
    }
}

/// Flip bit 7 of word `idx` from inside progress call number `call`; returns
/// the errors the test reported.
fn flip_at_call(id: TestId, len: usize, idx: usize, call: u64) -> (usize, Vec<MemError>) {
    let t = create_test(id);
    let mut b = buf(len);
    let target = b.as_mut_ptr() as usize + idx * 8;
    let rec = Rec {
        hook: Some(Box::new(move |n| {
            if n == call {
                // SAFETY: test-only; address is inside the live buffer and the
                // caller (`run`) is suspended in this callback.
                unsafe {
                    let p = target as *mut u64;
                    p.write_volatile(p.read_volatile() ^ (1 << 7));
                }
            }
        })),
        ..Rec::default()
    };
    t.run(&mut b, &ctx(), &rec);
    let errs = rec.errors.lock().unwrap().clone();
    (target, errs)
}

#[test]
fn modulo20_detects_coupling_style_fault() {
    use super::simple::MODULO_WRITE_REPEATS;
    // Offset k = 0: p cells are multiples of 20. Flip one right after the
    // last full-region neighbour rewrite, before verification.
    let (target, errs) = flip_at_call(TestId::Modulo20, 4096 + 5, 100, MODULO_WRITE_REPEATS as u64);
    assert!(
        errs.iter()
            .any(|e| e.address == target && e.xor_mask() == 1 << 7),
        "{errs:?}"
    );
    // A flip in a non-p (complement) cell is caught too.
    let (target, errs) = flip_at_call(TestId::Modulo20, 4096 + 5, 101, MODULO_WRITE_REPEATS as u64);
    assert!(errs.iter().any(|e| e.address == target), "{errs:?}");
}

#[test]
fn row_stress_detects_corruption_after_hammering() {
    // Single chunk: progress call 1 = fill, 2 = hammer; flip after hammering.
    let (target, errs) = flip_at_call(TestId::RowStress, 4096 + 5, 77, 2);
    assert!(
        errs.iter()
            .any(|e| e.address == target && e.xor_mask() == 1 << 7),
        "{errs:?}"
    );
}

#[test]
fn row_stress_clean_on_multi_chunk_region() {
    let t = create_test(TestId::RowStress);
    let mut b = buf(3 * CHUNK_WORDS + 5);
    let rec = Rec::default();
    t.run(&mut b, &ctx(), &rec);
    assert!(rec.errors.lock().unwrap().is_empty());
}

#[test]
fn huge_parameters_do_not_panic() {
    // bit_fade_delay overflowing Instant: waits until stopped.
    let t = create_test(TestId::BitFade);
    let mut b = buf(1000);
    let rec = Rec {
        stop_after_calls: Some(1),
        ..Rec::default()
    };
    let c = TestContext {
        bit_fade_delay: Duration::MAX,
        ..ctx()
    };
    t.run(&mut b, &c, &rec);
    // Huge fault interval: no overflow, no panic.
    for one_in in [u64::MAX, u64::MAX / 2 + 1, 1] {
        let t = create_test(TestId::SolidBits);
        let rec = Rec::default();
        let c = TestContext {
            inject_fault_one_in: Some(one_in),
            ..ctx()
        };
        t.run(&mut b, &c, &rec);
    }
}

#[test]
fn names_and_descriptions() {
    for &id in TestId::ALL {
        assert!(id.description().len() > 20);
    }
}

#[test]
#[ignore = "benchmark: run with --release -- --ignored --nocapture"]
fn bench_throughput() {
    let words = 256 * 1024 * 1024 / 8;
    let mut b = vec![0u64; words];
    for id in [
        TestId::SimdStream,
        TestId::RandomPattern,
        TestId::SolidBits,
        TestId::MovingInversions,
        TestId::Modulo20,
        TestId::RowStress,
    ] {
        let t = create_test(id);
        // MovingInversions has 48 sweeps; use a smaller slice to keep it short.
        let slice = if matches!(
            id,
            TestId::MovingInversions | TestId::Modulo20 | TestId::RowStress
        ) {
            &mut b[..words / 8]
        } else {
            &mut b[..]
        };
        let rec = Rec::default();
        let start = Instant::now();
        t.run(slice, &TestContext::default(), &rec);
        let secs = start.elapsed().as_secs_f64();
        let bytes = rec.bytes.load(Ordering::Relaxed) as f64;
        println!(
            "{id:?}: {:.2} GiB/s swept ({} sweeps, {:.2}s)",
            bytes / secs / (1u64 << 30) as f64,
            t.sweeps(),
            secs
        );
        assert!(rec.errors.lock().unwrap().is_empty());
    }
}
