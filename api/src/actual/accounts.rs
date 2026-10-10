//! 会計科目（gl_accounts）。会計システムの勘定科目と、アプリの科目への対応。
//! 対象外（P/L に関係ない会計科目）の行は取り込まない。hide_details の会計科目は、明細を FP&A 以外に見せない。

use std::collections::{HashMap, HashSet};

use axum::extract::State;
use axum::extract::multipart::{Multipart, MultipartRejection};
use axum::response::Response;
use chrono::NaiveDateTime;
use mysql_async::prelude::*;
use serde::{Deserialize, Serialize};

use super::ReasonRequest;
use super::allocate::code_index;
use crate::auth::{AdminUser, CurrentUser};
use crate::csvio::{self, ImportResult, RowErrors};
use crate::db::{self, Tx};
use crate::error::{AppError, conflict, not_found, validation1};
use crate::http::{self, Id, Json, Query, Validator};
use crate::{AppResult, AppState, audit, codes, jst};

#[derive(Clone, Debug, Serialize)]
pub struct GlAccount {
    pub id: i64,
    pub code: String,
    pub name: String,
    pub subject_id: Option<i64>,
    pub is_excluded: bool,
    pub hide_details: bool,
    #[serde(serialize_with = "jst::ser")]
    pub created_at: NaiveDateTime,
    #[serde(serialize_with = "jst::ser")]
    pub updated_at: NaiveDateTime,
}
from_row!(GlAccount { id, code, name, subject_id, is_excluded, hide_details, created_at, updated_at });

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct GlAccountRequest {
    code: String,
    name: String,
    subject_id: Option<i64>,
    is_excluded: bool,
    hide_details: bool,
    reason: String,
}

const ACCOUNT_SELECT: &str =
    "SELECT id, code, name, subject_id, is_excluded, hide_details, created_at, updated_at FROM gl_accounts";

pub async fn find_account<Q: Queryable>(q: &mut Q, id: i64, lock: &str) -> AppResult<GlAccount> {
    let a: Option<GlAccount> = q.exec_first(format!("{ACCOUNT_SELECT} WHERE id = ?{lock}"), (id,)).await?;
    a.ok_or_else(|| not_found("会計科目が見つかりません"))
}

/// 全会計科目を ID → 会計科目で返す。
pub async fn load_accounts(tx: &mut Tx) -> AppResult<HashMap<i64, GlAccount>> {
    let rows: Vec<GlAccount> = tx.query(ACCOUNT_SELECT).await?;
    Ok(rows.into_iter().map(|a| (a.id, a)).collect())
}

/// GET /api/gl-accounts。コードの順に返す。
pub async fn list(State(st): State<AppState>, _: CurrentUser) -> AppResult<Response> {
    let mut conn = st.db.get_conn().await?;
    let items: Vec<GlAccount> = conn.query(format!("{ACCOUNT_SELECT} ORDER BY code")).await?;
    Ok(http::list(&items))
}

/// 会計科目の入力を検証し、保存するコード・名前・科目を返す。対象外なら科目は持たず、対象外でなければ科目が必須。
async fn validate_account(tx: &mut Tx, req: &GlAccountRequest) -> AppResult<(String, String, Option<i64>)> {
    let mut v = Validator::new();
    let code = req.code.trim().to_string();
    if !codes::PATTERN.is_match(&code) {
        v.add("code", codes::PATTERN_MESSAGE);
    }
    let name = v.text("name", "名前", &req.name, 100);
    let mut subject_id = req.subject_id;
    if req.is_excluded {
        subject_id = None;
    } else if let Some(id) = subject_id {
        if !db::exists(tx, "subjects", id).await? {
            v.add("subject_id", "科目が見つかりません");
        }
    } else {
        v.add("subject_id", "対応する科目を選んでください（P/L に関係ない会計科目は「対象外」にします）");
    }
    v.check()?;
    Ok((code, name, subject_id))
}

fn duplicate_code<T>(r: Result<T, mysql_async::Error>) -> AppResult<T> {
    if db::is_dup(&r) {
        return Err(validation1("code", "このコードは既に使われています"));
    }
    Ok(r?)
}

/// POST /api/gl-accounts
pub async fn create(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    Json(req): Json<GlAccountRequest>,
) -> AppResult<Response> {
    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    let (code, name, subject_id) = validate_account(&mut tx, &req).await?;
    duplicate_code(
        tx.exec_drop(
            "INSERT INTO gl_accounts (code, name, subject_id, is_excluded, hide_details) VALUES (?, ?, ?, ?, ?)",
            p![code, name, subject_id, req.is_excluded, req.hide_details],
        )
        .await,
    )?;
    let id = db::last_id(&tx);
    let created = find_account(&mut tx, id, "").await?;
    rec.insert(&mut tx, "gl_accounts", id, &created).await?;
    tx.commit().await?;
    Ok(http::created(&created))
}

/// PUT /api/gl-accounts/{id}。
/// 科目の対応や対象外を変えても、取込済みの明細は変わらない（次の取込・再割当から反映する）。
pub async fn update(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    Id(id): Id,
    Json(req): Json<GlAccountRequest>,
) -> AppResult<Response> {
    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    let before = find_account(&mut tx, id, " FOR UPDATE").await?;
    let (code, name, subject_id) = validate_account(&mut tx, &req).await?;
    duplicate_code(
        tx.exec_drop(
            "UPDATE gl_accounts SET code = ?, name = ?, subject_id = ?, is_excluded = ?, hide_details = ? WHERE id = ?",
            p![code, name, subject_id, req.is_excluded, req.hide_details, id],
        )
        .await,
    )?;
    let updated = find_account(&mut tx, id, "").await?;
    rec.update(&mut tx, "gl_accounts", id, &before, &updated).await?;
    tx.commit().await?;
    Ok(http::ok(&updated))
}

