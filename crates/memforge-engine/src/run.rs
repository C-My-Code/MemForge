//! Run orchestration internals.
//!
//! Threading model: `Inner::start` spawns one controller thread. The
//! controller allocates, then opens a `thread::scope` with N worker threads
//! and runs the monitor loop itself until every worker has exited.
//!
//! All workers execute the same (pass, test) step at the same time. Steps are
//! delimited by a stop-aware [`StepBarrier`]; the last worker to arrive runs
//! the step-transition closure (statistics, events, stop decisions) and the
//! resulting `done` flag is delivered identically to every worker, so either
//! all workers leave the loop or none does. Chunk ownership rotates by pass
//! (`(worker + pass) % N`) and only changes across barriers, so no two
//! workers ever hold the same chunk at once.

use crate::{
    EngineError, Event, LogLevel, MemorySize, Phase, Receiver, RunConfig, RunReport, Status,
    SystemSnapshot, TestStats, ThreadStatus, Verdict,
};
use crossbeam_channel::{unbounded, Sender};
use memforge_core::{create_test, MemError, MemTest, Reporter, TestContext, TestId};
use memforge_platform::{
    AllocOptions, CpuInfo, LogicalCpu, MemoryStatus, ThreadPriority, WheaEvent,
};
use std::any::Any;
use std::collections::HashSet;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{
    AtomicBool, AtomicU64, AtomicUsize,
    Ordering::{Acquire, Relaxed, Release},
};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime};

const MIB: u64 = 1024 * 1024;
const MIN_FREE_BYTES: u64 = 256 * MIB;
const CHUNK_ALIGN_WORDS: usize = 512; // 4 KiB
const ERROR_SAMPLE_CAP: usize = 1000;
const ERROR_EVENTS_PER_SEC: u64 = 100;
const NONE_IDX: usize = usize::MAX;
const MAX_BIT_FADE_DELAY: Duration = Duration::from_secs(24 * 3600);
const MAX_INJECT_ONE_IN: u64 = 1 << 48;
const WHEA_MAX_FAILS: u32 = 3;
const WHEA_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Below this per-thread chunk size, large CPU caches may hold part of a chunk.
const CACHE_WARN_CHUNK_BYTES: u64 = 64 * MIB;

fn lk<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ------------------------------------------------------------------ env

/// Memory region handed to the engine (real or mock).
pub(crate) struct Region {
    ptr: *mut u64,
    len_words: usize,
    large_pages: bool,
    locked: bool,
    _owner: Box<dyn Any + Send>,
}

// SAFETY: the region exclusively owns its allocation through `_owner`; the
// raw pointer is only dereferenced under the chunk-disjointness protocol.
unsafe impl Send for Region {}
unsafe impl Sync for Region {}

impl Region {
    #[cfg(test)]
    pub(crate) fn from_vec(mut v: Vec<u64>) -> Region {
        Region {
            ptr: v.as_mut_ptr(),
            len_words: v.len(),
            large_pages: false,
            locked: false,
            _owner: Box::new(v),
        }
    }
}

type Res<T> = Result<T, String>;

/// Seams for everything outside the engine (platform + test factory).
#[derive(Clone)]
#[allow(clippy::type_complexity)]
pub(crate) struct Env {
    pub factory: Arc<dyn Fn(TestId) -> Box<dyn MemTest> + Send + Sync>,
    pub alloc: Arc<dyn Fn(usize, &AllocOptions) -> Res<Region> + Send + Sync>,
    pub cpu_info: Arc<dyn Fn() -> CpuInfo + Send + Sync>,
    pub memory_status: Arc<dyn Fn() -> MemoryStatus + Send + Sync>,
    pub is_elevated: Arc<dyn Fn() -> bool + Send + Sync>,
    pub keep_awake: Arc<dyn Fn(bool) + Send + Sync>,
    pub enable_privilege: Arc<dyn Fn(&str) -> Res<bool> + Send + Sync>,
    pub set_priority: Arc<dyn Fn(ThreadPriority) -> Res<()> + Send + Sync>,
    pub pin: Arc<dyn Fn(&LogicalCpu) -> Res<()> + Send + Sync>,
    pub whea: Arc<dyn Fn(SystemTime) -> Res<Vec<WheaEvent>> + Send + Sync>,
    pub snapshot: Arc<dyn Fn() -> SystemSnapshot + Send + Sync>,
}

