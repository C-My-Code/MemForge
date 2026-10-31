use super::*;
use crate::StopConditions;
use memforge_platform::CpuFeatures;
use std::time::Duration;

type TestFn = Arc<dyn Fn(&mut [u64], &TestContext, &dyn Reporter) + Send + Sync>;

struct MockTest {
    id: TestId,
    f: TestFn,
}

impl MemTest for MockTest {
    fn id(&self) -> TestId {
        self.id
    }
    fn sweeps(&self) -> u32 {
        1
    }
    fn run(&self, region: &mut [u64], ctx: &TestContext, rep: &dyn Reporter) {
        (self.f)(region, ctx, rep)
    }
}

fn cpu(index: usize, core: u32) -> LogicalCpu {
    LogicalCpu {
        index,
        group: 0,
        number: index as u8,
        core,
        llc: 0,
        efficiency_class: 0,
    }
}

fn mock_cpu_info() -> CpuInfo {
    CpuInfo {
        brand: "mock".into(),
        logical: vec![cpu(0, 0), cpu(1, 0), cpu(2, 1), cpu(3, 1)],
        physical_cores: 2,
        features: CpuFeatures::default(),
    }
}

fn mock_env(f: TestFn) -> Env {
    Env {
        factory: Arc::new(move |id| Box::new(MockTest { id, f: f.clone() })),
        alloc: Arc::new(|bytes, _| Ok(Region::from_vec(vec![0u64; bytes / 8]))),
        cpu_info: Arc::new(mock_cpu_info),
        memory_status: Arc::new(|| MemoryStatus {
            total_physical: 8 << 30,
            available_physical: 4 << 30,
            page_size: 4096,
            large_page_size: None,
        }),
        is_elevated: Arc::new(|| true),
        keep_awake: Arc::new(|_| {}),
        enable_privilege: Arc::new(|_| Ok(true)),
        set_priority: Arc::new(|_| Ok(())),
        pin: Arc::new(|_| Ok(())),
        whea: Arc::new(|_| Ok(Vec::new())),
        snapshot: Arc::new(|| SystemSnapshot {
            cpu: mock_cpu_info(),
            memory: MemoryStatus {
                total_physical: 0,
                available_physical: 0,
                page_size: 4096,
                large_page_size: None,
            },
            dimms: Vec::new(),
            elevated: true,
            os: "mock".into(),
        }),
    }
}

fn cfg(threads: usize, ntests: usize, passes: Option<u64>) -> RunConfig {
    RunConfig {
        memory: MemorySize::Bytes(MIB * 4),
        threads: Some(threads),
        tests: TestId::ALL[..ntests].to_vec(),
        stop: StopConditions {
            passes,
            duration: None,
            max_errors: None,
        },
        large_pages: false,
        lock_memory: false,
        pin_threads: true,
        priority: ThreadPriority::Normal,
        seed: Some(42),
        bit_fade_delay: Duration::from_millis(1),
        inject_fault_one_in: None,
        monitor_whea: true,
        keep_awake: false,
    }
}

fn simple(f: impl Fn(&mut [u64], &TestContext, &dyn Reporter) + Send + Sync + 'static) -> TestFn {
    Arc::new(f)
}

fn run(cfg: RunConfig, f: TestFn) -> RunReport {
    Inner::start_with_env(cfg, mock_env(f)).unwrap().wait()
}

fn err(addr: usize, expected: u64, actual: u64) -> MemError {
    MemError {
        test: TestId::OwnAddress,
        thread: 0,
        pass: 0,
        address: addr,
        expected,
        actual,
    }
}

