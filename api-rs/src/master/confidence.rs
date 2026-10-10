//! 確度の段階（docs/plan.md「2.8 確度とリスク」）。名前・標準の確率・判定基準を持つ。
//! 施策（activities.confidence_level）はコードで参照するため、コードは作成後に変更できない。

use std::str::FromStr;
use std::sync::LazyLock;

use axum::extract::State;
use axum::response::Response;
use chrono::NaiveDateTime;
use mysql_async::prelude::*;
use regex::Regex;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::tree::ReasonRequest;
use super::{delete_error, not_found_label};
use crate::auth::{AdminUser, CurrentUser};
use crate::db;
use crate::error::validation1;
use crate::http::{self, Id, Json, RawNum, Validator};
use crate::{AppResult, AppState, audit, jst};

static CODE_PATTERN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Z0-9]{1,10}$").unwrap());

#[derive(Clone, Debug, Serialize)]
pub struct ConfidenceLevel {
    pub id: i64,
    pub code: String,
    pub name: String,
    /// 標準の確率（0〜1）
    pub rate: RawNum,
    pub criteria: String,
    pub sort_order: i32,
    #[serde(serialize_with = "jst::ser")]
    pub created_at: NaiveDateTime,
    #[serde(serialize_with = "jst::ser")]
    pub updated_at: NaiveDateTime,
}

type ConfidenceRow = (i64, String, String, String, String, i32, NaiveDateTime, NaiveDateTime);

impl From<ConfidenceRow> for ConfidenceLevel {
    fn from((id, code, name, rate, criteria, sort_order, created_at, updated_at): ConfidenceRow) -> Self {
        ConfidenceLevel { id, code, name, rate: trim_rate(&rate), criteria, sort_order, created_at, updated_at }
    }
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ConfidenceRequest {
    /// 作成時のみ
    code: String,
    name: String,
    rate: RawNum,
    criteria: String,
    sort_order: i32,
    reason: String,
}

const CONFIDENCE_SELECT: &str =
    "SELECT id, code, name, rate, COALESCE(criteria, ''), sort_order, created_at, updated_at FROM confidence_levels";

/// DECIMAL(5,4) の文字列から末尾の 0 を取り除く（"0.8000" → "0.8"）。
pub fn trim_rate(s: &str) -> RawNum {
    let mut s = s.to_string();
    if s.contains('.') {
        s = s.trim_end_matches('0').trim_end_matches('.').to_string();
    }
    if s.is_empty() {
        s = "0".into();
    }
    RawNum(s)
}

async fn find<Q: Queryable>(q: &mut Q, id: i64, lock: &str) -> AppResult<ConfidenceLevel> {
    let r: Option<ConfidenceRow> = q.exec_first(format!("{CONFIDENCE_SELECT} WHERE id = ?{lock}"), (id,)).await?;
    r.map(Into::into).ok_or_else(|| not_found_label("確度の段階"))
}

/// GET /api/confidence-levels。表示順・コードの順に返す。
pub async fn list(State(st): State<AppState>, _: CurrentUser) -> AppResult<Response> {
    let mut conn = st.db.get_conn().await?;
    let rows: Vec<ConfidenceRow> = conn.exec(format!("{CONFIDENCE_SELECT} ORDER BY sort_order, code"), ()).await?;
    let items: Vec<ConfidenceLevel> = rows.into_iter().map(Into::into).collect();
    Ok(http::list(&items))
}

/// 入力を検証し、(名前, 確率（小数点以下4桁の文字列）, 判定基準) を返す。
fn validate(req: &mut ConfidenceRequest, creating: bool) -> AppResult<(String, String, String)> {
    let mut v = Validator::new();
    if creating {
        req.code = req.code.trim().to_string();
        if !CODE_PATTERN.is_match(&req.code) {
            v.add("code", "コードは英大文字・数字の10文字以内で入力してください（例: A）");
        }
    }
    let name = v.text("name", "名前", &req.name, 50);
    let criteria = v.optional_text("criteria", "判定基準", &req.criteria, 1000);
    let mut rate = String::new();
    let x = Decimal::from_str(&req.rate.0).or_else(|_| Decimal::from_scientific(&req.rate.0));
    match x {
        Err(_) => v.add("rate", "標準の確率は 0〜1 の数値で入力してください（例: 0.5）"),
        Ok(x) if x.is_sign_negative() && !x.is_zero() || x > Decimal::ONE => {
            v.add("rate", "標準の確率は 0〜1 の範囲で入力してください")
        }
        Ok(x) if !(x * Decimal::from(10000)).fract().is_zero() => {
            v.add("rate", "標準の確率は小数点以下4桁までで入力してください")
        }
        Ok(x) => rate = format!("{:.4}", x.round_dp(4)),
    }
    v.check()?;
    Ok((name, rate, criteria))
}

/// POST /api/confidence-levels
pub async fn create(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    Json(mut req): Json<ConfidenceRequest>,
) -> AppResult<Response> {
    let (name, rate, criteria) = validate(&mut req, true)?;
    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    let r = tx
        .exec_drop(
            "INSERT INTO confidence_levels (code, name, rate, criteria, sort_order) VALUES (?, ?, ?, ?, ?)",
            p![req.code.clone(), name, rate, db::null_str(&criteria), req.sort_order],
        )
        .await;
    if db::is_dup(&r) {
        return Err(validation1("code", "このコードは既に使われています"));
    }
    r?;
    let id = db::last_id(&tx);
    let created = find(&mut tx, id, "").await?;
    rec.insert(&mut tx, "confidence_levels", id, &created).await?;
    tx.commit().await?;
    Ok(http::created(&created))
}

/// PUT /api/confidence-levels/{id}。コードは変更できない。
/// 標準の確率を変えると、加重見込・楽観・悲観の集計結果が変わる（金額そのものは満額のまま）。
pub async fn update(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    Id(id): Id,
    Json(mut req): Json<ConfidenceRequest>,
) -> AppResult<Response> {
    let (name, rate, criteria) = validate(&mut req, false)?;
    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    let before = find(&mut tx, id, " FOR UPDATE").await?;
    tx.exec_drop(
        "UPDATE confidence_levels SET name = ?, rate = ?, criteria = ?, sort_order = ? WHERE id = ?",
        p![name, rate, db::null_str(&criteria), req.sort_order, id],
    )
    .await?;
    let updated = find(&mut tx, id, "").await?;
    rec.update(&mut tx, "confidence_levels", id, &before, &updated).await?;
    tx.commit().await?;
    Ok(http::ok(&updated))
}

/// DELETE /api/confidence-levels/{id}。施策から参照されている段階は削除できない。
pub async fn delete(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    Id(id): Id,
    Json(req): Json<ReasonRequest>,
) -> AppResult<Response> {
    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    let before = find(&mut tx, id, " FOR UPDATE").await?;
    let r = tx.exec_drop("DELETE FROM confidence_levels WHERE id = ?", (id,)).await;
    delete_error(r, "確度の段階")?;
    rec.delete(&mut tx, "confidence_levels", id, &before).await?;
    tx.commit().await?;
    Ok(http::no_content())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_rate() {
        assert_eq!(trim_rate("0.8000").0, "0.8");
        assert_eq!(trim_rate("1.0000").0, "1");
        assert_eq!(trim_rate("0.0000").0, "0");
    }
}
