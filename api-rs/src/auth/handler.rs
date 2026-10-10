//! 認証 API のハンドラー。

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};

use super::oidc::{AuthState, OidcError};
use super::{ALL_ROLES, COOKIE_NAME, CurrentUser, LoginError, User, cookie, set_cookie};
use crate::error::{AppError, not_found, unauthorized};
use crate::http::{self, Json, Query};
use crate::{AppResult, AppState};

const OIDC_STATE_COOKIE: &str = "fpanda_oidc";
const OIDC_COOKIE_PATH: &str = "/api/auth/oidc";

/// 接続元のアドレス（サーバーを into_make_service_with_connect_info で動かしたとき）。
pub struct RemoteAddr(pub Option<SocketAddr>);

impl<S: Send + Sync> FromRequestParts<S> for RemoteAddr {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(RemoteAddr(parts.extensions.get::<ConnectInfo<SocketAddr>>().map(|c| c.0)))
    }
}

/// ログイン失敗の制限に使う接続元の IP を返す。
fn client_ip(st: &AppState, headers: &HeaderMap, remote: Option<SocketAddr>) -> String {
    if st.trust_proxy
        && let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok())
        && !xff.is_empty()
    {
        return xff.rsplit(',').next().unwrap_or("").trim().to_string();
    }
    remote.map(|a| a.ip().to_string()).unwrap_or_default()
}

fn with_cookie(mut res: Response, c: String) -> Response {
    if let Ok(v) = HeaderValue::from_str(&c) {
        res.headers_mut().append(header::SET_COOKIE, v);
    }
    res
}

fn session_cookie(st: &AppState, token: &str) -> String {
    set_cookie(COOKIE_NAME, token, "/", st.auth.ttl().as_secs() as i64, st.cookie_secure)
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct LoginRequest {
    #[serde(deserialize_with = "http::nullable")]
    email: String,
    #[serde(deserialize_with = "http::nullable")]
    password: String,
}

#[derive(Serialize)]
struct UserBody<'a> {
    user: &'a User,
}

/// POST /api/auth/login
pub async fn login(
    State(st): State<AppState>,
    RemoteAddr(remote): RemoteAddr,
    headers: HeaderMap,
    Json(req): Json<LoginRequest>,
) -> AppResult<Response> {
    let mut v = http::Validator::new();
    if req.email.is_empty() {
        v.add("email", "メールアドレスを入力してください");
    }
    if req.password.is_empty() {
        v.add("password", "パスワードを入力してください");
    }
    v.check()?;

    let ip = client_ip(&st, &headers, remote);
    match st.auth.login(&req.email, &req.password, &ip).await {
        Ok((token, u)) => Ok(with_cookie(http::ok(&UserBody { user: &u }), session_cookie(&st, &token))),
        Err(LoginError::Throttled) => Err(AppError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "too_many_attempts",
            "ログインの失敗が続いたため、しばらくログインできません。15分ほど待ってからやり直してください",
        )),
        Err(LoginError::PasswordNotAllowed) => Err(AppError::new(
            StatusCode::FORBIDDEN,
            "sso_required",
            "Google アカウントでログインしてください（パスワードでのログインは FP&A の非常用だけです）",
        )),
        Err(LoginError::Other(e)) => Err(e),
        Err(_) => Err(unauthorized("メールアドレスまたはパスワードが正しくありません")),
    }
}

/// GET /api/auth/config。ログイン画面用に、SSO が有効か、パスワードでログインできるロールを返す（未ログインで呼べる）。
pub async fn config(State(st): State<AppState>) -> Response {
    let roles: Vec<String> = match st.auth.password_roles() {
        Some(r) => r.to_vec(),
        None => ALL_ROLES.iter().map(|r| r.to_string()).collect(),
    };
    http::ok(&serde_json::json!({"sso": st.oidc.is_some(), "password_roles": roles}))
}

