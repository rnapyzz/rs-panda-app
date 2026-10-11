//! ユニット（施策を束ねる単位）。セグメントと組織の末端ノードに1つずつ所属する。

use axum::extract::State;
use axum::response::Response;
use chrono::NaiveDateTime;
use mysql_async::prelude::*;
use serde::{Deserialize, Serialize};

use super::tree::ReasonRequest;
use super::{MAX_NAME_LEN, ORGANIZATIONS, SEGMENTS, delete_error, not_found_label, validate_code};
use crate::auth::{AdminUser, CurrentUser};
use crate::db::{self, Tx};
use crate::error::{conflict, validation1};
use crate::http::{self, Id, Json, Query, Validator};
use crate::{AppResult, AppState, audit, codes, jst};

/// 自動採番するユニットコードの接頭辞（UNIT-0001 の形式）。
const UNIT_CODE_PREFIX: &str = "UNIT-";

/// ユニットの種別。service = サービス（プロフィットセンター）、cost_center = 共通費、corporate = 管理部門。
const UNIT_TYPES: [&str; 3] = ["service", "cost_center", "corporate"];

#[derive(Clone, Debug, Serialize)]
pub struct Unit {
    pub id: i64,
    pub code: String,
    pub name: String,
    pub unit_type: String,
    pub segment_id: i64,
    pub organization_id: i64,
    pub owner_user_id: Option<i64>,
    /// 廃止（統合したユニットなど）。一覧・選択肢に出さず、施策を所属させられない（docs/plan.md「2.15」）
    pub is_archived: bool,
    #[serde(serialize_with = "jst::ser")]
    pub created_at: NaiveDateTime,
    #[serde(serialize_with = "jst::ser")]
    pub updated_at: NaiveDateTime,
}
from_row!(Unit {
    id,
    code,
    name,
    unit_type,
    segment_id,
    organization_id,
    owner_user_id,
    is_archived,
    created_at,
    updated_at
});

#[derive(Deserialize, Default, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct UnitRequest {
    /// 作成時は空なら自動採番、更新時は空なら変更しない
    pub code: String,
    pub name: String,
    /// 省略時は service
    pub unit_type: String,
    pub segment_id: i64,
    pub organization_id: i64,
    pub owner_user_id: Option<i64>,
    pub reason: String,
}

pub const UNIT_SELECT: &str = "SELECT id, code, name, unit_type, segment_id, organization_id, owner_user_id, is_archived, created_at, updated_at FROM units";

pub async fn find_unit<Q: Queryable>(q: &mut Q, id: i64, lock: &str) -> AppResult<Unit> {
    let u: Option<Unit> = q.exec_first(format!("{UNIT_SELECT} WHERE id = ?{lock}"), (id,)).await?;
    u.ok_or_else(|| not_found_label("ユニット"))
}

/// GET /api/units。廃止したユニットは include_archived=true のときだけ含める。
pub async fn list(State(st): State<AppState>, _: CurrentUser, q: Query) -> AppResult<Response> {
    let filter = if q.get("include_archived") == "true" { "" } else { " WHERE NOT is_archived" };
    let mut conn = st.db.get_conn().await?;
    let items: Vec<Unit> = conn.exec(format!("{UNIT_SELECT}{filter} ORDER BY id"), ()).await?;
    Ok(http::list(&items))
}

/// GET /api/units/{id}
pub async fn get(State(st): State<AppState>, _: CurrentUser, Id(id): Id) -> AppResult<Response> {
    let mut conn = st.db.get_conn().await?;
    Ok(http::ok(&find_unit(&mut conn, id, "").await?))
}

/// POST /api/units
pub async fn create(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    Json(mut req): Json<UnitRequest>,
) -> AppResult<Response> {
    let name = validate_unit_request(&mut req, true)?;

    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    check_unit_refs(&mut tx, &req).await?;
    if req.code.is_empty() {
        req.code = codes::next(&mut tx, "units", UNIT_CODE_PREFIX, None).await?;
    }
    let r = tx
        .exec_drop(
            "INSERT INTO units (code, name, unit_type, segment_id, organization_id, owner_user_id) VALUES (?, ?, ?, ?, ?, ?)",
            p![req.code.clone(), name, req.unit_type.clone(), req.segment_id, req.organization_id, req.owner_user_id],
        )
        .await;
    if db::is_dup(&r) {
        return Err(validation1("code", "このコードは既に使われています"));
    }
    r?;
    let id = db::last_id(&tx);
    let created = find_unit(&mut tx, id, "").await?;
    rec.insert(&mut tx, "units", id, &created).await?;
    tx.commit().await?;
    Ok(http::created(&created))
}

