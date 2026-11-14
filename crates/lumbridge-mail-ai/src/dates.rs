//! Calendar dates without a date crate: models answer `YYYY-MM-DD` (and maybe `HH:MM`) in the user's
//! zone, we store Unix seconds. Algorithms from Howard Hinnant's `days_from_civil` (public domain).

/// Days since 1970-01-01 for a proleptic Gregorian date.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// (year, month, day) for days since 1970-01-01.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        _ => 28,
    }
}

/// `YYYY-MM-DD` plus an optional `HH:MM` in a zone `offset_min` minutes east of UTC, to Unix seconds.
/// A date alone means the end of that day (23:59), which is what "by Friday" means.
pub fn parse(date: &str, time: Option<&str>, offset_min: i32) -> Option<i64> {
    let date = date.trim();
    let (y, rest) = date.split_once('-')?;
    let (m, d) = rest.split_once('-')?;
    if y.len() != 4 || m.len() != 2 || d.len() != 2 {
        return None;
    }
    let (y, m, d): (i64, u32, u32) = (y.parse().ok()?, m.parse().ok()?, d.parse().ok()?);
    if !(1..=12).contains(&m) || d == 0 || d > days_in_month(y, m) {
        return None;
    }
    let (hh, mm) = match time.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => {
            let (h, mi) = t.split_once(':')?;
            let (h, mi): (i64, i64) = (h.parse().ok()?, mi.get(..2).unwrap_or(mi).parse().ok()?);
            if !(0..24).contains(&h) || !(0..60).contains(&mi) {
                return None;
            }
            (h, mi)
        }
        None => (23, 59),
    };
    Some(days_from_civil(y, m, d) * 86400 + hh * 3600 + mm * 60 - offset_min as i64 * 60)
}

/// `2026-11-10 (Tuesday)` for `ts` in the zone, for prompts.
pub fn describe_day(ts: i64, offset_min: i32) -> String {
    const DAYS: [&str; 7] = ["Thursday", "Friday", "Saturday", "Sunday", "Monday", "Tuesday", "Wednesday"];
    let local = ts + offset_min as i64 * 60;
    let days = local.div_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02} ({})", DAYS[days.rem_euclid(7) as usize])
}

/// `2026-11-10 14:05` for `ts` in the zone, for prompts.
pub fn describe_time(ts: i64, offset_min: i32) -> String {
    let local = ts + offset_min as i64 * 60;
    let (y, m, d) = civil_from_days(local.div_euclid(86400));
    let s = local.rem_euclid(86400);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", s / 3600, (s % 3600) / 60)
}

/// Whether a date a model gave is believable for mail sent at `sent`: not more than a day before it,
/// not more than two years after `now`.
pub fn plausible(ts: i64, sent: i64, now: i64) -> bool {
    ts >= sent - 86400 && ts <= now.max(sent) + 2 * 366 * 86400
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2026, 11, 10), 20767);
        for z in [-1000, 0, 59, 20767, 30000] {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z);
        }
    }

    #[test]
    fn parses_dates_and_times() {
        assert_eq!(parse("2026-11-10", Some("14:30"), 0), Some(20767 * 86400 + 14 * 3600 + 1800));
        assert_eq!(parse("2026-11-10", None, 0), Some(20767 * 86400 + 23 * 3600 + 59 * 60));
        assert_eq!(parse("2026-11-10", Some("09:00"), -480), Some(20767 * 86400 + 17 * 3600));
        assert_eq!(parse("2026-02-30", None, 0), None);
        assert_eq!(parse("next friday", None, 0), None);
        assert_eq!(parse("2026-11-10", Some("25:00"), 0), None);
    }

    #[test]
    fn describes_days() {
        assert_eq!(describe_day(20767 * 86400 + 3600, 0), "2026-11-10 (Tuesday)");
        assert_eq!(describe_time(20767 * 86400 + 3660, 0), "2026-11-10 01:01");
    }
}