impl Env {
    pub(crate) fn real() -> Env {
        use memforge_platform as p;
        Env {
            factory: Arc::new(create_test),
            alloc: Arc::new(|bytes, opts| {
                let mut r = p::allocate(bytes, opts).map_err(|e| e.to_string())?;
                let (large_pages, locked) = (r.large_pages, r.locked);
                Ok(Region {
                    ptr: r.as_mut_slice().as_mut_ptr(),
                    len_words: r.len_words(),
                    large_pages,
                    locked,
                    _owner: Box::new(r),
                })
            }),
            cpu_info: Arc::new(p::cpu_info),
            memory_status: Arc::new(p::memory_status),
            is_elevated: Arc::new(p::is_elevated),
            keep_awake: Arc::new(p::keep_system_awake),
            enable_privilege: Arc::new(|n| p::enable_privilege(n).map_err(|e| e.to_string())),
            set_priority: Arc::new(|pr| {
                p::set_current_thread_priority(pr).map_err(|e| e.to_string())
            }),
            pin: Arc::new(|c| p::pin_current_thread(c).map_err(|e| e.to_string())),
            whea: Arc::new(|t| p::whea_events_since(t).map_err(|e| e.to_string())),
            snapshot: Arc::new(crate::system_snapshot),
        }
    }
}

// --------------------------------------------------------------- barrier

struct BarrierState {
    count: usize,
    generation: u64,
    done: bool,
    broken: bool,
}

/// Reusable barrier whose last arriver runs a closure that yields a `done`
/// flag shared by all participants of that generation. Can be poisoned so
/// that nobody ever blocks forever (e.g. after a worker panic).
struct StepBarrier {
    n: usize,
    state: Mutex<BarrierState>,
    cv: Condvar,
}

impl StepBarrier {
    fn new(n: usize) -> Self {
        StepBarrier {
            n,
            state: Mutex::new(BarrierState {
                count: 0,
                generation: 0,
                done: false,
                broken: false,
            }),
            cv: Condvar::new(),
        }
    }

    /// Returns true when the run is over (all participants get the same answer).
    fn wait(&self, last: impl FnOnce() -> bool) -> bool {
        let mut s = lk(&self.state);
        if s.broken {
            return true;
        }
        s.count += 1;
        if s.count == self.n {
            s.count = 0;
            s.done = last();
            s.generation += 1;
            self.cv.notify_all();
            return s.done;
        }
        let g = s.generation;
        while s.generation == g && !s.broken {
            s = self.cv.wait(s).unwrap_or_else(|e| e.into_inner());
        }
        s.broken || s.done
    }

    fn poison(&self) {
        lk(&self.state).broken = true;
        self.cv.notify_all();
    }
}

// ---------------------------------------------------------------- shared

#[repr(align(64))]
#[derive(Default)]
struct ThreadShared {
    bytes: AtomicU64,
    errors: AtomicU64,
    cpu: AtomicUsize,
    current_test: AtomicUsize,
    /// Set by the worker when its last step reached the expected progress.
    step_ok: AtomicBool,
}

#[derive(Default)]
struct TestShared {
    runs: AtomicU64,
    errors: AtomicU64,
    nanos: AtomicU64,
}

struct StepState {
    started: bool,
    pass: u64,
    ti: usize,
    step_start: Instant,
}

struct Shared {
    cfg: RunConfig,
    env: Env,
    tests: Vec<Arc<dyn MemTest>>,
    sweeps_sum: u64,
    start: Instant,
    started_at: SystemTime,
    tx: Sender<Event>,
    rx: Receiver<Event>,

    stop: AtomicBool,
    stop_reason: Mutex<Option<String>>,
    /// First internal failure (panic etc.); forces a non-Pass verdict.
    internal_error: Mutex<Option<String>>,
    phase: AtomicUsize, // 0 allocating, 1 running
    finished: AtomicBool,
    end_nanos: AtomicU64,
    report: OnceLock<RunReport>,

    seed: AtomicU64,
    bytes_under_test: AtomicU64,
    large_pages: AtomicBool,
    locked: AtomicBool,

    threads: OnceLock<Vec<ThreadShared>>,
    per_test: Vec<TestShared>,
    total_errors: AtomicU64,
    bits: Vec<AtomicU64>,
    sample: Mutex<Vec<MemError>>,
    sample_len: AtomicUsize,
    ev_window: AtomicU64,
    ev_count: AtomicU64,

    passes_completed: AtomicU64,
    cur_test: AtomicUsize,
    cur_pass: AtomicU64,
    pass_base: AtomicU64,
    step: Mutex<StepState>,

    throughput_bits: AtomicU64,
    whea: Mutex<Vec<WheaEvent>>,
    whea_count: AtomicU64,
    whea_memory: AtomicU64,
}

impl Shared {
    fn log(&self, level: LogLevel, message: impl Into<String>) {
        let _ = self.tx.send(Event::Log {
            level,
            message: message.into(),
        });
    }

    fn request_stop(&self, reason: &str) {
        {
            let mut r = lk(&self.stop_reason);
            if r.is_none() {
                *r = Some(reason.to_string());
            }
        }
        self.stop.store(true, Relaxed);
    }

