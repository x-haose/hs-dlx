//! 本地时刻的写法：按系统的时区。

use jiff::tz::TimeZone;
use jiff::{Timestamp, ZonedDateTime};

/// Unix 秒 `seconds` 按 `pattern`（strftime 的写法）写成本地时刻；超出能表示的范围时为 None。
pub fn local(seconds: i64, pattern: &str) -> Option<String> {
    let at = Timestamp::from_second(seconds).ok()?;
    Some(
        at.to_zoned_datetime(TimeZone::system())
            .strftime(pattern)
            .to_string(),
    )
}

/// 此刻的本地时刻，按 `pattern`（strftime 的写法）。
pub fn now(pattern: &str) -> String {
    ZonedDateTime::now().strftime(pattern).to_string()
}

/// Unix 秒 `seconds` 是哪天几点，按离此刻多近写：今天的写 `21:40`，昨天的写 `昨天 21:40`，今年更早的写 `11-25`，
/// 往年的写 `2025-11-25`；超出能表示的范围时为 None。
pub fn day(seconds: i64) -> Option<String> {
    day_in(seconds, Timestamp::now(), &TimeZone::system())
}

/// [`day`]：此刻为 `now`、按时区 `zone`。
fn day_in(seconds: i64, now: Timestamp, zone: &TimeZone) -> Option<String> {
    let at = Timestamp::from_second(seconds)
        .ok()?
        .to_zoned_datetime(zone.clone());
    let today = now.to_zoned_datetime(zone.clone()).date();
    let date = at.date();
    let pattern = if date == today {
        "%H:%M"
    } else if today.yesterday().ok() == Some(date) {
        "昨天 %H:%M"
    } else if date.year() == today.year() {
        "%m-%d"
    } else {
        "%Y-%m-%d"
    };
    Some(at.strftime(pattern).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_written_in_local_time_and_out_of_range_ones_are_none() {
        let written = local(0, "%Y").unwrap();
        // 1970 年元旦零点在各时区是 1969 或 1970 年
        assert!(written == "1970" || written == "1969", "{written}");
        assert_eq!(local(i64::MAX, "%Y"), None);
        assert_eq!(now("%H:%M:%S").len(), 8);
    }

    #[test]
    fn days_are_written_by_how_close_they_are() {
        let zone = TimeZone::fixed(jiff::tz::offset(8));
        // 2026-11-27 10:00 东八区
        let now = Timestamp::from_second(1_795_744_800).unwrap();
        let hours = |h: i64| 1_795_744_800 + h * 3600;
        assert_eq!(day_in(hours(-1), now, &zone).as_deref(), Some("09:00"));
        assert_eq!(
            day_in(hours(-12), now, &zone).as_deref(),
            Some("昨天 22:00")
        );
        assert_eq!(day_in(hours(-48), now, &zone).as_deref(), Some("11-25"));
        assert_eq!(
            day_in(hours(-24 * 400), now, &zone).as_deref(),
            Some("2025-10-23")
        );
    }
}