#[test]
fn chunks_disjoint_and_rotate() {
    use std::sync::Mutex;
    let active: Arc<Mutex<Vec<(usize, usize)>>> = Arc::default();
    let seen: Arc<Mutex<Vec<(u64, usize, usize)>>> = Arc::default(); // pass, thread, start
    let (a2, s2) = (active.clone(), seen.clone());
    let f = simple(move |r, ctx, rep| {
        let me = (r.as_ptr() as usize, r.len() * 8);
        {
            let mut a = a2.lock().unwrap();
            for &(s, l) in a.iter() {
                assert!(me.0 + me.1 <= s || s + l <= me.0, "overlapping chunks");
            }
            a.push(me);
        }
        s2.lock().unwrap().push((ctx.pass, ctx.thread, me.0));
        std::thread::sleep(Duration::from_millis(2));
        rep.progress(me.1 as u64);
        let mut a = a2.lock().unwrap();
        let i = a.iter().position(|x| *x == me).unwrap();
        a.remove(i);
    });
    let rep = run(cfg(4, 2, Some(4)), f);
    assert_eq!(rep.stop_reason, "passes reached");
    assert_eq!(rep.passes_completed, 4);
    let seen = seen.lock().unwrap();
    // Per pass: 4 distinct chunks (bijection); per thread: all 4 chunks across passes.
    for pass in 0..4u64 {
        let mut starts: Vec<_> = seen
            .iter()
            .filter(|s| s.0 == pass)
            .map(|s| s.2)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        starts.sort();
        assert_eq!(starts.len(), 4, "pass {pass}");
    }
    for t in 0..4 {
        let starts: HashSet<_> = seen.iter().filter(|s| s.1 == t).map(|s| s.2).collect();
        assert_eq!(starts.len(), 4, "thread {t} should cover every chunk");
    }
    assert_eq!(rep.bytes_tested, 4 * MIB);
    assert_eq!(
        rep.per_test.iter().map(|t| t.runs).collect::<Vec<_>>(),
        vec![4, 4]
    );
}

#[test]
fn stop_terminates_quickly() {
    let f = simple(|_, _, rep| {
        while !rep.should_stop() {
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    let h = Inner::start_with_env(cfg(4, 3, None), mock_env(f)).unwrap();
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(h.status().phase, Phase::Running);
    let t = Instant::now();
    h.stop();
    let rep = h.wait();
    assert!(t.elapsed() < Duration::from_secs(2));
    assert_eq!(rep.stop_reason, "user stop");
    assert_eq!(rep.verdict, Verdict::Incomplete);
    assert_eq!(rep.passes_completed, 0);
}

#[test]
fn passes_limit_and_pass_verdict() {
    let f = simple(|r, _, rep| rep.progress(r.len() as u64 * 8));
    let rep = run(cfg(2, 3, Some(3)), f);
    assert_eq!(rep.passes_completed, 3);
    assert_eq!(rep.verdict, Verdict::Pass);
    assert_eq!(rep.stop_reason, "passes reached");
    assert_eq!(rep.bytes_processed, 3 * 3 * 4 * MIB);
    assert_eq!(rep.threads, 2);
}

#[test]
fn duration_limit() {
    let f = simple(|_, _, _| std::thread::sleep(Duration::from_millis(5)));
    let mut c = cfg(2, 2, None);
    c.stop.duration = Some(Duration::from_millis(200));
    let rep = run(c, f);
    assert_eq!(rep.stop_reason, "duration reached");
    assert!(rep.duration >= Duration::from_millis(200));
    assert!(rep.duration < Duration::from_secs(3));
    assert!(rep.passes_completed >= 1);
}

#[test]
fn max_errors_stops() {
    let f = simple(|r, _, rep| {
        while !rep.should_stop() {
            rep.error(err(r.as_ptr() as usize, 0, 1));
            std::thread::sleep(Duration::from_micros(200));
        }
    });
    let mut c = cfg(4, 2, None);
    c.stop.max_errors = Some(50);
    let rep = run(c, f);
    assert_eq!(rep.stop_reason, "max errors reached");
    assert!(rep.errors_total >= 50);
    assert_eq!(rep.verdict, Verdict::Fail);
}

#[test]
fn errors_aggregated_and_sample_bounded() {
    let f = simple(|r, _, rep| {
        for i in 0..5000 {
            rep.error(err(r.as_ptr() as usize + i * 8, 0, (1 << 3) | (1 << 10)));
        }
    });
    let h = Inner::start_with_env(cfg(4, 1, Some(1)), mock_env(f)).unwrap();
    let rx = h.events();
    let rep = h.wait();
    assert_eq!(rep.errors_total, 4 * 5000);
    assert_eq!(rep.verdict, Verdict::Fail);
    assert_eq!(rep.error_sample.len(), 1000);
    assert_eq!(rep.errors_by_bit.len(), 64);
    for (b, c) in rep.errors_by_bit.iter().enumerate() {
        assert_eq!(*c, if b == 3 || b == 10 { 20_000 } else { 0 }, "bit {b}");
    }
    assert_eq!(rep.per_test[0].errors, 20_000);
    let n_err_events = rx
        .try_iter()
        .filter(|e| matches!(e, Event::MemoryError(_)))
        .count();
    // Rate limited: at most 100 per wall-clock second (run lasts well under 2 s).
    assert!(n_err_events > 0 && n_err_events <= 200, "{n_err_events}");
}

#[test]
fn finished_event_exactly_once_and_last() {
    let f = simple(|r, _, rep| rep.progress(r.len() as u64 * 8));
    let h = Inner::start_with_env(cfg(2, 2, Some(2)), mock_env(f)).unwrap();
    let rx = h.events();
    let rx2 = h.events();
    let report = h.wait();
    let mut events: Vec<Event> = rx.try_iter().collect();
    assert!(
        rx2.try_recv().is_err(),
        "events are delivered once across clones"
    );
    assert!(matches!(events.last(), Some(Event::Finished(_))));
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::Finished(_)))
            .count(),
        1
    );
    assert!(matches!(events.first(), Some(Event::Started { .. })));
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::PassCompleted { .. }))
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::TestStarted { .. }))
            .count(),
        4
    );
    let Some(Event::Finished(r)) = events.pop() else {
        unreachable!()
    };
    assert_eq!(r.seed, 42);
    assert_eq!(r.passes_completed, report.passes_completed);
    assert_eq!(r.stop_reason, report.stop_reason);
    assert_eq!(r.tool_version, env!("CARGO_PKG_VERSION"));
}