    /// Record an internal failure and stop the run. The verdict can no
    /// longer be `Pass` and `stop_reason` names the failure.
    fn fail_internal(&self, reason: &str) {
        {
            let mut e = lk(&self.internal_error);
            if e.is_none() {
                *e = Some(reason.to_string());
            }
        }
        self.request_stop(reason);
    }

    fn total_bytes(&self) -> u64 {
        self.threads
            .get()
            .map_or(0, |t| t.iter().map(|x| x.bytes.load(Relaxed)).sum())
    }

    fn test_stats(&self) -> Vec<TestStats> {
        self.cfg
            .tests
            .iter()
            .zip(&self.per_test)
            .map(|(id, t)| TestStats {
                test: Some(*id),
                runs: t.runs.load(Relaxed),
                errors: t.errors.load(Relaxed),
                time: Duration::from_nanos(t.nanos.load(Relaxed)),
            })
            .collect()
    }

    fn bits(&self) -> Vec<u64> {
        self.bits.iter().map(|b| b.load(Relaxed)).collect()
    }

    fn elapsed(&self) -> Duration {
        if self.finished.load(Acquire) {
            Duration::from_nanos(self.end_nanos.load(Relaxed))
        } else {
            self.start.elapsed()
        }
    }

    /// Step transition; runs on exactly one worker while all others wait.
    /// Returns true when the run is over.
    fn advance(&self) -> bool {
        let mut st = lk(&self.step);
        if st.started {
            // A stop that lands after every worker finished its step must not
            // discard the step: judge by per-worker progress in that case.
            let complete = !self.stop.load(Relaxed)
                || self
                    .threads
                    .get()
                    .is_some_and(|ts| ts.iter().all(|t| t.step_ok.load(Relaxed)));
            if let Some(ts) = self.threads.get() {
                for t in ts {
                    t.step_ok.store(false, Relaxed);
                }
            }
            if complete {
                let t = &self.per_test[st.ti];
                t.runs.fetch_add(1, Relaxed);
                t.nanos
                    .fetch_add(st.step_start.elapsed().as_nanos() as u64, Relaxed);
                if st.ti + 1 == self.tests.len() {
                    let done = self.passes_completed.fetch_add(1, Relaxed) + 1;
                    let _ = self.tx.send(Event::PassCompleted {
                        pass: st.pass,
                        errors_so_far: self.total_errors.load(Relaxed),
                    });
                    if self.cfg.stop.passes.is_some_and(|p| done >= p) {
                        self.request_stop("passes reached");
                    }
                }
            }
            st.ti += 1;
            if st.ti == self.tests.len() {
                st.ti = 0;
                st.pass += 1;
            }
        } else {
            st.started = true;
        }
        if self.stop.load(Relaxed) {
            return true;
        }
        self.cur_pass.store(st.pass, Relaxed);
        self.cur_test.store(st.ti, Relaxed);
        if st.ti == 0 {
            self.pass_base.store(self.total_bytes(), Relaxed);
        }
        st.step_start = Instant::now();
        let _ = self.tx.send(Event::TestStarted {
            pass: st.pass,
            test: self.cfg.tests[st.ti],
        });
        false
    }
}

struct WorkerReporter<'a> {
    sh: &'a Shared,
    thread: usize,
    test_idx: usize,
}

impl Reporter for WorkerReporter<'_> {
    fn error(&self, err: MemError) {
        let sh = self.sh;
        let total = sh.total_errors.fetch_add(1, Relaxed) + 1;
        if let Some(t) = sh.threads.get().and_then(|t| t.get(self.thread)) {
            t.errors.fetch_add(1, Relaxed);
        }
        sh.per_test[self.test_idx].errors.fetch_add(1, Relaxed);
        let mut mask = err.xor_mask();
        while mask != 0 {
            let b = mask.trailing_zeros() as usize;
            sh.bits[b].fetch_add(1, Relaxed);
            mask &= mask - 1;
        }
        if sh.sample_len.load(Relaxed) < ERROR_SAMPLE_CAP {
            let mut s = lk(&sh.sample);
            if s.len() < ERROR_SAMPLE_CAP {
                s.push(err.clone());
                sh.sample_len.store(s.len(), Relaxed);
            }
        }
        // Rate limit: at most ERROR_EVENTS_PER_SEC events per wall-clock second.
        let sec = sh.start.elapsed().as_secs();
        if sh.ev_window.load(Relaxed) != sec {
            sh.ev_window.store(sec, Relaxed);
            sh.ev_count.store(0, Relaxed);
        }
        if sh.ev_count.fetch_add(1, Relaxed) < ERROR_EVENTS_PER_SEC {
            let _ = sh.tx.send(Event::MemoryError(err));
        }
        if sh.cfg.stop.max_errors.is_some_and(|m| total >= m) {
            sh.request_stop("max errors reached");
        }
    }

    fn progress(&self, bytes: u64) {
        if let Some(t) = self.sh.threads.get().and_then(|t| t.get(self.thread)) {
            t.bytes.fetch_add(bytes, Relaxed);
        }
    }

    fn should_stop(&self) -> bool {
        self.sh.stop.load(Relaxed)
    }
}

