//! `memforge` command-line front-end.

mod parse;
mod runner;
mod term;

use clap::{Parser, Subcommand, ValueEnum};
use memforge_core::TestId;
use memforge_engine::{MemorySize, RunConfig, RunReport, StopConditions, SystemSnapshot, Verdict};
use memforge_platform::ThreadPriority;
use parse::ThreadsArg;
use runner::{exit_code, is_setup_failure, with_json_outcome, Display, EXIT_SETUP};
use std::collections::BTreeMap;
use std::time::Duration;
use term::{fmt_bytes, Term};

const AFTER_HELP: &str = "\
EXIT CODES:
  0  run completed with no errors (PASS), or self-test detected the injected faults
  1  memory errors (or memory-related WHEA events) were found (FAIL), or self-test
     detection is broken
  2  run stopped before completing a full pass, with no errors (INCOMPLETE), or
     self-test was inconclusive (stopped early)
  3  setup or usage error (bad arguments, allocation failure, not enough memory,
     report file not writable, ...), including a self-test that could not be set up
  130  second Ctrl+C: immediate exit, the run is abandoned (first Ctrl+C stops gracefully)";

#[derive(Parser)]
#[command(name = "memforge", version, about = "Memory stability tester for overclocked DDR4/DDR5", after_help = AFTER_HELP)]
struct Cli {
    /// Verbose output (show each test start)
    #[arg(short, long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a memory stability test
    #[command(after_help = AFTER_HELP)]
    Run(RunArgs),
    /// List available tests
    ListTests,
    /// Show CPU, memory and DIMM information
    Info {
        /// Print the system snapshot as JSON
        #[arg(long)]
        json: bool,
    },
    /// Verify the error-detection pipeline using fault injection (does not test your RAM)
    #[command(after_help = AFTER_HELP)]
    SelfTest {
        /// Amount of memory to use
        #[arg(long, default_value = "256MiB", value_parser = parse::parse_size)]
        memory: MemorySize,
        /// Worker threads: a number or 'all'
        #[arg(long, default_value = "all", value_parser = parse::parse_threads_arg)]
        threads: ThreadsArg,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum PriorityArg {
    Normal,
    AboveNormal,
    Highest,
}

#[derive(clap::Args)]
struct RunArgs {
    /// Memory to test: a percentage of available RAM (90%) or a size (16GiB, 512MiB)
    #[arg(long, default_value = "90%", value_parser = parse::parse_size)]
    memory: MemorySize,
    /// Worker threads: a number or 'all'
    #[arg(long, default_value = "all", value_parser = parse::parse_threads_arg)]
    threads: ThreadsArg,
    /// Tests: 'default', 'all', or a comma-separated list (see `list-tests`)
    #[arg(long, default_value = "default", value_parser = parse::parse_tests)]
    tests: std::vec::Vec<TestId>,
    /// Number of passes (0 = unlimited)
    #[arg(long, default_value_t = 1)]
    passes: u64,
    /// Stop after this long (30s, 15m, 8h, 1h30m)
    #[arg(long, value_parser = parse::parse_run_duration)]
    duration: Option<Duration>,
    /// Stop after this many errors (at least 1)
    #[arg(long, value_parser = parse::parse_max_errors)]
    max_errors: Option<u64>,
    /// Stop at the first error (same as --max-errors 1)
    #[arg(long)]
    stop_on_error: bool,
    /// Use large pages (needs SeLockMemoryPrivilege; run elevated)
    #[arg(long)]
    large_pages: bool,
    /// Do not lock the memory in RAM
    #[arg(long)]
    no_lock: bool,
    /// Do not pin worker threads to CPUs
    #[arg(long)]
    no_pin: bool,
    /// Worker thread priority
    #[arg(long, value_enum, default_value = "above-normal")]
    priority: PriorityArg,
    /// Seed (decimal or 0x-hex); random if omitted
    #[arg(long, value_parser = parse::parse_seed)]
    seed: Option<u64>,
    /// Idle time for the bit-fade test (30s, 5m, ...)
    #[arg(long, value_parser = parse::parse_duration)]
    bit_fade_delay: Option<Duration>,
    /// Do not monitor the WHEA event log
    #[arg(long)]
    no_whea: bool,
    /// Write the full report as JSON to this path
    #[arg(long, value_name = "PATH")]
    json: Option<String>,
    /// Only print the final summary
    #[arg(long, short)]
    quiet: bool,
    /// Inject a simulated bit flip roughly every N verified words (testing aid)
    #[arg(long, hide = true, value_name = "N")]
    inject_faults: Option<u64>,
}

fn main() {
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) => {
            let code = if e.use_stderr() { EXIT_SETUP } else { 0 };
            let _ = e.print();
            std::process::exit(code);
        }
    };
    let term = Term::detect();
    let code = match cli.command {
        Command::Run(a) => cmd_run(a, cli.verbose, term),
        Command::ListTests => cmd_list_tests(),
        Command::Info { json } => cmd_info(json, term),
        Command::SelfTest { memory, threads } => {
            cmd_self_test(memory, threads.0, cli.verbose, term)
        }
    };
    std::process::exit(code);
}

