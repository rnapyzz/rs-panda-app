//! マスタ（組織・セグメント・ユニット・勘定科目・ユーザー・確度の段階）の管理 API。
//!
//! 参照はログインユーザー全員、更新は FP&A（fpa_admin）のみが行える。
//! 更新はすべて変更セットと監査ログに記録する。変更理由（reason）は任意。

mod confidence;
mod subject;
mod transfer;
mod tree;
mod unit;
mod user;

use axum::Router;
use axum::routing::{get, post, put};

use crate::AppState;
use crate::codes;
use crate::error::{AppError, conflict, not_found};
use crate::http::Validator;

pub use tree::{ORGANIZATIONS, SEGMENTS, TreeKind};
pub use user::UNUSABLE_PASSWORD_HASH;

const MAX_NAME_LEN: usize = 100;

/// ルート。CSV のインポート・エクスポート（/export・/import）は /{id} より先に一致する。
pub fn routes() -> Router<AppState> {
    let mut r = Router::new();
    for (path, kind) in [("organizations", &ORGANIZATIONS), ("segments", &SEGMENTS)] {
        r = tree::routes(r, path, kind);
    }
    r.route("/api/units", get(unit::list).post(unit::create))
        .route("/api/units/export", get(transfer::export_units))
        .route("/api/units/import", post(transfer::import_units))
        .route("/api/units/{id}", get(unit::get).put(unit::update).delete(unit::delete))
        .route("/api/subjects", get(subject::list).post(subject::create))
        .route("/api/subjects/export", get(transfer::export_subjects))
        .route("/api/subjects/import", post(transfer::import_subjects))
        .route("/api/subjects/{id}", get(subject::get).put(subject::update).delete(subject::delete))
        .route("/api/confidence-levels", get(confidence::list).post(confidence::create))
        .route("/api/confidence-levels/{id}", put(confidence::update).delete(confidence::delete))
        .route("/api/users", get(user::list).post(user::create))
        .route("/api/users/export", get(transfer::export_users))
        .route("/api/users/import", post(transfer::import_users))
        .route("/api/users/{id}", get(user::get).put(user::update))
        .route("/api/users/{id}/password", put(user::reset_password))
}

/// コードを検証する。allow_empty なら空欄を許す（自動採番する）。
fn validate_code(v: &mut Validator, code: &str, allow_empty: bool) -> String {
    let code = code.trim();
    if code.is_empty() && allow_empty {
        return String::new();
    }
    if !codes::PATTERN.is_match(code) {
        v.add("code", codes::PATTERN_MESSAGE);
    }
    code.to_string()
}

/// 削除時の MySQL エラーを API エラーに変換する。
fn delete_error(r: Result<(), mysql_async::Error>, label: &str) -> Result<(), AppError> {
    if crate::db::is_referenced(&r) {
        return Err(conflict(format!("{label}は他のデータから参照されているため削除できません")));
    }
    Ok(r?)
}

fn not_found_label(label: &str) -> AppError {
    not_found(format!("{label}が見つかりません"))
}

/// 親が同じか（どちらも None も同じ）。
fn same_parent(a: Option<i64>, b: Option<i64>) -> bool {
    a == b
}