// ----------------------------------------------------------------- cpus

/// Choose `n` CPUs: one logical CPU per physical core first, then SMT
/// siblings, wrapping around if oversubscribed. Empty topology -> unpinned.
pub(crate) fn pick_cpus(logical: &[LogicalCpu], n: usize) -> Vec<Option<LogicalCpu>> {
    let mut cores: Vec<(u32, Vec<&LogicalCpu>)> = Vec::new();
    for c in logical {
        match cores.iter_mut().find(|(id, _)| *id == c.core) {
            Some((_, v)) => v.push(c),
            None => cores.push((c.core, vec![c])),
        }
    }
    let mut order = Vec::new();
    let mut round = 0;
    loop {
        let before = order.len();
        for (_, v) in &cores {
            if let Some(c) = v.get(round) {
                order.push((*c).clone());
            }
        }
        if order.len() == before {
            break;
        }
        round += 1;
    }
    if order.is_empty() {
        return vec![None; n];
    }
    (0..n)
        .map(|i| Some(order[i % order.len()].clone()))
        .collect()
}

pub(crate) fn chunk_bounds(total_words: usize, n: usize) -> Vec<(usize, usize)> {
    let base = (total_words / n) / CHUNK_ALIGN_WORDS * CHUNK_ALIGN_WORDS;
    (0..n)
        .map(|i| {
            let start = i * base;
            let len = if i + 1 == n {
                total_words - start
            } else {
                base
            };
            (start, len)
        })
        .collect()
}

