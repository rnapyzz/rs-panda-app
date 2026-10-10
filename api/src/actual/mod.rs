//! 実績（会計の明細）の取込と、施策への割当の API（docs/plan.md「2.12 実績の割当」）。
//!
//! 会計の明細（actual_entries）を取り込み、施策コード・外部コード → 割当ルール（会計科目 × 部門）→ 未割当 の順に
//! 施策へ割り当てる。施策 × 科目 × 月の合計（actual_facts）は明細から作り、施策が NULL の行を未割当として持つ。
//! アプリの実績の合計は、会計の合計（対象外の会計科目を除く）と常に一致する。

mod accounts;
mod allocate;
mod closings;
pub mod drift;
mod entries;
mod facts;
mod importer;
mod rules;
mod unallocated;

use std::sync::LazyLock;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post, put};
use mysql_async::prelude::*;
use num_bigint::BigInt;
use serde::{Deserialize, Serialize};

use crate::db::Value;
use crate::error::validation1;
use crate::{AppResult, AppState};

pub use drift::drifted_scenarios;

/// ルート。参照はログインユーザー全員、更新と未割当・食い違いの一覧は FP&A のみ（各ハンドラーの AdminUser で判定する）。
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/gl-accounts", get(accounts::list).post(accounts::create))
        .route("/api/gl-accounts/export", get(accounts::export))
        .route("/api/gl-accounts/import", post(accounts::import_csv))
        .route("/api/gl-accounts/{id}", put(accounts::update).delete(accounts::delete))
        .route("/api/allocation-rules", get(rules::list).post(rules::create))
        .route("/api/allocation-rules/export", get(rules::export))
        .route("/api/allocation-rules/import", post(rules::import_csv))
        .route("/api/allocation-rules/{id}", put(rules::update).delete(rules::delete))
        .route(
            "/api/actuals/import",
            post(importer::import_actuals).layer(DefaultBodyLimit::max(importer::MAX_IMPORT_BYTES)),
        )
        .route("/api/actuals/months", get(importer::actual_months))
        .route("/api/actuals/unallocated", get(unallocated::list))
        .route("/api/actuals/unallocated/assign", post(unallocated::assign))
        .route("/api/actuals/reallocate", post(unallocated::reallocate))
        .route("/api/activities/{id}/actual-entries", get(entries::list))
        .route("/api/activities/{id}/actual-entries/export", get(entries::export))
        // 締めた後の実績の修正と年度の締め（docs/plan.md「2.14」）
        .route("/api/scenarios/actual-drift", get(drift::list_drift))
        .route("/api/scenarios/{id}/refresh-actuals", post(drift::refresh_actuals))
        .route("/api/fiscal-years/closings", get(closings::list))
        .route("/api/fiscal-years/{fy}/close", post(closings::close_year))
        .route("/api/fiscal-years/{fy}/reopen", post(closings::reopen_year))
}

/// DECIMAL(18,0) に入る絶対値の上限。
static MAX_AMOUNT: LazyLock<BigInt> = LazyLock::new(|| "999999999999999999".parse().unwrap());

/// 金額が DECIMAL(18,0) に入るか。
fn amount_fits(v: &BigInt) -> bool {
    v.magnitude() <= MAX_AMOUNT.magnitude()
}

/// 円単位の整数（符号は省略可）を読む。Go の big.Int.SetString(s, 10) と同じく、区切り文字は受け付けない。
fn parse_amount(s: &str) -> Option<BigInt> {
    let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ReasonRequest {
    reason: String,
}

/// 施策のコードと名前。
#[derive(Clone, Debug, Serialize)]
pub struct ActivityRef {
    pub id: i64,
    pub code: String,
    pub name: String,
}

async fn find_activity<Q: Queryable>(q: &mut Q, id: i64) -> AppResult<ActivityRef> {
    let r: Option<(String, String)> = q.exec_first("SELECT code, name FROM activities WHERE id = ?", (id,)).await?;
    let (code, name) = r.ok_or_else(|| validation1("activity_id", "施策が見つかりません"))?;
    Ok(ActivityRef { id, code, name })
}

/// n 個の "?" をカンマでつないだ文字列を返す。
fn placeholders(n: usize) -> String {
    vec!["?"; n].join(",")
}

/// YYYY-MM の一覧を月初日の引数にする。
fn month_args(months: &[String]) -> Vec<Value> {
    months.iter().map(|m| Value::from(format!("{m}-01"))).collect()
}

/// YYYY-MM の形式で、月が 1〜12 か。
fn is_year_month(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 7 || b[4] != b'-' {
        return false;
    }
    if !b.iter().enumerate().all(|(i, c)| i == 4 || c.is_ascii_digit()) {
        return false;
    }
    let m = (b[5] - b'0') * 10 + (b[6] - b'0');
    (1..=12).contains(&m)
}

/// 合計（actual_facts）の更新件数。
#[derive(Clone, Debug, Default, Serialize)]
pub struct FactCounts {
    /// 更新後の件数
    pub facts: usize,
    pub inserted: usize,
    pub updated: usize,
    pub deleted: usize,
    pub unchanged: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn year_months() {
        assert!(is_year_month("2026-09"));
        assert!(!is_year_month("2026-13"));
        assert!(!is_year_month("2026/09"));
        assert!(!is_year_month("2026-9"));
    }

    #[test]
    fn amounts() {
        assert_eq!(parse_amount("-350000").unwrap().to_string(), "-350000");
        assert_eq!(parse_amount("+1").unwrap().to_string(), "1");
        for bad in ["", "-", "1.5", "1_000", "1,000", " 1"] {
            assert!(parse_amount(bad).is_none(), "{bad}");
        }
        assert!(amount_fits(&parse_amount("-999999999999999999").unwrap()));
        assert!(!amount_fits(&parse_amount("1000000000000000000").unwrap()));
    }
}
