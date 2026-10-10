//! 閲覧の制限（docs/plan.md「2.17」）の共通処理。
//!
//! 閲覧制限のある科目（subjects.is_restricted）の金額は、FP&A と経営陣・レビュアーだけが見られる。
//! それ以外のロールには、金額を返す API のどこでも、その科目を除いて（合計からも除いて）返す。

use mysql_async::prelude::*;

use crate::AppResult;
use crate::auth::{ROLE_FPA_ADMIN, ROLE_VIEWER, User};

/// ユーザーに閲覧制限のある科目の金額を見せないかを返す。
pub fn hides_restricted(u: &User) -> bool {
    u.role != ROLE_FPA_ADMIN && u.role != ROLE_VIEWER
}

/// ユーザーが見られない科目を除く SQL の条件（" AND ..."）を返す。
/// col は科目 ID の列（例: "b.subject_id"）。すべて見られるユーザーには空文字を返す。
pub fn subject_filter(u: &User, col: &str) -> String {
    if !hides_restricted(u) {
        return String::new();
    }
    format!(" AND {col} NOT IN (SELECT rs.id FROM subjects rs WHERE rs.is_restricted)")
}

/// ユーザーの画面で閲覧制限のある科目を除いているか（レスポンスの restricted_hidden）を返す。
/// 見られないユーザーで、閲覧制限のある科目が1つでもあれば true。
pub async fn hidden<Q: Queryable>(q: &mut Q, u: &User) -> AppResult<bool> {
    if !hides_restricted(u) {
        return Ok(false);
    }
    let n = crate::db::count(q, "SELECT COUNT(*) FROM subjects WHERE is_restricted", p![]).await?;
    Ok(n > 0)
}

/// 科目に閲覧制限があるかを返す。
pub async fn is_restricted<Q: Queryable>(q: &mut Q, subject_id: i64) -> AppResult<bool> {
    let r: Option<bool> = q.exec_first("SELECT is_restricted FROM subjects WHERE id = ?", (subject_id,)).await?;
    Ok(r.unwrap_or(false))
}
