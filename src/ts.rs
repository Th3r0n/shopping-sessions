//! Timestamps F1 `YYYY-MM-DDTHH:MM:SSZ`, F2 `YYYY-MM-DDTHH:MM:SS±HH:MM` and F3
//! (ASCII digits, epoch milliseconds) to UTC milliseconds. Anything else is bad.

pub const SECOND: i64 = 1_000;
pub const DAY: i64 = 86_400_000;

#[inline]
fn two(b: &[u8], i: usize) -> Option<i64> {
    let hi = b[i].wrapping_sub(b'0');
    let lo = b[i + 1].wrapping_sub(b'0');
    if hi < 10 && lo < 10 {
        Some(hi as i64 * 10 + lo as i64)
    } else {
        None
    }
}

fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 => {
            if is_leap(year) {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

pub fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    era * 146_097 + year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year - 719_468
}

/// UTC milliseconds, or None for a bad timestamp. An F3 value that does not fit in
/// an i64 is returned as None: it lies outside every window anyway.
#[inline]
pub fn parse(b: &[u8]) -> Option<i64> {
    let n = b.len();
    if n == 0 {
        return None;
    }
    if b.iter().all(u8::is_ascii_digit) {
        return parse_epoch(b);
    }
    let offset = match n {
        20 => {
            if b[19] != b'Z' {
                return None;
            }
            0
        }
        25 => {
            let sign = match b[19] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            if b[22] != b':' {
                return None;
            }
            let hours = two(b, 20)?;
            let minutes = two(b, 23)?;
            if hours > 23 || minutes > 59 {
                return None;
            }
            sign * (hours * 60 + minutes) * 60_000
        }
        _ => return None,
    };
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let year = two(b, 0)? * 100 + two(b, 2)?;
    let month = two(b, 5)?;
    let day = two(b, 8)?;
    let hour = two(b, 11)?;
    let minute = two(b, 14)?;
    let second = two(b, 17)?;
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let local = ((days_from_civil(year, month, day) * 24 + hour) * 60 + minute) * 60_000 + second * SECOND;
    Some(local - offset)
}

#[inline]
fn parse_epoch(b: &[u8]) -> Option<i64> {
    let first = b.iter().position(|&c| c != b'0').unwrap_or(b.len());
    let digits = &b[first..];
    if digits.len() > 19 {
        return None;
    }
    let mut value: u64 = 0;
    for &c in digits {
        value = value * 10 + (c - b'0') as u64;
    }
    if value > i64::MAX as u64 {
        None
    } else {
        Some(value as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f1(s: &str) -> i64 {
        parse(s.as_bytes()).unwrap()
    }

    #[test]
    fn valid_forms() {
        assert_eq!(f1("2026-10-03T23:58:10Z"), 1_791_071_890_000);
        assert_eq!(f1("2026-10-03T19:58:10-04:00"), 1_791_071_890_000);
        assert_eq!(f1("1791071890000"), 1_791_071_890_000);
        assert_eq!(f1("0001791071890000"), 1_791_071_890_000);
        assert_eq!(f1("000000000000000000000000001791071890000"), 1_791_071_890_000);
        assert_eq!(f1("2026-10-04T01:30:00+02:00"), f1("2026-10-03T23:30:00Z"));
        assert_eq!(f1("2026-10-03T10:00:00-00:00"), f1("2026-10-03T10:00:00Z"));
        assert_eq!(f1("2026-10-03T10:00:00+23:59"), f1("2026-10-02T10:01:00Z"));
        assert_eq!(f1("2028-02-29T00:00:00Z"), f1("2028-02-28T00:00:00Z") + DAY);
        assert_eq!(f1("2000-02-29T00:00:00Z"), f1("2000-02-28T00:00:00Z") + DAY);
        assert_eq!(f1("86400000"), DAY);
        assert_eq!(f1("0"), 0);
        assert_eq!(f1("1970-01-01T00:00:00Z"), 0);
        assert_eq!(f1("9223372036854775807"), i64::MAX);
        assert_eq!(f1("2027-01-01T05:00:00+12:00"), f1("2026-12-31T17:00:00Z"));
    }

    #[test]
    fn bad_forms() {
        for s in [
            "",
            "2026-10-03T23:58:10.000Z",
            "2026-10-03T23:58:10z",
            "2026-10-03t23:58:10Z",
            "2026-10-03 23:58:10Z",
            "2026-10-03T23:58:10",
            "1791071890000.0",
            "1.79e12",
            "-5",
            "+5",
            "2026-13-03T23:58:10Z",
            "2026-00-03T23:58:10Z",
            "2026-02-30T00:00:00Z",
            "2027-02-29T00:00:00Z",
            "1900-02-29T00:00:00Z",
            "2026-10-00T00:00:00Z",
            "2026-04-31T00:00:00Z",
            "2026-10-03T24:00:00Z",
            "2026-10-03T23:60:00Z",
            "2026-10-03T23:58:60Z",
            "2026-10-03T23:58:10+24:00",
            "2026-10-03T23:58:10+05:60",
            "2026-10-03T23:58:10+0200",
            "2026-10-03T23:58:10 02:00",
            "2026-10-03T23:58:10Z\r",
            "1791071890000\r",
            " 1791071890000",
            "\u{0662}\u{0665}\u{0660}",
            "\u{ff12}\u{ff15}\u{ff10}",
            "２026-10-03T23:58:10Z",
            "9223372036854775808",
            "123456789012345678901",
            "2026-1a-03T23:58:10Z",
            "2026/10/03T23:58:10Z",
        ] {
            assert_eq!(parse(s.as_bytes()), None, "{s:?} should be bad");
        }
    }
}
