//! 施策の外部コード（会計・基幹システムの案件番号など）。

use std::sync::LazyLock;

use axum::extract::State;
use axum::response::Response;
use chrono::NaiveDateTime;
use mysql_async::prelude::*;
use regex::Regex;
use serde::{Deserialize, Serialize};

use super::{ReasonRequest, lock_editable};
use crate::auth::CurrentUser;
use crate::db::{self, Tx};
use crate::error::{not_found, validation1};
use crate::http::{self, Json, PathIds, Validator};
use crate::{AppResult, AppState, audit, jst};

/// 外部コードの形式。CSV で扱えるよう、空白とカンマ・引用符は使えない。
pub static EXTERNAL_CODE_PATTERN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"^[^\s,"]{1,100}$"#).unwrap());

/// 自動採番する施策コードの接頭辞（ACT-0001 の形式）。
pub const AUTO_CODE_PREFIX: &str = "ACT-";

#[derive(Clone, Debug, Serialize)]
pub struct ExternalCode {
    pub id: i64,
    pub activity_id: i64,
    pub code: String,
    pub note: String,
    #[serde(serialize_with = "jst::ser")]
    pub created_at: NaiveDateTime,
    #[serde(serialize_with = "jst::ser")]
    pub updated_at: NaiveDateTime,
}
from_row!(ExternalCode { id, activity_id, code, note, created_at, updated_at });

pub const EXTERNAL_CODE_SELECT: &str =
    "SELECT id, activity_id, code, COALESCE(note, ''), created_at, updated_at FROM activity_external_codes";

pub async fn list<Q: Queryable>(q: &mut Q, activity_id: i64) -> AppResult<Vec<ExternalCode>> {
    Ok(q.exec(format!("{EXTERNAL_CODE_SELECT} WHERE activity_id = ? ORDER BY code"), (activity_id,)).await?)
}

pub async fn find_by_id(tx: &mut Tx, id: i64) -> AppResult<ExternalCode> {
    let e: Option<ExternalCode> = tx.exec_first(format!("{EXTERNAL_CODE_SELECT} WHERE id = ?"), (id,)).await?;
    e.ok_or_else(|| not_found("外部コードが見つかりません"))
}

/// 施策コードが既存の外部コードと重ならないことを確認する。
pub async fn check_code_not_external(tx: &mut Tx, code: &str) -> AppResult<()> {
    if db::count(tx, "SELECT COUNT(*) FROM activity_external_codes WHERE code = ?", p![code]).await? > 0 {
        return Err(validation1("code", "この施策コードは外部コードとして使われています"));
    }
    Ok(())
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ExternalCodeRequest {
    code: String,
    note: String,
    reason: String,
}

/// POST /api/activities/{id}/external-codes。
/// 外部コードは、他の施策の外部コードや施策コードと重ならないこと。
pub async fn create(
    State(st): State<AppState>,
    CurrentUser(u): CurrentUser,
    ids: PathIds,
    Json(req): Json<ExternalCodeRequest>,
) -> AppResult<Response> {
    let activity_id = ids.get("id")?;
    let mut v = Validator::new();
    let code = v.text("code", "外部コード", &req.code, 100);
    if !code.is_empty() && !EXTERNAL_CODE_PATTERN.is_match(&code) {
        v.add("code", "外部コードに空白・カンマ・引用符は使えません");
    }
    let note = v.optional_text("note", "メモ", &req.note, 200);
    v.check()?;

    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    lock_editable(&mut tx, &u, activity_id).await?;
    // 施策コードとの重複
    let owner: Option<(i64, String)> =
        tx.exec_first("SELECT id, name FROM activities WHERE code = ?", (code.clone(),)).await?;
    if let Some((_, name)) = owner {
        return Err(validation1("code", format!("施策「{name}」の施策コードと同じです")));
    }
    // 他の施策の外部コードとの重複（どの施策かを示す）
    let owner: Option<(i64, String)> = tx
        .exec_first(
            "SELECT a.id, a.name FROM activity_external_codes e JOIN activities a ON a.id = e.activity_id WHERE e.code = ?",
            (code.clone(),),
        )
        .await?;
    if let Some((id, name)) = owner {
        if id == activity_id {
            return Err(validation1("code", "この施策に登録済みです"));
        }
        return Err(validation1(
            "code",
            format!("施策「{name}」に登録されています（外部コードは1つの施策にだけ紐づけられます）"),
        ));
    }
    let r = tx
        .exec_drop(
            "INSERT INTO activity_external_codes (activity_id, code, note) VALUES (?, ?, ?)",
            p![activity_id, code, db::null_str(&note)],
        )
        .await;
    if db::is_dup(&r) {
        return Err(validation1("code", "この外部コードは既に登録されています"));
    }
    r?;
    let id = db::last_id(&tx);
    let created = find_by_id(&mut tx, id).await?;
    rec.insert(&mut tx, "activity_external_codes", id, &created).await?;
    tx.commit().await?;
    Ok(http::created(&created))
}

/// DELETE /api/activities/{id}/external-codes/{eid}。
/// 取込済みの実績は施策に紐づいているため、外部コードを外しても実績は残る。
pub async fn delete(
    State(st): State<AppState>,
    CurrentUser(u): CurrentUser,
    ids: PathIds,
    Json(req): Json<ReasonRequest>,
) -> AppResult<Response> {
    let activity_id = ids.get("id")?;
    let id = ids.get("eid")?;
    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    lock_editable(&mut tx, &u, activity_id).await?;
    let before: Option<ExternalCode> = tx
        .exec_first(format!("{EXTERNAL_CODE_SELECT} WHERE id = ? AND activity_id = ? FOR UPDATE"), (id, activity_id))
        .await?;
    let before = before.ok_or_else(|| not_found("外部コードが見つかりません"))?;
    tx.exec_drop("DELETE FROM activity_external_codes WHERE id = ?", (id,)).await?;
    rec.delete(&mut tx, "activity_external_codes", id, &before).await?;
    tx.commit().await?;
    Ok(http::no_content())
}
