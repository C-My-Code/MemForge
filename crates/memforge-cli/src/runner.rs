//! Executes a run with live console output and prints the final summary.

use crate::term::{fmt_bytes, fmt_dur, fmt_dur_precise, fmt_rate, Term};
use memforge_engine::{Event, LogLevel, MemorySize, Phase, RunConfig, RunReport, Status, Verdict};
use std::io::Write;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Console errors printed individually before further ones are suppressed.
const MAX_PRINTED_ERRORS: usize = 20;

pub struct Display {
    pub quiet: bool,
    pub verbose: bool,
}

/// 1-based pass number to display, never above the configured total.
fn pass_display(completed: u64, total: Option<u64>) -> u64 {
    let cur = completed.saturating_add(1);
    total.map_or(cur, |n| cur.min(n.max(1)))
}

fn status_line(st: &Status, cfg: &RunConfig) -> String {
    let passes = match cfg.stop.passes {
        Some(n) => n.to_string(),
        None => "inf".into(),
    };
    let cur_pass = pass_display(st.passes_completed, cfg.stop.passes);
    match st.phase {
        Phase::Allocating => format!("[{}] allocating memory...", fmt_dur(st.elapsed)),
        _ => {
            let test = st.current_test.map(|t| t.name()).unwrap_or("-");
            let stopping = if st.phase == Phase::Stopping {
                " (stopping)"
            } else {
                ""
            };
            format!(
                "[{}] pass {}/{} | {} {:>3.0}% | {} | errors {} | WHEA {}{}",
                fmt_dur(st.elapsed),
                cur_pass,
                passes,
                test,
                (st.pass_progress * 100.0).clamp(0.0, 100.0),
                fmt_rate(st.throughput),
                st.errors,
                st.whea_events,
                stopping
            )
        }
    }
}

/// Run the test to completion, returning the final report.
pub fn execute(cfg: RunConfig, disp: &Display, term: Term) -> Result<RunReport, String> {
    let handle = memforge_engine::start(cfg.clone()).map_err(|e| e.to_string())?;
    let events = handle.events();

    let presses = Arc::new(AtomicU32::new(0));
    {
        let presses = presses.clone();
        let _ = ctrlc::set_handler(move || {
            if presses.fetch_add(1, Ordering::SeqCst) >= 1 {
                std::process::exit(130);
            }
        });
    }

    let live = term.vt && !disp.quiet;
    let mut shown_line = false;
    let mut printed_errors = 0usize;
    let mut suppressed_notice = false;
    let mut stop_sent = false;
    let mut last_draw = Instant::now() - Duration::from_secs(10);
    let mut last_plain = Instant::now();
    let mut report: Option<RunReport> = None;
    let out = std::io::stdout();

    // Print a message above the status line.
    let emit = |msg: &str, shown: &mut bool| {
        let mut o = out.lock();
        if *shown {
            let _ = write!(o, "\r\x1b[2K");
            *shown = false;
        }
        let _ = writeln!(o, "{msg}");
    };

    loop {
        if presses.load(Ordering::SeqCst) > 0 && !stop_sent {
            stop_sent = true;
            handle.stop();
            if !disp.quiet {
                emit(
                    &term.yellow("stopping... (press Ctrl+C again to exit immediately)"),
                    &mut shown_line,
                );
            }
        }

        if let Ok(ev) = events.recv_timeout(Duration::from_millis(100)) {
            {
                match ev {
                    Event::Finished(r) => report = Some(*r),
                    _ if disp.quiet => {}
                    Event::Log { level, message } => {
                        let m = match level {
                            LogLevel::Info => message,
                            LogLevel::Warn => term.yellow(&format!("warning: {message}")),
                            LogLevel::Error => term.red(&format!("error: {message}")),
                        };
                        emit(&m, &mut shown_line);
                    }
                    Event::Started { bytes, threads, large_pages, locked, seed } => emit(
                        &format!(
                            "Started: {} under test, {} threads, large pages: {}, locked: {}, seed: {:#x}",
                            fmt_bytes(bytes),
                            threads,
                            yn(large_pages),
                            yn(locked),
                            seed
                        ),
                        &mut shown_line,
                    ),
                    Event::TestStarted { pass, test } => {
                        if disp.verbose {
                            emit(&term.dim(&format!("pass {} test {}", pass + 1, test.name())), &mut shown_line);
                        }
                    }
                    Event::PassCompleted { pass, errors_so_far } => emit(
                        &format!("Pass {} completed ({} errors so far)", pass + 1, errors_so_far),
                        &mut shown_line,
                    ),
                    Event::MemoryError(e) => {
                        if printed_errors < MAX_PRINTED_ERRORS {
                            printed_errors += 1;
                            let x = e.xor_mask();
                            emit(
                                &term.red(&format!(
                                    "ERROR addr {:#018x} expected {:#018x} actual {:#018x} xor {:#018x} ({} bit{} flipped) test {} thread {}",
                                    e.address, e.expected, e.actual, x, e.flipped_bits(),
                                    if e.flipped_bits() == 1 { "" } else { "s" },
                                    e.test.name(), e.thread
                                )),
                                &mut shown_line,
                            );
                        } else if !suppressed_notice {
                            suppressed_notice = true;
                            emit(
                                &term.red(&format!(
                                    "(further individual errors not printed; first {MAX_PRINTED_ERRORS} shown, counts continue)"
                                )),
                                &mut shown_line,
                            );
                        }
                    }
                    Event::Whea(w) => emit(
                        &term.yellow(&format!(
                            "WHEA event {}{}: {}",
                            w.event_id,
                            if w.memory_related { " (memory)" } else { "" },
                            w.message.lines().next().unwrap_or("")
                        )),
                        &mut shown_line,
                    ),
                }
            }
        }

        if report.is_some() {
            break;
        }
        if handle.is_finished() && events.is_empty() {
            break;
        }

        if !disp.quiet {
            if live && last_draw.elapsed() >= Duration::from_millis(500) {
                last_draw = Instant::now();
                let mut line = status_line(&handle.status(), &cfg);
                let w = crate::term::width().saturating_sub(1);
                if line.chars().count() > w {
                    line = line.chars().take(w).collect();
                }
                let mut o = out.lock();
                let _ = write!(o, "\r\x1b[2K{line}");
                let _ = o.flush();
                shown_line = true;
            } else if !live && last_plain.elapsed() >= Duration::from_secs(10) {
                last_plain = Instant::now();
                println!("{}", status_line(&handle.status(), &cfg));
            }
        }
    }

    if shown_line {
        let _ = write!(out.lock(), "\r\x1b[2K");
    }
    let final_report = handle.wait();
    Ok(report.unwrap_or(final_report))
}

