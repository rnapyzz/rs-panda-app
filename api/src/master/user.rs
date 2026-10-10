//! ユーザー。パスワードハッシュはレスポンスにも監査ログにも含めない。

use std::sync::LazyLock;

use axum::extract::State;
use axum::response::Response;
use chrono::NaiveDateTime;
use mysql_async::prelude::*;
use regex::Regex;
use serde::{Deserialize, Serialize};

use super::{MAX_NAME_LEN, not_found_label};
use crate::auth::{AdminUser, CurrentUser, normalize_email, valid_role};
use crate::db;
use crate::error::validation1;
use crate::http::{self, Id, Json, Validator};
use crate::{AppResult, AppState, audit, jst, password};

/// CSV で追加したユーザーの仮のパスワードハッシュ。どのパスワードとも一致しない。
/// FP&A が画面でパスワードを設定するまでログインできない。
pub const UNUSABLE_PASSWORD_HASH: &str = "!";

#[derive(Clone, Debug, Serialize)]
pub struct UserRecord {
    pub id: i64,
    pub name: String,
    pub email: String,
    pub role: String,
    pub is_active: bool,
    /// Slack のメンバー ID（通知のメンション用、docs/plan.md「2.13」）。空は未登録
    pub slack_user_id: String,
    /// パスワードが設定されているか（CSV で追加したユーザーは未設定）
    pub has_password: bool,
    #[serde(serialize_with = "jst::ser")]
    pub created_at: NaiveDateTime,
    #[serde(serialize_with = "jst::ser")]
    pub updated_at: NaiveDateTime,
}
from_row!(UserRecord { id, name, email, role, is_active, slack_user_id, has_password, created_at, updated_at });

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct UserCreateRequest {
    name: String,
    email: String,
    role: String,
    password: String,
    slack_user_id: String,
    reason: String,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct UserUpdateRequest {
    name: String,
    email: String,
    role: String,
    is_active: bool,
    slack_user_id: String,
    reason: String,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct PasswordRequest {
    password: String,
    reason: String,
}

pub fn user_select() -> String {
    format!(
        "SELECT id, name, email, role, is_active, COALESCE(slack_user_id, ''), password_hash <> '{UNUSABLE_PASSWORD_HASH}', created_at, updated_at FROM users"
    )
}

pub async fn find_user<Q: Queryable>(q: &mut Q, id: i64, lock: &str) -> AppResult<UserRecord> {
    let u: Option<UserRecord> = q.exec_first(format!("{} WHERE id = ?{lock}", user_select()), (id,)).await?;
    u.ok_or_else(|| not_found_label("ユーザー"))
}

/// GET /api/users
pub async fn list(State(st): State<AppState>, _: CurrentUser) -> AppResult<Response> {
    let mut conn = st.db.get_conn().await?;
    let items: Vec<UserRecord> = conn.exec(format!("{} ORDER BY id", user_select()), ()).await?;
    Ok(http::list(&items))
}

/// GET /api/users/{id}
pub async fn get(State(st): State<AppState>, _: CurrentUser, Id(id): Id) -> AppResult<Response> {
    let mut conn = st.db.get_conn().await?;
    Ok(http::ok(&find_user(&mut conn, id, "").await?))
}

/// POST /api/users
pub async fn create(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    Json(req): Json<UserCreateRequest>,
) -> AppResult<Response> {
    let mut v = Validator::new();
    let (name, email) = validate_user_fields(&mut v, &req.name, &req.email, &req.role);
    let slack_id = validate_slack_id(&mut v, &req.slack_user_id);
    if let Err(msg) = password::validate(&req.password) {
        v.add("password", msg);
    }
    v.check()?;
    let hash = password::hash_async(req.password.clone()).await;

    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    let r = tx
        .exec_drop(
            "INSERT INTO users (name, email, password_hash, role, slack_user_id) VALUES (?, ?, ?, ?, ?)",
            p![name, email, hash, req.role.clone(), db::null_str(&slack_id)],
        )
        .await;
    if db::is_dup(&r) {
        return Err(validation1("email", "このメールアドレスは既に登録されています"));
    }
    r?;
    let id = db::last_id(&tx);
    let created = find_user(&mut tx, id, "").await?;
    rec.insert(&mut tx, "users", id, &created).await?;
    tx.commit().await?;
    Ok(http::created(&created))
}

/// PUT /api/users/{id}。氏名・メールアドレス・ロール・有効/無効を更新する。
/// 自分自身のロール変更と無効化はできない（FP&A 管理者が誰もいなくなるのを防ぐため）。
pub async fn update(
    State(st): State<AppState>,
    AdminUser(me): AdminUser,
    Id(id): Id,
    Json(req): Json<UserUpdateRequest>,
) -> AppResult<Response> {
    let mut v = Validator::new();
    let (name, email) = validate_user_fields(&mut v, &req.name, &req.email, &req.role);
    let slack_id = validate_slack_id(&mut v, &req.slack_user_id);
    v.check()?;

    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, me.id, None, &req.reason).await?;
    let before = find_user(&mut tx, id, " FOR UPDATE").await?;
    if id == me.id {
        if req.role != before.role {
            return Err(validation1("role", "自分自身のロールは変更できません"));
        }
        if !req.is_active {
            return Err(validation1("is_active", "自分自身は無効化できません"));
        }
    }
    let r = tx
        .exec_drop(
            "UPDATE users SET name = ?, email = ?, role = ?, is_active = ?, slack_user_id = ? WHERE id = ?",
            p![name, email, req.role.clone(), req.is_active, db::null_str(&slack_id), id],
        )
        .await;
    if db::is_dup(&r) {
        return Err(validation1("email", "このメールアドレスは既に登録されています"));
    }
    r?;
    // 無効化したユーザーのセッションは削除する。
    if !req.is_active {
        tx.exec_drop("DELETE FROM sessions WHERE user_id = ?", (id,)).await?;
    }
    let updated = find_user(&mut tx, id, "").await?;
    rec.update(&mut tx, "users", id, &before, &updated).await?;
    tx.commit().await?;
    Ok(http::ok(&updated))
}

/// PUT /api/users/{id}/password。FP&A がパスワードを再設定する。
/// 対象ユーザーの既存セッションはすべて削除する。監査ログにはパスワードを変更した事実のみ残す。
pub async fn reset_password(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    Id(id): Id,
    Json(req): Json<PasswordRequest>,
) -> AppResult<Response> {
    if let Err(msg) = password::validate(&req.password) {
        return Err(validation1("password", msg));
    }
    let hash = password::hash_async(req.password.clone()).await;

    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    find_user(&mut tx, id, " FOR UPDATE").await?;
    tx.exec_drop("UPDATE users SET password_hash = ? WHERE id = ?", p![hash, id]).await?;
    tx.exec_drop("DELETE FROM sessions WHERE user_id = ?", (id,)).await?;
    rec.record(&mut tx, "users", id, "update", None, Some(r#"{"password_changed":true}"#.into())).await?;
    tx.commit().await?;
    Ok(http::no_content())
}

/// Slack のメンバー ID の形式（例: U012AB3CD）。
static SLACK_ID_PATTERN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[UW][A-Z0-9]{2,19}$").unwrap());

/// Slack のメンバー ID を検証する。空は未登録。
pub fn validate_slack_id(v: &mut Validator, s: &str) -> String {
    let s = s.trim();
    if !s.is_empty() && !SLACK_ID_PATTERN.is_match(s) {
        v.add(
            "slack_user_id",
            "Slack のメンバー ID は U または W で始まる英大文字・数字で入力してください（例: U012AB3CD）",
        );
    }
    s.to_string()
}

pub fn validate_user_fields(v: &mut Validator, name: &str, email: &str, role: &str) -> (String, String) {
    let name = v.text("name", "氏名", name, MAX_NAME_LEN);
    let email = normalize_email(&v.text("email", "メールアドレス", email, 255));
    if !email.is_empty() && !valid_email(&email) {
        v.add("email", "メールアドレスの形式が正しくありません");
    }
    if !valid_role(role) {
        v.add("role", "ロールは fpa_admin / manager / member / viewer のいずれかを指定してください");
    }
    (name, email)
}

/// メールアドレスの最低限の形式（ローカル部@ドメイン.xx）を確認する。
fn valid_email(s: &str) -> bool {
    let Some((local, domain)) = s.split_once('@') else { return false };
    !local.is_empty()
        && s.matches('@').count() == 1
        && domain.contains('.')
        && !s.contains([' ', '\t', '\r', '\n'])
        && !domain.starts_with('.')
        && !domain.ends_with('.')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_format() {
        assert!(valid_email("a@example.com"));
        assert!(!valid_email("a@b@example.com"));
        assert!(!valid_email("a@example"));
        assert!(!valid_email("a@.example.com"));
    }
}
