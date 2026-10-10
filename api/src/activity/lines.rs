//! 金額の内訳。施策 × 科目の下に、名前付きの内訳を複数持てる（例: 売上高 = 月額利用料 + 初期導入費）。
//! 内訳ごとに、計算式で反映する（formula_enabled）か、直接入力するかを決める。反映しない場合も式は残せる。
//! 科目の金額は、内訳の金額と、科目への直接入力（内訳なし）の金額の合計。

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use chrono::NaiveDateTime;
use mysql_async::prelude::*;
use serde::{Deserialize, Serialize};

use super::{ReasonRequest, driver, lock_editable, require_reason};
use crate::auth::{CurrentUser, User};
use crate::calc::{self, Fact};
use crate::db::{self, Tx};
use crate::error::{AppError, conflict, not_found, validation1};
use crate::formula::Expr;
use crate::http::{self, Json, PathIds, Validator};
use crate::{AppResult, AppState, audit, jst, visibility};

const MAX_EXPRESSION_LEN: usize = 1000;

/// 見通しの種類。
const OUTLOOKS: [&str; 3] = ["base", "addon", "downside"];

/// 金額の内訳。
#[derive(Clone, Debug, Serialize)]
pub struct Line {
    pub id: i64,
    pub activity_id: i64,
    pub subject_id: i64,
    pub name: String,
    pub expression: String,
    pub formula_enabled: bool,
    /// 内訳の確度の段階。None なら施策の段階を使う
    pub confidence_level: Option<String>,
    /// 見通しの種類（base: ベース / addon: アドオン / downside: ダウンサイド）
    pub outlook: String,
    pub sort_order: i32,
    #[serde(serialize_with = "jst::ser")]
    pub created_at: NaiveDateTime,
    #[serde(serialize_with = "jst::ser")]
    pub updated_at: NaiveDateTime,
}
from_row!(Line {
    id,
    activity_id,
    subject_id,
    name,
    expression,
    formula_enabled,
    confidence_level,
    outlook,
    sort_order,
    created_at,
    updated_at
});

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct LineRequest {
    /// 作成時のみ
    subject_id: i64,
    name: String,
    expression: String,
    formula_enabled: bool,
    /// 確度の段階のコード。空なら施策の段階を使う
    confidence_level: String,
    /// 見通しの種類。空ならベース
    outlook: String,
    sort_order: i32,
    reason: String,
}

const LINE_SELECT: &str = "SELECT id, activity_id, subject_id, name, COALESCE(expression, ''), formula_enabled, confidence_level, outlook, sort_order, created_at, updated_at FROM activity_lines";

/// 施策の内訳を返す。subject_filter は見られない科目を除く条件（visibility::subject_filter、空文字はすべて）。
pub async fn list<Q: Queryable>(q: &mut Q, activity_id: i64, subject_filter: &str) -> AppResult<Vec<Line>> {
    Ok(q.exec(
        format!("{LINE_SELECT} WHERE activity_id = ?{subject_filter} ORDER BY subject_id, sort_order, id"),
        (activity_id,),
    )
    .await?)
}

/// 閲覧制限のある科目の内訳を作成・変更・削除できるか（FP&A のみ）を確かめる（docs/plan.md「2.17」）。
async fn check_restricted_line(tx: &mut Tx, u: &User, subject_id: i64) -> AppResult<()> {
    if u.is_admin() {
        return Ok(());
    }
    if visibility::is_restricted(tx, subject_id).await? {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "閲覧制限のある科目の内訳は FP&A のみが作成・変更できます",
        ));
    }
    Ok(())
}

async fn find(tx: &mut Tx, activity_id: i64, id: i64) -> AppResult<Line> {
    let l: Option<Line> =
        tx.exec_first(format!("{LINE_SELECT} WHERE id = ? AND activity_id = ? FOR UPDATE"), (id, activity_id)).await?;
    l.ok_or_else(|| not_found("内訳が見つかりません"))
}

async fn find_by_id(tx: &mut Tx, id: i64) -> AppResult<Line> {
    let l: Option<Line> = tx.exec_first(format!("{LINE_SELECT} WHERE id = ?"), (id,)).await?;
    l.ok_or_else(|| not_found("内訳が見つかりません"))
}