fn die(msg: &str) -> i32 {
    eprintln!("error: {msg}");
    EXIT_SETUP
}

fn cmd_run(a: RunArgs, verbose: bool, term: Term) -> i32 {
    let max_errors = if a.stop_on_error {
        Some(1)
    } else {
        a.max_errors
    };
    let defaults = RunConfig::default();
    let cfg = RunConfig {
        memory: a.memory,
        threads: a.threads.0,
        tests: a.tests,
        stop: StopConditions {
            passes: if a.passes == 0 { None } else { Some(a.passes) },
            duration: a.duration,
            max_errors,
        },
        large_pages: a.large_pages,
        lock_memory: !a.no_lock,
        pin_threads: !a.no_pin,
        priority: match a.priority {
            PriorityArg::Normal => ThreadPriority::Normal,
            PriorityArg::AboveNormal => ThreadPriority::AboveNormal,
            PriorityArg::Highest => ThreadPriority::Highest,
        },
        seed: a.seed,
        bit_fade_delay: a.bit_fade_delay.unwrap_or(defaults.bit_fade_delay),
        inject_fault_one_in: a.inject_faults.filter(|&n| n > 0),
        monitor_whea: !a.no_whea,
        keep_awake: true,
    };

    // Make sure the report can be created before spending hours testing.
    if let Some(path) = &a.json {
        if let Err(e) = std::fs::File::create(path) {
            return die(&format!("cannot create JSON report file {path}: {e}"));
        }
    }

    let disp = Display {
        quiet: a.quiet,
        verbose,
    };
    let report = match runner::execute(cfg, &disp, term) {
        Ok(r) => r,
        Err(e) => return die(&e),
    };

    let mut json_written = None;
    let mut json_failed = false;
    if let Some(path) = &a.json {
        match std::fs::write(path, report.to_json_pretty()) {
            Ok(()) => json_written = Some(path.as_str()),
            Err(e) => {
                json_failed = true;
                eprintln!("error: could not write JSON report to {path}: {e}");
            }
        }
    }
    runner::print_summary(&report, term, json_written);
    if is_setup_failure(report.bytes_tested, &report.stop_reason) && report.verdict != Verdict::Fail
    {
        eprintln!("setup error: {}", report.stop_reason);
    }
    with_json_outcome(
        exit_code(report.verdict, report.bytes_tested, &report.stop_reason),
        json_failed,
    )
}

fn cmd_list_tests() -> i32 {
    let default = memforge_core::default_test_suite();
    for &t in TestId::ALL {
        println!(
            "{:<26} {}{}",
            t.name(),
            t.title(),
            if default.contains(&t) {
                "  [default]"
            } else {
                ""
            }
        );
        println!("{:<26} {}", "", t.description());
    }
    println!();
    println!("Use --tests default | all | name1,name2,...  ([default] marks the default suite)");
    0
}

