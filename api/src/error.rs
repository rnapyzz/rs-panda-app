//! API のエラー。エラーレスポンスは次の形式で返す:
//!
//! ```json
//! {"error": {"code": "validation_error", "message": "...", "details": {"field": "..."}}}
//! ```

use std::collections::BTreeMap;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

/// 取込ファイルの行ごとのエラー。line は1始まりの行番号（ヘッダー行を含む）。
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct RowError {
    pub line: usize,
    pub message: String,
}

/// API のエラーレスポンス。
#[derive(Clone, Debug, Serialize)]
pub struct ApiError {
    #[serde(skip)]
    pub status: StatusCode,
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub details: BTreeMap<String, String>,
    /// ファイル取込などで、行ごとのエラーを返すときに使う。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rows: Vec<RowError>,
}

/// ハンドラーが返すエラー。Api はそのまま返し、Internal は 500 にして詳細はログにだけ出す。
#[derive(Debug)]
pub enum AppError {
    Api(ApiError),
    Internal(anyhow::Error),
}

pub type AppResult<T> = Result<T, AppError>;

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(e: E) -> Self {
        AppError::Internal(e.into())
    }
}

impl AppError {
    pub fn new(status: StatusCode, code: &str, message: impl Into<String>) -> AppError {
        AppError::Api(ApiError {
            status,
            code: code.into(),
            message: message.into(),
            details: BTreeMap::new(),
            rows: Vec::new(),
        })
    }

    /// 状態コード（Internal は 500）。
    pub fn status(&self) -> StatusCode {
        match self {
            AppError::Api(e) => e.status,
            AppError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// 404 の API エラーか。
    pub fn is_not_found(&self) -> bool {
        self.status() == StatusCode::NOT_FOUND && matches!(self, AppError::Api(_))
    }

    /// 検証エラーの項目 → メッセージ（検証エラーでなければ空）。
    pub fn details(&self) -> Option<&BTreeMap<String, String>> {
        match self {
            AppError::Api(e) if !e.details.is_empty() => Some(&e.details),
            _ => None,
        }
    }
}

/// リクエストの形式が不正なときのエラー。
pub fn bad_request(msg: impl Into<String>) -> AppError {
    AppError::new(StatusCode::BAD_REQUEST, "bad_request", msg)
}

/// 入力値の検証エラー。details はフィールド名 → メッセージ。
pub fn validation(details: BTreeMap<String, String>) -> AppError {
    AppError::Api(ApiError {
        status: StatusCode::UNPROCESSABLE_ENTITY,
        code: "validation_error".into(),
        message: "入力内容に誤りがあります".into(),
        details,
        rows: Vec::new(),
    })
}

/// 1項目だけの検証エラー。
pub fn validation1(field: &str, msg: impl Into<String>) -> AppError {
    validation(BTreeMap::from([(field.to_string(), msg.into())]))
}

/// 未ログインまたは認証失敗のエラー。
pub fn unauthorized(msg: impl Into<String>) -> AppError {
    AppError::new(StatusCode::UNAUTHORIZED, "unauthorized", msg)
}

/// 権限不足のエラー。
pub fn forbidden() -> AppError {
    AppError::new(StatusCode::FORBIDDEN, "forbidden", "この操作を行う権限がありません")
}

/// 対象が存在しないときのエラー。
pub fn not_found(msg: impl Into<String>) -> AppError {
    AppError::new(StatusCode::NOT_FOUND, "not_found", msg)
}

/// 一意制約違反や参照中のデータ削除など、現在の状態と矛盾するときのエラー。
pub fn conflict(msg: impl Into<String>) -> AppError {
    AppError::new(StatusCode::CONFLICT, "conflict", msg)
}

/// リクエストが大きすぎるときのエラー。
pub fn too_large(msg: impl Into<String>) -> AppError {
    AppError::new(StatusCode::PAYLOAD_TOO_LARGE, "too_large", msg)
}

pub fn internal_error_response() -> Response {
    AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", "サーバーでエラーが発生しました").into_response()
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let api = match self {
            AppError::Api(e) => e,
            AppError::Internal(e) => {
                tracing::error!(error = format!("{e:#}"), "internal error");
                return internal_error_response();
            }
        };
        #[derive(Serialize)]
        struct Body<'a> {
            error: &'a ApiError,
        }
        crate::http::json(api.status, &Body { error: &api })
    }
}