/// GET /api/auth/oidc/login?return_to=/path。検証値を短命の Cookie に入れ、IdP へリダイレクトする。
pub async fn oidc_login(State(st): State<AppState>, q: Query) -> AppResult<Response> {
    let Some(oidc) = &st.oidc else {
        return Err(not_found("SSO は設定されていません"));
    };
    let state = AuthState::new(q.get("return_to"));
    let target = match oidc.auth_url(&state).await {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(error = %e, "oidc: discovery");
            return Ok(http::redirect("/?login_error=failed"));
        }
    };
    let value = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&state)?);
    // SameSite=Lax: IdP からのリダイレクト（GET）で送られる
    Ok(with_cookie(
        http::redirect(&target),
        set_cookie(OIDC_STATE_COOKIE, &value, OIDC_COOKIE_PATH, 600, st.cookie_secure),
    ))
}

/// GET /api/auth/oidc/callback。ID トークンを検証してセッションを作り、元の画面へ戻す。
/// 失敗したら /?login_error=<理由> へ戻す（not_registered / domain / failed）。
pub async fn oidc_callback(State(st): State<AppState>, headers: HeaderMap, q: Query) -> AppResult<Response> {
    let Some(oidc) = &st.oidc else {
        return Err(not_found("SSO は設定されていません"));
    };
    // 検証値の Cookie は一度だけ使う
    let clear = set_cookie(OIDC_STATE_COOKIE, "", OIDC_COOKIE_PATH, -1, st.cookie_secure);
    let fail = |reason: &str, err: String| {
        tracing::warn!(reason, error = err, "oidc: login failed");
        let mut target = String::from("/?login_error=");
        target.extend(form_urlencoded::byte_serialize(reason.as_bytes()));
        Ok(with_cookie(http::redirect(&target), clear.clone()))
    };

    let Some(raw) = cookie(&headers, OIDC_STATE_COOKIE) else {
        return fail("failed", "state cookie missing".into());
    };
    let state: Option<AuthState> = URL_SAFE_NO_PAD
        .decode(raw)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .filter(|s: &AuthState| !s.state.is_empty());
    let Some(state) = state else {
        return fail("failed", "invalid state cookie".into());
    };
    if !q.get("error").is_empty() {
        return fail("failed", format!("idp: {}", q.get("error")));
    }
    if q.get("state") != state.state {
        return fail("failed", "state mismatch".into());
    }
    let claims = match oidc.exchange(q.get("code"), &state).await {
        Ok(c) => c,
        Err(OidcError::DomainNotAllowed) => return fail("domain", "domain not allowed".into()),
        Err(OidcError::NotRegistered) => return fail("not_registered", "user not registered".into()),
        Err(OidcError::Other(e)) => return fail("failed", format!("{e:#}")),
    };
    let token = match st.auth.login_oidc(&claims.email, &claims.sub).await {
        Ok((token, _)) => token,
        Err(LoginError::NotRegistered) => return fail("not_registered", "user not registered".into()),
        Err(LoginError::SubjectMismatch) => return fail("not_registered", "subject mismatch".into()),
        Err(LoginError::Other(e)) => return Err(e),
        Err(e) => return Err(anyhow::anyhow!("oidc login: {e:?}").into()),
    };
    let res = with_cookie(http::redirect(&state.return_to), clear);
    Ok(with_cookie(res, session_cookie(&st, &token)))
}

/// POST /api/auth/logout
pub async fn logout(State(st): State<AppState>, headers: HeaderMap) -> AppResult<Response> {
    st.auth.logout(&cookie(&headers, COOKIE_NAME).unwrap_or_default()).await?;
    Ok(with_cookie(http::no_content(), set_cookie(COOKIE_NAME, "", "/", -1, st.cookie_secure)))
}

/// GET /api/auth/me
pub async fn me(CurrentUser(u): CurrentUser) -> Response {
    http::ok(&UserBody { user: &u })
}