fn cmd_info(json: bool, term: Term) -> i32 {
    let snap = memforge_engine::system_snapshot();
    if json {
        match serde_json::to_string_pretty(&snap) {
            Ok(s) => {
                println!("{s}");
                return 0;
            }
            Err(e) => return die(&e.to_string()),
        }
    }
    print_info(&snap, term);
    0
}

fn print_info(s: &SystemSnapshot, term: Term) {
    println!("{}", term.bold("System"));
    println!("  OS:             {}", s.os);
    println!(
        "  Elevated:       {}",
        if s.elevated { "yes" } else { "no" }
    );
    println!();
    println!("{}", term.bold("CPU"));
    println!("  Brand:          {}", s.cpu.brand);
    println!(
        "  Cores/threads:  {} cores / {} logical",
        s.cpu.physical_cores,
        s.cpu.logical.len()
    );
    let f = &s.cpu.features;
    let yn = |b: bool| if b { "yes" } else { "no" };
    println!(
        "  Features:       SSE2 {}, AVX2 {}, AVX-512F {}",
        yn(f.sse2),
        yn(f.avx2),
        yn(f.avx512f)
    );
    let mut llc: BTreeMap<u32, usize> = BTreeMap::new();
    for l in &s.cpu.logical {
        *llc.entry(l.llc).or_default() += 1;
    }
    println!("  LLC domains:    {}", llc.len());
    for (id, n) in &llc {
        println!("    LLC {id}: {n} logical CPUs");
    }
    println!();
    println!("{}", term.bold("Memory"));
    println!("  Total:          {}", fmt_bytes(s.memory.total_physical));
    println!(
        "  Available:      {}",
        fmt_bytes(s.memory.available_physical)
    );
    println!("  Page size:      {}", fmt_bytes(s.memory.page_size as u64));
    match s.memory.large_page_size {
        Some(l) => println!("  Large page:     {}", fmt_bytes(l as u64)),
        None => println!("  Large page:     unsupported"),
    }
    if !s.elevated {
        println!(
            "  {}",
            term.yellow(
                "note: not elevated. Large pages (--large-pages) need SeLockMemoryPrivilege, which"
            )
        );
        println!(
            "  {}",
            term.yellow("      requires an Administrator shell with the privilege granted; DIMM and WHEA data may be limited.")
        );
    }
    println!();
    println!("{}", term.bold("DIMMs"));
    if s.dimms.is_empty() {
        println!("  (no DIMM information available)");
    } else {
        println!(
            "  {:<14} {:<14} {:<20} {:>9} {:<6} {:>8} {:>10} {:>7} {:>4}",
            "Locator",
            "Manufacturer",
            "Part number",
            "Size",
            "Type",
            "Rated",
            "Configured",
            "Voltage",
            "Rank"
        );
        for d in &s.dimms {
            let v = if d.configured_voltage_mv > 0 {
                format!("{:.3} V", d.configured_voltage_mv as f64 / 1000.0)
            } else {
                "?".into()
            };
            println!(
                "  {:<14} {:<14} {:<20} {:>9} {:<6} {:>8} {:>10} {:>7} {:>4}",
                d.locator,
                d.manufacturer,
                d.part_number,
                fmt_bytes(d.size_bytes),
                d.memory_type,
                format!("{} MT/s", d.speed_mts),
                format!("{} MT/s", d.configured_speed_mts),
                v,
                d.rank
            );
        }
    }
}

const SELF_TEST_MIN_BYTES: u64 = 16 << 20;
const PASSES_REACHED: &str = "passes reached";

/// Fault rate for the self-test: roughly 64 faults per thread per verified
/// sweep, so every test is near-certain to see at least one.
fn self_test_fault_one_in(bytes: u64, threads: usize) -> u64 {
    let words_per_thread = bytes / 8 / threads.max(1) as u64;
    (words_per_thread / 64).clamp(1_000, 1_000_000)
}

