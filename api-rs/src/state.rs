//! ハンドラーが共有する状態（Go 版の server.Deps）。

use std::ops::Deref;
use std::sync::Arc;

use crate::auth::AuthService;
use crate::auth::oidc::Oidc;
use crate::db::Pool;

pub struct Inner {
    pub db: Pool,
    pub auth: AuthService,
    /// true のとき Cookie に Secure 属性を付ける（HTTPS 環境用）
    pub cookie_secure: bool,
    /// true のとき、接続元の IP を X-Forwarded-For の最後の値（ALB が付けたもの）から読む
    pub trust_proxy: bool,
    /// SSO。None なら SSO は無効（パスワードでのログインだけ）
    pub oidc: Option<Oidc>,
    /// 締切と通知。None なら通知しない
    pub notifier: Option<std::sync::Arc<dyn crate::scenario::Notifier>>,
}

#[derive(Clone)]
pub struct AppState(pub Arc<Inner>);

impl AppState {
    pub fn new(inner: Inner) -> AppState {
        AppState(Arc::new(inner))
    }
}

impl Deref for AppState {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}
