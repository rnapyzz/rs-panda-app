//! API のルーティングと共通ミドルウェアを組み立てる。

use std::panic::AssertUnwindSafe;
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post};
use futures::FutureExt;
use mysql_async::prelude::*;

use crate::error::{AppError, internal_error_response, not_found};
use crate::{AppState, activity, actual, auth, csvio, http, master, scenario};

/// API 全体のルーター。
pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/api/health", get(health))
        .route("/api/auth/login", post(auth::handler::login))
        .route("/api/auth/logout", post(auth::handler::logout))
        .route("/api/auth/config", get(auth::handler::config))
        .route("/api/auth/oidc/login", get(auth::handler::oidc_login))
        .route("/api/auth/oidc/callback", get(auth::handler::oidc_callback))
        .route("/api/auth/me", get(auth::handler::me))
        .merge(master::routes())
        .merge(activity::routes())
        .merge(scenario::routes())
        .merge(actual::routes());

    api
        // 未定義の /api パス（メソッドの違いを含む）は JSON で 404 を返す
        .fallback(api_not_found)
        .method_not_allowed_fallback(api_not_found)
        .layer(DefaultBodyLimit::max(csvio::MAX_UPLOAD_BYTES))
        // CSRF 対策: 別オリジンからのブラウザによる更新系リクエストを拒否する
        .layer(middleware::from_fn(cross_origin_protection))
        .layer(middleware::from_fn(access_log))
        .with_state(state)
}

async fn api_not_found() -> AppError {
    not_found("API が見つかりません")
}

/// GET /api/health。DB に接続できなければ 503。
async fn health(State(st): State<AppState>) -> Response {
    let ping = async {
        let mut conn = st.db.get_conn().await?;
        conn.ping().await?;
        Ok::<_, mysql_async::Error>(())
    };
    let ok = matches!(tokio::time::timeout(Duration::from_secs(2), ping).await, Ok(Ok(())));
    if ok {
        http::ok(&serde_json::json!({"status": "ok", "database": "ok"}))
    } else {
        http::json(
            StatusCode::SERVICE_UNAVAILABLE,
            &serde_json::json!({"status": "degraded", "database": "unavailable"}),
        )
    }
}

/// 別オリジンからの更新系リクエストを拒否する（Go の http.CrossOriginProtection と同じ判定）。
async fn cross_origin_protection(req: Request, next: Next) -> Response {
    if matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS) {
        return next.run(req).await;
    }
    let h = req.headers();
    let site = h.get("sec-fetch-site").and_then(|v| v.to_str().ok()).unwrap_or("");
    let allowed = match site {
        "same-origin" | "none" => true,
        "" => {
            let origin = h.get(header::ORIGIN).and_then(|v| v.to_str().ok()).unwrap_or("");
            let host = h
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
                .or_else(|| req.uri().authority().map(|a| a.to_string()))
                .unwrap_or_default();
            origin.is_empty() || url::Url::parse(origin).ok().is_some_and(|o| origin_host(&o) == host)
        }
        _ => false,
    };
    if allowed {
        next.run(req).await
    } else {
        AppError::new(StatusCode::FORBIDDEN, "forbidden", "別のサイトからのリクエストは受け付けません").into_response()
    }
}

fn origin_host(u: &url::Url) -> String {
    match (u.host_str(), u.port()) {
        (Some(h), Some(p)) => format!("{h}:{p}"),
        (Some(h), None) => h.to_string(),
        _ => String::new(),
    }
}

/// アクセスログを出し、ハンドラーの panic は 500 にする。
async fn access_log(req: Request, next: Next) -> Response {
    let start = Instant::now();
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let res = match AssertUnwindSafe(next.run(req)).catch_unwind().await {
        Ok(r) => r,
        Err(p) => {
            let msg = p
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            tracing::error!(method = %method, path, panic = msg, "panic");
            internal_error_response()
        }
    };
    tracing::info!(
        method = %method,
        path,
        status = res.status().as_u16(),
        duration_ms = start.elapsed().as_millis() as u64,
        "request"
    );
    res
}

use axum::response::IntoResponse;
