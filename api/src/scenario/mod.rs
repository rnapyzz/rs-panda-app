//! シナリオと、シナリオごとの数値（ドライバー値・金額）の API。
//!
//! シナリオの金額は、決算確定月（actual_through）以前の月は実績、それより後の月は計画値とする（docs/plan.md「2.6 シナリオ」）。
//! 実績はシナリオに属さない実績データ（actual_facts）で、ロック時に決算確定月以前の実績をシナリオに保存して固定する（scenario_actuals）。
//!
//! シナリオの作成・変更・作成中の指定・ロック/ロック解除、実績の取込は FP&A（fpa_admin）のみ。参照はログインユーザー全員。
//! 数値の入力は、作成中のシナリオなら施策の編集権限を持つユーザー、それ以外のロックされていないシナリオは FP&A のみが行える。
//!
//! シナリオは削除できない（変更履歴がシナリオを参照するため）。

mod comments;
mod cycle;
mod home;
mod milestones;
mod notes;
mod transfer;
mod values;

use std::collections::BTreeMap;

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use chrono::NaiveDateTime;
use mysql_async::prelude::*;
use serde::{Deserialize, Serialize};

use crate::audit::Recorder;
use crate::auth::{AdminUser, CurrentUser, User};
use crate::db::{self, Tx, Value};
use crate::error::{bad_request, conflict, not_found, validation1};
use crate::http::{self, Id, Json, Query, Validator};
use crate::{AppResult, AppState, audit, calc, jst};

pub use home::{AccuracyRow, accuracy_of, target_condition};
pub use notes::mark_edited;

/// ルート。
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/scenarios", get(list).post(create))
        .route("/api/scenarios/active", get(active))
        .route("/api/scenarios/start-monthly", post(cycle::start_monthly))
        .route("/api/scenarios/start-fiscal-year", post(cycle::start_fiscal_year))
        .route("/api/scenarios/{id}", get(get_one).put(update))
        .route("/api/scenarios/{id}/activity-status", get(home::activity_statuses))
        .route("/api/scenarios/{id}/milestones", get(milestones::milestones))
        .route("/api/scenarios/{id}/activate", post(activate))
        .route("/api/scenarios/{id}/lock", post(lock))
        .route("/api/scenarios/{id}/unlock", post(unlock))
        // 数値の入力は施策ごとに権限を判定する
        .route("/api/scenarios/{id}/amounts/export", get(transfer::export_amounts))
        .route("/api/scenarios/{id}/driver-values/export", get(transfer::export_driver_values))
        .route("/api/scenarios/{id}/plan-values/import", post(transfer::import_plan_values))
        .route("/api/scenarios/{id}/activities/{aid}", get(values::get_values))
        .route("/api/scenarios/{id}/activities/{aid}/driver-values", put(values::put_driver_values))
        .route("/api/scenarios/{id}/activities/{aid}/amounts", put(values::put_amounts))
        .route("/api/scenarios/{id}/activities/{aid}/condition", put(values::put_condition))
        .route("/api/scenarios/{id}/activities/{aid}/note", get(notes::get_note).put(notes::put_note))
        .route("/api/scenarios/{id}/activities/{aid}/complete", post(notes::complete).delete(notes::uncomplete))
        .route("/api/scenarios/{id}/activities/{aid}/comments", get(comments::list).post(comments::post))
        .route("/api/scenarios/{id}/activities/{aid}/comments/{cid}", delete(comments::delete))
}

/// シナリオのエイリアス（期初計画・修正計画・最新見込）。年度ごとに1つのシナリオにだけ付けられる。
const PLAN_ROLES: [&str; 3] = ["initial", "revised", "latest"];

/// シナリオ。
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Scenario {
    pub id: i64,
    pub name: String,
    pub fiscal_year: i32,
    /// initial / revised / latest
    pub plan_role: Option<String>,
    /// 決算確定月（YYYY-MM）。この月以前は実績
    pub actual_through: Option<String>,
    /// 作成中
    pub is_active: bool,
    /// 現場の更新の締切日（YYYY-MM-DD）
    pub update_deadline: Option<String>,
    pub base_scenario_id: Option<i64>,
    /// 前回見込（同じ年度のシナリオ）。比較やホームで「前回締めた見込」として使う
    pub previous_scenario_id: Option<i64>,
    pub is_locked: bool,
    pub created_by: i64,
    #[serde(serialize_with = "jst::ser")]
    pub created_at: NaiveDateTime,
    #[serde(serialize_with = "jst::ser")]
    pub updated_at: NaiveDateTime,
}
from_row!(Scenario {
    id,
    name,
    fiscal_year,
    plan_role,
    actual_through,
    is_active,
    update_deadline,
    base_scenario_id,
    previous_scenario_id,
    is_locked,
    created_by,
    created_at,
    updated_at
});

