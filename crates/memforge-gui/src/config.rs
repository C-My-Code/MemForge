//! UI-side configuration state and its conversion to an engine `RunConfig`.

use memforge_engine::memforge_core::{default_test_suite, TestId};
use memforge_engine::memforge_platform::ThreadPriority;
use memforge_engine::{MemorySize, RunConfig, StopConditions};
use std::collections::BTreeSet;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemMode {
    Percent,
    ExactGib,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeUnit {
    Minutes,
    Hours,
}

#[derive(Clone, Debug)]
pub struct ConfigState {
    pub mem_mode: MemMode,
    pub mem_percent: f32,
    pub mem_gib: f64,
    pub threads_all: bool,
    pub threads_custom: usize,
    pub tests: BTreeSet<TestId>,
    pub passes_enabled: bool,
    pub passes: u64,
    pub duration_enabled: bool,
    pub duration_value: f64,
    pub duration_unit: TimeUnit,
    pub stop_on_first_error: bool,
    pub max_errors_enabled: bool,
    pub max_errors: u64,
    pub large_pages: bool,
    pub lock_memory: bool,
    pub pin_threads: bool,
    pub priority: ThreadPriority,
    pub monitor_whea: bool,
    pub keep_awake: bool,
    pub seed_text: String,
    pub bit_fade_secs: u64,
}

impl Default for ConfigState {
    fn default() -> Self {
        let d = RunConfig::default();
        ConfigState {
            mem_mode: MemMode::Percent,
            mem_percent: 90.0,
            mem_gib: 4.0,
            threads_all: true,
            threads_custom: 4,
            tests: default_test_suite().into_iter().collect(),
            passes_enabled: true,
            passes: 1,
            duration_enabled: false,
            duration_value: 60.0,
            duration_unit: TimeUnit::Minutes,
            stop_on_first_error: false,
            max_errors_enabled: false,
            max_errors: 100,
            large_pages: d.large_pages,
            lock_memory: d.lock_memory,
            pin_threads: d.pin_threads,
            priority: d.priority,
            monitor_whea: d.monitor_whea,
            keep_awake: d.keep_awake,
            seed_text: String::new(),
            bit_fade_secs: d.bit_fade_delay.as_secs(),
        }
    }
}

/// Parse a seed: empty = random, `0x...` = hex, otherwise decimal.
pub fn parse_seed(s: &str) -> Result<Option<u64>, String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    let parsed = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(h) => u64::from_str_radix(h, 16),
        None => s.parse::<u64>(),
    };
    parsed
        .map(Some)
        .map_err(|_| format!("invalid seed '{s}' (use decimal or 0x hex)"))
}

/// Memory kept free for the OS when choosing an exact size.
const RAM_RESERVE_BYTES: u64 = 256 << 20;
const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// Largest exact size (GiB) selectable given the available RAM, if known.
pub fn max_exact_gib(available: Option<u64>) -> Option<f64> {
    available.map(|a| a.saturating_sub(RAM_RESERVE_BYTES) as f64 / GIB)
}