fn random_seed() -> u64 {
    let t = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let local = 0u8;
    let mut x = t ^ (&local as *const u8 as u64).rotate_left(32) ^ u64::from(std::process::id());
    // splitmix64 finaliser
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

// ----------------------------------------------------------------- inner

pub(crate) struct Inner {
    sh: Arc<Shared>,
    join: Mutex<Option<std::thread::JoinHandle<()>>>,
}

fn validate(config: &RunConfig) -> Result<(), EngineError> {
    let bad = |m: &str| Err(EngineError::InvalidConfig(m.to_string()));
    if config.tests.is_empty() {
        return bad("no tests selected");
    }
    if config.threads == Some(0) {
        return bad("threads must be at least 1");
    }
    if config.stop.passes == Some(0) {
        return bad("passes must be at least 1");
    }
    if config.stop.max_errors == Some(0) {
        return bad("max_errors must be at least 1");
    }
    if config.inject_fault_one_in == Some(0) {
        return bad("inject_fault_one_in must be at least 1");
    }
    if config
        .inject_fault_one_in
        .is_some_and(|v| v > MAX_INJECT_ONE_IN)
    {
        return bad("inject_fault_one_in is absurdly large (max 2^48)");
    }
    if config.bit_fade_delay > MAX_BIT_FADE_DELAY {
        return bad("bit_fade_delay must be at most 24 hours");
    }
    match config.memory {
        MemorySize::PercentAvailable(p) if !(p.is_finite() && p > 0.0 && p <= 100.0) => {
            return bad("memory percent must be in (0, 100]");
        }
        MemorySize::Bytes(b) if b < MIB * config.threads.unwrap_or(1) as u64 => {
            return bad("memory must be at least 1 MiB per thread");
        }
        _ => {}
    }
    Ok(())
}

impl Inner {
    pub(crate) fn start(config: RunConfig) -> Result<Inner, EngineError> {
        Self::start_with_env(config, Env::real())
    }

    pub(crate) fn start_with_env(config: RunConfig, env: Env) -> Result<Inner, EngineError> {
        validate(&config)?;
        let tests: Vec<Arc<dyn MemTest>> = config
            .tests
            .iter()
            .map(|id| Arc::from((env.factory)(*id)))
            .collect();
        let sweeps_sum = tests
            .iter()
            .map(|t| u64::from(t.sweeps()))
            .sum::<u64>()
            .max(1);
        let (tx, rx) = unbounded();
        let n_tests = tests.len();
        let sh = Arc::new(Shared {
            per_test: (0..n_tests).map(|_| TestShared::default()).collect(),
            tests,
            sweeps_sum,
            env,
            start: Instant::now(),
            started_at: SystemTime::now(),
            tx,
            rx,
            stop: AtomicBool::new(false),
            stop_reason: Mutex::new(None),
            internal_error: Mutex::new(None),
            phase: AtomicUsize::new(0),
            finished: AtomicBool::new(false),
            end_nanos: AtomicU64::new(0),
            report: OnceLock::new(),
            seed: AtomicU64::new(0),
            bytes_under_test: AtomicU64::new(0),
            large_pages: AtomicBool::new(false),
            locked: AtomicBool::new(false),
            threads: OnceLock::new(),
            total_errors: AtomicU64::new(0),
            bits: (0..64).map(|_| AtomicU64::new(0)).collect(),
            sample: Mutex::new(Vec::new()),
            sample_len: AtomicUsize::new(0),
            ev_window: AtomicU64::new(u64::MAX),
            ev_count: AtomicU64::new(0),
            passes_completed: AtomicU64::new(0),
            cur_test: AtomicUsize::new(NONE_IDX),
            cur_pass: AtomicU64::new(0),
            pass_base: AtomicU64::new(0),
            step: Mutex::new(StepState {
                started: false,
                pass: 0,
                ti: 0,
                step_start: Instant::now(),
            }),
            throughput_bits: AtomicU64::new(0),
            whea: Mutex::new(Vec::new()),
            whea_count: AtomicU64::new(0),
            whea_memory: AtomicU64::new(0),
            cfg: config,
        });
        let sh2 = sh.clone();
        let handle = std::thread::Builder::new()
            .name("memforge-controller".into())
            .spawn(move || controller(sh2))
            .map_err(|e| EngineError::Other(format!("cannot spawn controller: {e}")))?;
        Ok(Inner {
            sh,
            join: Mutex::new(Some(handle)),
        })
    }

    pub(crate) fn stop(&self) {
        self.sh.request_stop("user stop");
    }

    pub(crate) fn status(&self) -> Status {
        let sh = &*self.sh;
        let finished = sh.finished.load(Acquire);
        let phase = if finished {
            Phase::Finished
        } else if sh.stop.load(Relaxed) {
            Phase::Stopping
        } else if sh.phase.load(Relaxed) == 0 {
            Phase::Allocating
        } else {
            Phase::Running
        };
        let cur = sh.cur_test.load(Relaxed);
        let current_test = (cur != NONE_IDX && !finished).then(|| sh.cfg.tests[cur]);
        let bytes_under_test = sh.bytes_under_test.load(Relaxed);
        let bytes_processed = sh.total_bytes();
        let denom = bytes_under_test as f64 * sh.sweeps_sum as f64;
        let pass_progress = if denom > 0.0 {
            (bytes_processed.saturating_sub(sh.pass_base.load(Relaxed)) as f64 / denom)
                .clamp(0.0, 1.0)
        } else {
            0.0
        };
        let threads = sh.threads.get().map_or_else(Vec::new, |ts| {
            ts.iter()
                .enumerate()
                .map(|(i, t)| {
                    let cpu = t.cpu.load(Relaxed);
                    let ct = t.current_test.load(Relaxed);
                    ThreadStatus {
                        index: i,
                        cpu: (cpu != NONE_IDX).then_some(cpu),
                        current_test: (ct != NONE_IDX).then(|| sh.cfg.tests[ct]),
                        bytes_processed: t.bytes.load(Relaxed),
                        errors: t.errors.load(Relaxed),
                    }
                })
                .collect()
        });
        Status {
            phase,
            elapsed: sh.elapsed(),
            bytes_under_test,
            large_pages: sh.large_pages.load(Relaxed),
            locked: sh.locked.load(Relaxed),
            passes_completed: sh.passes_completed.load(Relaxed),
            current_test,
            current_test_index: if cur == NONE_IDX { 0 } else { cur },
            pass_progress,
            bytes_processed,
            throughput: f64::from_bits(sh.throughput_bits.load(Relaxed)),
            errors: sh.total_errors.load(Relaxed),
            errors_by_bit: sh.bits(),
            per_test: sh.test_stats(),
            threads,
            whea_events: sh.whea_count.load(Relaxed),
        }
    }

    pub(crate) fn events(&self) -> Receiver<Event> {
        self.sh.rx.clone()
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.sh.finished.load(Acquire)
    }

    pub(crate) fn wait(self) -> RunReport {
        self.join_controller();
        self.sh
            .report
            .get()
            .expect("controller produced a report")
            .clone()
    }

    fn join_controller(&self) {
        let h = lk(&self.join).take();
        if let Some(h) = h {
            let _ = h.join();
        }
    }
}

/// Dropping requests stop and joins. An allocation already in progress cannot
/// be interrupted, so drop may block until it returns; the controller then
/// bails out before starting any worker.
impl Drop for Inner {
    fn drop(&mut self) {
        self.sh.request_stop("user stop");
        self.join_controller();
    }
}

// ------------------------------------------------------------ controller

fn controller(sh: Arc<Shared>) {
    // A panic anywhere must still yield a report so wait() cannot hang.
    let r = catch_unwind(AssertUnwindSafe(|| controller_body(&sh)));
    if r.is_err() && sh.report.get().is_none() {
        sh.fail_internal("internal error (controller panicked)");
        finish(
            &sh,
            "internal error (controller panicked)".to_string(),
            0,
            None,
        );
    }
}

fn controller_body(sh: &Shared) {
    let cfg = &sh.cfg;
    let env = &sh.env;
    let seed = cfg.seed.unwrap_or_else(random_seed);
    sh.seed.store(seed, Relaxed);
    if cfg.keep_awake {
        (env.keep_awake)(true);
    }

    let cpu = (env.cpu_info)();
    let n = cfg.threads.unwrap_or(cpu.logical.len()).max(1);

    let bytes = match cfg.memory {
        MemorySize::Bytes(b) => {
            let avail = (env.memory_status)().available_physical;
            let cap = avail.saturating_sub(MIN_FREE_BYTES);
            // avail == 0 means "unknown"; do not clamp then.
            if avail > 0 && b > cap {
                sh.log(
                    LogLevel::Warn,
                    format!(
                        "requested {} MiB exceeds available memory minus reserve;                          clamping to {} MiB to avoid paging",
                        b / MIB,
                        cap / MIB
                    ),
                );
                cap
            } else {
                b
            }
        }
        MemorySize::PercentAvailable(p) => {
            let avail = (env.memory_status)().available_physical;
            let want = (avail as f64 * f64::from(p) / 100.0) as u64;
            want.min(avail.saturating_sub(MIN_FREE_BYTES))
        }
    };
    if bytes < MIB * n as u64 {
        let msg = format!(
            "setup failed: allocation failed: not enough memory ({bytes} bytes for {n} threads)"
        );
        sh.log(LogLevel::Error, msg.clone());
        finish(sh, msg, n, None);
        return;
    }

    if cfg.large_pages {
        match (env.enable_privilege)("SeLockMemoryPrivilege") {
            Ok(true) => {}
            Ok(false) => sh.log(
                LogLevel::Warn,
                "SeLockMemoryPrivilege not held; large pages unlikely",
            ),
            Err(e) => sh.log(
                LogLevel::Warn,
                format!("cannot enable SeLockMemoryPrivilege: {e}"),
            ),
        }
    }
    if sh.stop.load(Relaxed) {
        let reason = lk(&sh.stop_reason)
            .clone()
            .unwrap_or_else(|| "user stop".to_string());
        finish(sh, reason, n, None);
        return;
    }
    let opts = AllocOptions {
        large_pages: cfg.large_pages,
        lock: cfg.lock_memory,
    };
    let region = match (env.alloc)(bytes as usize, &opts) {
        Ok(r) => r,
        Err(e) => {
            let msg = format!("setup failed: allocation failed: {e}");
            sh.log(LogLevel::Error, msg.clone());
            finish(sh, msg, n, None);
            return;
        }
    };
    if sh.stop.load(Relaxed) {
        // Stop requested while allocating (allocation itself is not interruptible).
        let reason = lk(&sh.stop_reason)
            .clone()
            .unwrap_or_else(|| "user stop".to_string());
        drop(region);
        finish(sh, reason, n, None);
        return;
    }
    let total_words = region.len_words;
    let bytes_tested = total_words as u64 * 8;
    if total_words / n < CHUNK_ALIGN_WORDS {
        let msg = "setup failed: allocation failed: region too small for thread count".to_string();
        sh.log(LogLevel::Error, msg.clone());
        finish(sh, msg, n, None);
        return;
    }
    sh.bytes_under_test.store(bytes_tested, Relaxed);
    sh.large_pages.store(region.large_pages, Relaxed);
    sh.locked.store(region.locked, Relaxed);

    if cfg.lock_memory && !region.locked {
        sh.log(
            LogLevel::Warn,
            "memory is not locked; paging may invalidate results",
        );
    }
    if cfg.large_pages && !region.large_pages {
        sh.log(
            LogLevel::Warn,
            "large pages not obtained; using normal pages",
        );
    }
    if (cfg.lock_memory || cfg.large_pages) && !(env.is_elevated)() {
        sh.log(
            LogLevel::Warn,
            "not running elevated; locking/large pages may be unavailable",
        );
    }

    let cpus = if cfg.pin_threads {
        pick_cpus(&cpu.logical, n)
    } else {
        vec![None; n]
    };
    let thread_state: Vec<ThreadShared> = cpus
        .iter()
        .map(|c| {
            let t = ThreadShared::default();
            t.cpu
                .store(c.as_ref().map_or(NONE_IDX, |c| c.index), Relaxed);
            t.current_test.store(NONE_IDX, Relaxed);
            t
        })
        .collect();
    let _ = sh.threads.set(thread_state);

    let _ = sh.tx.send(Event::Started {
        bytes: bytes_tested,
        threads: n,
        large_pages: region.large_pages,
        locked: region.locked,
        seed,
    });
    let chunk_bytes = bytes_tested / n as u64;
    if chunk_bytes < CACHE_WARN_CHUNK_BYTES {
        sh.log(
            LogLevel::Warn,
            format!(
                "per-thread chunk is {:.1} MiB; with large CPU caches (e.g. X3D) part of each \
                 verify may hit cache - test more memory or fewer threads",
                chunk_bytes as f64 / MIB as f64
            ),
        );
    }
    // The duration stop condition is measured from here, not from allocation.
    let run_start = Instant::now();
    sh.phase.store(1, Relaxed);

    let chunks = chunk_bounds(total_words, n);
    let barrier = StepBarrier::new(n);
    let alive = AtomicUsize::new(n);
    let base = region.ptr as usize;

    std::thread::scope(|scope| {
        for w in 0..n {
            let (chunks, barrier, alive, cpus) = (&chunks, &barrier, &alive, &cpus);
            let spawned = std::thread::Builder::new()
                .name(format!("memforge-worker-{w}"))
                .spawn_scoped(scope, move || {
                    let r = catch_unwind(AssertUnwindSafe(|| {
                        worker(sh, w, n, base, chunks, barrier, cpus[w].as_ref(), seed)
                    }));
                    if r.is_err() {
                        sh.log(LogLevel::Error, format!("worker {w} panicked"));
                        sh.fail_internal("worker panicked");
                        barrier.poison();
                    }
                    alive.fetch_sub(1, Relaxed);
                });
            if spawned.is_err() {
                sh.log(LogLevel::Error, format!("cannot spawn worker {w}"));
                sh.fail_internal("setup failed: worker spawn failed");
                alive.fetch_sub(1, Relaxed);
                barrier.poison();
            }
        }
        let m = catch_unwind(AssertUnwindSafe(|| monitor(sh, &alive, run_start)));
        if m.is_err() {
            sh.log(LogLevel::Error, "monitor panicked");
            sh.fail_internal("monitor panicked");
            barrier.poison();
        }
    });

    drop(region);
    let reason = lk(&sh.stop_reason)
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    finish(sh, reason, n, Some((bytes_tested, seed)));
}

#[allow(clippy::too_many_arguments)]
fn worker(
    sh: &Shared,
    w: usize,
    n: usize,
    base: usize,
    chunks: &[(usize, usize)],
    barrier: &StepBarrier,
    cpu: Option<&LogicalCpu>,
    seed: u64,
) {
    let env = &sh.env;
    if let Some(c) = cpu {
        if let Err(e) = (env.pin)(c) {
            sh.log(
                LogLevel::Warn,
                format!("worker {w}: pinning to CPU {} failed: {e}", c.index),
            );
        }
    }
    if let Err(e) = (env.set_priority)(sh.cfg.priority) {
        sh.log(
            LogLevel::Warn,
            format!("worker {w}: setting priority failed: {e}"),
        );
    }
    let ts = &sh.threads.get().expect("threads initialised")[w];
    while !barrier.wait(|| sh.advance()) {
        let pass = sh.cur_pass.load(Relaxed);
        let ti = sh.cur_test.load(Relaxed);
        let (start, len) = chunks[(w + pass as usize) % n];
        // SAFETY: `base` points to a live region that outlives the thread
        // scope. `chunks` partitions it into disjoint ranges, and worker w
        // owns chunk (w + pass) % n, a bijection over workers for a fixed
        // pass. `pass` only changes inside the barrier closure while every
        // worker is parked, so concurrently running workers never overlap.
        let slice = unsafe { std::slice::from_raw_parts_mut((base as *mut u64).add(start), len) };
        let ctx = TestContext {
            seed,
            pass,
            thread: w,
            thread_count: n,
            bit_fade_delay: sh.cfg.bit_fade_delay,
            inject_fault_one_in: sh.cfg.inject_fault_one_in,
        };
        let rep = WorkerReporter {
            sh,
            thread: w,
            test_idx: ti,
        };
        ts.current_test.store(ti, Relaxed);
        let before = ts.bytes.load(Relaxed);
        sh.tests[ti].run(slice, &ctx, &rep);
        let expected = len as u64 * 8 * u64::from(sh.tests[ti].sweeps());
        ts.step_ok.store(
            ts.bytes.load(Relaxed).saturating_sub(before) >= expected,
            Relaxed,
        );
    }
    ts.current_test.store(NONE_IDX, Relaxed);
}

fn monitor(sh: &Shared, alive: &AtomicUsize, run_start: Instant) {
    // Workers saturate every CPU; keep the (mostly sleeping) monitor from
    // being starved so duration stops and WHEA polls are timely.
    if let Err(e) = (sh.env.set_priority)(ThreadPriority::Highest) {
        sh.log(
            LogLevel::Warn,
            format!("monitor: setting priority failed: {e}"),
        );
    }
    let mut last_tick = Instant::now();
    let mut last_bytes = 0u64;
    let mut ema: Option<f64> = None;
    let mut last_whea: Option<Instant> = None; // None = poll on first tick
    let mut whea_ok = sh.cfg.monitor_whea;
    let mut whea_fails = 0u32;
    let mut seen: HashSet<(u32, SystemTime, String)> = HashSet::new();

    while alive.load(Relaxed) > 0 {
        std::thread::sleep(Duration::from_millis(10));
        if sh
            .cfg
            .stop
            .duration
            .is_some_and(|d| run_start.elapsed() >= d)
        {
            sh.request_stop("duration reached");
        }
        let dt = last_tick.elapsed();
        if dt >= Duration::from_millis(250) {
            let b = sh.total_bytes();
            let inst = b.saturating_sub(last_bytes) as f64 / dt.as_secs_f64();
            let v = ema.map_or(inst, |e| 0.7 * e + 0.3 * inst);
            ema = Some(v);
            sh.throughput_bits.store(v.to_bits(), Relaxed);
            last_bytes = b;
            last_tick = Instant::now();
        }
        if whea_ok && last_whea.is_none_or(|t| t.elapsed() >= WHEA_POLL_INTERVAL) {
            last_whea = Some(Instant::now());
            whea_ok = poll_whea(sh, &mut seen, &mut whea_fails);
        }
    }
    if sh.cfg.monitor_whea {
        poll_whea(sh, &mut seen, &mut whea_fails);
    }
    sh.throughput_bits.store(0, Relaxed);
}

/// Returns false once polling should stop (3 consecutive failures).
fn poll_whea(sh: &Shared, seen: &mut HashSet<(u32, SystemTime, String)>, fails: &mut u32) -> bool {
    match (sh.env.whea)(sh.started_at) {
        Ok(events) => {
            for e in events {
                if !seen.insert((e.event_id, e.time, e.message.clone())) {
                    continue;
                }
                sh.whea_count.fetch_add(1, Relaxed);
                if e.memory_related {
                    sh.whea_memory.fetch_add(1, Relaxed);
                }
                lk(&sh.whea).push(e.clone());
                let _ = sh.tx.send(Event::Whea(e));
            }
            *fails = 0;
            true
        }
        Err(e) => {
            *fails += 1;
            if *fails == WHEA_MAX_FAILS {
                sh.log(
                    LogLevel::Warn,
                    format!("WHEA monitoring disabled after {WHEA_MAX_FAILS} failures: {e}"),
                );
            }
            *fails < WHEA_MAX_FAILS
        }
    }
}

/// Build the report, mark finished (end time first, Release), then emit
/// `Finished` (last event). `duration` is total wall time since `start`.
fn finish(sh: &Shared, stop_reason: String, threads: usize, alloc: Option<(u64, u64)>) {
    if sh.cfg.keep_awake {
        (sh.env.keep_awake)(false);
    }
    let elapsed = sh.start.elapsed();
    let errors_total = sh.total_errors.load(Relaxed);
    let passes = sh.passes_completed.load(Relaxed);
    let internal = lk(&sh.internal_error).clone();
    let stop_reason = internal.clone().unwrap_or(stop_reason);
    // Setup failures never report a tested size.
    let alloc = alloc.filter(|_| !stop_reason.starts_with("setup failed: "));
    let verdict = if errors_total > 0 || sh.whea_memory.load(Relaxed) > 0 {
        Verdict::Fail
    } else if internal.is_some() {
        Verdict::Incomplete
    } else if passes >= 1 {
        Verdict::Pass
    } else {
        Verdict::Incomplete
    };
    let report = RunReport {
        tool_version: env!("CARGO_PKG_VERSION").to_string(),
        config: sh.cfg.clone(),
        system: (sh.env.snapshot)(),
        seed: alloc.map_or_else(|| sh.seed.load(Relaxed), |a| a.1),
        started_at: sh.started_at,
        duration: elapsed,
        bytes_tested: alloc.map_or(0, |a| a.0),
        threads,
        large_pages: sh.large_pages.load(Relaxed),
        locked: sh.locked.load(Relaxed),
        passes_completed: passes,
        bytes_processed: sh.total_bytes(),
        errors_total,
        errors_by_bit: sh.bits(),
        error_sample: lk(&sh.sample).clone(),
        per_test: sh.test_stats(),
        whea_events: lk(&sh.whea).clone(),
        verdict,
        stop_reason,
    };
    sh.end_nanos.store(elapsed.as_nanos() as u64, Relaxed);
    let _ = sh.report.set(report.clone());
    sh.finished.store(true, Release);
    let _ = sh.tx.send(Event::Finished(Box::new(report)));
}

#[cfg(test)]
mod tests;