impl Scenario {
    pub fn through(&self) -> &str {
        self.actual_through.as_deref().unwrap_or("")
    }

    /// 計画値の月（決算確定月より後の月）。数値を入力できるのはこの月だけ。
    pub fn plan_months(&self) -> Vec<String> {
        calc::plan_months(self.fiscal_year, self.through())
    }

    /// 実績の月（決算確定月以前の月）。
    pub fn actual_months(&self) -> Vec<String> {
        let plan = self.plan_months();
        calc::fiscal_months(self.fiscal_year).into_iter().filter(|m| !plan.contains(m)).collect()
    }
}

pub const SCENARIO_SELECT: &str = "SELECT id, name, fiscal_year, plan_role, DATE_FORMAT(actual_through, '%Y-%m'), is_active, DATE_FORMAT(update_deadline, '%Y-%m-%d'),
    base_scenario_id, previous_scenario_id, is_locked, created_by, created_at, updated_at FROM scenarios";

pub async fn find_scenario<Q: Queryable>(q: &mut Q, id: i64, lock: &str) -> AppResult<Scenario> {
    let s: Option<Scenario> = q.exec_first(format!("{SCENARIO_SELECT} WHERE id = ?{lock}"), (id,)).await?;
    s.ok_or_else(|| not_found("シナリオが見つかりません"))
}

/// 条件に合う最初のシナリオ（なければ None）。
async fn scenario_where<Q: Queryable>(q: &mut Q, cond: &str, params: db::Params) -> AppResult<Option<Scenario>> {
    Ok(q.exec_first(format!("{SCENARIO_SELECT} {cond}"), params).await?)
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct CreateRequest {
    name: String,
    fiscal_year: i32,
    base_scenario_id: Option<i64>,
    /// 前回見込。省略（null）なら複製元を使う
    previous_scenario_id: Option<i64>,
    /// 空ならエイリアスなし
    plan_role: String,
    /// YYYY-MM。空なら未設定（12か月すべて計画値）
    actual_through: String,
    reason: String,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ReasonRequest {
    pub reason: String,
}

/// エイリアスと決算確定月を検証し、DB に保存する値を返す。
fn validate_role_and_through(v: &mut Validator, fiscal_year: i32, role: &str, through: &str) -> (Value, Value) {
    let (role, through) = (role.trim(), through.trim());
    if !role.is_empty() && !PLAN_ROLES.contains(&role) {
        v.add(
            "plan_role",
            "エイリアスは initial（期初計画）/ revised（修正計画）/ latest（最新見込）のいずれかを指定してください",
        );
    }
    if !through.is_empty() {
        let months = calc::fiscal_months(fiscal_year);
        if !months.iter().any(|m| m == through) {
            v.add(
                "actual_through",
                format!("決算確定月は {}〜{} の YYYY-MM で指定してください", months[0], months[11]),
            );
        }
    }
    (db::null_str(role), db::null_str(&month_date(through)))
}

pub fn month_date(ym: &str) -> String {
    if ym.is_empty() { String::new() } else { format!("{ym}-01") }
}

/// 同じ年度で role のエイリアスを持つほかのシナリオからエイリアスを外す（付け替え）。
async fn take_role(tx: &mut Tx, rec: &Recorder, fiscal_year: i32, role: &str, except_id: i64) -> AppResult<()> {
    if role.is_empty() {
        return Ok(());
    }
    let id: Option<i64> = tx
        .exec_first(
            "SELECT id FROM scenarios WHERE fiscal_year = ? AND plan_role = ? AND id <> ? FOR UPDATE",
            p![fiscal_year, role, except_id],
        )
        .await?;
    let Some(id) = id else { return Ok(()) };
    let before = find_scenario(tx, id, "").await?;
    tx.exec_drop("UPDATE scenarios SET plan_role = NULL WHERE id = ?", (id,)).await?;
    let after = find_scenario(tx, id, "").await?;
    rec.update(tx, "scenarios", id, &before, &after).await
}

/// GET /api/scenarios。fiscal_year で絞り込める。新しい年度・新しい順に返す。
async fn list(State(st): State<AppState>, _: CurrentUser, q: Query) -> AppResult<Response> {
    let mut sql = SCENARIO_SELECT.to_string();
    let mut args: Vec<Value> = Vec::new();
    let fy = q.get("fiscal_year");
    if !fy.is_empty() {
        let y: i64 = fy.parse().map_err(|_| bad_request("fiscal_year は数値で指定してください"))?;
        sql.push_str(" WHERE fiscal_year = ?");
        args.push(y.into());
    }
    sql.push_str(" ORDER BY fiscal_year DESC, id DESC");
    let mut conn = st.db.get_conn().await?;
    let items: Vec<Scenario> = conn.exec(sql, db::Params::Positional(args)).await?;
    Ok(http::list(&items))
}

/// GET /api/scenarios/{id}
async fn get_one(State(st): State<AppState>, _: CurrentUser, Id(id): Id) -> AppResult<Response> {
    let mut conn = st.db.get_conn().await?;
    Ok(http::ok(&find_scenario(&mut conn, id, "").await?))
}

/// GET /api/scenarios/active。作成中のシナリオを返す（未設定なら null）。
async fn active(State(st): State<AppState>, _: CurrentUser) -> AppResult<Response> {
    let mut conn = st.db.get_conn().await?;
    match scenario_where(&mut conn, "WHERE is_active", p![]).await? {
        Some(s) => Ok(http::ok(&s)),
        None => Ok(([(header::CONTENT_TYPE, "application/json; charset=utf-8")], "null\n").into_response()),
    }
}

/// 監査ログに記録する、複製した件数付きのシナリオ。
#[derive(Serialize)]
struct WithCopied<'a> {
    #[serde(flatten)]
    scenario: &'a Scenario,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    copied: BTreeMap<&'static str, u64>,
}

/// 監査ログに記録する、保存した実績の件数付きのシナリオ。
#[derive(Serialize)]
struct WithFrozen<'a> {
    #[serde(flatten)]
    scenario: &'a Scenario,
    frozen_actuals: u64,
}