/// DELETE /api/gl-accounts/{id}。明細・割当ルールから参照されている会計科目は削除できない。
pub async fn delete(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    Id(id): Id,
    Json(req): Json<ReasonRequest>,
) -> AppResult<Response> {
    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    let before = find_account(&mut tx, id, " FOR UPDATE").await?;
    let r = tx.exec_drop("DELETE FROM gl_accounts WHERE id = ?", (id,)).await;
    if db::is_referenced(&r) {
        return Err(conflict("会計科目は明細または割当ルールから参照されているため削除できません"));
    }
    r?;
    rec.delete(&mut tx, "gl_accounts", id, &before).await?;
    tx.commit().await?;
    Ok(http::no_content())
}

// --- CSV ---

const ACCOUNT_COLUMNS: [&str; 4] = ["code", "name", "subject_code", "hide_details"];

/// GET /api/gl-accounts/export。対象外の会計科目は subject_code を空にする。
pub async fn export(State(st): State<AppState>, _: CurrentUser) -> AppResult<Response> {
    let mut conn = st.db.get_conn().await?;
    let rows: Vec<(String, String, String, bool)> = conn
        .query(
            "SELECT g.code, g.name, COALESCE(s.code, ''), g.hide_details
             FROM gl_accounts g LEFT JOIN subjects s ON s.id = g.subject_id ORDER BY g.code",
        )
        .await?;
    let out: Vec<Vec<String>> = rows.into_iter().map(|(c, n, s, h)| vec![c, n, s, h.to_string()]).collect();
    csvio::write_csv("gl-accounts", &ACCOUNT_COLUMNS, &out)
}

/// true / false（1 / 0・yes / no も可）を読む。空欄は false とする。
pub fn parse_bool(s: &str) -> Option<bool> {
    match s.to_lowercase().as_str() {
        "true" | "1" | "yes" => Some(true),
        "" | "false" | "0" | "no" => Some(false),
        _ => None,
    }
}

struct AccountItem {
    code: String,
    name: String,
    subject_id: Option<i64>,
    is_excluded: bool,
    hide_details: bool,
}

/// POST /api/gl-accounts/import。コードで既存の会計科目と結びつけ、追加・更新する。
/// subject_code が空の行は対象外にする。
pub async fn import_csv(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    q: Query,
    mp: Result<Multipart, MultipartRejection>,
) -> AppResult<Response> {
    let up = csvio::read_upload(&q, mp).await?;
    let rows = csvio::parse(&up.data, &ACCOUNT_COLUMNS)?;
    let mut result = ImportResult { dry_run: up.dry_run, rows: rows.len(), ..Default::default() };

    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &up.reason).await?;
    let subjects = code_index(&mut tx, "SELECT code, id FROM subjects").await?;
    let mut errs = RowErrors::default();
    let mut items: Vec<AccountItem> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for row in &rows {
        let mut v = Validator::new();
        let code = row.get("code").to_string();
        if !codes::PATTERN.is_match(&code) {
            v.add("code", codes::PATTERN_MESSAGE);
        }
        let name = v.text("name", "名前", row.get("name"), 100);
        let mut it = AccountItem { code, name, subject_id: None, is_excluded: false, hide_details: false };
        let sc = row.get("subject_code");
        if sc.is_empty() {
            it.is_excluded = true;
        } else if let Some(&id) = subjects.get(sc) {
            it.subject_id = Some(id);
        } else {
            v.add("subject_code", format!("科目コード {sc} は登録されていません"));
        }
        match parse_bool(row.get("hide_details")) {
            Some(b) => it.hide_details = b,
            None => v.add("hide_details", "hide_details は true / false で入力してください"),
        }
        if !seen.insert(it.code.clone()) {
            v.add("code", format!("コード {} が CSV 内で重複しています", it.code));
        }
        if let Err(e) = v.check() {
            errs.add_details(row.line, e);
            continue;
        }
        items.push(it);
    }
    errs.check()?;

    for it in items {
        let id: Option<i64> =
            tx.exec_first("SELECT id FROM gl_accounts WHERE code = ? FOR UPDATE", (it.code.clone(),)).await?;
        let Some(id) = id else {
            tx.exec_drop(
                "INSERT INTO gl_accounts (code, name, subject_id, is_excluded, hide_details) VALUES (?, ?, ?, ?, ?)",
                p![it.code, it.name, it.subject_id, it.is_excluded, it.hide_details],
            )
            .await?;
            let id = db::last_id(&tx);
            let created = find_account(&mut tx, id, "").await?;
            rec.insert(&mut tx, "gl_accounts", id, &created).await?;
            result.inserted += 1;
            continue;
        };
        let before = find_account(&mut tx, id, "").await?;
        if before.name == it.name
            && before.subject_id == it.subject_id
            && before.is_excluded == it.is_excluded
            && before.hide_details == it.hide_details
        {
            result.unchanged += 1;
            continue;
        }
        tx.exec_drop(
            "UPDATE gl_accounts SET name = ?, subject_id = ?, is_excluded = ?, hide_details = ? WHERE id = ?",
            p![it.name, it.subject_id, it.is_excluded, it.hide_details, id],
        )
        .await?;
        let after = find_account(&mut tx, id, "").await?;
        rec.update(&mut tx, "gl_accounts", id, &before, &after).await?;
        result.updated += 1;
    }
    csvio::finish(tx, result).await
}

/// 見つからない会計科目を、項目のエラーにする（未割当の割当で使う）。
pub fn as_field_error(e: AppError, field: &str, msg: &str) -> AppError {
    if e.is_not_found() { validation1(field, msg) } else { e }
}
