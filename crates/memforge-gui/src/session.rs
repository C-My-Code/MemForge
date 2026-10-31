//! A single test run as seen by the UI: engine handle plus accumulated
//! events, history and the final report. No rendering code here.

use memforge_engine::memforge_core::MemError;
use memforge_engine::{
    Event, LogLevel, Phase, Receiver, RunConfig, RunHandle, RunReport, Status, Verdict,
};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

const MAX_ERRORS_KEPT: usize = 500;
const MAX_LOG_KEPT: usize = 2000;
const MAX_HISTORY: usize = 300;
const HISTORY_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LogKind {
    Info,
    Warn,
    Error,
    Pass,
    Whea,
}

pub struct LogLine {
    pub elapsed: Duration,
    pub kind: LogKind,
    pub text: String,
}

/// What the big indicator should show.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Headline {
    Allocating,
    Running,
    Stopping,
    /// Errors seen but run still in progress.
    Failing,
    Pass,
    Fail,
    Incomplete,
    /// Nothing could be set up (allocation failed, not enough memory, ...).
    SetupError,
}

/// The engine reports setup failures as a report with nothing tested and a
/// "setup failed: ..." stop reason.
pub fn is_setup_failure(bytes_tested: u64, stop_reason: &str) -> bool {
    bytes_tested == 0 || stop_reason.starts_with("setup failed:")
}

pub struct Session {
    handle: Option<RunHandle>,
    rx: Receiver<Event>,
    pub config: RunConfig,
    pub status: Option<Status>,
    /// Throughput history in GB/s.
    pub history: Vec<f32>,
    last_sample: Option<Instant>,
    pub errors: VecDeque<MemError>,
    pub log: VecDeque<LogLine>,
    pub report: Option<Box<RunReport>>,
    pub seed: Option<u64>,
    stop_requested: bool,
}

impl Session {
    pub fn new(handle: RunHandle, config: RunConfig) -> Self {
        let rx = handle.events();
        Session {
            handle: Some(handle),
            rx,
            config,
            status: None,
            history: Vec::new(),
            last_sample: None,
            errors: VecDeque::new(),
            log: VecDeque::new(),
            report: None,
            seed: None,
            stop_requested: false,
        }
    }

    pub fn stop(&mut self) {
        self.stop_requested = true;
        if let Some(h) = &self.handle {
            h.stop();
        }
    }

    pub fn is_finished(&self) -> bool {
        self.report.is_some()
            || (self.handle.as_ref().is_some_and(|h| h.is_finished())
                && self
                    .status
                    .as_ref()
                    .is_some_and(|s| s.phase == Phase::Finished))
    }

    /// Run is active (not finished); used to lock the config panel.
    pub fn is_active(&self) -> bool {
        !self.is_finished()
    }

    fn push_log(&mut self, kind: LogKind, text: String) {
        let elapsed = self.status.as_ref().map(|s| s.elapsed).unwrap_or_default();
        if self.log.len() >= MAX_LOG_KEPT {
            self.log.pop_front();
        }
        self.log.push_back(LogLine {
            elapsed,
            kind,
            text,
        });
    }

