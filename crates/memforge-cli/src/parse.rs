//! Parsing helpers for command-line values.

use memforge_core::TestId;
use memforge_engine::MemorySize;
use std::time::Duration;

fn split_number(s: &str) -> Result<(f64, &str), String> {
    let end = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    if end == 0 {
        return Err(format!("expected a number at the start of '{s}'"));
    }
    let n: f64 = s[..end]
        .parse()
        .map_err(|_| format!("invalid number '{}'", &s[..end]))?;
    Ok((n, s[end..].trim()))
}

/// Parse a memory size: `90%`, `16GiB`, `512MiB`, `1.5G`, `1048576` (bytes).
/// All byte units are binary (1 GB == 1 GiB == 1024^3).
pub fn parse_size(input: &str) -> Result<MemorySize, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("empty memory size".into());
    }
    let (n, unit) = split_number(s)?;
    let unit_l = unit.to_ascii_lowercase();
    if unit_l == "%" {
        if n <= 0.0 || n > 100.0 {
            return Err(format!("percentage must be in (0, 100], got '{s}'"));
        }
        return Ok(MemorySize::PercentAvailable(n as f32));
    }
    let mult: f64 = match unit_l.as_str() {
        "" | "b" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024.0 * 1024.0,
        "g" | "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tb" | "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => {
            return Err(format!(
                "unknown size unit '{unit}' in '{s}' (use %, B, KiB, MiB, GiB, TiB)"
            ))
        }
    };
    let bytes = n * mult;
    if bytes < 1.0 {
        return Err(format!("memory size must be greater than zero, got '{s}'"));
    }
    if bytes >= u64::MAX as f64 {
        return Err(format!("memory size too large: '{s}'"));
    }
    Ok(MemorySize::Bytes(bytes as u64))
}

/// Like [`parse_duration`] but rejects zero (a zero-length run is never intended).
pub fn parse_run_duration(input: &str) -> Result<Duration, String> {
    let d = parse_duration(input)?;
    if d.is_zero() {
        return Err(
            "duration must be greater than zero (omit --duration to run until passes complete)"
                .into(),
        );
    }
    Ok(d)
}

/// Parse `--max-errors`: a positive integer.
pub fn parse_max_errors(input: &str) -> Result<u64, String> {
    match input.trim().parse::<u64>() {
        Ok(0) => Err("--max-errors must be at least 1 (omit it to not stop on errors)".into()),
        Ok(n) => Ok(n),
        Err(_) => Err(format!("invalid error count '{input}'")),
    }
}

/// Parse a duration: `30s`, `15m`, `8h`, `1h30m`, `500ms`, `2d`; a bare number is seconds.
pub fn parse_duration(input: &str) -> Result<Duration, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("empty duration".into());
    }
    let mut rest = s;
    let mut total = 0.0f64;
    let mut first = true;
    while !rest.is_empty() {
        let (n, after) = split_number(rest).map_err(|_| format!("invalid duration '{input}'"))?;
        let end = after
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(after.len());
        let unit = after[..end].trim().to_ascii_lowercase();
        rest = after[end..].trim_start();
        let mult = match unit.as_str() {
            "" if first && rest.is_empty() => 1.0,
            "ms" => 0.001,
            "s" | "sec" | "secs" => 1.0,
            "m" | "min" | "mins" => 60.0,
            "h" | "hr" | "hrs" => 3600.0,
            "d" => 86400.0,
            _ => {
                return Err(format!(
                    "invalid duration '{input}': unknown or missing unit '{unit}' (use ms, s, m, h, d; e.g. 1h30m)"
                ))
            }
        };
        total += n * mult;
        first = false;
    }
    if !total.is_finite() || total > 1.0e9 {
        return Err(format!("duration too large: '{input}'"));
    }
    Ok(Duration::from_secs_f64(total))
}

pub fn valid_test_names() -> String {
    TestId::ALL
        .iter()
        .map(|t| t.name())
        .collect::<Vec<_>>()
        .join(", ")
}

fn push_unique(out: &mut Vec<TestId>, ids: &[TestId]) {
    for &t in ids {
        if !out.contains(&t) {
            out.push(t);
        }
    }
}

/// Parse `default`, `all`, or a comma-separated list of test names.
pub fn parse_tests(input: &str) -> Result<Vec<TestId>, String> {
    let mut out = Vec::new();
    for part in input.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if part.eq_ignore_ascii_case("default") {
            push_unique(&mut out, &memforge_core::default_test_suite());
        } else if part.eq_ignore_ascii_case("all") {
            push_unique(&mut out, TestId::ALL);
        } else {
            let id = TestId::from_name(part).ok_or_else(|| {
                format!(
                    "unknown test '{part}'. Valid tests: {} (or 'default', 'all')",
                    valid_test_names()
                )
            })?;
            push_unique(&mut out, &[id]);
        }
    }
    if out.is_empty() {
        return Err(format!(
            "no tests selected. Valid tests: {}",
            valid_test_names()
        ));
    }
    Ok(out)
}