#[test]
fn status_progress_and_finish() {
    let f = simple(|r, _, rep| {
        rep.progress(r.len() as u64 * 8);
        std::thread::sleep(Duration::from_millis(30));
    });
    let h = Inner::start_with_env(cfg(2, 2, Some(1)), mock_env(f)).unwrap();
    let mut max_prog = 0.0f64;
    while !h.is_finished() {
        let s = h.status();
        assert!((0.0..=1.0).contains(&s.pass_progress));
        max_prog = max_prog.max(s.pass_progress);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(max_prog > 0.0);
    let s = h.status();
    assert_eq!(s.phase, Phase::Finished);
    assert_eq!(s.passes_completed, 1);
    assert_eq!(s.threads.len(), 2);
    h.wait();
}

#[test]
fn allocation_failure_reports_incomplete() {
    let mut env = mock_env(simple(|_, _, _| {}));
    env.alloc = Arc::new(|_, _| Err("boom".to_string()));
    let h = Inner::start_with_env(cfg(2, 1, Some(1)), env).unwrap();
    let rx = h.events();
    let rep = h.wait();
    assert_eq!(rep.verdict, Verdict::Incomplete);
    assert_eq!(rep.bytes_tested, 0);
    assert!(
        rep.stop_reason
            .starts_with("setup failed: allocation failed")
            && rep.stop_reason.contains("boom")
    );
    let evs: Vec<_> = rx.try_iter().collect();
    assert!(evs.iter().any(|e| matches!(
        e,
        Event::Log {
            level: LogLevel::Error,
            ..
        }
    )));
    assert!(matches!(evs.last(), Some(Event::Finished(_))));
}

#[test]
fn memory_whea_fails_verdict() {
    let mut env = mock_env(simple(|r, _, rep| rep.progress(r.len() as u64 * 8)));
    env.whea = Arc::new(|_| {
        Ok(vec![WheaEvent {
            time: SystemTime::UNIX_EPOCH,
            event_id: 47,
            level: "Warning".into(),
            message: "mem".into(),
            memory_related: true,
        }])
    });
    let rep = Inner::start_with_env(cfg(2, 1, Some(1)), env)
        .unwrap()
        .wait();
    assert_eq!(rep.whea_events.len(), 1);
    assert_eq!(rep.verdict, Verdict::Fail);
}

#[test]
fn drop_stops_and_joins() {
    let f = simple(|_, _, rep| {
        while !rep.should_stop() {
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    let h = Inner::start_with_env(cfg(2, 1, None), mock_env(f)).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    let t = Instant::now();
    drop(h);
    assert!(t.elapsed() < Duration::from_secs(2));
}

#[test]
fn worker_panic_does_not_hang() {
    let f = simple(|_, ctx, _| {
        if ctx.thread == 1 {
            panic!("mock failure");
        }
    });
    let rep = run(cfg(3, 1, None), f);
    assert_eq!(rep.stop_reason, "worker panicked");
}

#[test]
fn invalid_configs_rejected() {
    let ok = cfg(2, 1, Some(1));
    let mut c = ok.clone();
    c.tests.clear();
    assert!(validate(&c).is_err());
    let mut c = ok.clone();
    c.threads = Some(0);
    assert!(validate(&c).is_err());
    let mut c = ok.clone();
    c.memory = MemorySize::PercentAvailable(0.0);
    assert!(validate(&c).is_err());
    c.memory = MemorySize::PercentAvailable(100.5);
    assert!(validate(&c).is_err());
    c.memory = MemorySize::PercentAvailable(50.0);
    assert!(validate(&c).is_ok());
    let mut c = ok;
    c.memory = MemorySize::Bytes(MIB); // 1 MiB for 2 threads
    assert!(validate(&c).is_err());
}

#[test]
fn cpu_picking_prefers_distinct_cores() {
    let cpus = mock_cpu_info().logical;
    let picked: Vec<_> = pick_cpus(&cpus, 6)
        .into_iter()
        .map(|c| c.unwrap().index)
        .collect();
    assert_eq!(picked, vec![0, 2, 1, 3, 0, 2]);
    assert!(pick_cpus(&[], 2).iter().all(|c| c.is_none()));
}

#[test]
fn chunk_bounds_cover_region() {
    let total = 1_000_003usize;
    let b = chunk_bounds(total, 7);
    assert_eq!(b.iter().map(|c| c.1).sum::<usize>(), total);
    for (i, (s, _)) in b.iter().enumerate() {
        assert_eq!(s % CHUNK_ALIGN_WORDS, 0, "chunk {i}");
    }
    for w in b.windows(2) {
        assert_eq!(w[0].0 + w[0].1, w[1].0);
    }
}

fn wait_timeout(h: Inner, secs: u64) -> RunReport {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(h.wait());
    });
    rx.recv_timeout(Duration::from_secs(secs))
        .expect("run hung")
}

#[test]
fn monitor_panic_does_not_hang() {
    let f = simple(|_, _, rep| {
        while !rep.should_stop() {
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    let mut env = mock_env(f);
    env.whea = Arc::new(|_| panic!("whea hook failure"));
    let h = Inner::start_with_env(cfg(2, 1, None), env).unwrap();
    let rep = wait_timeout(h, 10);
    assert_eq!(rep.stop_reason, "monitor panicked");
    assert_eq!(rep.verdict, Verdict::Incomplete);
}

#[test]
fn worker_panic_verdict_is_not_pass() {
    let f = simple(|r, ctx, rep| {
        if ctx.pass == 1 && ctx.thread == 1 {
            panic!("mock failure");
        }
        rep.progress(r.len() as u64 * 8);
    });
    let rep = run(cfg(3, 1, Some(4)), f);
    assert_eq!(rep.stop_reason, "worker panicked");
    assert_eq!(rep.errors_total, 0);
    assert!(rep.passes_completed >= 1 && rep.passes_completed < 4);
    assert_eq!(rep.verdict, Verdict::Incomplete);
}

#[test]
fn duration_measured_from_running() {
    let f = simple(|_, _, rep| {
        while !rep.should_stop() {
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    let mut env = mock_env(f);
    env.alloc = Arc::new(|bytes, _| {
        std::thread::sleep(Duration::from_millis(400));
        Ok(Region::from_vec(vec![0u64; bytes / 8]))
    });
    let mut c = cfg(2, 1, None);
    c.stop.duration = Some(Duration::from_millis(200));
    let rep = Inner::start_with_env(c, env).unwrap().wait();
    assert_eq!(rep.stop_reason, "duration reached");
    // Wall time includes allocation; the 200 ms budget starts after it.
    assert!(
        rep.duration >= Duration::from_millis(580),
        "{:?}",
        rep.duration
    );
}

#[test]
fn stop_after_step_completion_still_counts_pass() {
    let f = simple(|r, _, rep| {
        rep.progress(r.len() as u64 * 8);
        while !rep.should_stop() {
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    let h = Inner::start_with_env(cfg(2, 1, None), mock_env(f)).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    h.stop();
    let rep = h.wait();
    assert_eq!(rep.stop_reason, "user stop");
    assert_eq!(rep.passes_completed, 1);
    assert_eq!(rep.per_test[0].runs, 1);
}

#[test]
fn new_limits_validated() {
    let ok = cfg(2, 1, Some(1));
    let mut c = ok.clone();
    c.bit_fade_delay = Duration::from_secs(24 * 3600);
    assert!(validate(&c).is_ok());
    c.bit_fade_delay = Duration::from_secs(24 * 3600 + 1);
    assert!(validate(&c).is_err());
    let mut c = ok;
    c.inject_fault_one_in = Some(1 << 48);
    assert!(validate(&c).is_ok());
    c.inject_fault_one_in = Some((1 << 48) + 1);
    assert!(validate(&c).is_err());
}

#[test]
fn small_chunk_emits_cache_warning() {
    let f = simple(|r, _, rep| rep.progress(r.len() as u64 * 8));
    let h = Inner::start_with_env(cfg(2, 1, Some(1)), mock_env(f)).unwrap();
    let rx = h.events();
    h.wait();
    assert!(rx.try_iter().any(|e| matches!(
        e,
        Event::Log { level: LogLevel::Warn, message } if message.contains("per-thread chunk is 2.0 MiB")
    )));
}

/// Real core + platform. Ignored by default until both are implemented.
#[test]
#[ignore]
fn end_to_end_real() {
    let mut c = RunConfig {
        memory: MemorySize::Bytes(64 * MIB),
        threads: Some(4),
        tests: memforge_core::default_test_suite(),
        stop: StopConditions {
            passes: Some(1),
            duration: None,
            max_errors: None,
        },
        lock_memory: false,
        monitor_whea: false,
        keep_awake: false,
        seed: Some(7),
        ..RunConfig::default()
    };
    let rep = crate::start(c.clone()).unwrap().wait();
    assert_eq!(rep.errors_total, 0, "{}", rep.to_json_pretty());
    assert_eq!(rep.verdict, Verdict::Pass);
    c.inject_fault_one_in = Some(100_000);
    let rep = crate::start(c).unwrap().wait();
    assert_eq!(rep.verdict, Verdict::Fail);
    assert!(rep.errors_total > 0);
}

#[test]
fn bytes_clamped_to_available_with_warning() {
    let mut env = mock_env(simple(|r, _, rep| rep.progress(r.len() as u64 * 8)));
    env.memory_status = Arc::new(|| MemoryStatus {
        total_physical: 8 << 30,
        available_physical: 256 * MIB + 2 * MIB,
        page_size: 4096,
        large_page_size: None,
    });
    let h = Inner::start_with_env(cfg(2, 1, Some(1)), env).unwrap();
    let rx = h.events();
    let rep = h.wait();
    assert_eq!(rep.bytes_tested, 2 * MIB);
    assert!(rx.try_iter().any(|e| matches!(
        e, Event::Log { level: LogLevel::Warn, message } if message.contains("clamping")
    )));
}

#[test]
fn transient_whea_errors_tolerated() {
    let calls = Arc::new(AtomicU64::new(0));
    let c2 = calls.clone();
    let mut env = mock_env(simple(|r, _, rep| {
        rep.progress(r.len() as u64 * 8);
        std::thread::sleep(Duration::from_millis(20));
    }));
    env.whea = Arc::new(move |_| {
        if c2.fetch_add(1, Relaxed) == 0 {
            Err("transient".to_string())
        } else {
            Ok(Vec::new())
        }
    });
    let rep = Inner::start_with_env(cfg(2, 1, Some(1)), env)
        .unwrap()
        .wait();
    assert_eq!(rep.verdict, Verdict::Pass);
    // first poll failed; final poll still ran
    assert!(calls.load(Relaxed) >= 2);
}
