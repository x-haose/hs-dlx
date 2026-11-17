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
}
