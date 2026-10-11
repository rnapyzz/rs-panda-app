//! 勘定科目。親科目を持てる（同じ区分の科目のみ）。

use axum::extract::State;
use axum::response::Response;
use chrono::NaiveDateTime;
use mysql_async::prelude::*;
use serde::{Deserialize, Serialize};

use super::tree::ReasonRequest;
use super::{MAX_NAME_LEN, delete_error, not_found_label};
use crate::auth::{AdminUser, CurrentUser};
use crate::db::{self, Tx};
use crate::error::{conflict, validation1};
use crate::http::{self, Id, Json, Validator};
use crate::{AppResult, AppState, audit, jst};

#[derive(Clone, Debug, Serialize)]
pub struct Subject {
    pub id: i64,
    pub parent_id: Option<i64>,
    pub code: String,
    pub name: String,
    pub category: String,
    pub sort_order: i32,
    /// 閲覧制限（docs/plan.md「2.17」）。金額は FP&A と経営陣だけが見られる
    pub is_restricted: bool,
    #[serde(serialize_with = "jst::ser")]
    pub created_at: NaiveDateTime,
    #[serde(serialize_with = "jst::ser")]
    pub updated_at: NaiveDateTime,
}
from_row!(Subject { id, parent_id, code, name, category, sort_order, is_restricted, created_at, updated_at });

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct SubjectRequest {
    pub parent_id: Option<i64>,
    pub code: String,
    pub name: String,
    pub category: String,
    pub sort_order: i32,
    pub is_restricted: bool,
    pub reason: String,
}

pub const SUBJECT_SELECT: &str =
    "SELECT id, parent_id, code, name, category, sort_order, is_restricted, created_at, updated_at FROM subjects";

pub async fn find_subject<Q: Queryable>(q: &mut Q, id: i64, lock: &str) -> AppResult<Subject> {
    let s: Option<Subject> = q.exec_first(format!("{SUBJECT_SELECT} WHERE id = ?{lock}"), (id,)).await?;
    s.ok_or_else(|| not_found_label("科目"))
}

/// GET /api/subjects。区分（収益→費用）・表示順・コードの順に返す。
pub async fn list(State(st): State<AppState>, _: CurrentUser) -> AppResult<Response> {
    let mut conn = st.db.get_conn().await?;
    let items: Vec<Subject> = conn.exec(format!("{SUBJECT_SELECT} ORDER BY category, sort_order, code"), ()).await?;
    Ok(http::list(&items))
}

/// GET /api/subjects/{id}
pub async fn get(State(st): State<AppState>, _: CurrentUser, Id(id): Id) -> AppResult<Response> {
    let mut conn = st.db.get_conn().await?;
    Ok(http::ok(&find_subject(&mut conn, id, "").await?))
}

/// POST /api/subjects
pub async fn create(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    Json(req): Json<SubjectRequest>,
) -> AppResult<Response> {
    let (code, name) = validate_subject_request(&req)?;

    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    check_subject_parent(&mut tx, 0, &req).await?;
    let r = tx
        .exec_drop(
            "INSERT INTO subjects (parent_id, code, name, category, sort_order, is_restricted) VALUES (?, ?, ?, ?, ?, ?)",
            p![req.parent_id, code, name, req.category.clone(), req.sort_order, req.is_restricted],
        )
        .await;
    if db::is_dup(&r) {
        return Err(validation1("code", "この科目コードは既に使われています"));
    }
    r?;
    let id = db::last_id(&tx);
    let created = find_subject(&mut tx, id, "").await?;
    rec.insert(&mut tx, "subjects", id, &created).await?;
    tx.commit().await?;
    Ok(http::created(&created))
}

/// PUT /api/subjects/{id}
pub async fn update(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    Id(id): Id,
    Json(req): Json<SubjectRequest>,
) -> AppResult<Response> {
    let (code, name) = validate_subject_request(&req)?;

    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    let before = find_subject(&mut tx, id, " FOR UPDATE").await?;
    check_subject_parent(&mut tx, id, &req).await?;
    if req.category != before.category
        && db::count(&mut tx, "SELECT COUNT(*) FROM subjects WHERE parent_id = ?", p![id]).await? > 0
    {
        return Err(validation1("category", "子科目がある科目の区分は変更できません"));
    }
    let r = tx
        .exec_drop(
            "UPDATE subjects SET parent_id = ?, code = ?, name = ?, category = ?, sort_order = ?, is_restricted = ? WHERE id = ?",
            p![req.parent_id, code, name, req.category.clone(), req.sort_order, req.is_restricted, id],
        )
        .await;
    if db::is_dup(&r) {
        return Err(validation1("code", "この科目コードは既に使われています"));
    }
    r?;
    let updated = find_subject(&mut tx, id, "").await?;
    rec.update(&mut tx, "subjects", id, &before, &updated).await?;
    tx.commit().await?;
    Ok(http::ok(&updated))
}

/// DELETE /api/subjects/{id}。子科目や金額データから参照されている場合は削除できない。
pub async fn delete(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    Id(id): Id,
    Json(req): Json<ReasonRequest>,
) -> AppResult<Response> {
    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    let before = find_subject(&mut tx, id, " FOR UPDATE").await?;
    if db::count(&mut tx, "SELECT COUNT(*) FROM subjects WHERE parent_id = ?", p![id]).await? > 0 {
        return Err(conflict("子科目があるため削除できません"));
    }
    let r = tx.exec_drop("DELETE FROM subjects WHERE id = ?", (id,)).await;
    delete_error(r, "科目")?;
    rec.delete(&mut tx, "subjects", id, &before).await?;
    tx.commit().await?;
    Ok(http::no_content())
}

pub fn validate_subject_request(req: &SubjectRequest) -> AppResult<(String, String)> {
    let mut v = Validator::new();
    let code = v.text("code", "科目コード", &req.code, 50);
    let name = v.text("name", "科目名", &req.name, MAX_NAME_LEN);
    if req.category != "revenue" && req.category != "expense" {
        v.add("category", "区分は revenue（収益）か expense（費用）を指定してください");
    }
    v.check()?;
    Ok((code, name))
}

/// 親科目が存在し、同じ区分で、循環しないことを確認する。id は更新対象（新規作成時は 0）。
async fn check_subject_parent(tx: &mut Tx, id: i64, req: &SubjectRequest) -> AppResult<()> {
    let Some(parent_id) = req.parent_id else { return Ok(()) };
    if parent_id == id {
        return Err(validation1("parent_id", "自分自身を親にはできません"));
    }
    let parent = match find_subject(tx, parent_id, " FOR SHARE").await {
        Err(e) if e.is_not_found() => return Err(validation1("parent_id", "親科目が見つかりません")),
        r => r?,
    };
    if parent.category != req.category {
        return Err(validation1("parent_id", "親科目と区分を揃えてください"));
    }
    if id == 0 {
        return Ok(());
    }
    // 親をたどって自分自身に行き着くなら循環している。
    let n = db::count(
        tx,
        "WITH RECURSIVE anc AS (
            SELECT id, parent_id FROM subjects WHERE id = ?
            UNION ALL
            SELECT s.id, s.parent_id FROM subjects s JOIN anc ON s.id = anc.parent_id
        )
        SELECT COUNT(*) FROM anc WHERE id = ?",
        p![parent_id, id],
    )
    .await?;
    if n > 0 {
        return Err(validation1("parent_id", "配下の科目を親にはできません"));
    }
    Ok(())
}