impl ConfigState {
    /// Validate and convert. `available` is the currently available physical
    /// RAM in bytes, when known; exact sizes above it (minus a reserve) are rejected.
    pub fn to_run_config(&self, available: Option<u64>) -> Result<RunConfig, String> {
        if self.tests.is_empty() {
            return Err("select at least one test".into());
        }
        let memory = match self.mem_mode {
            MemMode::Percent => {
                if !(self.mem_percent > 0.0 && self.mem_percent <= 100.0) {
                    return Err("memory percentage must be in (0, 100]".into());
                }
                MemorySize::PercentAvailable(self.mem_percent)
            }
            MemMode::ExactGib => {
                if self.mem_gib <= 0.0 || !self.mem_gib.is_finite() {
                    return Err("memory size must be positive".into());
                }
                if let Some(max) = max_exact_gib(available) {
                    if self.mem_gib > max {
                        return Err(format!(
                            "memory size {:.2} GiB exceeds the {:.2} GiB available (minus a 256 MiB reserve)",
                            self.mem_gib, max
                        ));
                    }
                }
                MemorySize::Bytes((self.mem_gib * GIB) as u64)
            }
        };
        let duration = self.duration_enabled.then(|| {
            let mult = match self.duration_unit {
                TimeUnit::Minutes => 60.0,
                TimeUnit::Hours => 3600.0,
            };
            Duration::from_secs_f64(self.duration_value.max(0.0) * mult)
        });
        if matches!(duration, Some(d) if d.is_zero()) {
            return Err("duration must be greater than zero".into());
        }
        if self.passes_enabled && self.passes == 0 {
            return Err("passes must be at least 1".into());
        }
        let max_errors = if self.stop_on_first_error {
            Some(1)
        } else if self.max_errors_enabled {
            Some(self.max_errors.max(1))
        } else {
            None
        };
        Ok(RunConfig {
            memory,
            threads: (!self.threads_all).then_some(self.threads_custom.max(1)),
            // Keep canonical execution order.
            tests: TestId::ALL
                .iter()
                .copied()
                .filter(|t| self.tests.contains(t))
                .collect(),
            stop: StopConditions {
                passes: self.passes_enabled.then_some(self.passes),
                duration,
                max_errors,
            },
            large_pages: self.large_pages,
            lock_memory: self.lock_memory,
            pin_threads: self.pin_threads,
            priority: self.priority,
            seed: parse_seed(&self.seed_text)?,
            bit_fade_delay: Duration::from_secs(self.bit_fade_secs),
            inject_fault_one_in: None,
            monitor_whea: self.monitor_whea,
            keep_awake: self.keep_awake,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_matches_engine_default() {
        let c = ConfigState::default().to_run_config(None).unwrap();
        let d = RunConfig::default();
        assert_eq!(c.memory, d.memory);
        assert_eq!(c.threads, d.threads);
        assert_eq!(c.tests, d.tests);
        assert_eq!(c.stop, d.stop);
        assert_eq!(c.priority, d.priority);
        assert_eq!(c.bit_fade_delay, d.bit_fade_delay);
        assert_eq!(c.seed, None);
    }

    #[test]
    fn exact_memory_threads_stop() {
        let mut s = ConfigState {
            mem_mode: MemMode::ExactGib,
            mem_gib: 2.0,
            threads_all: false,
            threads_custom: 6,
            passes_enabled: false,
            duration_enabled: true,
            duration_value: 2.0,
            duration_unit: TimeUnit::Hours,
            max_errors_enabled: true,
            max_errors: 10,
            ..ConfigState::default()
        };
        let c = s.to_run_config(None).unwrap();
        assert_eq!(c.memory, MemorySize::Bytes(2 << 30));
        assert_eq!(c.threads, Some(6));
        assert_eq!(c.stop.passes, None);
        assert_eq!(c.stop.duration, Some(Duration::from_secs(7200)));
        assert_eq!(c.stop.max_errors, Some(10));
        s.stop_on_first_error = true;
        assert_eq!(s.to_run_config(None).unwrap().stop.max_errors, Some(1));
    }

    #[test]
    fn tests_keep_canonical_order() {
        let s = ConfigState {
            tests: [TestId::BitFade, TestId::OwnAddress].into_iter().collect(),
            ..ConfigState::default()
        };
        assert_eq!(
            s.to_run_config(None).unwrap().tests,
            vec![TestId::OwnAddress, TestId::BitFade]
        );
    }

    #[test]
    fn validation_errors() {
        let bad = [
            ConfigState {
                tests: BTreeSet::new(),
                ..ConfigState::default()
            },
            ConfigState {
                mem_percent: 0.0,
                ..ConfigState::default()
            },
            ConfigState {
                passes: 0,
                ..ConfigState::default()
            },
            ConfigState {
                seed_text: "zzz".into(),
                ..ConfigState::default()
            },
        ];
        for s in bad {
            assert!(s.to_run_config(None).is_err());
        }
    }

    #[test]
    fn exact_memory_limited_by_available() {
        let s = ConfigState {
            mem_mode: MemMode::ExactGib,
            mem_gib: 8.0,
            ..ConfigState::default()
        };
        let avail = |gib: f64| Some((gib * GIB) as u64);
        assert!(s.to_run_config(None).is_ok());
        assert!(s.to_run_config(avail(16.0)).is_ok());
        assert!(s.to_run_config(avail(8.5)).is_ok());
        assert!(s.to_run_config(avail(8.0)).is_err());
        assert!(s.to_run_config(avail(4.0)).is_err());
        assert_eq!(max_exact_gib(avail(4.25)), Some(4.0));
        assert_eq!(max_exact_gib(Some(0)), Some(0.0));
        assert_eq!(max_exact_gib(None), None);
    }

    #[test]
    fn seed_parsing() {
        assert_eq!(parse_seed(""), Ok(None));
        assert_eq!(parse_seed(" 42 "), Ok(Some(42)));
        assert_eq!(parse_seed("0xFF"), Ok(Some(255)));
        assert!(parse_seed("-1").is_err());
    }
}
