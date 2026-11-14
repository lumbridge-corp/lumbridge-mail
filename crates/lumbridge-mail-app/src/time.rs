//! Dates for people: "14:05", "Tue", "3 Nov", "3 Nov 2025", in the local time zone (the OS's zone,
//! through chrono; `TZ` is honoured). Tests and screenshots pin the zone with [`use_fixed_offset`].

use std::sync::atomic::{AtomicI64, Ordering};

const DAY: i64 = 86_400;
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// (year, month 1-12, day 1-31) for days since 1970-01-01.
pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `i64::MIN` = the OS zone; anything else is a fixed offset in seconds.
static FIXED: AtomicI64 = AtomicI64::new(i64::MIN);

/// Pin every date to a UTC offset (tests, screenshots), or go back to the OS zone with None.
pub fn use_fixed_offset(seconds: Option<i32>) {
    FIXED.store(seconds.map(i64::from).unwrap_or(i64::MIN), Ordering::SeqCst);
}

/// The local UTC offset, in seconds, at instant `t` (daylight saving moves it).
pub fn offset_at(t: i64) -> i64 {
    let fixed = FIXED.load(Ordering::SeqCst);
    if fixed != i64::MIN {
        return fixed;
    }
    use chrono::{Offset, TimeZone};
    chrono::DateTime::from_timestamp(t, 0)
        .map(|utc| i64::from(chrono::Local.offset_from_utc_datetime(&utc.naive_utc()).fix().local_minus_utc()))
        .unwrap_or(0)
}

/// `t` as local wall-clock seconds.
fn local(t: i64) -> i64 {
    t + offset_at(t)
}

fn hm(t: i64) -> String {
    let s = t.rem_euclid(DAY);
    format!("{:02}:{:02}", s / 3600, (s % 3600) / 60)
}

/// For a list row: the time today, the weekday this week, the date this year, else the full date.
pub fn short_date(t: i64, now: i64) -> String {
    let (t, now) = (local(t), local(now));
    let (day, today) = (t.div_euclid(DAY), now.div_euclid(DAY));
    let (y, m, d) = civil_from_days(day);
    let (ny, _, _) = civil_from_days(today);
    if day == today {
        hm(t)
    } else if day == today - 1 {
        "Yesterday".into()
    } else if today - day < 7 && day < today {
        WEEKDAYS[day.rem_euclid(7) as usize].into()
    } else if y == ny {
        format!("{d} {}", MONTHS[(m - 1) as usize])
    } else {
        format!("{d} {} {y}", MONTHS[(m - 1) as usize])
    }
}

/// For a message header: "Tue 10 Nov 2026, 14:05".
pub fn long_date(t: i64) -> String {
    let t = local(t);
    let day = t.div_euclid(DAY);
    let (y, m, d) = civil_from_days(day);
    format!("{} {d} {} {y}, {}", WEEKDAYS[day.rem_euclid(7) as usize], MONTHS[(m - 1) as usize], hm(t))
}

/// For a due date: "today", "tomorrow", "Thu", "14 Dec".
pub fn due(t: i64, now: i64) -> String {
    let (lt, ln) = (local(t), local(now));
    let (day, today) = (lt.div_euclid(DAY), ln.div_euclid(DAY));
    match day - today {
        i64::MIN..=-1 => format!("overdue, {}", short_date(t, now)),
        0 => "today".into(),
        1 => "tomorrow".into(),
        2..=6 => WEEKDAYS[day.rem_euclid(7) as usize].into(),
        _ => {
            let (_, m, d) = civil_from_days(day);
            format!("{d} {}", MONTHS[(m - 1) as usize])
        }
    }
}

/// "2 min ago", "3 h ago", for the status bar.
pub fn ago(t: i64, now: i64) -> String {
    let s = (now - t).max(0);
    match s {
        0..=44 => "just now".into(),
        45..=3599 => format!("{} min ago", (s + 30) / 60),
        3600..=86_399 => format!("{} h ago", s / 3600),
        _ => format!("{} d ago", s / DAY),
    }
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// "248 KB", "2.4 MB".
pub fn size(bytes: u64) -> String {
    match bytes {
        0..=1023 => format!("{bytes} B"),
        1024..=1_048_575 => format!("{} KB", bytes / 1024),
        _ => format!("{:.1} MB", bytes as f64 / 1_048_576.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-11-10 16:42 UTC, a Tuesday.
    const NOW: i64 = 1_794_328_920;

    #[test]
    fn civil_round_trips() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2026, 11, 10) * DAY, 1_794_268_800);
        for z in [-1000, 0, 365, 20_000, 20_402] {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z);
        }
    }

    /// The zone is process-wide; tests that pin it run under this lock.
    pub(crate) static ZONE: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    #[test]
    fn list_dates_read_like_a_mail_client() {
        let _z = ZONE.lock();
        use_fixed_offset(Some(0));
        assert_eq!(short_date(NOW - 60, NOW), "16:41");
        assert_eq!(short_date(NOW - DAY, NOW), "Yesterday");
        assert_eq!(short_date(NOW - 3 * DAY, NOW), "Sat");
        assert_eq!(short_date(NOW - 20 * DAY, NOW), "21 Oct");
        assert_eq!(short_date(NOW - 400 * DAY, NOW), "6 Oct 2025");
        assert_eq!(long_date(NOW), "Tue 10 Nov 2026, 16:42");
        assert_eq!(due(NOW + DAY, NOW), "tomorrow");
        assert_eq!(due(NOW + 2 * DAY, NOW), "Thu");
        assert_eq!(due(NOW + 34 * DAY, NOW), "14 Dec");
        assert_eq!(ago(NOW - 90, NOW), "2 min ago");
        assert_eq!(size(248_000), "242 KB");
        assert_eq!(size(2_400_000), "2.3 MB");
        use_fixed_offset(None);
    }

    #[test]
    fn dates_follow_the_local_zone() {
        let _z = ZONE.lock();
        // UTC+9: 16:42 UTC on Tuesday is 01:42 on Wednesday.
        use_fixed_offset(Some(9 * 3600));
        assert_eq!(long_date(NOW), "Wed 11 Nov 2026, 01:42");
        assert_eq!(short_date(NOW, NOW), "01:42");
        assert_eq!(short_date(NOW - 2 * 3600, NOW), "Yesterday", "23:42 local was the day before");
        // UTC-8: still Tuesday morning.
        use_fixed_offset(Some(-8 * 3600));
        assert_eq!(long_date(NOW), "Tue 10 Nov 2026, 08:42");
        assert_eq!(due(NOW + 16 * 3600, NOW), "tomorrow");
        use_fixed_offset(None);
        // The OS zone gives some whole-minute offset within a day.
        let off = offset_at(NOW);
        assert!(off.abs() <= 14 * 3600 && off % 60 == 0, "{off}");
    }
}