/// 内訳の入力を検証し、計算式を解析する。式に使える識別子は、施策のドライバーの code。
async fn validate(tx: &mut Tx, activity_id: i64, req: &mut LineRequest) -> AppResult<()> {
    let mut v = Validator::new();
    req.name = v.text("name", "内訳名", &req.name, 100);
    req.confidence_level = req.confidence_level.trim().to_string();
    if !req.confidence_level.is_empty()
        && db::count(tx, "SELECT COUNT(*) FROM confidence_levels WHERE code = ?", p![req.confidence_level.clone()])
            .await?
            == 0
    {
        v.add("confidence_level", format!("確度の段階「{}」は登録されていません", req.confidence_level));
    }
    if req.outlook.is_empty() {
        req.outlook = "base".into();
    }
    if !OUTLOOKS.contains(&req.outlook.as_str()) {
        v.add(
            "outlook",
            "見通しの種類は base（ベース）/ addon（アドオン）/ downside（ダウンサイド）のいずれかを指定してください",
        );
    }
    req.expression = v.optional_text("expression", "計算式", &req.expression, MAX_EXPRESSION_LEN);
    if req.formula_enabled && req.expression.is_empty() {
        v.add("expression", "計算式で反映するには、計算式を入力してください");
    }
    if !req.expression.is_empty() {
        match Expr::parse(&req.expression) {
            Err(e) => v.add("expression", e),
            Ok(parsed) => {
                let known: Vec<String> = driver::list(tx, activity_id).await?.into_iter().map(|d| d.code).collect();
                let unknown: Vec<String> = parsed.idents().into_iter().filter(|id| !known.contains(id)).collect();
                if !unknown.is_empty() {
                    v.add("expression", format!("未定義のドライバーが使われています: {}", unknown.join(", ")));
                }
            }
        }
    }
    v.check()
}

/// POST /api/activities/{id}/lines。計算式で反映する内訳を作る場合は変更理由が必須。
pub async fn create(
    State(st): State<AppState>,
    CurrentUser(u): CurrentUser,
    ids: PathIds,
    Json(mut req): Json<LineRequest>,
) -> AppResult<Response> {
    let activity_id = ids.get("id")?;
    if req.formula_enabled {
        require_reason(&req.reason)?;
    }

    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    lock_editable(&mut tx, &u, activity_id).await?;
    if !db::exists(&mut tx, "subjects", req.subject_id).await? {
        return Err(validation1("subject_id", "科目を選択してください"));
    }
    check_restricted_line(&mut tx, &u, req.subject_id).await?;
    validate(&mut tx, activity_id, &mut req).await?;
    let r = tx
        .exec_drop(
            "INSERT INTO activity_lines (activity_id, subject_id, name, expression, formula_enabled, confidence_level, outlook, sort_order) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            p![
                activity_id,
                req.subject_id,
                req.name.clone(),
                db::null_str(&req.expression),
                req.formula_enabled,
                db::null_str(&req.confidence_level),
                req.outlook.clone(),
                req.sort_order
            ],
        )
        .await;
    if db::is_dup(&r) {
        return Err(validation1("name", "この科目に同じ名前の内訳があります"));
    }
    r?;
    let id = db::last_id(&tx);
    let created = find_by_id(&mut tx, id).await?;
    rec.insert(&mut tx, "activity_lines", id, &created).await?;
    if created.formula_enabled {
        calc::recalculate_activity(&mut tx, &rec, activity_id).await?;
    }
    tx.commit().await?;
    Ok(http::created(&created))
}

