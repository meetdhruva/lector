//! RFC 3339 timestamps (as returned by GitHub and GitLab) to Unix seconds.

pub(crate) fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Parses `YYYY-MM-DDTHH:MM:SS[.frac](Z|±HH:MM)` to Unix seconds.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let s = s.trim();
    let num = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    if s.len() < 19 || !matches!(s.as_bytes().get(10), Some(b'T' | b't' | b' ')) {
        return None;
    }
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, se) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let mut rest = &s[19..];
    if let Some(r) = rest.strip_prefix('.') {
        rest = r.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    let offset = match rest {
        "" | "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let oh = rest.get(1..3)?.parse::<i64>().ok()?;
            let om = rest.get(4..6)?.parse::<i64>().ok()?;
            sign * (oh * 3600 + om * 60)
        }
    };
    Some(days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + se - offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2024-03-01T12:30:00Z"), Some(1_709_296_200));
        assert_eq!(
            parse_rfc3339("2024-03-01T14:30:00.123+02:00"),
            Some(1_709_296_200)
        );
        assert_eq!(parse_rfc3339("nope"), None);
    }
}