#[derive(Debug, PartialEq, Eq)]
enum SelfTestOutcome {
    /// Setup failed; nothing was tested.
    Setup(String),
    /// Run ended early; no conclusion can be drawn.
    Inconclusive(String),
    /// Detection pipeline is broken.
    Broken(String),
    /// All `n` tests that ran detected faults.
    Detected(usize),
}

impl SelfTestOutcome {
    fn exit_code(&self) -> i32 {
        match self {
            SelfTestOutcome::Detected(_) => 0,
            SelfTestOutcome::Broken(_) => 1,
            SelfTestOutcome::Inconclusive(_) => 2,
            SelfTestOutcome::Setup(_) => EXIT_SETUP,
        }
    }
}

/// Pure decision logic. `per_test` is `(name, runs, errors)`.
fn self_test_decision(
    verdict: Verdict,
    stop_reason: &str,
    bytes_tested: u64,
    errors_total: u64,
    sample_len: usize,
    per_test: &[(&str, u64, u64)],
) -> SelfTestOutcome {
    if is_setup_failure(bytes_tested, stop_reason) {
        return SelfTestOutcome::Setup(stop_reason.to_string());
    }
    if stop_reason != PASSES_REACHED {
        return SelfTestOutcome::Inconclusive(format!("run ended early ({stop_reason})"));
    }
    let ran: Vec<_> = per_test.iter().filter(|t| t.1 > 0).collect();
    if ran.is_empty() {
        return SelfTestOutcome::Broken("no tests ran".into());
    }
    let missed: Vec<&str> = ran.iter().filter(|t| t.2 == 0).map(|t| t.0).collect();
    if !missed.is_empty() {
        return SelfTestOutcome::Broken(format!(
            "no injected faults were detected in: {}",
            missed.join(", ")
        ));
    }
    if verdict != Verdict::Fail {
        return SelfTestOutcome::Broken(format!(
            "faults were counted but the verdict was {verdict:?}, not Fail"
        ));
    }
    if errors_total == 0 {
        return SelfTestOutcome::Broken("report contains no errors".into());
    }
    if sample_len == 0 {
        return SelfTestOutcome::Broken("report contains no error details".into());
    }
    SelfTestOutcome::Detected(ran.len())
}

