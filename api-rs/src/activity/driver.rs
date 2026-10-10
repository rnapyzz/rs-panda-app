//! 施策のドライバー定義。

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use axum::extract::State;
use axum::response::Response;
use chrono::NaiveDateTime;
use mysql_async::prelude::*;
use regex::Regex;
use serde::{Deserialize, Serialize};

use super::{ReasonRequest, lines, lock_editable};
use crate::auth::CurrentUser;
use crate::db::{self, Tx};
use crate::error::{conflict, not_found, validation1};
use crate::formula::Expr;
use crate::http::{self, Json, PathIds, Validator};
use crate::{AppResult, AppState, audit, jst};

const DRIVER_KINDS: [&str; 3] = ["value", "cost", "kpi"];

/// ドライバーの code（計算式で参照する識別子）の形式。
static DRIVER_CODE_PATTERN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z][a-z0-9_]{0,49}$").unwrap());

#[derive(Clone, Debug, Serialize)]
pub struct Driver {
    pub id: i64,
    pub activity_id: i64,
    pub code: String,
    pub name: String,
    pub driver_kind: String,
    pub unit: String,
    pub sort_order: i32,
    #[serde(serialize_with = "jst::ser")]
    pub created_at: NaiveDateTime,
    #[serde(serialize_with = "jst::ser")]
    pub updated_at: NaiveDateTime,
}
from_row!(Driver { id, activity_id, code, name, driver_kind, unit, sort_order, created_at, updated_at });

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct DriverRequest {
    code: String,
    name: String,
    driver_kind: String,
    unit: String,
    reason: String,
}

const DRIVER_SELECT: &str = "SELECT id, activity_id, code, name, driver_kind, COALESCE(unit, ''), sort_order, created_at, updated_at FROM activity_drivers";

pub async fn list<Q: Queryable>(q: &mut Q, activity_id: i64) -> AppResult<Vec<Driver>> {
    Ok(q.exec(format!("{DRIVER_SELECT} WHERE activity_id = ? ORDER BY sort_order, id"), (activity_id,)).await?)
}

async fn find(tx: &mut Tx, activity_id: i64, id: i64) -> AppResult<Driver> {
    let d: Option<Driver> = tx
        .exec_first(format!("{DRIVER_SELECT} WHERE id = ? AND activity_id = ? FOR UPDATE"), (id, activity_id))
        .await?;
    d.ok_or_else(|| not_found("ドライバーが見つかりません"))
}

fn validate(req: &mut DriverRequest) -> AppResult<()> {
    let mut v = Validator::new();
    req.name = v.text("name", "ドライバー名", &req.name, 100);
    req.unit = v.optional_text("unit", "単位", &req.unit, 30);
    if !DRIVER_CODE_PATTERN.is_match(&req.code) {
        v.add("code", "コードは半角英小文字で始まる、英小文字・数字・アンダースコアの50文字以内で入力してください");
    }
    if !DRIVER_KINDS.contains(&req.driver_kind.as_str()) {
        v.add("driver_kind", "種別は value / cost / kpi のいずれかを指定してください");
    }
    v.check()
}

/// POST /api/activities/{id}/drivers
pub async fn create(
    State(st): State<AppState>,
    CurrentUser(u): CurrentUser,
    ids: PathIds,
    Json(mut req): Json<DriverRequest>,
) -> AppResult<Response> {
    let activity_id = ids.get("id")?;
    validate(&mut req)?;

    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    lock_editable(&mut tx, &u, activity_id).await?;
    let r = tx
        .exec_drop(
            "INSERT INTO activity_drivers (activity_id, code, name, driver_kind, unit, sort_order)
             SELECT ?, ?, ?, ?, ?, COALESCE(MAX(sort_order), 0) + 1 FROM activity_drivers WHERE activity_id = ?",
            p![
                activity_id,
                req.code.clone(),
                req.name.clone(),
                req.driver_kind.clone(),
                db::null_str(&req.unit),
                activity_id
            ],
        )
        .await;
    if db::is_dup(&r) {
        return Err(validation1("code", "このコードは施策内で既に使われています"));
    }
    r?;
    let id = db::last_id(&tx);
    let created = find(&mut tx, activity_id, id).await?;
    rec.insert(&mut tx, "activity_drivers", id, &created).await?;
    tx.commit().await?;
    Ok(http::created(&created))
}

