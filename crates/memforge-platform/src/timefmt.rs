//! Pure ISO-8601 (UTC) formatting/parsing for `SystemTime`, no chrono.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Format as `YYYY-MM-DDTHH:MM:SS.mmmZ` (UTC). Times before the epoch clamp to the epoch.
pub fn format_iso8601(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    let secs = d.as_secs() as i64;
    let (y, mo, da) = civil_from_days(secs.div_euclid(86400));
    let sod = secs.rem_euclid(86400);
    format!(
        "{y:04}-{mo:02}-{da:02}T{:02}:{:02}:{:02}.{:03}Z",
        sod / 3600,
        sod % 3600 / 60,
        sod % 60,
        d.subsec_millis()
    )
}

/// Parse `YYYY-MM-DDTHH:MM:SS[.fraction]Z` (fraction up to 9 digits).
pub fn parse_iso8601(s: &str) -> Option<SystemTime> {
    let b = s.trim().as_bytes();
    if b.len() < 19
        || b[4] != b'-'
        || b[7] != b'-'
        || (b[10] != b'T' && b[10] != b' ')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        std::str::from_utf8(&b[r]).ok()?.parse().ok()
    };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, se) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let mut nanos = 0u32;
    if b.len() > 19 && b[19] == b'.' {
        let digits: Vec<u8> = b[20..]
            .iter()
            .copied()
            .take_while(u8::is_ascii_digit)
            .collect();
        if digits.is_empty() {
            return None;
        }
        let mut v = 0u32;
        for i in 0..9 {
            v = v * 10 + digits.get(i).map_or(0, |c| (c - b'0') as u32);
        }
        nanos = v;
    }
    let total = days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + se;
    if total < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::new(total as u64, nanos))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_and_known_dates() {
        assert_eq!(format_iso8601(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        // 2000-02-29T12:34:56.789Z = 951827696.789
        let t = UNIX_EPOCH + Duration::from_millis(951_827_696_789);
        assert_eq!(format_iso8601(t), "2000-02-29T12:34:56.789Z");
        // 2026-10-31T00:00:00Z = 1793404800
        let t = UNIX_EPOCH + Duration::from_secs(1_793_404_800);
        assert_eq!(format_iso8601(t), "2026-10-31T00:00:00.000Z");
    }

    #[test]
    fn roundtrip() {
        for secs in [
            0u64,
            86399,
            86400,
            951_827_696,
            1_793_404_800,
            4_102_444_800,
        ] {
            let t = UNIX_EPOCH + Duration::new(secs, 123_000_000);
            assert_eq!(parse_iso8601(&format_iso8601(t)), Some(t));
        }
        let t = parse_iso8601("2026-10-31T12:00:00.1234567Z").unwrap();
        assert_eq!(
            t.duration_since(UNIX_EPOCH).unwrap().subsec_nanos(),
            123_456_700
        );
        assert!(parse_iso8601("garbage").is_none());
        assert!(parse_iso8601("2026-13-31T12:00:00Z").is_none());
    }
}