fn cmd_self_test(memory: MemorySize, threads: Option<usize>, verbose: bool, term: Term) -> i32 {
    let snap = memforge_engine::system_snapshot();
    let bytes = match memory {
        MemorySize::Bytes(b) => b,
        MemorySize::PercentAvailable(p) => {
            (snap.memory.available_physical as f64 * p as f64 / 100.0) as u64
        }
    };
    if bytes < SELF_TEST_MIN_BYTES {
        return die(&format!(
            "self-test needs at least 16MiB of memory (got {}); use --memory 16MiB or more",
            fmt_bytes(bytes)
        ));
    }
    let n_threads = threads.unwrap_or(snap.cpu.logical.len()).max(1);
    let one_in = self_test_fault_one_in(bytes, n_threads);

    println!("MemForge self-test: running with simulated bit flips (fault injection).");
    println!("This validates the error DETECTION and REPORTING pipeline, not your RAM.");
    println!();
    // Slow tests are left out so the self-test finishes quickly.
    let tests: Vec<TestId> = memforge_core::default_test_suite()
        .into_iter()
        .filter(|t| !matches!(t, TestId::RowStress | TestId::BitFade))
        .collect();
    let cfg = RunConfig {
        memory,
        threads,
        tests,
        stop: StopConditions {
            passes: Some(1),
            duration: None,
            max_errors: None,
        },
        lock_memory: false,
        monitor_whea: false,
        keep_awake: false,
        inject_fault_one_in: Some(one_in),
        ..RunConfig::default()
    };
    let disp = Display {
        quiet: false,
        verbose,
    };
    let report: RunReport = match runner::execute(cfg, &disp, term) {
        Ok(r) => r,
        Err(e) => return die(&e),
    };
    runner::print_summary(&report, term, None);
    println!();

    let per_test: Vec<(&str, u64, u64)> = report
        .per_test
        .iter()
        .map(|t| (t.test.map(|t| t.name()).unwrap_or("?"), t.runs, t.errors))
        .collect();
    let outcome = self_test_decision(
        report.verdict,
        &report.stop_reason,
        report.bytes_tested,
        report.errors_total,
        report.error_sample.len(),
        &per_test,
    );
    match &outcome {
        SelfTestOutcome::Setup(why) => {
            eprintln!("setup error: {why}");
            println!("{}", term.red("Self-test could not run: setup error."));
        }
        SelfTestOutcome::Inconclusive(why) => {
            println!(
                "{}",
                term.yellow(&format!("Self-test INCONCLUSIVE: {why}."))
            );
        }
        SelfTestOutcome::Broken(why) => {
            println!("{}", term.red(&format!("Self-test FAILED: {why}")));
        }
        SelfTestOutcome::Detected(n) => {
            println!(
                "{}",
                term.green(&format!(
                    "Self-test PASSED: injected faults were detected in all {n} tests that ran."
                ))
            );
            println!("(The errors above are simulated; they say nothing about your memory.)");
        }
    }
    outcome.exit_code()
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: [(&str, u64, u64); 2] = [("a", 1, 5), ("b", 1, 2)];

    fn decide(
        v: Verdict,
        why: &str,
        bytes: u64,
        errs: u64,
        sample: usize,
        t: &[(&str, u64, u64)],
    ) -> i32 {
        self_test_decision(v, why, bytes, errs, sample, t).exit_code()
    }

    #[test]
    fn self_test_decisions() {
        assert_eq!(
            decide(Verdict::Fail, "passes reached", 1 << 28, 7, 7, &GOOD),
            0
        );
        assert_eq!(decide(Verdict::Fail, "user stop", 1 << 28, 7, 7, &GOOD), 2);
        assert_eq!(
            decide(Verdict::Incomplete, "setup failed: x", 0, 0, 0, &[]),
            3
        );
        assert_eq!(
            decide(Verdict::Incomplete, "passes reached", 0, 0, 0, &[]),
            3
        );
        assert_eq!(
            decide(
                Verdict::Fail,
                "passes reached",
                1 << 28,
                5,
                5,
                &[("a", 1, 5), ("b", 1, 0)]
            ),
            1
        );
        assert_eq!(
            decide(
                Verdict::Fail,
                "passes reached",
                1 << 28,
                5,
                5,
                &[("a", 1, 5), ("b", 0, 0)]
            ),
            0
        );
        assert_eq!(
            decide(
                Verdict::Fail,
                "passes reached",
                1 << 28,
                5,
                5,
                &[("b", 0, 0)]
            ),
            1
        );
        assert_eq!(
            decide(Verdict::Pass, "passes reached", 1 << 28, 7, 7, &GOOD),
            1
        );
        assert_eq!(
            decide(Verdict::Fail, "passes reached", 1 << 28, 0, 7, &GOOD),
            1
        );
        assert_eq!(
            decide(Verdict::Fail, "passes reached", 1 << 28, 7, 0, &GOOD),
            1
        );
    }

    #[test]
    fn fault_rate_scales_with_size() {
        assert_eq!(
            self_test_fault_one_in(256 << 20, 16),
            (256u64 << 20) / 8 / 16 / 64
        );
        assert_eq!(self_test_fault_one_in(16 << 20, 64), 1_000);
        assert_eq!(self_test_fault_one_in(1 << 40, 1), 1_000_000);
        assert_eq!(self_test_fault_one_in(1 << 30, 0), 1_000_000);
    }
}
