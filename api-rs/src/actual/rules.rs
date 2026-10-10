//! 割当ルール（allocation_rules）。会計科目 × 部門（NULL は全部門）→ 施策。
//! 箱の ID で施策が決まらない明細を、受け皿の施策に割り当てる。部門まで一致するルールが優先する。
//! ルールを変えても取込済みの明細は変わらない（次の取込・再割当から反映する）。

use std::collections::HashSet;
use std::sync::LazyLock;

use axum::extract::State;
use axum::extract::multipart::{Multipart, MultipartRejection};
use axum::response::Response;
use chrono::NaiveDateTime;
use mysql_async::prelude::*;
use regex::Regex;
use serde::{Deserialize, Serialize};

use super::ReasonRequest;
use super::allocate::code_index;
use crate::audit::Recorder;
use crate::auth::{AdminUser, CurrentUser};
use crate::csvio::{self, ImportResult, RowErrors};
use crate::db::{self, Tx};
use crate::error::{not_found, validation1};
use crate::http::{self, Id, Json, Query, Validator};
use crate::{AppResult, AppState, audit, jst};

/// 部門コードの形式。CSV で扱えるよう、空白とカンマ・引用符は使えない。
pub static DEPARTMENT_PATTERN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"^[^\s,"]{1,50}$"#).unwrap());

#[derive(Clone, Debug, Serialize)]
pub struct AllocationRule {
    pub id: i64,
    pub gl_account_id: i64,
    pub gl_account_code: String,
    pub gl_account_name: String,
    pub department_code: Option<String>,
    pub activity_id: i64,
    pub activity_code: String,
    pub activity_name: String,
    #[serde(serialize_with = "jst::ser")]
    pub created_at: NaiveDateTime,
    #[serde(serialize_with = "jst::ser")]
    pub updated_at: NaiveDateTime,
}
from_row!(AllocationRule {
    id,
    gl_account_id,
    gl_account_code,
    gl_account_name,
    department_code,
    activity_id,
    activity_code,
    activity_name,
    created_at,
    updated_at
});

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct RuleRequest {
    gl_account_id: i64,
    department_code: String,
    activity_id: i64,
    reason: String,
}

const RULE_SELECT: &str = "SELECT r.id, r.gl_account_id, g.code, g.name, r.department_code, r.activity_id, a.code, a.name, r.created_at, r.updated_at
    FROM allocation_rules r JOIN gl_accounts g ON g.id = r.gl_account_id JOIN activities a ON a.id = r.activity_id";

async fn find_rule(tx: &mut Tx, id: i64, lock: &str) -> AppResult<AllocationRule> {
    let r: Option<AllocationRule> = tx.exec_first(format!("{RULE_SELECT} WHERE r.id = ?{lock}"), (id,)).await?;
    r.ok_or_else(|| not_found("割当ルールが見つかりません"))
}

/// GET /api/allocation-rules。会計科目・部門の順に返す（全部門のルールが先）。
pub async fn list(State(st): State<AppState>, _: CurrentUser) -> AppResult<Response> {
    let mut conn = st.db.get_conn().await?;
    let items: Vec<AllocationRule> = conn.query(format!("{RULE_SELECT} ORDER BY g.code, r.department_key")).await?;
    Ok(http::list(&items))
}

/// 部門コードを検証する。空なら None（全部門）。
fn validate_department(v: &mut Validator, s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if !DEPARTMENT_PATTERN.is_match(s) {
        v.add("department_code", "部門コードは空白・カンマ・引用符を含まない50文字以内で入力してください");
    }
    Some(s.to_string())
}

async fn validate_rule(tx: &mut Tx, req: &RuleRequest) -> AppResult<Option<String>> {
    let mut v = Validator::new();
    let dept = validate_department(&mut v, &req.department_code);
    if !db::exists(tx, "gl_accounts", req.gl_account_id).await? {
        v.add("gl_account_id", "会計科目を選んでください");
    }
    if !db::exists(tx, "activities", req.activity_id).await? {
        v.add("activity_id", "施策を選んでください");
    }
    v.check()?;
    Ok(dept)
}

const DUPLICATE_RULE: &str = "この会計科目・部門の割当ルールは既にあります";

fn duplicate_rule<T>(r: Result<T, mysql_async::Error>) -> AppResult<T> {
    if db::is_dup(&r) {
        return Err(validation1("department_code", DUPLICATE_RULE));
    }
    Ok(r?)
}

/// 割当ルールが重複したときのエラーか。
pub fn is_duplicate_rule(e: &crate::error::AppError) -> bool {
    e.details().is_some_and(|d| d.contains_key("department_code"))
}

/// POST /api/allocation-rules
pub async fn create(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    Json(req): Json<RuleRequest>,
) -> AppResult<Response> {
    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    let dept = validate_rule(&mut tx, &req).await?;
    let created = insert_rule(&mut tx, &rec, req.gl_account_id, dept.as_deref(), req.activity_id).await?;
    tx.commit().await?;
    Ok(http::created(&created))
}

