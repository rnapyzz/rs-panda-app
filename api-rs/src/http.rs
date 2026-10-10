//! JSON API のリクエスト・レスポンスの共通処理（Go 版の httpx）。

use std::collections::{BTreeMap, HashMap};

use axum::extract::{FromRequest, FromRequestParts, RawPathParams, Request};
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::{AppError, bad_request, not_found, too_large, validation};

const MAX_BODY_BYTES: usize = 1 << 20; // 1MB

/// v を JSON で書き出す（Go の json.Encoder と同じく末尾に改行を付ける）。
pub fn json<T: Serialize + ?Sized>(status: StatusCode, v: &T) -> Response {
    match serde_json::to_vec(v) {
        Ok(mut b) => {
            b.push(b'\n');
            let mut res = (status, b).into_response();
            res.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json; charset=utf-8"));
            res
        }
        Err(e) => {
            tracing::error!(error = %e, "write json");
            crate::error::internal_error_response()
        }
    }
}

/// 200 で v を返す。
pub fn ok<T: Serialize + ?Sized>(v: &T) -> Response {
    json(StatusCode::OK, v)
}

/// 201 で v を返す。
pub fn created<T: Serialize + ?Sized>(v: &T) -> Response {
    json(StatusCode::CREATED, v)
}

/// 204 を返す。
pub fn no_content() -> Response {
    StatusCode::NO_CONTENT.into_response()
}

/// items を {"items": [...]} の形で書き出す。
pub fn list<T: Serialize>(items: &[T]) -> Response {
    #[derive(Serialize)]
    struct Items<'a, T> {
        items: &'a [T],
    }
    ok(&Items { items })
}

/// リクエストボディの JSON。未知のフィールドは型の `deny_unknown_fields` でエラーにする。
/// ボディが空の場合は既定値（Go 版のゼロ値と同じ）にする。
pub struct Json<T>(pub T);

impl<S, T> FromRequest<S> for Json<T>
where
    S: Send + Sync,
    T: DeserializeOwned + Default,
{
    type Rejection = AppError;

    async fn from_request(req: Request, _: &S) -> Result<Self, Self::Rejection> {
        let bytes = axum::body::to_bytes(req.into_body(), MAX_BODY_BYTES)
            .await
            .map_err(|_| too_large("リクエストが大きすぎます"))?;
        decode_json(&bytes).map(Json)
    }
}

/// JSON をデコードする（空なら既定値）。
pub fn decode_json<T: DeserializeOwned + Default>(bytes: &[u8]) -> Result<T, AppError> {
    if bytes.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(T::default());
    }
    let mut v: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| {
        if e.to_string().contains("trailing") {
            bad_request("リクエストの JSON が不正です: 複数の値が含まれています")
        } else {
            bad_request(format!("リクエストの JSON が不正です: {e}"))
        }
    })?;
    // 本文が null なら既定値（Go の encoding/json は null を「値を変えない」として扱う）
    if v.is_null() {
        return Ok(T::default());
    }
    strip_nulls(&mut v);
    serde_json::from_value(v).map_err(|e| bad_request(format!("リクエストの JSON が不正です: {e}")))
}

/// オブジェクトの null の項目を取り除く。Go の encoding/json は null を「値を変えない」として扱うため、
/// 項目がないとき（既定値）と同じにする。
fn strip_nulls(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(m) => {
            m.retain(|_, x| !x.is_null());
            m.values_mut().for_each(strip_nulls);
        }
        serde_json::Value::Array(a) => a.iter_mut().for_each(strip_nulls),
        _ => {}
    }
}

/// JSON の数値をそのままの文字列で持つ（Go の json.Number）。リクエストでは数値か数値の文字列を受け付け、
/// レスポンスでは数値として書き出す。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RawNum(pub String);

impl Serialize for RawNum {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use std::str::FromStr;
        match serde_json::Number::from_str(&self.0) {
            Ok(n) => n.serialize(s),
            Err(_) => s.serialize_str(&self.0),
        }
    }
}

impl<'de> serde::Deserialize<'de> for RawNum {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use std::str::FromStr;
        match serde_json::Value::deserialize(d)? {
            serde_json::Value::Number(n) => Ok(RawNum(n.to_string())),
            serde_json::Value::String(s) if serde_json::Number::from_str(&s).is_ok() => Ok(RawNum(s)),
            other => Err(serde::de::Error::custom(format!("invalid number {other}"))),
        }
    }
}

/// パスパラメーターを正の整数として取り出す。正しくなければ 404。
pub fn parse_id(s: &str) -> Result<i64, AppError> {
    match s.parse::<i64>() {
        Ok(id) if id > 0 => Ok(id),
        _ => Err(not_found("指定されたデータが見つかりません")),
    }
}

/// パスパラメーター（名前 → 値）。`ids.get("id")?` で正の整数として取り出す。
pub struct PathIds(HashMap<String, String>);

