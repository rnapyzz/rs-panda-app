//! メールアドレス＋パスワードによるログインとセッション管理。
//!
//! セッショントークンはランダムな32バイトを Cookie で渡し、DB には SHA-256 ハッシュのみ保存する。

pub mod handler;
pub mod oidc;
mod throttle;

use std::sync::OnceLock;
use std::time::Duration;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderMap, header};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use mysql_async::prelude::*;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::db::Pool;
use crate::error::{forbidden, unauthorized};
use crate::{AppError, AppResult, AppState, jst, password};

/// ユーザーのロール。
pub const ROLE_FPA_ADMIN: &str = "fpa_admin";
pub const ROLE_MANAGER: &str = "manager";
pub const ROLE_MEMBER: &str = "member";
pub const ROLE_VIEWER: &str = "viewer";
pub const ALL_ROLES: [&str; 4] = [ROLE_FPA_ADMIN, ROLE_MANAGER, ROLE_MEMBER, ROLE_VIEWER];

/// 定義済みのロールかを返す。
pub fn valid_role(r: &str) -> bool {
    ALL_ROLES.contains(&r)
}

/// セッション Cookie の名前。
pub const COOKIE_NAME: &str = "fpanda_session";

/// ログイン中のユーザー。
#[derive(Clone, Debug, Serialize, Default)]
pub struct User {
    pub id: i64,
    pub name: String,
    pub email: String,
    pub role: String,
}
from_row!(User { id, name, email, role });

impl User {
    pub fn is_admin(&self) -> bool {
        self.role == ROLE_FPA_ADMIN
    }
}

/// ログインの失敗。
#[derive(Debug)]
pub enum LoginError {
    /// メールアドレスまたはパスワードが誤っている、またはユーザーが無効化されている（どれかは区別しない）
    InvalidCredentials,
    /// ログインの失敗が多すぎて、ログインを止めている
    Throttled,
    /// SSO が有効な環境で、パスワードでのログインが許されていないロール
    PasswordNotAllowed,
    /// SSO: アプリに登録されていない（または無効な）ユーザー
    NotRegistered,
    /// SSO: 記録した SSO のアカウント ID と一致しない
    SubjectMismatch,
    Other(AppError),
}

impl<E: Into<AppError>> From<E> for LoginError {
    fn from(e: E) -> Self {
        LoginError::Other(e.into())
    }
}

/// ログインとセッションを扱う。
pub struct AuthService {
    db: Pool,
    ttl: Duration,
    /// パスワードでログインできるロール。None ならすべて（SSO が無効な環境）
    password_roles: Option<Vec<String>>,
}

/// ユーザーが存在しない場合もハッシュ計算を行い、応答時間からメールアドレスの登録有無を推測されないようにする。
static DUMMY_HASH: OnceLock<String> = OnceLock::new();

impl AuthService {
    /// ttl はセッションの有効期間。
    pub fn new(db: Pool, ttl: Duration) -> AuthService {
        AuthService { db, ttl, password_roles: None }
    }

    /// パスワードでログインできるロールを制限する（SSO が有効な環境で FP&A の非常用だけにする）。
    pub fn restrict_password_login(&mut self, roles: &[&str]) {
        self.password_roles = Some(roles.iter().map(|r| r.to_string()).collect());
    }

