# MemForge — Architecture

MemForge is an open-source, user-mode memory stability tester for Windows,
aimed at people validating overclocked / tuned DDR4 and DDR5 (frequency,
primary/secondary/tertiary timings, voltages). It complements boot-time
testers (MemTest86) with a tester that runs inside Windows, under realistic
thermal and power conditions, on all cores at once.

## Goals

* Find marginal instability fast: saturate the memory controller from every
  logical CPU with a mix of patterns known to expose timing/signal-integrity
  faults (moving inversions, random data, address-in-address, block moves,
  SIMD non-temporal streaming, row-stress).
* Test as much RAM as possible: locked memory (`VirtualLock` +
  `SeLockMemoryPrivilege`), optional 2 MiB large pages, configurable %.
* Make results diagnosable: every error records virtual address, expected,
  actual, XOR mask, test and pass; aggregated per-bit (DQ lane) histogram;
  WHEA corrected-error events from the Windows event log are captured too.
* Be trustworthy: deterministic seeds, a fault-injection self-test that proves
  the detection pipeline works, and unit tests for every algorithm.
* Two front-ends: scriptable CLI (`memforge`) and a GUI (`memforge-gui`).

Non-goals: physical-address mapping (impossible from user mode without a
driver), testing memory reserved by the OS/kernel, ECC injection.

## Workspace layout

```
crates/
  memforge-core/      platform-independent test algorithms (no OS calls)
  memforge-platform/  Windows abstraction: alloc/lock/large pages, CPU
                      topology + affinity, priority, privileges, SMBIOS DIMM
                      info, WHEA event log; portable fallback for non-Windows
  memforge-engine/    orchestration: sizing, worker threads, scheduling,
                      stop conditions, error aggregation, status, reports
  memforge-cli/       `memforge` binary (clap), live console status, JSON report
  memforge-gui/       `memforge-gui` binary (eframe/egui)
docs/                 this file, development plan
```

Dependency graph (strictly layered, no cycles):

```
 memforge-cli ─┐
               ├─► memforge-engine ─┬─► memforge-core
 memforge-gui ─┘                    └─► memforge-platform
```

`core` knows nothing about threads or the OS. `platform` knows nothing about
tests. `engine` is the only crate that knows both. Front-ends only talk to the
engine's public API (`RunConfig`, `start`, `RunHandle`, `Status`, `Event`,
`RunReport`, `system_snapshot`).

## memforge-core

* `MemTest` trait: `run(&self, region: &mut [u64], ctx: &TestContext, rep: &dyn Reporter)`.
  A test writes and verifies a word slice in place. It must:
  * use volatile / intrinsic memory accesses so the compiler cannot elide
    the write-then-read;
  * process in chunks (≈1 MiB), calling `rep.progress(bytes)` and polling
    `rep.should_stop()` between chunks;
  * report every mismatch through `ctx.check(...)`-style helpers which build
    a `MemError` and apply optional fault injection (`ctx.inject_fault_one_in`).
* `TestId` enumerates the algorithms; `create_test(id)` is the factory.
* `rng`: fast deterministic generator (xoshiro256**/splitmix64) seeded from
  `ctx.seed ^ pass ^ thread` so any failure is reproducible.
* SIMD paths (AVX2 / AVX-512 non-temporal stores) selected at runtime via
  `is_x86_feature_detected!`, with scalar fallback.

Algorithms (initial set):

| Id | Purpose |
|----|---------|
| `OwnAddress` | address-in-address, catches addressing/aliasing faults |
| `WalkingOnes` / `WalkingZeros` | single-bit data-line faults |
| `SolidBits` | all-0 / all-1 alternating, checkerboard |
| `MovingInversions` | 0/1 and 8-bit walking patterns, up then down |
| `MovingInversionsRandom` | moving inversions with random words |
| `RandomPattern` | seeded random fill + verify |
| `BlockMove` | large memmove between halves, verify |
| `Modulo20` | modulo-20 offset pattern (beats caching/buffering) |
| `SimdStream` | AVX2/AVX-512 non-temporal stream write + verify, max bandwidth |
| `RowStress` | alternating hammering of distant rows with checker data |
| `BitFade` | fill, idle `bit_fade_delay`, verify (refresh / tREFI issues) |

## memforge-platform

All `unsafe` OS interaction lives here (`windows-sys`). Public API:

* `memory_status()`, `allocate(bytes, &AllocOptions) -> TestRegion`
  (VirtualAlloc, optional `MEM_LARGE_PAGES`, `SetProcessWorkingSetSizeEx` +
  `VirtualLock`, `VirtualFree` on drop). Graceful downgrade: large pages →
  normal pages; locked → unlocked, with the result recorded on the region.
* `cpu_info()` (brand string, features, logical CPUs incl. processor group,
  core id, LLC/CCD id, efficiency class from
  `GetLogicalProcessorInformationEx`), `pin_current_thread`,
  `set_current_thread_priority`.
* `enable_privilege`, `is_elevated`, `keep_system_awake`.
* `dimm_info()` — SMBIOS type 17 via `GetSystemFirmwareTable('RSMB')`.
* `whea_events_since(SystemTime)` — `EvtQuery` on the System log,
  provider `Microsoft-Windows-WHEA-Logger`.
* Non-Windows: `std::alloc` / no-op fallbacks so core+engine are testable in
  Linux CI.

## memforge-engine

* `start(RunConfig) -> RunHandle` spawns:
  * **N worker threads**, one per selected logical CPU, pinned and
    priority-raised. The allocated region is split into N cache-line-aligned
    chunks. Each pass, each test runs on all workers simultaneously
    (barrier-synchronised), and chunk ownership rotates by one per pass so
    every chunk is exercised by different cores/CCDs over time.
  * **monitor thread**: polls WHEA events every few seconds, enforces
    duration limits, emits `Event`s.
* Shared state uses atomics (bytes processed, errors, per-bit histogram,
  per-test counters); a bounded error sample (first 1000 + per-test) is kept
  under a mutex so a catastrophically bad DIMM cannot exhaust memory.
* `RunHandle::status()` returns a cheap `Status` snapshot for UIs;
  `events()` returns a crossbeam `Receiver<Event>`; `stop()` requests a
  cooperative stop; `wait()` joins and returns `RunReport` (serde-serialisable,
  written as JSON by front-ends).
* Verdict: `Pass` (≥1 full pass, 0 errors, 0 WHEA memory events),
  `Fail` (any error), `Incomplete` (stopped before one full pass).

## Front-ends

* **CLI**: `memforge run --memory 90% --threads all --passes 4 --tests default
  --large-pages --json report.json`, `memforge list-tests`, `memforge info`
  (CPU/DIMMs/memory), `memforge self-test` (fault injection must be detected,
  exit code 0/1). Exit codes: 0 pass, 1 errors found, 2 incomplete, 3 setup error, 130 forced exit (second Ctrl+C).
* **GUI**: config panel (memory %, threads, tests checklist, stop conditions,
  options), live dashboard (coverage %, pass, throughput, elapsed, errors,
  per-thread activity, bit-error histogram, error table, WHEA log), save report.

## Error-handling & safety principles

* Never test memory we did not allocate; every pointer comes from `TestRegion`.
* Leave headroom (default 90% of *available* RAM, min 256 MiB free) to avoid
  paging, which would make the test both slow and meaningless.
* A test result is only meaningful if memory stayed resident; the report
  records whether the region was locked.