fn yn(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

pub fn verdict_str(term: Term, v: Verdict) -> String {
    match v {
        Verdict::Pass => term.green("PASS"),
        Verdict::Fail => term.red("FAIL"),
        Verdict::Incomplete => term.yellow("INCOMPLETE"),
    }
}

pub const EXIT_SETUP: i32 = 3;

/// The engine reports setup failures (allocation, thread spawn, ...) as a
/// report with nothing tested and a "setup failed: ..." stop reason.
pub fn is_setup_failure(bytes_tested: u64, stop_reason: &str) -> bool {
    bytes_tested == 0 || stop_reason.starts_with("setup failed:")
}

/// Process exit code for a finished run. Errors found (1) take precedence.
pub fn exit_code(v: Verdict, bytes_tested: u64, stop_reason: &str) -> i32 {
    match v {
        Verdict::Fail => 1,
        _ if is_setup_failure(bytes_tested, stop_reason) => EXIT_SETUP,
        Verdict::Pass => 0,
        Verdict::Incomplete => 2,
    }
}

/// Combine the run's exit code with a JSON write failure: a FAIL stays 1,
/// otherwise a failed report write is a setup-class error (3).
pub fn with_json_outcome(code: i32, json_failed: bool) -> i32 {
    if json_failed && code != 1 {
        EXIT_SETUP
    } else {
        code
    }
}

pub fn print_summary(r: &RunReport, term: Term, json_path: Option<&str>) {
    println!();
    println!(
        "{}",
        term.bold("==================== MemForge summary ====================")
    );
    println!(
        "Verdict:        {}   ({})",
        verdict_str(term, r.verdict),
        r.stop_reason
    );
    println!("Duration:       {}", fmt_dur_precise(r.duration));
    let mem = match r.config.memory {
        MemorySize::Bytes(_) => String::new(),
        MemorySize::PercentAvailable(p) => format!(" ({p}% of available)"),
    };
    println!("Memory tested:  {}{}", fmt_bytes(r.bytes_tested), mem);
    println!("Threads:        {}", r.threads);
    println!("Passes:         {}", r.passes_completed);
    let avg = if r.duration.as_secs_f64() > 0.0 {
        r.bytes_processed as f64 / r.duration.as_secs_f64()
    } else {
        0.0
    };
    println!(
        "Processed:      {} (avg {})",
        fmt_bytes(r.bytes_processed),
        fmt_rate(avg)
    );
    let errs = if r.errors_total > 0 {
        term.red(&r.errors_total.to_string())
    } else {
        r.errors_total.to_string()
    };
    println!("Errors:         {errs}");
    println!("Seed:           {:#x}", r.seed);
    println!(
        "Large pages:    {}   Locked in RAM: {}",
        yn(r.large_pages),
        yn(r.locked)
    );
    if !r.locked {
        println!(
            "{}",
            term.yellow(
                "  note: memory was not locked; paging during the run could reduce coverage"
            )
        );
    }

    if !r.per_test.is_empty() {
        println!();
        println!(
            "{:<26} {:>6} {:>10} {:>10}",
            "Test", "Runs", "Errors", "Time"
        );
        for t in &r.per_test {
            let name = t.test.map(|t| t.name()).unwrap_or("?");
            let e = format!("{:>10}", t.errors);
            let e = if t.errors > 0 { term.red(&e) } else { e };
            println!(
                "{:<26} {:>6} {} {:>10}",
                name,
                t.runs,
                e,
                fmt_dur_precise(t.time)
            );
        }
    }

    let bits: Vec<(usize, u64)> = r
        .errors_by_bit
        .iter()
        .copied()
        .enumerate()
        .filter(|&(_, c)| c > 0)
        .collect();
    if !bits.is_empty() {
        println!();
        println!("Bit error histogram (bit: count):");
        let max = bits.iter().map(|b| b.1).max().unwrap_or(1).max(1);
        for (b, c) in &bits {
            let bar = "#".repeat(((*c as f64 / max as f64) * 30.0).ceil() as usize);
            println!("  bit {:>2} (byte {}): {:>10}  {}", b, b / 8, c, bar);
        }
        let mut lanes: Vec<usize> = bits.iter().map(|b| b.0 / 8).collect();
        lanes.dedup();
        if lanes.len() == 1 {
            println!(
                "  hint: all failing bits are in byte lane {}; this suggests a single DQ group / DIMM",
                lanes[0]
            );
        } else if bits.len() > 1 {
            println!("  hint: failing bits span {} byte lanes (more likely timing/voltage/IMC than one DQ group)", lanes.len());
        }
    }

    println!();
    if r.whea_events.is_empty() {
        println!("WHEA events:    none");
    } else {
        let mem = r.whea_events.iter().filter(|w| w.memory_related).count();
        println!(
            "WHEA events:    {} ({} memory-related)",
            r.whea_events.len(),
            mem
        );
        for w in r.whea_events.iter().take(10) {
            println!(
                "  id {}{}: {}",
                w.event_id,
                if w.memory_related { " [memory]" } else { "" },
                w.message.lines().next().unwrap_or("")
            );
        }
    }
    if let Some(p) = json_path {
        println!("JSON report:    {p}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes() {
        assert_eq!(exit_code(Verdict::Pass, 1 << 20, "passes reached"), 0);
        assert_eq!(exit_code(Verdict::Fail, 1 << 20, "max errors"), 1);
        assert_eq!(exit_code(Verdict::Incomplete, 1 << 20, "user stop"), 2);
        assert_eq!(
            exit_code(Verdict::Incomplete, 0, "setup failed: allocation failed"),
            3
        );
        assert_eq!(exit_code(Verdict::Incomplete, 0, "user stop"), 3);
        assert_eq!(
            exit_code(Verdict::Incomplete, 1 << 20, "setup failed: spawn"),
            3
        );
        assert_eq!(exit_code(Verdict::Fail, 1 << 20, "setup failed: x"), 1);
    }

    #[test]
    fn json_failure_precedence() {
        assert_eq!(with_json_outcome(1, true), 1);
        assert_eq!(with_json_outcome(0, true), 3);
        assert_eq!(with_json_outcome(2, true), 3);
        assert_eq!(with_json_outcome(0, false), 0);
    }

    #[test]
    fn pass_number_clamped() {
        assert_eq!(pass_display(0, Some(3)), 1);
        assert_eq!(pass_display(3, Some(3)), 3);
        assert_eq!(pass_display(5, Some(3)), 3);
        assert_eq!(pass_display(4, None), 5);
    }
}
