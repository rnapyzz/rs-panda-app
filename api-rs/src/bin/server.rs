//! F-Panda の API サーバー。

use std::net::SocketAddr;
use std::time::Duration;

use fpanda_api::auth::oidc::Oidc;
use fpanda_api::auth::{AuthService, ROLE_FPA_ADMIN};
use fpanda_api::config::Config;
use fpanda_api::state::Inner;
use fpanda_api::{AppState, db, notify, orgchange, server};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().json().with_current_span(false).init();
    if let Err(e) = run().await {
        tracing::error!(error = format!("{e:#}"), "server stopped with error");
        std::process::exit(1);
    }
}

async fn run() -> anyhow::Result<()> {
    let cfg = Config::load();
    let pool = db::pool(&cfg.db);

    let mut auth = AuthService::new(pool.clone(), cfg.session_ttl);
    // SSO（OIDC）を設定した環境では、パスワードでのログインは FP&A の非常用だけにする
    let oidc = if cfg.oidc.client_id.is_empty() {
        None
    } else {
        auth.restrict_password_login(&[ROLE_FPA_ADMIN]);
        tracing::info!(issuer = cfg.oidc.issuer, allowed_domains = ?cfg.oidc.allowed_domains, "sso enabled");
        Some(Oidc::new(cfg.oidc.clone()))
    };
    let notify = notify::Service::new(
        pool.clone(),
        notify::Options {
            slack_webhook_url: cfg.slack_webhook_url.clone(),
            base_url: cfg.app_base_url.clone(),
            now: None,
        },
    );
    let state = AppState::new(Inner {
        db: pool.clone(),
        auth,
        cookie_secure: cfg.cookie_secure,
        trust_proxy: cfg.trust_proxy,
        oidc,
        notify: notify.clone(),
    });

    tokio::spawn(cleanup_sessions(state.clone()));
    // 締切の通知と組織変更の予約の適用は、1分ごとに確かめる
    tokio::spawn(notify.run());
    tokio::spawn(orgchange::run(pool.clone()));

    let addr = cfg.listen_addr();
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(addr, "listening");
    axum::serve(listener, server::router(state).into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!("shutting down");
    pool.disconnect().await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
}

/// 期限切れのセッションを1時間ごとに削除する。
async fn cleanup_sessions(state: AppState) {
    let mut ticker = tokio::time::interval(Duration::from_secs(3600));
    ticker.tick().await; // 最初の tick はすぐに来るので読み飛ばす
    loop {
        ticker.tick().await;
        match state.auth.delete_expired_sessions().await {
            Ok(n) if n > 0 => tracing::info!(count = n, "expired sessions deleted"),
            Ok(_) => {}
            Err(e) => tracing::error!(error = ?e, "cleanup sessions"),
        }
    }
}