pub async fn insert_rule(
    tx: &mut Tx,
    rec: &Recorder,
    account_id: i64,
    dept: Option<&str>,
    activity_id: i64,
) -> AppResult<AllocationRule> {
    duplicate_rule(
        tx.exec_drop(
            "INSERT INTO allocation_rules (gl_account_id, department_code, activity_id) VALUES (?, ?, ?)",
            (account_id, dept, activity_id),
        )
        .await,
    )?;
    let id = db::last_id(tx);
    let created = find_rule(tx, id, "").await?;
    rec.insert(tx, "allocation_rules", id, &created).await?;
    Ok(created)
}

/// PUT /api/allocation-rules/{id}
pub async fn update(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    Id(id): Id,
    Json(req): Json<RuleRequest>,
) -> AppResult<Response> {
    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    let before = find_rule(&mut tx, id, " FOR UPDATE").await?;
    let dept = validate_rule(&mut tx, &req).await?;
    duplicate_rule(
        tx.exec_drop(
            "UPDATE allocation_rules SET gl_account_id = ?, department_code = ?, activity_id = ? WHERE id = ?",
            (req.gl_account_id, dept, req.activity_id, id),
        )
        .await,
    )?;
    let updated = find_rule(&mut tx, id, "").await?;
    rec.update(&mut tx, "allocation_rules", id, &before, &updated).await?;
    tx.commit().await?;
    Ok(http::ok(&updated))
}

/// DELETE /api/allocation-rules/{id}。取込済みの明細の割当はそのまま残る。
pub async fn delete(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    Id(id): Id,
    Json(req): Json<ReasonRequest>,
) -> AppResult<Response> {
    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    let before = find_rule(&mut tx, id, " FOR UPDATE").await?;
    tx.exec_drop("DELETE FROM allocation_rules WHERE id = ?", (id,)).await?;
    rec.delete(&mut tx, "allocation_rules", id, &before).await?;
    tx.commit().await?;
    Ok(http::no_content())
}

// --- CSV ---

const RULE_COLUMNS: [&str; 3] = ["account_code", "department_code", "activity_code"];

/// GET /api/allocation-rules/export
pub async fn export(State(st): State<AppState>, _: CurrentUser) -> AppResult<Response> {
    let mut conn = st.db.get_conn().await?;
    let items: Vec<AllocationRule> = conn.query(format!("{RULE_SELECT} ORDER BY g.code, r.department_key")).await?;
    let out: Vec<Vec<String>> = items
        .into_iter()
        .map(|x| vec![x.gl_account_code, x.department_code.unwrap_or_default(), x.activity_code])
        .collect();
    csvio::write_csv("allocation-rules", &RULE_COLUMNS, &out)
}

struct RuleItem {
    account: i64,
    activity: i64,
    dept: Option<String>,
}

/// POST /api/allocation-rules/import。会計科目 × 部門で既存のルールと結びつけ、追加・更新する。
pub async fn import_csv(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    q: Query,
    mp: Result<Multipart, MultipartRejection>,
) -> AppResult<Response> {
    let up = csvio::read_upload(&q, mp).await?;
    let rows = csvio::parse(&up.data, &RULE_COLUMNS)?;
    let mut result = ImportResult { dry_run: up.dry_run, rows: rows.len(), ..Default::default() };

    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &up.reason).await?;
    let accounts = code_index(&mut tx, "SELECT code, id FROM gl_accounts").await?;
    let activities = code_index(&mut tx, "SELECT code, id FROM activities").await?;
    let mut errs = RowErrors::default();
    let mut items: Vec<RuleItem> = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    for row in &rows {
        let mut v = Validator::new();
        let dept = validate_department(&mut v, row.get("department_code"));
        let code = row.get("account_code");
        let account = accounts.get(code).copied().unwrap_or_else(|| {
            v.add("account_code", format!("会計科目コード {code} は登録されていません"));
            0
        });
        let ac = row.get("activity_code");
        let activity = activities.get(ac).copied().unwrap_or_else(|| {
            v.add("activity_code", format!("施策コード {ac} は登録されていません"));
            0
        });
        let dc = row.get("department_code");
        if !seen.insert((code.to_string(), dc.to_string())) {
            v.add("department_code", format!("会計科目 {code}・部門 {dc} が CSV 内で重複しています"));
        }
        if let Err(e) = v.check() {
            errs.add_details(row.line, e);
            continue;
        }
        items.push(RuleItem { account, activity, dept });
    }
    errs.check()?;

    for it in items {
        let id: Option<i64> = tx
            .exec_first(
                "SELECT id FROM allocation_rules WHERE gl_account_id = ? AND department_key = ? FOR UPDATE",
                (it.account, it.dept.clone().unwrap_or_default()),
            )
            .await?;
        let Some(id) = id else {
            insert_rule(&mut tx, &rec, it.account, it.dept.as_deref(), it.activity).await?;
            result.inserted += 1;
            continue;
        };
        let before = find_rule(&mut tx, id, "").await?;
        if before.activity_id == it.activity {
            result.unchanged += 1;
            continue;
        }
        tx.exec_drop("UPDATE allocation_rules SET activity_id = ? WHERE id = ?", (it.activity, id)).await?;
        let after = find_rule(&mut tx, id, "").await?;
        rec.update(&mut tx, "allocation_rules", id, &before, &after).await?;
        result.updated += 1;
    }
    csvio::finish(tx, result).await
}