/// PUT /api/activities/{id}/drivers/{did}。計算式で使われているドライバーの code は変更できない。
pub async fn update(
    State(st): State<AppState>,
    CurrentUser(u): CurrentUser,
    ids: PathIds,
    Json(mut req): Json<DriverRequest>,
) -> AppResult<Response> {
    let activity_id = ids.get("id")?;
    let id = ids.get("did")?;
    validate(&mut req)?;

    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    lock_editable(&mut tx, &u, activity_id).await?;
    let before = find(&mut tx, activity_id, id).await?;
    if before.code != req.code && code_used_in_formulas(&mut tx, activity_id, &before.code).await? {
        return Err(validation1("code", "計算式で使われているためコードを変更できません"));
    }
    let r = tx
        .exec_drop(
            "UPDATE activity_drivers SET code = ?, name = ?, driver_kind = ?, unit = ? WHERE id = ?",
            p![req.code.clone(), req.name.clone(), req.driver_kind.clone(), db::null_str(&req.unit), id],
        )
        .await;
    if db::is_dup(&r) {
        return Err(validation1("code", "このコードは施策内で既に使われています"));
    }
    r?;
    let updated = find(&mut tx, activity_id, id).await?;
    rec.update(&mut tx, "activity_drivers", id, &before, &updated).await?;
    tx.commit().await?;
    Ok(http::ok(&updated))
}

/// DELETE /api/activities/{id}/drivers/{did}。
/// 計算式で使われている、または値が登録されているドライバーは削除できない。
pub async fn delete(
    State(st): State<AppState>,
    CurrentUser(u): CurrentUser,
    ids: PathIds,
    Json(req): Json<ReasonRequest>,
) -> AppResult<Response> {
    let activity_id = ids.get("id")?;
    let id = ids.get("did")?;
    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    lock_editable(&mut tx, &u, activity_id).await?;
    let before = find(&mut tx, activity_id, id).await?;
    if code_used_in_formulas(&mut tx, activity_id, &before.code).await? {
        return Err(conflict("計算式で使われているため削除できません"));
    }
    if db::count(&mut tx, "SELECT COUNT(*) FROM driver_values WHERE activity_driver_id = ?", p![id]).await? > 0 {
        return Err(conflict("ドライバー値が登録されているため削除できません"));
    }
    tx.exec_drop("DELETE FROM activity_drivers WHERE id = ?", (id,)).await?;
    rec.delete(&mut tx, "activity_drivers", id, &before).await?;
    tx.commit().await?;
    Ok(http::no_content())
}

/// 施策の内訳の計算式のいずれか（反映しないものも含む）が code を参照しているかを返す。
async fn code_used_in_formulas(tx: &mut Tx, activity_id: i64, code: &str) -> AppResult<bool> {
    Ok(lines::list(tx, activity_id, "").await?.iter().any(|l| {
        !l.expression.is_empty() && Expr::parse(&l.expression).is_ok_and(|e| e.idents().iter().any(|i| i == code))
    }))
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ReorderRequest {
    ids: Vec<i64>,
    reason: String,
}

/// PUT /api/activities/{id}/drivers/order。ドライバーの表示順を、指定した ID の順に並べ替える。
/// すべてのドライバーの ID を1回ずつ指定する。表示だけの変更なので変更理由は任意。
pub async fn reorder(
    State(st): State<AppState>,
    CurrentUser(u): CurrentUser,
    ids: PathIds,
    Json(req): Json<ReorderRequest>,
) -> AppResult<Response> {
    let activity_id = ids.get("id")?;
    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    lock_editable(&mut tx, &u, activity_id).await?;
    let current = list(&mut tx, activity_id).await?;
    let by_id: HashMap<i64, Driver> = current.iter().map(|d| (d.id, d.clone())).collect();
    let mut seen = HashSet::new();
    for id in &req.ids {
        if !by_id.contains_key(id) || !seen.insert(*id) {
            return Err(validation1("ids", "この施策のドライバーを、重複なく指定してください"));
        }
    }
    if req.ids.len() != current.len() {
        return Err(validation1("ids", "すべてのドライバーを指定してください"));
    }
    for (i, id) in req.ids.iter().enumerate() {
        let before = &by_id[id];
        let order = i as i32 + 1;
        if before.sort_order == order {
            continue;
        }
        tx.exec_drop("UPDATE activity_drivers SET sort_order = ? WHERE id = ?", (order, id)).await?;
        let mut after = before.clone();
        after.sort_order = order;
        rec.update(&mut tx, "activity_drivers", *id, before, &after).await?;
    }
    let items = list(&mut tx, activity_id).await?;
    tx.commit().await?;
    Ok(http::list(&items))
}