impl PathIds {
    pub fn get(&self, name: &str) -> Result<i64, AppError> {
        parse_id(self.0.get(name).map(String::as_str).unwrap_or(""))
    }

    pub fn raw(&self, name: &str) -> &str {
        self.0.get(name).map(String::as_str).unwrap_or("")
    }
}

impl<S: Send + Sync> FromRequestParts<S> for PathIds {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let params = RawPathParams::from_request_parts(parts, state)
            .await
            .map_err(|_| not_found("指定されたデータが見つかりません"))?;
        Ok(PathIds(params.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()))
    }
}

/// パスパラメーター {id}。正の整数でなければ 404。
pub struct Id(pub i64);

impl<S: Send + Sync> FromRequestParts<S> for Id {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        PathIds::from_request_parts(parts, state).await?.get("id").map(Id)
    }
}

/// クエリ文字列（同じ名前が複数あれば最初の値。Go の r.URL.Query().Get と同じ）。
#[derive(Default)]
pub struct Query(HashMap<String, String>);

impl Query {
    pub fn parse(q: &str) -> Query {
        let mut m = HashMap::new();
        for (k, v) in form_urlencoded::parse(q.as_bytes()) {
            m.entry(k.into_owned()).or_insert_with(|| v.into_owned());
        }
        Query(m)
    }

    /// 値（なければ空文字）。
    pub fn get(&self, key: &str) -> &str {
        self.0.get(key).map(String::as_str).unwrap_or("")
    }

    pub fn has(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Query {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Query::parse(parts.uri.query().unwrap_or("")))
    }
}

/// 入力値の検証エラーをフィールドごとに集める。
#[derive(Default, Debug)]
pub struct Validator(pub BTreeMap<String, String>);

impl Validator {
    pub fn new() -> Validator {
        Validator::default()
    }

    /// フィールドにエラーを追加する。同じフィールドには最初のエラーだけを残す。
    pub fn add(&mut self, field: &str, msg: impl Into<String>) {
        self.0.entry(field.to_string()).or_insert_with(|| msg.into());
    }

    /// 必須の文字列を検証し、前後の空白を除いた値を返す。
    pub fn text(&mut self, field: &str, label: &str, s: &str, max_len: usize) -> String {
        let s = s.trim();
        if s.is_empty() {
            self.add(field, format!("{label}を入力してください"));
        } else if s.chars().count() > max_len {
            self.add(field, format!("{label}は{max_len}文字以下にしてください"));
        }
        s.to_string()
    }

    /// 任意の文字列を検証し、前後の空白を除いた値を返す。
    pub fn optional_text(&mut self, field: &str, label: &str, s: &str, max_len: usize) -> String {
        let s = s.trim();
        if s.chars().count() > max_len {
            self.add(field, format!("{label}は{max_len}文字以下にしてください"));
        }
        s.to_string()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// エラーがあれば検証エラーを返す。
    pub fn check(self) -> Result<(), AppError> {
        if self.0.is_empty() { Ok(()) } else { Err(validation(self.0)) }
    }
}

/// 添付ファイルのレスポンス。
pub fn attachment(content_type: &str, filename: &str, body: Vec<u8>) -> Response {
    let mut res = (StatusCode::OK, body).into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_str(content_type).unwrap());
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    } else {
        // 日本語のファイル名は RFC 5987 の形で渡す
        let encoded: String = form_urlencoded::byte_serialize(filename.as_bytes()).collect();
        if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename*=UTF-8''{}", encoded.replace('+', "%20"))) {
            h.insert(header::CONTENT_DISPOSITION, v);
        }
    }
    res
}

/// 302 のリダイレクト。
pub fn redirect(location: &str) -> Response {
    let mut res = StatusCode::FOUND.into_response();
    if let Ok(v) = HeaderValue::from_str(location) {
        res.headers_mut().insert(header::LOCATION, v);
    }
    res
}

/// serde の `deserialize_with` 用: null を既定値として読む（Go の encoding/json は null を無視する）。
pub fn nullable<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de> + Default,
{
    use serde::Deserialize;
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// まだデコードしていないリクエストボディ（1MB まで）。権限の確認をデコードより先に行うハンドラーで使う。
pub struct RawBody(pub axum::body::Bytes);

impl<S: Send + Sync> FromRequest<S> for RawBody {
    type Rejection = AppError;

    async fn from_request(req: Request, _: &S) -> Result<Self, Self::Rejection> {
        axum::body::to_bytes(req.into_body(), MAX_BODY_BYTES)
            .await
            .map(RawBody)
            .map_err(|_| too_large("リクエストが大きすぎます"))
    }
}

impl RawBody {
    pub fn decode<T: DeserializeOwned + Default>(&self) -> Result<T, AppError> {
        decode_json(&self.0)
    }
}
