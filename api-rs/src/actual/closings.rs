//! 年度の締め（docs/plan.md「2.14」）。締めた年度の月は、実績の取込・再割当・未割当の割当ができない。

use std::collections::HashSet;

use axum::extract::State;
use axum::response::Response;
use chrono::NaiveDateTime;
use mysql_async::prelude::*;
use serde::Serialize;

use super::ReasonRequest;
use crate::auth::{AdminUser, CurrentUser};
use crate::db::{self, Tx};
use crate::error::{AppError, bad_request, conflict, validation1};
use crate::http::{self, PathIds, RawBody};
use crate::{AppResult, AppState, audit, jst};

#[derive(Clone, Debug, Serialize)]
pub struct Closing {
    pub fiscal_year: i32,
    #[serde(serialize_with = "jst::ser")]
    pub closed_at: NaiveDateTime,
    pub closed_by: i64,
    pub closed_by_name: String,
}
from_row!(Closing { fiscal_year, closed_at, closed_by, closed_by_name });

/// 年月（YYYY-MM）の年度（4月開始）を返す。
pub fn fiscal_year_of(month: &str) -> i32 {
    let y: i32 = month.get(..4).and_then(|s| s.parse().ok()).unwrap_or(0);
    let m: u32 = month.get(5..7).and_then(|s| s.parse().ok()).unwrap_or(0);
    if m < 4 { y - 1 } else { y }
}

/// 締めた年度の集合を返す。
pub async fn closed_years<Q: Queryable>(q: &mut Q) -> AppResult<HashSet<i32>> {
    let rows: Vec<i32> = q.query("SELECT fiscal_year FROM fiscal_year_closings").await?;
    Ok(rows.into_iter().collect())
}

/// months のうち、締めた年度の月を返す。
pub fn closed_months(months: &[String], closed: &HashSet<i32>) -> Vec<String> {
    months.iter().filter(|m| closed.contains(&fiscal_year_of(m))).cloned().collect()
}

/// target_month が締めた年度の月でないことを表す SQL の条件。
pub const NOT_CLOSED_CONDITION: &str = "NOT EXISTS (
    SELECT 1 FROM fiscal_year_closings c
    WHERE target_month BETWEEN MAKEDATE(c.fiscal_year, 1) + INTERVAL 3 MONTH AND MAKEDATE(c.fiscal_year + 1, 1) + INTERVAL 2 MONTH)";

const CLOSING_SELECT: &str = "SELECT c.fiscal_year, c.closed_at, c.closed_by, u.name
    FROM fiscal_year_closings c JOIN users u ON u.id = c.closed_by";

/// GET /api/fiscal-years/closings。締めた年度の一覧（新しい年度から）。
pub async fn list(State(st): State<AppState>, _: CurrentUser) -> AppResult<Response> {
    let mut conn = st.db.get_conn().await?;
    let items: Vec<Closing> = conn.query(format!("{CLOSING_SELECT} ORDER BY c.fiscal_year DESC")).await?;
    Ok(http::list(&items))
}

fn path_fiscal_year(ids: &PathIds) -> Result<i32, AppError> {
    match ids.raw("fy").parse::<i32>() {
        Ok(fy) if (2000..=2100).contains(&fy) => Ok(fy),
        _ => Err(bad_request("年度は 2000〜2100 の数値で指定してください")),
    }
}

async fn find_closing(tx: &mut Tx, fy: i32, lock: &str) -> AppResult<Option<Closing>> {
    Ok(tx.exec_first(format!("{CLOSING_SELECT} WHERE c.fiscal_year = ?{lock}"), (fy,)).await?)
}

/// POST /api/fiscal-years/{fy}/close。FP&A のみ。締めに条件はない。
pub async fn close_year(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    ids: PathIds,
    body: RawBody,
) -> AppResult<Response> {
    let fy = path_fiscal_year(&ids)?;
    let req: ReasonRequest = body.decode()?;
    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    let r = tx.exec_drop("INSERT INTO fiscal_year_closings (fiscal_year, closed_by) VALUES (?, ?)", (fy, u.id)).await;
    if db::is_dup(&r) {
        return Err(conflict(format!("{fy}年度は既に締めています")));
    }
    r?;
    let created = find_closing(&mut tx, fy, "").await?.ok_or_else(|| anyhow::anyhow!("closing {fy} not found"))?;
    rec.insert(&mut tx, "fiscal_year_closings", fy as i64, &created).await?;
    tx.commit().await?;
    Ok(http::ok(&created))
}

/// POST /api/fiscal-years/{fy}/reopen。FP&A のみ。締めの解除は理由が必須。
pub async fn reopen_year(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    ids: PathIds,
    body: RawBody,
) -> AppResult<Response> {
    let fy = path_fiscal_year(&ids)?;
    let req: ReasonRequest = body.decode()?;
    if req.reason.trim().is_empty() {
        return Err(validation1("reason", "締めの解除には変更理由の入力が必要です"));
    }
    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    let Some(before) = find_closing(&mut tx, fy, " FOR UPDATE").await? else {
        return Err(conflict(format!("{fy}年度は締めていません")));
    };
    tx.exec_drop("DELETE FROM fiscal_year_closings WHERE fiscal_year = ?", (fy,)).await?;
    rec.delete(&mut tx, "fiscal_year_closings", fy as i64, &before).await?;
    tx.commit().await?;
    Ok(http::no_content())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fiscal_years() {
        assert_eq!(fiscal_year_of("2026-04"), 2026);
        assert_eq!(fiscal_year_of("2027-03"), 2026);
        let closed = HashSet::from([2026]);
        let months = ["2026-03".to_string(), "2026-04".into(), "2027-04".into()];
        assert_eq!(closed_months(&months, &closed), vec!["2026-04".to_string()]);
    }
}
