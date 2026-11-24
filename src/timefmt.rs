//! Small time helpers, so we don't pull in a date library for one parser.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Milliseconds since the Unix epoch for an RFC 3339 timestamp such as
/// "2026-11-15T10:20:30.123Z" or "2026-11-15T18:20:30+08:00".
pub fn parse_rfc3339_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b'T' | b't' | b' ')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let num = |from: usize, to: usize| -> Option<i64> {
        let part = s.get(from..to)?;
        if !part.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        part.parse().ok()
    };
    let (year, month, day) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (hour, minute, second) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let mut i = 19;
    let mut millis = 0;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return None;
        }
        let digits: String = s[start..i].chars().chain("00".chars()).take(3).collect();
        millis = digits.parse().ok()?;
    }
    let offset_minutes = match *b.get(i)? {
        b'Z' | b'z' if i + 1 == b.len() => 0,
        sign @ (b'+' | b'-') if i + 6 == b.len() && b[i + 3] == b':' => {
            let minutes = num(i + 1, i + 3)? * 60 + num(i + 4, i + 6)?;
            if sign == b'-' { -minutes } else { minutes }
        }
        _ => return None,
    };
    let days = days_from_civil(year, month, day);
    let seconds = days * 86_400 + hour * 3_600 + minute * 60 + second - offset_minutes * 60;
    Some(seconds * 1_000 + millis)
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = (if year >= 0 { year } else { year - 399 }) / 400;
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// "刚刚", "12 分钟前", "3 小时前", "5 天前".
pub fn ago(ms: i64, now: i64) -> String {
    if ms <= 0 {
        return "未知".to_string();
    }
    let minutes = (now - ms).max(0) / 60_000;
    match minutes {
        0 => "刚刚".to_string(),
        1..=59 => format!("{minutes} 分钟前"),
        60..=1_439 => format!("{} 小时前", minutes / 60),
        _ => format!("{} 天前", minutes / 1_440),
    }
}

pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_utc_and_offsets() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339_ms("2000-03-01T00:00:00Z"), Some(951_868_800_000));
        assert_eq!(parse_rfc3339_ms("2000-03-01T08:00:00+08:00"), Some(951_868_800_000));
        assert_eq!(parse_rfc3339_ms("2000-03-01T00:00:00.5Z"), Some(951_868_800_500));
        assert_eq!(parse_rfc3339_ms("2000-03-01T00:00:00.123456Z"), Some(951_868_800_123));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_rfc3339_ms(""), None);
        assert_eq!(parse_rfc3339_ms("2000-03-01"), None);
        assert_eq!(parse_rfc3339_ms("2000-13-01T00:00:00Z"), None);
        assert_eq!(parse_rfc3339_ms("2000-03-01T00:00:00Zjunk"), None);
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
    }
}