/// PUT /api/activities/{id}/lines/{lid}。科目は変えられない。
/// 計算式・反映の有無・確度の段階・見通しの種類を変える場合は変更理由が必須（金額や加重見込に影響するため）。
/// 反映をやめた内訳の金額はそのまま残り、以後は直接入力で編集できる。
pub async fn update(
    State(st): State<AppState>,
    CurrentUser(u): CurrentUser,
    ids: PathIds,
    Json(mut req): Json<LineRequest>,
) -> AppResult<Response> {
    let activity_id = ids.get("id")?;
    let id = ids.get("lid")?;

    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    lock_editable(&mut tx, &u, activity_id).await?;
    let before = find(&mut tx, activity_id, id).await?;
    check_restricted_line(&mut tx, &u, before.subject_id).await?;
    validate(&mut tx, activity_id, &mut req).await?;
    let formula_changed = before.expression != req.expression || before.formula_enabled != req.formula_enabled;
    // 確度の段階・見通しの種類も、加重見込や楽観・悲観に影響するため理由が必須（docs/plan.md「2.7」）
    let level_changed =
        before.confidence_level.as_deref().unwrap_or("") != req.confidence_level || before.outlook != req.outlook;
    if formula_changed || level_changed {
        require_reason(&req.reason)?;
    }
    let r = tx
        .exec_drop(
            "UPDATE activity_lines SET name = ?, expression = ?, formula_enabled = ?, confidence_level = ?, outlook = ?, sort_order = ? WHERE id = ?",
            p![
                req.name.clone(),
                db::null_str(&req.expression),
                req.formula_enabled,
                db::null_str(&req.confidence_level),
                req.outlook.clone(),
                req.sort_order,
                id
            ],
        )
        .await;
    if db::is_dup(&r) {
        return Err(validation1("name", "この科目に同じ名前の内訳があります"));
    }
    r?;
    let updated = find_by_id(&mut tx, id).await?;
    rec.update(&mut tx, "activity_lines", id, &before, &updated).await?;
    if formula_changed && updated.formula_enabled {
        calc::recalculate_activity(&mut tx, &rec, activity_id).await?;
    }
    tx.commit().await?;
    Ok(http::ok(&updated))
}

/// 内訳の金額の行（id, scenario_id, subject_id, 月, 金額, 算出方式, 仮の値か, 仮の値の理由）。
type LineFactRow = (i64, i64, i64, String, String, String, bool, String);

/// DELETE /api/activities/{id}/lines/{lid}。変更理由が必須。
/// ロック済み・実績のシナリオに金額がある内訳は削除できない（確定した値を消さないため）。
/// それ以外のシナリオの、この内訳の金額は合わせて削除する。
pub async fn delete(
    State(st): State<AppState>,
    CurrentUser(u): CurrentUser,
    ids: PathIds,
    Json(req): Json<ReasonRequest>,
) -> AppResult<Response> {
    let activity_id = ids.get("id")?;
    let id = ids.get("lid")?;
    require_reason(&req.reason)?;

    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    lock_editable(&mut tx, &u, activity_id).await?;
    let before = find(&mut tx, activity_id, id).await?;
    check_restricted_line(&mut tx, &u, before.subject_id).await?;
    let n = db::count(
        &mut tx,
        "SELECT COUNT(*) FROM budget_facts b JOIN scenarios s ON s.id = b.scenario_id WHERE b.line_id = ? AND s.is_locked",
        p![id],
    )
    .await?;
    if n > 0 {
        return Err(conflict(
            "ロックされたシナリオに金額がある内訳は削除できません。計算式で反映しない設定にしてください",
        ));
    }
    let rows: Vec<LineFactRow> = tx
        .exec(
            "SELECT id, scenario_id, subject_id, DATE_FORMAT(target_month, '%Y-%m'), amount, source, is_provisional, COALESCE(provisional_reason, '')
             FROM budget_facts WHERE line_id = ? FOR UPDATE",
            (id,),
        )
        .await?;
    for (fid, scenario_id, subject_id, target_month, amount, source, is_provisional, provisional_reason) in rows {
        let f = Fact {
            id: fid,
            scenario_id,
            activity_id,
            subject_id,
            line_id: Some(id),
            target_month,
            amount,
            source,
            is_provisional,
            provisional_reason,
        };
        tx.exec_drop("DELETE FROM budget_facts WHERE id = ?", (fid,)).await?;
        rec.delete(&mut tx, "budget_facts", fid, &f).await?;
    }
    tx.exec_drop("DELETE FROM activity_lines WHERE id = ?", (id,)).await?;
    rec.delete(&mut tx, "activity_lines", id, &before).await?;
    tx.commit().await?;
    Ok(http::no_content())
}
