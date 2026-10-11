//! ユニットの統合と廃止、無効化のときの後任者への付け替え。

use axum::extract::State;
use axum::response::Response;
use mysql_async::prelude::*;
use serde::{Deserialize, Serialize};

use super::plans::ReasonRequest;
use super::{
    change_activity_owner, change_unit_owner, check_active_user, find_unit, merge_unit as merge, message, set_archived,
};
use crate::auth::AdminUser;
use crate::db;
use crate::error::{AppError, conflict, not_found, validation1};
use crate::http::{self, Id, Json, Validator};
use crate::{AppResult, AppState, audit};

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct MergeRequest {
    target_unit_id: i64,
    reason: String,
}

/// POST /api/units/{id}/merge。施策をすべて統合先へ移し、ユニットを廃止にする。理由は必須。
pub async fn merge_unit(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    Id(id): Id,
    Json(req): Json<MergeRequest>,
) -> AppResult<Response> {
    let mut v = Validator::new();
    if req.target_unit_id == 0 {
        v.add("target_unit_id", "統合先のユニットを選んでください");
    }
    if req.reason.trim().is_empty() {
        v.add("reason", "ユニットの統合には変更理由の入力が必要です");
    }
    v.check()?;
    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    let moved = merge(&mut tx, &rec, id, req.target_unit_id).await?;
    tx.commit().await?;
    Ok(http::ok(&serde_json::json!({"moved": moved})))
}

/// POST /api/units/{id}/archive。施策が所属しているユニットは廃止できない（統合を使う）。
pub async fn archive_unit(st: State<AppState>, u: AdminUser, id: Id, req: Json<ReasonRequest>) -> AppResult<Response> {
    set_unit_archived(st, u, id, req, true).await
}

/// POST /api/units/{id}/unarchive。廃止を取り消す。
pub async fn unarchive_unit(
    st: State<AppState>,
    u: AdminUser,
    id: Id,
    req: Json<ReasonRequest>,
) -> AppResult<Response> {
    set_unit_archived(st, u, id, req, false).await
}

async fn set_unit_archived(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    Id(id): Id,
    Json(req): Json<ReasonRequest>,
    archived: bool,
) -> AppResult<Response> {
    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    let before = find_unit(&mut tx, id).await.map_err(|e| match e {
        AppError::Api(_) => not_found("ユニットが見つかりません"),
        e => e,
    })?;
    if before.is_archived != archived {
        if archived && db::count(&mut tx, "SELECT COUNT(*) FROM activities WHERE unit_id = ?", p![id]).await? > 0 {
            return Err(conflict(
                "施策が所属しているユニットは廃止できません。「統合」で施策をほかのユニットへ移してください",
            ));
        }
        set_archived(&mut tx, &rec, &before, archived).await?;
    }
    tx.commit().await?;
    Ok(http::no_content())
}

/// 担当している施策と、所管しているユニット（廃止を除く）の数。
#[derive(Serialize)]
struct AssignmentCounts {
    activities: i64,
    units: i64,
}

/// GET /api/users/{id}/assignments
pub async fn assignments(State(st): State<AppState>, _: AdminUser, Id(id): Id) -> AppResult<Response> {
    let mut conn = st.db.get_conn().await?;
    if db::count(&mut conn, "SELECT COUNT(*) FROM users WHERE id = ?", p![id]).await? == 0 {
        return Err(not_found("ユーザーが見つかりません"));
    }
    let (activities, units): (i64, i64) = conn
        .exec_first(
            "SELECT (SELECT COUNT(*) FROM activities WHERE owner_user_id = ?),
                    (SELECT COUNT(*) FROM units WHERE owner_user_id = ? AND NOT is_archived)",
            (id, id),
        )
        .await?
        .unwrap_or_default();
    Ok(http::ok(&AssignmentCounts { activities, units }))
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct DeactivateRequest {
    /// 後任者。null なら担当者・マネージャーを未設定にする
    successor_user_id: Option<i64>,
    reason: String,
}

/// 監査ログに残すユーザー（パスワードハッシュは含めない）。
#[derive(Clone, Serialize)]
struct UserRow {
    id: i64,
    name: String,
    email: String,
    role: String,
    is_active: bool,
}
from_row!(UserRow { id, name, email, role, is_active });

/// POST /api/users/{id}/deactivate。担当している施策と所管ユニットを後任者へ付け替えてから、ユーザーを無効にする。
/// 自分自身は無効にできない。
pub async fn deactivate(
    State(st): State<AppState>,
    AdminUser(me): AdminUser,
    Id(id): Id,
    Json(req): Json<DeactivateRequest>,
) -> AppResult<Response> {
    if id == me.id {
        return Err(validation1("is_active", "自分自身は無効化できません"));
    }
    if req.successor_user_id == Some(id) {
        return Err(validation1("successor_user_id", "後任者に本人は選べません"));
    }
    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, me.id, None, &req.reason).await?;
    let before: Option<UserRow> =
        tx.exec_first("SELECT id, name, email, role, is_active FROM users WHERE id = ? FOR UPDATE", (id,)).await?;
    let before = before.ok_or_else(|| not_found("ユーザーが見つかりません"))?;
    check_active_user(&mut tx, req.successor_user_id)
        .await
        .map_err(|e| validation1("successor_user_id", message(&e)))?;
    let activity_ids: Vec<i64> =
        tx.exec("SELECT id FROM activities WHERE owner_user_id = ? ORDER BY id FOR UPDATE", (id,)).await?;
    for aid in &activity_ids {
        change_activity_owner(&mut tx, &rec, *aid, req.successor_user_id).await?;
    }
    let unit_ids: Vec<i64> =
        tx.exec("SELECT id FROM units WHERE owner_user_id = ? ORDER BY id FOR UPDATE", (id,)).await?;
    for uid in &unit_ids {
        change_unit_owner(&mut tx, &rec, *uid, req.successor_user_id).await?;
    }
    if before.is_active {
        tx.exec_drop("UPDATE users SET is_active = FALSE WHERE id = ?", (id,)).await?;
        tx.exec_drop("DELETE FROM sessions WHERE user_id = ?", (id,)).await?;
        let after = UserRow { is_active: false, ..before.clone() };
        rec.update(&mut tx, "users", id, &before, &after).await?;
    }
    tx.commit().await?;
    let moved = AssignmentCounts { activities: activity_ids.len() as i64, units: unit_ids.len() as i64 };
    Ok(http::ok(&serde_json::json!({"moved": moved})))
}