    /// Poll status and drain events. Call once per frame.
    pub fn poll(&mut self) {
        if let Some(h) = &self.handle {
            let st = h.status();
            let due = self
                .last_sample
                .is_none_or(|t| t.elapsed() >= HISTORY_INTERVAL);
            if due && st.phase == Phase::Running {
                self.last_sample = Some(Instant::now());
                if self.history.len() >= MAX_HISTORY {
                    self.history.remove(0);
                }
                self.history.push((st.throughput / 1e9) as f32);
            }
            self.status = Some(st);
        }
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Event::Log { level, message } => {
                    let kind = match level {
                        LogLevel::Info => LogKind::Info,
                        LogLevel::Warn => LogKind::Warn,
                        LogLevel::Error => LogKind::Error,
                    };
                    self.push_log(kind, message);
                }
                Event::Started {
                    bytes,
                    threads,
                    large_pages,
                    locked,
                    seed,
                } => {
                    self.seed = Some(seed);
                    self.push_log(
                        LogKind::Info,
                        format!(
                            "Started: {} on {threads} threads (large pages: {large_pages}, locked: {locked}, seed: {seed:#x})",
                            crate::format::bytes(bytes)
                        ),
                    );
                }
                Event::TestStarted { pass, test } => {
                    self.push_log(
                        LogKind::Info,
                        format!("Pass {} - {}", pass + 1, test.title()),
                    );
                }
                Event::PassCompleted {
                    pass,
                    errors_so_far,
                } => {
                    self.push_log(
                        LogKind::Pass,
                        format!(
                            "Pass {} completed ({errors_so_far} errors so far)",
                            pass + 1
                        ),
                    );
                }
                Event::MemoryError(e) => {
                    if self.errors.len() >= MAX_ERRORS_KEPT {
                        self.errors.pop_front();
                    }
                    self.errors.push_back(e);
                }
                Event::Whea(w) => {
                    self.push_log(
                        LogKind::Whea,
                        format!(
                            "WHEA #{} [{}]{}: {}",
                            w.event_id,
                            w.level,
                            if w.memory_related { " (memory)" } else { "" },
                            w.message
                        ),
                    );
                }
                Event::Finished(r) => {
                    if self.report.is_none() {
                        self.set_report(r);
                    }
                }
            }
        }
        // The engine marks itself finished before it sends `Finished`, so the
        // event can lag behind. Once finished, `wait()` returns the report
        // immediately; never drop the handle without having it.
        if self.report.is_none() && self.handle.as_ref().is_some_and(|h| h.is_finished()) {
            if let Some(h) = self.handle.take() {
                self.status = Some(h.status());
                let r = h.wait();
                self.set_report(Box::new(r));
            }
        }
        // Release the handle (joins workers) once the engine is done.
        if self.report.is_some() && self.handle.as_ref().is_some_and(|h| h.is_finished()) {
            if let Some(h) = self.handle.take() {
                self.status = Some(h.status());
            }
        }
    }

    fn set_report(&mut self, r: Box<RunReport>) {
        let line = format!("Finished: {:?} ({})", r.verdict, r.stop_reason);
        self.report = Some(r);
        self.push_log(LogKind::Info, line);
    }

    pub fn headline(&self) -> Headline {
        if let Some(r) = &self.report {
            return match r.verdict {
                Verdict::Incomplete if is_setup_failure(r.bytes_tested, &r.stop_reason) => {
                    Headline::SetupError
                }
                Verdict::Pass => Headline::Pass,
                Verdict::Fail => Headline::Fail,
                Verdict::Incomplete => Headline::Incomplete,
            };
        }
        let Some(st) = &self.status else {
            return Headline::Allocating;
        };
        if st.errors > 0 {
            return Headline::Failing;
        }
        match st.phase {
            Phase::Allocating => Headline::Allocating,
            Phase::Running => Headline::Running,
            Phase::Stopping => Headline::Stopping,
            Phase::Finished => Headline::Incomplete,
        }
    }

    /// Overall progress 0..=1 if the run is bounded by passes and/or duration.
    pub fn overall_progress(&self) -> Option<f64> {
        let st = self.status.as_ref()?;
        let by_passes = self.config.stop.passes.map(|n| {
            ((st.passes_completed as f64 + st.pass_progress) / n.max(1) as f64).clamp(0.0, 1.0)
        });
        let by_time = self
            .config
            .stop
            .duration
            .map(|d| (st.elapsed.as_secs_f64() / d.as_secs_f64().max(1e-9)).clamp(0.0, 1.0));
        match (by_passes, by_time) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        }
    }

    /// Plain-text summary for the clipboard.
    pub fn summary_text(&self) -> Option<String> {
        use crate::format as f;
        let r = self.report.as_ref()?;
        let mut s = String::new();
        s.push_str(&format!(
            "MemForge {} - verdict: {:?}\n",
            r.tool_version, r.verdict
        ));
        s.push_str(&format!("Stop reason: {}\n", r.stop_reason));
        s.push_str(&format!("CPU: {}\n", r.system.cpu.brand));
        s.push_str(&format!(
            "Memory tested: {} on {} threads (large pages: {}, locked: {})\n",
            f::bytes(r.bytes_tested),
            r.threads,
            r.large_pages,
            r.locked
        ));
        s.push_str(&format!(
            "Duration: {}  Passes: {}  Seed: {:#x}\n",
            f::duration(r.duration),
            r.passes_completed,
            r.seed
        ));
        s.push_str(&format!(
            "Errors: {}  WHEA events: {}\n",
            r.errors_total,
            r.whea_events.len()
        ));
        let bits: Vec<String> = r
            .errors_by_bit
            .iter()
            .enumerate()
            .filter(|(_, c)| **c > 0)
            .map(|(b, c)| format!("bit {b}: {c}"))
            .collect();
        if !bits.is_empty() {
            s.push_str(&format!("Flipped bits: {}\n", bits.join(", ")));
        }
        for t in &r.per_test {
            if let Some(id) = t.test {
                s.push_str(&format!(
                    "  {:<28} runs {:>4}  errors {:>6}  time {}\n",
                    id.title(),
                    t.runs,
                    t.errors,
                    f::duration(t.time)
                ));
            }
        }
        Some(s)
    }
}
