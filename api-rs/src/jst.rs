//! 日時の扱い。DB の DATETIME は日本時間（Asia/Tokyo）として読み書きし、
//! JSON では Go 版と同じ RFC 3339（例: 2026-04-01T09:00:00+09:00）で返す。

use chrono::{FixedOffset, NaiveDate, NaiveDateTime, Timelike, Utc};
use serde::Serializer;

pub fn offset() -> FixedOffset {
    FixedOffset::east_opt(9 * 3600).unwrap()
}

/// 現在の日本時間。
pub fn now() -> NaiveDateTime {
    Utc::now().with_timezone(&offset()).naive_local()
}

/// 今日の日付（日本時間）。
pub fn today() -> NaiveDate {
    now().date()
}

/// Go の time.Time を RFC3339Nano で書き出したのと同じ形にする（小数部は末尾の 0 を除く）。
pub fn format(t: &NaiveDateTime) -> String {
    let mut s = t.format("%Y-%m-%dT%H:%M:%S").to_string();
    let nanos = t.nanosecond() % 1_000_000_000;
    if nanos > 0 {
        let frac = format!("{nanos:09}");
        s.push('.');
        s.push_str(frac.trim_end_matches('0'));
    }
    s.push_str("+09:00");
    s
}

/// serde の serialize_with 用。
pub fn ser<S: Serializer>(t: &NaiveDateTime, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&format(t))
}

/// Option の日時（None は null）。
pub fn ser_opt<S: Serializer>(t: &Option<NaiveDateTime>, s: S) -> Result<S::Ok, S::Error> {
    match t {
        Some(t) => s.serialize_str(&format(t)),
        None => s.serialize_none(),
    }
}

/// Unix 秒の日本時間。
pub fn from_unix(secs: i64) -> NaiveDateTime {
    chrono::DateTime::from_timestamp(secs, 0).unwrap_or_default().with_timezone(&offset()).naive_local()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_like_go() {
        let t = NaiveDate::from_ymd_opt(2026, 4, 1).unwrap().and_hms_opt(9, 5, 0).unwrap();
        assert_eq!(format(&t), "2026-04-01T09:05:00+09:00");
        let t = t.with_nanosecond(120_000_000).unwrap();
        assert_eq!(format(&t), "2026-04-01T09:05:00.12+09:00");
    }
}
