//! Pure formatting helpers (no egui dependency).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Binary-prefixed byte count, e.g. `12.50 GiB`.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.2} {}", UNITS[i])
}

/// Duration as `H:MM:SS` (or `M:SS` below an hour).
pub fn duration(d: Duration) -> String {
    let s = d.as_secs();
    let (h, m, sec) = (s / 3600, (s / 60) % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{sec:02}")
    } else {
        format!("{m}:{sec:02}")
    }
}

/// Throughput in decimal GB/s (or MB/s when small).
pub fn rate(bytes_per_sec: f64) -> String {
    if !bytes_per_sec.is_finite() || bytes_per_sec < 0.0 {
        return "-".into();
    }
    if bytes_per_sec >= 1e9 {
        format!("{:.2} GB/s", bytes_per_sec / 1e9)
    } else {
        format!("{:.1} MB/s", bytes_per_sec / 1e6)
    }
}

pub fn hex64(v: u64) -> String {
    format!("0x{v:016X}")
}

/// Comma separated list of set bit indices (ascending), e.g. `3, 17`.
pub fn bit_list(mask: u64) -> String {
    (0..64)
        .filter(|b| (mask >> b) & 1 == 1)
        .map(|b| b.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// `YYYYMMDD-HHMMSS` in UTC, for file names.
pub fn file_timestamp(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_fmt() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1023), "1023 B");
        assert_eq!(bytes(1024), "1.00 KiB");
        assert_eq!(bytes(3 * 1024 * 1024 * 1024 / 2), "1.50 GiB");
    }

    #[test]
    fn duration_fmt() {
        assert_eq!(duration(Duration::from_secs(5)), "0:05");
        assert_eq!(duration(Duration::from_secs(125)), "2:05");
        assert_eq!(duration(Duration::from_secs(3661)), "1:01:01");
    }

    #[test]
    fn rate_fmt() {
        assert_eq!(rate(12.345e9), "12.35 GB/s");
        assert_eq!(rate(250e6), "250.0 MB/s");
        assert_eq!(rate(f64::NAN), "-");
    }

    #[test]
    fn bits_and_hex() {
        assert_eq!(bit_list(0b1000_1000), "3, 7");
        assert_eq!(bit_list(0), "");
        assert_eq!(hex64(255), "0x00000000000000FF");
    }

    #[test]
    fn timestamp() {
        let t = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(file_timestamp(t), "20231114-221320");
        assert_eq!(file_timestamp(UNIX_EPOCH), "19700101-000000");
    }
}