/// Parse a seed: `0x1234abcd` (hex), `12345` (decimal), or bare hex like `deadbeef`.
pub fn parse_seed(input: &str) -> Result<u64, String> {
    let s = input.trim().replace('_', "");
    if s.is_empty() {
        return Err("empty seed".into());
    }
    let r = if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(h, 16)
    } else {
        s.parse::<u64>().or_else(|_| u64::from_str_radix(&s, 16))
    };
    r.map_err(|_| format!("invalid seed '{input}' (use decimal or 0x-prefixed hex, max 64 bits)"))
}

/// Parse `--threads`: `all` (None) or a positive integer.
pub fn parse_threads(input: &str) -> Result<Option<usize>, String> {
    let s = input.trim();
    if s.eq_ignore_ascii_case("all") {
        return Ok(None);
    }
    match s.parse::<usize>() {
        Ok(0) => Err("thread count must be at least 1 (or 'all')".into()),
        Ok(n) => Ok(Some(n)),
        Err(_) => Err(format!(
            "invalid thread count '{input}' (use a number or 'all')"
        )),
    }
}

/// `--threads` value wrapper (clap needs a concrete parsed type).
#[derive(Clone, Copy, Debug)]
pub struct ThreadsArg(pub Option<usize>);

pub fn parse_threads_arg(input: &str) -> Result<ThreadsArg, String> {
    parse_threads(input).map(ThreadsArg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_rejected() {
        assert!(parse_run_duration("0").is_err());
        assert!(parse_run_duration("0s").is_err());
        assert_eq!(parse_run_duration("5m").unwrap(), Duration::from_secs(300));
        assert!(parse_max_errors("0").is_err());
        assert!(parse_max_errors("x").is_err());
        assert_eq!(parse_max_errors("7").unwrap(), 7);
    }

    #[test]
    fn sizes() {
        assert_eq!(
            parse_size("90%").unwrap(),
            MemorySize::PercentAvailable(90.0)
        );
        assert_eq!(
            parse_size("100%").unwrap(),
            MemorySize::PercentAvailable(100.0)
        );
        assert_eq!(parse_size("16GiB").unwrap(), MemorySize::Bytes(16 << 30));
        assert_eq!(parse_size("512 MiB").unwrap(), MemorySize::Bytes(512 << 20));
        assert_eq!(parse_size("1.5g").unwrap(), MemorySize::Bytes(3 << 29));
        assert_eq!(parse_size("4096").unwrap(), MemorySize::Bytes(4096));
    }

    #[test]
    fn size_errors() {
        assert!(parse_size("0%").is_err());
        assert!(parse_size("101%").is_err());
        assert!(parse_size("").is_err());
        assert!(parse_size("0").is_err());
        assert!(parse_size("GiB").is_err());
        assert!(parse_size("-5MiB").is_err());
        assert!(parse_size("1.2.3G").is_err());
        let e = parse_size("12 parsecs").unwrap_err();
        assert!(e.contains("unknown size unit"));
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("15m").unwrap(), Duration::from_secs(900));
        assert_eq!(parse_duration("8h").unwrap(), Duration::from_secs(8 * 3600));
        assert_eq!(parse_duration("1h30m").unwrap(), Duration::from_secs(5400));
        assert_eq!(
            parse_duration("1h 30m 5s").unwrap(),
            Duration::from_secs(5405)
        );
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        assert_eq!(parse_duration("45").unwrap(), Duration::from_secs(45));
    }

    #[test]
    fn duration_errors() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("10x").is_err());
        assert!(parse_duration("1h30").is_err());
        assert!(parse_duration("h").is_err());
    }

    #[test]
    fn tests_list() {
        assert_eq!(
            parse_tests("default").unwrap(),
            memforge_core::default_test_suite()
        );
        assert_eq!(parse_tests("ALL").unwrap(), TestId::ALL.to_vec());
        assert_eq!(
            parse_tests("random-pattern, own-address,random-pattern").unwrap(),
            vec![TestId::RandomPattern, TestId::OwnAddress]
        );
        let e = parse_tests("own-address,bogus").unwrap_err();
        assert!(e.contains("bogus") && e.contains("walking-ones"));
        assert!(parse_tests(",").is_err());
    }

    #[test]
    fn seeds() {
        assert_eq!(parse_seed("0xFF").unwrap(), 255);
        assert_eq!(parse_seed("255").unwrap(), 255);
        assert_eq!(parse_seed("deadbeef").unwrap(), 0xdead_beef);
        assert_eq!(parse_seed("0xffffffffffffffff").unwrap(), u64::MAX);
        assert!(parse_seed("0x1ffffffffffffffff").is_err());
        assert!(parse_seed("zzz").is_err());
        assert!(parse_seed("").is_err());
        assert!(parse_seed("0x").is_err());
    }

    #[test]
    fn threads() {
        assert_eq!(parse_threads("all").unwrap(), None);
        assert_eq!(parse_threads("8").unwrap(), Some(8));
        assert!(parse_threads("0").is_err());
        assert!(parse_threads("x").is_err());
    }
}