/// POST /api/scenarios。base_scenario_id を指定すると、そのシナリオの数値（ドライバー値・金額）をすべて複製する。
/// 複製元は同じ年度のシナリオに限る。plan_role を指定すると、同じ年度でそのエイリアスを持つシナリオからは外れる。
async fn create(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    Json(req): Json<CreateRequest>,
) -> AppResult<Response> {
    let mut v = Validator::new();
    let name = v.text("name", "シナリオ名", &req.name, 100);
    if !(2000..=2100).contains(&req.fiscal_year) {
        v.add("fiscal_year", "年度を正しく入力してください（例: 2026）");
    }
    let (role, through) = validate_role_and_through(&mut v, req.fiscal_year, &req.plan_role, &req.actual_through);
    v.check()?;

    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    if let Some(base_id) = req.base_scenario_id {
        let base = match find_scenario(&mut tx, base_id, " FOR SHARE").await {
            Err(e) if e.is_not_found() => {
                return Err(validation1("base_scenario_id", "複製元のシナリオが見つかりません"));
            }
            r => r?,
        };
        if base.fiscal_year != req.fiscal_year {
            return Err(validation1("base_scenario_id", "複製元は同じ年度のシナリオを選んでください"));
        }
    }
    take_role(&mut tx, &rec, req.fiscal_year, req.plan_role.trim(), 0).await?;
    let previous = req.previous_scenario_id.or(req.base_scenario_id);
    check_previous(&mut tx, 0, req.fiscal_year, previous).await?;
    let created = insert_scenario(
        &mut tx,
        &rec,
        ScenarioInsert {
            name,
            fiscal_year: req.fiscal_year,
            role,
            through,
            base: req.base_scenario_id,
            previous,
            deadline: Value::NULL,
            created_by: u.id,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(http::created(&created))
}

/// 新しいシナリオの内容。base があれば、その数値（ドライバー値・金額）を複製する。
pub struct ScenarioInsert {
    pub name: String,
    pub fiscal_year: i32,
    /// エイリアス（NULL 可）
    pub role: Value,
    /// 決算確定月（月初日、NULL 可）
    pub through: Value,
    pub base: Option<i64>,
    pub previous: Option<i64>,
    /// 締切日（NULL 可）
    pub deadline: Value,
    pub created_by: i64,
}

/// シナリオを作り（複製元があれば数値を複製し）、監査ログに残す。
pub async fn insert_scenario(tx: &mut Tx, rec: &Recorder, i: ScenarioInsert) -> AppResult<Scenario> {
    let r = tx
        .exec_drop(
            "INSERT INTO scenarios (name, fiscal_year, plan_role, actual_through, update_deadline, base_scenario_id, previous_scenario_id, created_by)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            p![i.name, i.fiscal_year, i.role, i.through, i.deadline, i.base, i.previous, i.created_by],
        )
        .await;
    if db::is_dup(&r) {
        return Err(validation1("name", "このシナリオ名は既に使われています"));
    }
    r?;
    let id = db::last_id(tx);
    let mut copied = BTreeMap::new();
    if let Some(base) = i.base {
        for (table, columns) in [
            ("driver_values", "activity_driver_id, target_month, value, is_provisional, provisional_reason"),
            (
                "budget_facts",
                "activity_id, subject_id, line_id, target_month, amount, source, is_provisional, provisional_reason",
            ),
        ] {
            tx.exec_drop(
                format!("INSERT INTO {table} (scenario_id, {columns}) SELECT ?, {columns} FROM {table} WHERE scenario_id = ?"),
                (id, base),
            )
            .await?;
            copied.insert(table, tx.affected_rows());
        }
    }
    let created = find_scenario(tx, id, "").await?;
    // 複製した行は件数だけを記録する（行ごとの監査ログは複製元に残っている）。
    rec.insert(tx, "scenarios", id, &WithCopied { scenario: &created, copied }).await?;
    Ok(created)
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct UpdateRequest {
    name: String,
    plan_role: String,
    actual_through: String,
    /// null なら前回見込なし
    previous_scenario_id: Option<i64>,
    /// YYYY-MM-DD。空なら締切なし
    update_deadline: String,
    reason: String,
}

/// PUT /api/scenarios/{id}。名称・エイリアス・決算確定月・前回見込・締切を変更する（年度は変更できない）。
/// 決算確定月の変更は表示する金額が変わるため変更理由が必須で、ロック済みのシナリオでは変更できない。
async fn update(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    Id(id): Id,
    Json(req): Json<UpdateRequest>,
) -> AppResult<Response> {
    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, Some(id), &req.reason).await?;
    let before = find_scenario(&mut tx, id, " FOR UPDATE").await?;
    let mut v = Validator::new();
    let name = v.text("name", "シナリオ名", &req.name, 100);
    let (role, through) = validate_role_and_through(&mut v, before.fiscal_year, &req.plan_role, &req.actual_through);
    let deadline = validate_deadline(&mut v, &req.update_deadline);
    v.check()?;
    if req.actual_through.trim() != before.through() {
        if before.is_locked {
            return Err(conflict("ロックされたシナリオの決算確定月は変更できません"));
        }
        if req.reason.trim().is_empty() {
            return Err(validation1("reason", "決算確定月の変更には変更理由の入力が必要です"));
        }
    }
    check_previous(&mut tx, id, before.fiscal_year, req.previous_scenario_id).await?;
    take_role(&mut tx, &rec, before.fiscal_year, req.plan_role.trim(), id).await?;
    let r = tx
        .exec_drop(
            "UPDATE scenarios SET name = ?, plan_role = ?, actual_through = ?, previous_scenario_id = ?, update_deadline = ? WHERE id = ?",
            p![name, role, through, req.previous_scenario_id, deadline, id],
        )
        .await;
    if db::is_dup(&r) {
        return Err(validation1("name", "このシナリオ名は既に使われています"));
    }
    r?;
    let updated = find_scenario(&mut tx, id, "").await?;
    if !same_scenario(&before, &updated) {
        rec.update(&mut tx, "scenarios", id, &before, &updated).await?;
        // 計画値の月が変わったら、計算式の金額を再計算する
        if before.through() != updated.through() {
            calc::recalculate_scenario(&mut tx, &rec, id).await?;
        }
    }
    tx.commit().await?;
    notify_after_update(&st, &before, &updated).await;
    Ok(http::ok(&updated))
}

/// 締切日（YYYY-MM-DD、空なら締切なし）を検証し、DB に保存する値を返す。
pub fn validate_deadline(v: &mut Validator, s: &str) -> Value {
    let s = s.trim();
    if s.is_empty() {
        return Value::NULL;
    }
    if jst::parse_date(s).is_none() {
        v.add("update_deadline", "締切日は YYYY-MM-DD の形式で入力してください");
    }
    Value::from(s)
}

/// 締切が入ったら「更新の開始」、決算確定月が進んだら「実績の反映」を送る（docs/plan.md「2.13」）。
async fn notify_after_update(st: &AppState, before: &Scenario, after: &Scenario) {
    let notifier = &st.notify;
    if !after.is_active || after.is_locked {
        return;
    }
    if after.update_deadline.is_some() && before.update_deadline.as_deref().unwrap_or("").is_empty() {
        notifier.update_started(after.id).await;
    }
    let Some(previous) = after.previous_scenario_id else { return };
    if after.through() <= before.through() {
        return;
    }
    let old = before.actual_months();
    let months: Vec<String> = after.actual_months().into_iter().filter(|m| !old.contains(m)).collect();
    if months.is_empty() {
        return;
    }
    // 通知の宛先は現場（閲覧制限のある科目を見られない）なので、その科目を除いて判定する（ホームの表示と同じ）
    let member = User { role: crate::auth::ROLE_MEMBER.into(), ..Default::default() };
    let acc = async {
        let mut conn = st.db.get_conn().await?;
        accuracy_of(&mut conn, &member, after.id, previous, &months).await
    };
    match acc.await {
        Ok(acc) => {
            let mut ids: Vec<i64> = acc.into_iter().filter(|(_, a)| a.large).map(|(id, _)| id).collect();
            ids.sort();
            notifier.actuals_reflected(after.id, months, ids).await;
        }
        Err(e) => tracing::error!(scenario_id = after.id, error = ?e, "notify: accuracy"),
    }
}

/// 変更できる項目が同じかを返す。
fn same_scenario(a: &Scenario, b: &Scenario) -> bool {
    a.name == b.name
        && a.plan_role == b.plan_role
        && a.actual_through == b.actual_through
        && a.previous_scenario_id == b.previous_scenario_id
        && a.update_deadline == b.update_deadline
}

/// 前回見込が、自分以外の同じ年度のシナリオであることを確認する。self_id は更新対象（作成時は 0）。
async fn check_previous(tx: &mut Tx, self_id: i64, fiscal_year: i32, previous: Option<i64>) -> AppResult<()> {
    let Some(previous) = previous else { return Ok(()) };
    if previous == self_id {
        return Err(validation1("previous_scenario_id", "前回見込に自分自身は選べません"));
    }
    let p = match find_scenario(tx, previous, "").await {
        Err(e) if e.is_not_found() => {
            return Err(validation1("previous_scenario_id", "前回見込のシナリオが見つかりません"));
        }
        r => r?,
    };
    if p.fiscal_year != fiscal_year {
        return Err(validation1("previous_scenario_id", "前回見込は同じ年度のシナリオを選んでください"));
    }
    Ok(())
}

/// POST /api/scenarios/{id}/activate。作成中のシナリオに指定する（前の作成中は外れる）。
async fn activate(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    Id(id): Id,
    Json(req): Json<ReasonRequest>,
) -> AppResult<Response> {
    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, Some(id), &req.reason).await?;
    let before = find_scenario(&mut tx, id, " FOR UPDATE").await?;
    if before.is_locked {
        return Err(conflict("ロックされたシナリオは作成中にできません"));
    }
    let updated = if before.is_active {
        before
    } else {
        deactivate_current(&mut tx, &rec).await?;
        tx.exec_drop("UPDATE scenarios SET is_active = TRUE WHERE id = ?", (id,)).await?;
        let updated = find_scenario(&mut tx, id, "").await?;
        rec.update(&mut tx, "scenarios", id, &before, &updated).await?;
        updated
    };
    tx.commit().await?;
    // 締切が入っていれば「更新の開始」を送る（シナリオごとに1回）
    if updated.update_deadline.is_some() {
        st.notify.update_started(id).await;
    }
    Ok(http::ok(&updated))
}

/// 作成中のシナリオの指定を外す。
pub async fn deactivate_current(tx: &mut Tx, rec: &Recorder) -> AppResult<()> {
    let Some(cur) = scenario_where(tx, "WHERE is_active FOR UPDATE", p![]).await? else { return Ok(()) };
    tx.exec_drop("UPDATE scenarios SET is_active = FALSE WHERE id = ?", (cur.id,)).await?;
    let mut after = cur.clone();
    after.is_active = false;
    rec.update(tx, "scenarios", cur.id, &cur, &after).await
}

/// シナリオをロックする。決算確定月以前の実績を scenario_actuals に保存して固定し、作成中の指定は外す。
/// before は行ロック付きで読んだロックされていないシナリオ。
pub async fn lock_scenario(tx: &mut Tx, rec: &Recorder, before: &Scenario) -> AppResult<Scenario> {
    let mut actuals = 0;
    if let Some(through) = &before.actual_through {
        tx.exec_drop(
            "INSERT INTO scenario_actuals (scenario_id, activity_id, subject_id, target_month, amount)
             SELECT ?, activity_id, subject_id, target_month, amount FROM actual_facts
             WHERE target_month BETWEEN ? AND ?",
            p![before.id, format!("{}-01", calc::fiscal_months(before.fiscal_year)[0]), month_date(through)],
        )
        .await?;
        actuals = tx.affected_rows();
    }
    tx.exec_drop("UPDATE scenarios SET is_locked = TRUE, is_active = FALSE WHERE id = ?", (before.id,)).await?;
    let updated = find_scenario(tx, before.id, "").await?;
    // 保存した実績は件数だけを記録する（行ごとの監査ログは actual_facts に残っている）
    rec.update(tx, "scenarios", before.id, before, &WithFrozen { scenario: &updated, frozen_actuals: actuals }).await?;
    Ok(updated)
}

/// POST /api/scenarios/{id}/lock。ロックしたシナリオの数値は変更できなくなる。
/// 決算確定月以前の実績を scenario_actuals に保存して固定し、作成中の指定は外す。
async fn lock(st: State<AppState>, u: AdminUser, id: Id, req: Json<ReasonRequest>) -> AppResult<Response> {
    set_locked(st, u, id, req, true).await
}

/// POST /api/scenarios/{id}/unlock。確定した版を変更可能に戻すため、変更理由が必須。
/// 保存した実績を外し、再び実績データを参照する。
async fn unlock(st: State<AppState>, u: AdminUser, id: Id, req: Json<ReasonRequest>) -> AppResult<Response> {
    set_locked(st, u, id, req, false).await
}

async fn set_locked(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    Id(id): Id,
    Json(req): Json<ReasonRequest>,
    locked: bool,
) -> AppResult<Response> {
    if !locked && req.reason.trim().is_empty() {
        return Err(validation1("reason", "ロック解除には変更理由の入力が必要です"));
    }
    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, Some(id), &req.reason).await?;
    let before = find_scenario(&mut tx, id, " FOR UPDATE").await?;
    if before.is_locked == locked {
        return Err(conflict(if locked {
            "このシナリオは既にロックされています"
        } else {
            "このシナリオはロックされていません"
        }));
    }
    let updated = if locked {
        lock_scenario(&mut tx, &rec, &before).await?
    } else {
        // 解除時は保存した実績を外す（再び実績データを参照する）
        tx.exec_drop("DELETE FROM scenario_actuals WHERE scenario_id = ?", (id,)).await?;
        let actuals = tx.affected_rows();
        tx.exec_drop("UPDATE scenarios SET is_locked = FALSE WHERE id = ?", (id,)).await?;
        let updated = find_scenario(&mut tx, id, "").await?;
        rec.update(&mut tx, "scenarios", id, &before, &WithFrozen { scenario: &updated, frozen_actuals: actuals })
            .await?;
        updated
    };
    tx.commit().await?;
    Ok(http::ok(&updated))
}

/// 作成中のシナリオなら施策の編集権限があれば入力でき、それ以外のシナリオは FP&A のみが入力できる
/// （ロックと施策の権限は別に判定する）。
pub fn can_edit_scenario(u: &User, s: &Scenario) -> bool {
    s.is_active || u.is_admin()
}

/// 410 Gone。
pub fn gone(msg: &str) -> crate::AppError {
    crate::AppError::new(StatusCode::GONE, "gone", msg)
}
