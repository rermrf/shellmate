//! How the file panel writes times.

/// A moment broken into local calendar fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Broken {
    pub year: i32,
    /// 1 to 12.
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

/// `11-12 08:55` for a time in the year `now` is in, `2025-11-12` for any other year. `local` turns
/// seconds since the epoch into local calendar fields (injected so that the format can be tested
/// without a time zone); a time that cannot be converted is written as `—`.
pub fn format_mtime(secs: i64, now: i64, local: impl Fn(i64) -> Option<Broken>) -> String {
    let (Some(then), Some(today)) = (local(secs), local(now)) else { return "—".to_string() };
    if then.year == today.year {
        format!("{:02}-{:02} {:02}:{:02}", then.month, then.day, then.hour, then.minute)
    } else {
        format!("{:04}-{:02}-{:02}", then.year, then.month, then.day)
    }
}

/// The local calendar fields of `secs`, in the time zone of this machine.
pub fn local_broken(secs: i64) -> Option<Broken> {
    let time = secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `time` and `tm` are valid for the call; `localtime_r` only writes into `tm`.
    let converted = unsafe { libc::localtime_r(&time, &mut tm) };
    (!converted.is_null()).then(|| Broken {
        year: tm.tm_year + 1900,
        month: (tm.tm_mon + 1) as u32,
        day: tm.tm_mday as u32,
        hour: tm.tm_hour as u32,
        minute: tm.tm_min as u32,
        second: tm.tm_sec as u32,
    })
}

/// The time of day: `09:12:03` with `seconds`, else `09:12`. `local` is the time zone (injected for tests);
/// a time that cannot be converted is written as `—`.
pub fn format_clock(secs: i64, seconds: bool, local: impl Fn(i64) -> Option<Broken>) -> String {
    match local(secs) {
        Some(t) if seconds => format!("{:02}:{:02}:{:02}", t.hour, t.minute, t.second),
        Some(t) => format!("{:02}:{:02}", t.hour, t.minute),
        None => "—".to_string(),
    }
}

/// Now, as `09:12:03` in this machine's time zone.
pub fn clock_now() -> String {
    format_clock(now_secs(), true, local_broken)
}

/// A time of day as `09:12` in this machine's time zone.
pub fn clock_hm(secs: i64) -> String {
    format_clock(secs, false, local_broken)
}

/// Seconds since the epoch, now.
pub fn now_secs() -> i64 {
    #[cfg(debug_assertions)] if crate::proto::requested(){return crate::proto::now();}
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// A modification time as the list shows it.
pub fn mtime_text(secs: Option<i64>) -> String {
    secs.map_or_else(|| "—".to_string(), |secs| format_mtime(secs, now_secs(), local_broken))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// UTC, from the days since the epoch (the civil-from-days algorithm), as a stand-in for a time zone.
    fn utc(secs: i64) -> Option<Broken> {
        let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        let year = (yoe + era * 400 + i64::from(month <= 2)) as i32;
        Some(Broken {
            year,
            month,
            day,
            hour: (rest / 3600) as u32,
            minute: (rest % 3600 / 60) as u32,
            second: (rest % 60) as u32,
        })
    }

    /// The days since the epoch of a civil date (the inverse of the conversion above).
    fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
        let y = i64::from(year) - i64::from(month <= 2);
        let (era, yoe) = (y.div_euclid(400), y.rem_euclid(400));
        let mp = (i64::from(month) + 9) % 12;
        let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    fn at(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> i64 {
        days_from_civil(year, month, day) * 86_400 + i64::from(hour) * 3600 + i64::from(minute) * 60
    }

    /// 2026-11-14 12:00 UTC.
    fn now() -> i64 {
        at(2026, 11, 14, 12, 0)
    }

    #[test]
    fn the_stand_in_zone_is_right() {
        assert_eq!(utc(0), Some(Broken { year: 1970, month: 1, day: 1, hour: 0, minute: 0, second: 0 }));
        assert_eq!(
            utc(at(2024, 2, 29, 23, 7) + 41),
            Some(Broken { year: 2024, month: 2, day: 29, hour: 23, minute: 7, second: 41 })
        );
        assert_eq!(now(), 1_794_657_600);
    }

    #[test]
    fn the_time_of_day_has_seconds_when_asked_and_a_dash_when_unknown() {
        let t = at(2026, 11, 14, 9, 5) + 3;
        assert_eq!(format_clock(t, true, utc), "09:05:03");
        assert_eq!(format_clock(t, false, utc), "09:05");
        assert_eq!(format_clock(t, true, |_| None), "—");
        assert_eq!(format_clock(at(2026, 11, 14, 23, 59) + 59, true, utc), "23:59:59");
    }

    #[test]
    fn this_year_shows_month_day_and_time() {
        assert_eq!(format_mtime(at(2026, 11, 12, 8, 55), now(), utc), "11-12 08:55");
        assert_eq!(format_mtime(at(2026, 1, 3, 0, 5), now(), utc), "01-03 00:05");
        assert_eq!(format_mtime(at(2026, 12, 31, 23, 59), now(), utc), "12-31 23:59", "later this year too");
    }

    #[test]
    fn other_years_show_the_date() {
        assert_eq!(format_mtime(at(2025, 11, 12, 8, 55), now(), utc), "2025-11-12");
        assert_eq!(format_mtime(at(2024, 2, 29, 23, 0), now(), utc), "2024-02-29");
        assert_eq!(format_mtime(0, now(), utc), "1970-01-01");
        assert_eq!(format_mtime(at(2027, 1, 1, 0, 0), now(), utc), "2027-01-01", "the future is another year");
    }

    #[test]
    fn the_year_boundary_follows_the_local_calendar_of_now() {
        // Just after New Year, yesterday evening is last year.
        let now = at(2026, 1, 1, 0, 30);
        assert_eq!(format_mtime(at(2025, 12, 31, 23, 50), now, utc), "2025-12-31");
        assert_eq!(format_mtime(at(2026, 1, 1, 0, 10), now, utc), "01-01 00:10");
    }

    #[test]
    fn an_unconvertible_time_is_a_dash() {
        assert_eq!(format_mtime(5, now(), |_| None), "—");
        assert_eq!(format_mtime(5, now(), |secs| if secs == 5 { None } else { utc(secs) }), "—");
    }

    #[test]
    fn the_local_conversion_agrees_with_mktime() {
        // Whatever the zone of the machine running the test: converting back gives the same moment.
        for secs in [1_700_000_000i64, 1_750_000_000, 1_600_000_000] {
            let b = local_broken(secs).expect("convertible");
            let mut tm: libc::tm = unsafe { std::mem::zeroed() };
            tm.tm_year = b.year - 1900;
            tm.tm_mon = b.month as i32 - 1;
            tm.tm_mday = b.day as i32;
            tm.tm_hour = b.hour as i32;
            tm.tm_min = b.minute as i32;
            tm.tm_isdst = -1;
            // SAFETY: `tm` is a valid, initialized struct.
            let back = unsafe { libc::mktime(&mut tm) } as i64;
            assert_eq!(back, secs - secs.rem_euclid(60), "{secs}");
        }
    }

    #[test]
    fn a_missing_time_is_a_dash_in_the_list() {
        assert_eq!(mtime_text(None), "—");
        assert_ne!(mtime_text(Some(1_700_000_000)), "—");
    }
}