/// PUT /api/units/{id}
pub async fn update(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    Id(id): Id,
    Json(mut req): Json<UnitRequest>,
) -> AppResult<Response> {
    let name = validate_unit_request(&mut req, true)?; // 空なら今のコードのまま

    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    let before = find_unit(&mut tx, id, " FOR UPDATE").await?;
    check_unit_refs(&mut tx, &req).await?;
    if req.code.is_empty() {
        req.code = before.code.clone();
    }
    let r = tx
        .exec_drop(
            "UPDATE units SET code = ?, name = ?, unit_type = ?, segment_id = ?, organization_id = ?, owner_user_id = ? WHERE id = ?",
            p![req.code.clone(), name, req.unit_type.clone(), req.segment_id, req.organization_id, req.owner_user_id, id],
        )
        .await;
    if db::is_dup(&r) {
        return Err(validation1("code", "このコードは既に使われています"));
    }
    r?;
    let updated = find_unit(&mut tx, id, "").await?;
    rec.update(&mut tx, "units", id, &before, &updated).await?;
    tx.commit().await?;
    Ok(http::ok(&updated))
}

/// DELETE /api/units/{id}。施策が所属している場合は削除できない。
pub async fn delete(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    Id(id): Id,
    Json(req): Json<ReasonRequest>,
) -> AppResult<Response> {
    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    let before = find_unit(&mut tx, id, " FOR UPDATE").await?;
    if db::count(&mut tx, "SELECT COUNT(*) FROM activities WHERE unit_id = ?", p![id]).await? > 0 {
        return Err(conflict("施策が所属しているため削除できません"));
    }
    let r = tx.exec_drop("DELETE FROM units WHERE id = ?", (id,)).await;
    delete_error(r, "ユニット")?;
    rec.delete(&mut tx, "units", id, &before).await?;
    tx.commit().await?;
    Ok(http::no_content())
}

/// ユニットの入力を検証する。allow_empty_code なら空のコードを許す（自動採番する）。
pub fn validate_unit_request(req: &mut UnitRequest, allow_empty_code: bool) -> AppResult<String> {
    let mut v = Validator::new();
    let name = v.text("name", "名称", &req.name, MAX_NAME_LEN);
    req.code = validate_code(&mut v, &req.code, allow_empty_code);
    if req.unit_type.is_empty() {
        req.unit_type = "service".into();
    }
    if !UNIT_TYPES.contains(&req.unit_type.as_str()) {
        v.add(
            "unit_type",
            "種別は service（サービス）/ cost_center（共通費）/ corporate（管理部門）のいずれかを指定してください",
        );
    }
    if req.segment_id <= 0 {
        v.add("segment_id", "セグメントを選択してください");
    }
    if req.organization_id <= 0 {
        v.add("organization_id", "組織を選択してください");
    }
    v.check()?;
    Ok(name)
}

/// 所属先のセグメント・組織が存在する末端ノードであること、担当者が存在することを確認する。
/// 所属先の行を FOR UPDATE でロックし、同時に子ノードが追加されるのを防ぐ。
async fn check_unit_refs(tx: &mut Tx, req: &UnitRequest) -> AppResult<()> {
    let mut v = Validator::new();
    for (kind, field, id) in
        [(&SEGMENTS, "segment_id", req.segment_id), (&ORGANIZATIONS, "organization_id", req.organization_id)]
    {
        match kind.find(tx, id, " FOR UPDATE").await {
            Err(e) if e.is_not_found() => {
                v.add(field, format!("{}が見つかりません", kind.label));
                continue;
            }
            r => r?,
        };
        let n = db::count(tx, &format!("SELECT COUNT(*) FROM {} WHERE parent_id = ?", kind.table), p![id]).await?;
        if n > 0 {
            v.add(field, format!("末端の{}を選択してください", kind.label));
        }
    }
    if let Some(owner) = req.owner_user_id
        && !db::exists(tx, "users", owner).await?
    {
        v.add("owner_user_id", "担当者が見つかりません");
    }
    v.check()
}
