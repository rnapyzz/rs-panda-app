//! 施策・組織・セグメント・ユニットなどのコード（人が指し示すための番号）を扱う。

use std::sync::LazyLock;

use mysql_async::prelude::*;
use regex::Regex;

use crate::AppResult;
use crate::db::Tx;

/// コードの形式。CSV やファイル名でも扱いやすいよう、半角英数字・ハイフン・アンダースコアに限る。
pub static PATTERN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_-]{1,50}$").unwrap());

/// コードの形式が正しくないときのメッセージ。
pub const PATTERN_MESSAGE: &str = "コードは半角英数字・ハイフン・アンダースコアの50文字以内で入力してください";

/// MySQL の正規表現で文字どおりに一致させるため、記号をエスケープする（Go の regexp.QuoteMeta と同じ）。
fn quote_meta(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if r"\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// table の code 列で、prefix に続く番号の次のコード（例: ORG-0001）を返す。
/// taken_table を指定すると、その表の code 列に既にあるコードは使わずに次の番号にする（他のテーブルとの重複を避けるため）。
pub async fn next(tx: &mut Tx, table: &str, prefix: &str, taken_table: Option<&str>) -> AppResult<String> {
    let max_no: Option<Option<u64>> = tx
        .exec_first(
            format!("SELECT MAX(CAST(SUBSTRING(code, ?) AS UNSIGNED)) FROM {table} WHERE code REGEXP ?"),
            p![prefix.len() as i64 + 1, format!("^{}[0-9]+$", quote_meta(prefix))],
        )
        .await?;
    let mut n = max_no.flatten().unwrap_or(0) + 1;
    loop {
        let code = format!("{prefix}{n:04}");
        let Some(other) = taken_table else { return Ok(code) };
        let taken =
            crate::db::count(tx, &format!("SELECT COUNT(*) FROM {other} WHERE code = ?"), p![code.clone()]).await?;
        if taken == 0 {
            return Ok(code);
        }
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pattern() {
        assert!(PATTERN.is_match("ORG-0001"));
        assert!(!PATTERN.is_match("人事"));
        assert!(!PATTERN.is_match(""));
        assert_eq!(quote_meta("A.B-"), r"A\.B-");
    }
}