    /// パスワードでログインできるロール（None はすべて）。
    pub fn password_roles(&self) -> Option<&[String]> {
        self.password_roles.as_deref()
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// メールアドレスとパスワードを検証し、新しいセッションを作る。ip はログイン失敗の制限に使う。
    pub async fn login(&self, email: &str, pw: &str, ip: &str) -> Result<(String, User), LoginError> {
        let email = normalize_email(email);
        if self.throttled(&email, ip).await? {
            return Err(LoginError::Throttled);
        }
        match self.login_inner(&email, pw).await {
            Err(LoginError::InvalidCredentials) => {
                self.record_failure(&email, ip).await?;
                Err(LoginError::InvalidCredentials)
            }
            Err(e) => Err(e),
            Ok(v) => {
                self.clear_failures(&email).await?;
                Ok(v)
            }
        }
    }

    async fn login_inner(&self, email: &str, pw: &str) -> Result<(String, User), LoginError> {
        let mut conn = self.db.get_conn().await?;
        let row: Option<(i64, String, String, String, String, bool)> = conn
            .exec_first("SELECT id, name, email, role, password_hash, is_active FROM users WHERE email = ?", (email,))
            .await?;
        drop(conn);
        let Some((id, name, email, role, hash, active)) = row else {
            let dummy = match DUMMY_HASH.get() {
                Some(h) => h.clone(),
                None => {
                    let h = password::hash_async("dummy password for timing".into()).await;
                    DUMMY_HASH.get_or_init(|| h).clone()
                }
            };
            let _ = password::verify_async(pw.to_string(), dummy).await;
            return Err(LoginError::InvalidCredentials);
        };
        // パスワード未設定（CSV で追加したユーザーなど）はログインできない
        let ok = password::verify_async(pw.to_string(), hash).await.unwrap_or(false);
        if !ok || !active {
            return Err(LoginError::InvalidCredentials);
        }
        if let Some(roles) = &self.password_roles
            && !roles.contains(&role)
        {
            return Err(LoginError::PasswordNotAllowed);
        }
        let user = User { id, name, email, role };
        let token = self.create_session(user.id).await?;
        Ok((token, user))
    }

    /// 新しいセッションを作り、トークンを返す。
    pub async fn create_session(&self, user_id: i64) -> AppResult<String> {
        let token = new_token();
        let expires = jst::now() + chrono::Duration::from_std(self.ttl).unwrap_or_default();
        let mut conn = self.db.get_conn().await?;
        conn.exec_drop(
            "INSERT INTO sessions (id, user_id, expires_at) VALUES (?, ?, ?)",
            p![hash_token(&token), user_id, expires],
        )
        .await?;
        Ok(token)
    }

    /// セッショントークンからユーザーを取得する。セッションがなければ None。
    pub async fn authenticate(&self, token: &str) -> AppResult<Option<User>> {
        if token.is_empty() {
            return Ok(None);
        }
        let mut conn = self.db.get_conn().await?;
        let u: Option<User> = conn
            .exec_first(
                "SELECT u.id, u.name, u.email, u.role
                 FROM sessions s
                 JOIN users u ON u.id = s.user_id
                 WHERE s.id = ? AND s.expires_at > ? AND u.is_active",
                p![hash_token(token), jst::now()],
            )
            .await?;
        Ok(u)
    }

    /// セッションを削除する。
    pub async fn logout(&self, token: &str) -> AppResult<()> {
        if token.is_empty() {
            return Ok(());
        }
        let mut conn = self.db.get_conn().await?;
        conn.exec_drop("DELETE FROM sessions WHERE id = ?", (hash_token(token),)).await?;
        Ok(())
    }

    /// 期限切れのセッションと、古いログインの失敗の記録を削除する。
    pub async fn delete_expired_sessions(&self) -> AppResult<u64> {
        self.delete_old_attempts().await?;
        let mut conn = self.db.get_conn().await?;
        conn.exec_drop("DELETE FROM sessions WHERE expires_at <= ?", (jst::now(),)).await?;
        Ok(conn.affected_rows())
    }

    /// SSO で確かめたアカウントのユーザーでセッションを作る。
    /// 未登録・無効なら NotRegistered、記録した sub と一致しなければ SubjectMismatch。初回は sub を記録する。
    pub async fn login_oidc(&self, email: &str, sub: &str) -> Result<(String, User), LoginError> {
        let mut conn = self.db.get_conn().await?;
        let row: Option<(i64, String, String, String, bool, Option<String>)> = conn
            .exec_first(
                "SELECT id, name, email, role, is_active, oidc_subject FROM users WHERE email = ?",
                (normalize_email(email),),
            )
            .await?;
        let Some((id, name, email, role, active, subject)) = row else {
            return Err(LoginError::NotRegistered);
        };
        if !active {
            return Err(LoginError::NotRegistered);
        }
        match subject {
            Some(s) if s != sub => return Err(LoginError::SubjectMismatch),
            Some(_) => {}
            None => {
                let r = conn
                    .exec_drop("UPDATE users SET oidc_subject = ? WHERE id = ? AND oidc_subject IS NULL", p![sub, id])
                    .await;
                if crate::db::is_dup(&r) {
                    return Err(LoginError::SubjectMismatch); // 別のユーザーに記録済みのアカウント
                }
                r?;
            }
        }
        drop(conn);
        let token = self.create_session(id).await?;
        Ok((token, User { id, name, email, role }))
    }
}

/// メールアドレスを比較用に正規化する（前後の空白除去・小文字化）。
pub fn normalize_email(email: &str) -> String {
    email.trim().to_lowercase()
}

/// ランダムな32バイトの URL セーフな文字列。
pub fn random_string() -> String {
    let mut b = [0u8; 32];
    rand::fill(&mut b);
    URL_SAFE_NO_PAD.encode(b)
}

fn new_token() -> String {
    random_string()
}

fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// リクエストの Cookie の値（同じ名前が複数あれば最初のもの）。
pub fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    for v in headers.get_all(header::COOKIE) {
        let Ok(s) = v.to_str() else { continue };
        for part in s.split(';') {
            if let Some((k, val)) = part.trim().split_once('=')
                && k.trim() == name
            {
                let val = val.trim();
                let val = val.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(val);
                return Some(val.to_string());
            }
        }
    }
    None
}

/// Set-Cookie の値。max_age が負なら削除（Max-Age=0）。
pub fn set_cookie(name: &str, value: &str, path: &str, max_age: i64, secure: bool) -> String {
    let mut s = format!("{name}={value}; Path={path}");
    if max_age > 0 {
        s.push_str(&format!("; Max-Age={max_age}"));
    } else if max_age < 0 {
        s.push_str("; Max-Age=0");
    }
    s.push_str("; HttpOnly");
    if secure {
        s.push_str("; Secure");
    }
    s.push_str("; SameSite=Lax");
    s
}

fn session_token(parts: &Parts) -> String {
    cookie(&parts.headers, COOKIE_NAME).unwrap_or_default()
}

/// ログイン中のユーザー。ログインしていなければ 401（Go 版の RequireAuth）。
pub struct CurrentUser(pub User);

impl FromRequestParts<AppState> for CurrentUser {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        if let Some(u) = parts.extensions.get::<User>() {
            return Ok(CurrentUser(u.clone()));
        }
        match state.auth.authenticate(&session_token(parts)).await? {
            Some(u) => {
                parts.extensions.insert(u.clone());
                Ok(CurrentUser(u))
            }
            None => Err(unauthorized("ログインしてください")),
        }
    }
}

/// FP&A（fpa_admin）のユーザー。ログインしていなければ 401、ほかのロールなら 403（Go 版の RequireRole）。
pub struct AdminUser(pub User);

impl FromRequestParts<AppState> for AdminUser {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let CurrentUser(u) = CurrentUser::from_request_parts(parts, state).await?;
        if !u.is_admin() {
            return Err(forbidden());
        }
        Ok(AdminUser(u))
    }
}

/// ログインユーザーが roles のいずれかでなければ 403 を返す。
pub fn require_role(u: &User, roles: &[&str]) -> AppResult<()> {
    if roles.contains(&u.role.as_str()) { Ok(()) } else { Err(forbidden()) }
}
