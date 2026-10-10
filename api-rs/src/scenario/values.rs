//! シナリオ × 施策の数値（ドライバー値・金額）の参照と入力。

use std::collections::{HashMap, HashSet};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use mysql_async::prelude::*;
use num_bigint::BigInt;
use num_traits::Signed;
use serde::{Deserialize, Serialize};

use super::notes::{Note, load_note, mark_edited};
use super::{Scenario, can_edit_scenario, find_scenario, gone};
use crate::activity::{self, Summary};
use crate::audit::Recorder;
use crate::auth::{CurrentUser, User};
use crate::calc::{self, Fact, Key};
use crate::db::{self, Tx};
use crate::error::{AppError, conflict, validation1};
use crate::formula::{self, Rat};
use crate::http::{self, Json, PathIds, Query, RawNum, Validator};
use crate::{AppResult, AppState, audit, visibility};

const MAX_ITEMS_PER_REQUEST: usize = 1000;
const MAX_REASON_LEN: usize = 2000;

// --- 参照 ---

#[derive(Clone, Debug, Serialize)]
struct ValueCell {
    target_month: String,
    value: RawNum,
    is_provisional: bool,
    provisional_reason: String,
}

#[derive(Clone, Debug, Serialize)]
struct DriverRow {
    id: i64,
    code: String,
    name: String,
    driver_kind: String,
    unit: String,
    values: Vec<ValueCell>,
}

#[derive(Clone, Debug, Serialize)]
struct AmountCell {
    target_month: String,
    amount: RawNum,
    source: String,
    is_provisional: bool,
    provisional_reason: String,
}

/// 科目の金額。values は科目への直接入力（内訳なし）の金額、lines は内訳ごとの金額。科目の金額はそれらの合計。
#[derive(Clone, Debug, Serialize)]
struct AmountRow {
    subject_id: i64,
    code: String,
    name: String,
    category: String,
    values: Vec<AmountCell>,
    lines: Vec<LineRow>,
}

/// 内訳の金額。formula_enabled なら計算式で算出され、直接入力できない。
#[derive(Clone, Debug, Serialize)]
struct LineRow {
    id: i64,
    name: String,
    expression: String,
    formula_enabled: bool,
    /// 内訳の確度の段階（None は施策の段階）
    confidence_level: Option<String>,
    /// 見通しの種類
    outlook: String,
    values: Vec<AmountCell>,
}

/// シナリオ×施策の入力画面用のデータ。
#[derive(Serialize)]
pub struct ValuesView {
    scenario: Scenario,
    activity: Summary,
    months: Vec<String>,
    /// 実績の月（決算確定月以前）。金額は実績で、入力できない
    actual_months: Vec<String>,
    editable: bool,
    drivers: Vec<DriverRow>,
    amounts: Vec<AmountRow>,
    /// 差異の説明と更新の状態（docs/plan.md「2.10」）
    note: Note,
    /// 閲覧制限のある科目を除いた金額か（docs/plan.md「2.17」）
    restricted_hidden: bool,
}

/// パスパラメーター {id}・{aid}。
pub fn path_ids(ids: &PathIds) -> AppResult<(i64, i64)> {
    Ok((ids.get("id")?, ids.get("aid")?))
}

/// GET /api/scenarios/{id}/activities/{aid}。ドライバー値・金額（計算式がある科目を含む）を月別に返す。
pub async fn get_values(State(st): State<AppState>, CurrentUser(u): CurrentUser, ids: PathIds) -> AppResult<Response> {
    let (scenario_id, activity_id) = path_ids(&ids)?;
    respond_values(&st, &u, scenario_id, activity_id).await
}

pub async fn respond_values(st: &AppState, u: &User, scenario_id: i64, activity_id: i64) -> AppResult<Response> {
    let mut conn = st.db.get_conn().await?;
    Ok(http::ok(&load_values(&mut conn, u, scenario_id, activity_id).await?))
}

type DriverValueRow = (i64, String, String, String, String, Option<String>, Option<String>, Option<bool>, String);

/// 数値入力画面のデータを読み込む。q はトランザクション（試算）でもよい。
pub async fn load_values<Q: Queryable>(
    q: &mut Q,
    u: &User,
    scenario_id: i64,
    activity_id: i64,
) -> AppResult<ValuesView> {
    let s = find_scenario(q, scenario_id, "").await?;
    let a = activity::load(q, u, activity_id).await?;

    // ドライバーと値
    let rows: Vec<DriverValueRow> = q
        .exec(
            "SELECT d.id, d.code, d.name, d.driver_kind, COALESCE(d.unit, ''),
                    DATE_FORMAT(v.target_month, '%Y-%m'), v.value, v.is_provisional, COALESCE(v.provisional_reason, '')
             FROM activity_drivers d
             LEFT JOIN driver_values v ON v.activity_driver_id = d.id AND v.scenario_id = ?
             WHERE d.activity_id = ?
             ORDER BY d.sort_order, d.id, v.target_month",
            (scenario_id, activity_id),
        )
        .await?;
    let mut drivers: Vec<DriverRow> = Vec::new();
    for (id, code, name, driver_kind, unit, month, value, provisional, reason) in rows {
        if drivers.last().is_none_or(|d| d.id != id) {
            drivers.push(DriverRow { id, code, name, driver_kind, unit, values: Vec::new() });
        }
        if let Some(month) = month {
            drivers.last_mut().unwrap().values.push(ValueCell {
                target_month: month,
                value: trim_decimal(value.as_deref().unwrap_or("")),
                is_provisional: provisional.unwrap_or(false),
                provisional_reason: reason,
            });
        }
    }

    let editable = a.can_edit && !s.is_locked && can_edit_scenario(u, &s);
    Ok(ValuesView {
        months: calc::fiscal_months(s.fiscal_year),
        actual_months: s.actual_months(),
        editable,
        drivers,
        // 金額: 内訳がある科目と、金額がある科目
        amounts: load_amounts(q, u, scenario_id, activity_id).await?,
        restricted_hidden: visibility::hidden(q, u).await?,
        note: load_note(q, scenario_id, activity_id, "").await?.0,
        scenario: s,
        activity: a,
    })
}

type LineSelectRow = (i64, i64, String, String, bool, Option<String>, String);
type ScenarioAmountRow = (i64, Option<i64>, String, String, String, bool, String);

/// 科目ごとの金額（科目への直接入力と内訳）を返す。
/// 決算確定月以前の月は実績（scenario_amounts ビュー、source は actual）で、科目への直接入力として返す。
/// ユーザーが見られない科目（閲覧制限）は、内訳も金額も返さない。
async fn load_amounts<Q: Queryable>(
    q: &mut Q,
    u: &User,
    scenario_id: i64,
    activity_id: i64,
) -> AppResult<Vec<AmountRow>> {
    let mut rows: HashMap<i64, AmountRow> = HashMap::new();
    let new_row = |subject_id: i64| AmountRow {
        subject_id,
        code: String::new(),
        name: String::new(),
        category: String::new(),
        values: Vec::new(),
        lines: Vec::new(),
    };
    let filter = visibility::subject_filter(u, "subject_id");

    let lines: Vec<LineSelectRow> = q
        .exec(
            format!(
                "SELECT id, subject_id, name, COALESCE(expression, ''), formula_enabled, confidence_level, outlook
                 FROM activity_lines WHERE activity_id = ?{filter} ORDER BY subject_id, sort_order, id"
            ),
            (activity_id,),
        )
        .await?;
    // 内訳 ID → (科目, 何番目の内訳か)
    let mut line_of: HashMap<i64, (i64, usize)> = HashMap::new();
    for (id, subject_id, name, expression, formula_enabled, confidence_level, outlook) in lines {
        let r = rows.entry(subject_id).or_insert_with(|| new_row(subject_id));
        line_of.insert(id, (subject_id, r.lines.len()));
        r.lines.push(LineRow { id, name, expression, formula_enabled, confidence_level, outlook, values: Vec::new() });
    }

    let facts: Vec<ScenarioAmountRow> = q
        .exec(
            format!(
                "SELECT subject_id, line_id, DATE_FORMAT(target_month, '%Y-%m'), amount, source, is_provisional, COALESCE(provisional_reason, '')
                 FROM scenario_amounts WHERE scenario_id = ? AND activity_id = ?{filter} ORDER BY target_month"
            ),
            (scenario_id, activity_id),
        )
        .await?;
    for (subject_id, line_id, target_month, amount, source, is_provisional, provisional_reason) in facts {
        let c = AmountCell { target_month, amount: RawNum(amount), source, is_provisional, provisional_reason };
        match line_id.and_then(|l| line_of.get(&l)) {
            Some(&(sid, i)) => rows.get_mut(&sid).unwrap().lines[i].values.push(c),
            None => rows.entry(subject_id).or_insert_with(|| new_row(subject_id)).values.push(c),
        }
    }

    // 科目の情報を付けて、区分（収益 → 費用）・表示順・コードの順に並べる
    let subjects: Vec<(i64, String, String, String)> =
        q.exec("SELECT id, code, name, category FROM subjects ORDER BY category, sort_order, code", ()).await?;
    let mut out = Vec::new();
    for (id, code, name, category) in subjects {
        if let Some(mut r) = rows.remove(&id) {
            r.code = code;
            r.name = name;
            r.category = category;
            out.push(r);
        }
    }
    Ok(out)
}

// --- 更新の共通処理 ---

/// 数値の編集を始める: シナリオと施策を行ロックし、数値を編集できることを確認する。
/// 数値を変更したら、呼び出し側で mark_edited を呼んで、施策 × シナリオの変更日時を記録して完了を取り消す。
pub async fn begin_edit(
    st: &AppState,
    u: &User,
    scenario_id: i64,
    activity_id: i64,
    reason: &str,
) -> AppResult<(Tx, Recorder, Scenario, Summary)> {
    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, Some(scenario_id), reason).await?;
    let s = find_scenario(&mut tx, scenario_id, " FOR SHARE").await?;
    if s.is_locked {
        return Err(conflict("ロックされたシナリオは変更できません"));
    }
    if !can_edit_scenario(u, &s) {
        return Err(conflict("作成中ではないシナリオの数値は FP&A のみが入力できます"));
    }
    let a = activity::lock_for_edit(&mut tx, u, activity_id).await?;
    Ok((tx, rec, s, a))
}

/// 月と「仮の値」の入力を検証する。入力できるのは計画値の月（決算確定月より後）だけ。仮の値には理由が必須。
fn validate_cell(
    v: &mut Validator,
    prefix: &str,
    month: &mut String,
    is_provisional: bool,
    reason: &mut String,
    s: &Scenario,
) {
    *month = month.trim().to_string();
    let months = calc::fiscal_months(s.fiscal_year);
    if !months.contains(month) {
        v.add(
            &format!("{prefix}.target_month"),
            format!("対象月は {}〜{} の YYYY-MM で指定してください", months[0], months[11]),
        );
    } else if !s.plan_months().contains(month) {
        v.add(
            &format!("{prefix}.target_month"),
            format!("{month} は決算確定月（{}）以前の実績の月のため入力できません", s.through()),
        );
    }
    *reason = v.optional_text(&format!("{prefix}.provisional_reason"), "仮の値の理由", reason, MAX_REASON_LEN);
    if is_provisional && reason.is_empty() {
        v.add(&format!("{prefix}.provisional_reason"), "仮の値には理由を入力してください");
    }
    if !is_provisional {
        reason.clear();
    }
}

// --- ドライバー値 ---

#[derive(Deserialize, Default, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct DriverValueInput {
    driver_id: i64,
    /// None（null）で削除
    value: Option<RawNum>,
    target_month: String,
    is_provisional: bool,
    provisional_reason: String,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct DriverValuesRequest {
    values: Vec<DriverValueInput>,
    reason: String,
}

/// 監査ログに記録するドライバー値。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct DriverValueRecord {
    pub id: i64,
    pub scenario_id: i64,
    pub activity_driver_id: i64,
    pub target_month: String,
    pub value: String,
    pub is_provisional: bool,
    pub provisional_reason: String,
}

/// PUT /api/scenarios/{id}/activities/{aid}/driver-values（?dry_run=true で試算）。
/// 指定した（ドライバー, 月）の値を登録・更新・削除（value: null）し、計算式の金額を再計算する。変更理由が必須。
pub async fn put_driver_values(
    State(st): State<AppState>,
    CurrentUser(u): CurrentUser,
    ids: PathIds,
    q: Query,
    Json(mut req): Json<DriverValuesRequest>,
) -> AppResult<Response> {
    let (scenario_id, activity_id) = path_ids(&ids)?;
    // dry_run=true は試算: 保存したときと同じ検証・再計算を行い、結果を返してロールバックする（変更理由は不要）
    let dry_run = q.get("dry_run") == "true";
    if dry_run {
        req.reason = "試算".into();
    }
    req.reason = default_reason(&st, scenario_id, &req.reason).await?;
    check_batch(req.values.len(), &req.reason)?;

    let (mut tx, rec, s, _) = begin_edit(&st, &u, scenario_id, activity_id, &req.reason).await?;
    let drivers: HashSet<i64> = tx
        .exec::<i64, _, _>("SELECT id FROM activity_drivers WHERE activity_id = ?", (activity_id,))
        .await?
        .into_iter()
        .collect();

    let mut v = Validator::new();
    let mut seen = HashSet::new();
    let mut parsed = vec![String::new(); req.values.len()];
    for (i, input) in req.values.iter_mut().enumerate() {
        let prefix = format!("values[{i}]");
        if !drivers.contains(&input.driver_id) {
            v.add(&format!("{prefix}.driver_id"), "この施策のドライバーを指定してください");
        }
        validate_cell(
            &mut v,
            &prefix,
            &mut input.target_month,
            input.is_provisional,
            &mut input.provisional_reason,
            &s,
        );
        if !seen.insert(format!("{}/{}", input.driver_id, input.target_month)) {
            v.add(&prefix, "同じドライバー・月が重複しています");
        }
        if let Some(value) = &input.value {
            parsed[i] = parse_driver_value(&mut v, &format!("{prefix}.value"), &value.0);
        }
    }
    v.check()?;

    for (i, input) in req.values.iter().enumerate() {
        write_driver_value(
            &mut tx,
            &rec,
            scenario_id,
            input.driver_id,
            &input.target_month,
            input.value.as_ref().map(|_| parsed[i].as_str()),
            input.is_provisional,
            &input.provisional_reason,
        )
        .await?;
    }
    calc::recalculate(&mut tx, &rec, scenario_id, activity_id).await?;
    if dry_run {
        // 試算の結果を読んでからロールバックする（更新の状態は変えない）
        let preview = load_values(&mut tx, &u, scenario_id, activity_id).await?;
        tx.rollback().await?;
        return Ok(http::ok(&preview));
    }
    mark_edited(&mut tx, &rec, scenario_id, activity_id).await?;
    tx.commit().await?;
    respond_values(&st, &u, scenario_id, activity_id).await
}

/// ドライバー値を1件書き込む（value が None なら削除）。変わらなければ何もしない。(insert, update, delete) のどれをしたかを返す。
#[allow(clippy::too_many_arguments)]
pub async fn write_driver_value(
    tx: &mut Tx,
    rec: &Recorder,
    scenario_id: i64,
    driver_id: i64,
    month: &str,
    value: Option<&str>,
    is_provisional: bool,
    provisional_reason: &str,
) -> AppResult<Option<&'static str>> {
    let row: Option<(i64, String, bool, String)> = tx
        .exec_first(
            "SELECT id, value, is_provisional, COALESCE(provisional_reason, '')
             FROM driver_values WHERE activity_driver_id = ? AND scenario_id = ? AND target_month = ? FOR UPDATE",
            p![driver_id, scenario_id, format!("{month}-01")],
        )
        .await?;
    let mut before = DriverValueRecord {
        scenario_id,
        activity_driver_id: driver_id,
        target_month: month.to_string(),
        value: "0".into(),
        ..Default::default()
    };
    if let Some((id, v, p, r)) = &row {
        before.id = *id;
        before.value = trim_decimal(v).0;
        before.is_provisional = *p;
        before.provisional_reason = r.clone();
    }

    let Some(value) = value else {
        if row.is_some() {
            tx.exec_drop("DELETE FROM driver_values WHERE id = ?", (before.id,)).await?;
            rec.delete(tx, "driver_values", before.id, &before).await?;
            return Ok(Some("delete"));
        }
        return Ok(None);
    };
    let mut after = DriverValueRecord {
        id: 0,
        scenario_id,
        activity_driver_id: driver_id,
        target_month: month.to_string(),
        value: trim_decimal(value).0,
        is_provisional,
        provisional_reason: provisional_reason.to_string(),
    };
    if row.is_some() {
        after.id = before.id;
        if before == after {
            return Ok(None);
        }
        tx.exec_drop(
            "UPDATE driver_values SET value = ?, is_provisional = ?, provisional_reason = ? WHERE id = ?",
            p![value, is_provisional, db::null_str(provisional_reason), before.id],
        )
        .await?;
        rec.update(tx, "driver_values", before.id, &before, &after).await?;
        return Ok(Some("update"));
    }
    tx.exec_drop(
        "INSERT INTO driver_values (activity_driver_id, scenario_id, target_month, value, is_provisional, provisional_reason)
         VALUES (?, ?, ?, ?, ?, ?)",
        p![driver_id, scenario_id, format!("{month}-01"), value, is_provisional, db::null_str(provisional_reason)],
    )
    .await?;
    after.id = db::last_id(tx);
    rec.insert(tx, "driver_values", after.id, &after).await?;
    Ok(Some("insert"))
}

/// ドライバー値を検証し、DB に保存する文字列（小数点以下6桁）を返す。
pub fn parse_driver_value(v: &mut Validator, field: &str, n: &str) -> String {
    // driver_values.value（DECIMAL(24,6)）の整数部に入る絶対値の上限
    let max = Rat::from_integer(num_traits::pow(BigInt::from(10), 18));
    match formula::parse_rat(n) {
        None => v.add(field, "数値を入力してください"),
        Some(x) if !(&x * Rat::from_integer(BigInt::from(1_000_000))).is_integer() => {
            v.add(field, "小数点以下は6桁までで入力してください")
        }
        Some(x) if x.abs() >= max => v.add(field, "値が大きすぎます"),
        Some(x) => return formula::format_decimal(&x, 6),
    }
    String::new()
}

// --- 金額（直接入力） ---

#[derive(Deserialize, Default, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct AmountInput {
    subject_id: i64,
    /// 内訳。None は科目への直接入力
    line_id: Option<i64>,
    /// None（null）で削除
    amount: Option<RawNum>,
    target_month: String,
    is_provisional: bool,
    provisional_reason: String,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct AmountsRequest {
    amounts: Vec<AmountInput>,
    reason: String,
}

/// PUT /api/scenarios/{id}/activities/{aid}/amounts。
/// 金額（円）を直接入力する。line_id で内訳を指定する（null は科目への直接入力）。
/// 計算式で反映する内訳は入力できない。変更理由が必須。
pub async fn put_amounts(
    State(st): State<AppState>,
    CurrentUser(u): CurrentUser,
    ids: PathIds,
    Json(mut req): Json<AmountsRequest>,
) -> AppResult<Response> {
    let (scenario_id, activity_id) = path_ids(&ids)?;
    req.reason = default_reason(&st, scenario_id, &req.reason).await?;
    check_batch(req.amounts.len(), &req.reason)?;

    let (mut tx, rec, s, _) = begin_edit(&st, &u, scenario_id, activity_id, &req.reason).await?;
    let lines = calc::lines(&mut tx, activity_id).await?;

    let mut v = Validator::new();
    let mut seen = HashSet::new();
    let mut amounts = vec![String::new(); req.amounts.len()];
    for (i, input) in req.amounts.iter_mut().enumerate() {
        let prefix = format!("amounts[{i}]");
        if !db::exists(&mut tx, "subjects", input.subject_id).await? {
            v.add(&format!("{prefix}.subject_id"), "科目が見つかりません");
        } else {
            check_restricted_input(&mut tx, &u, input.subject_id).await?;
        }
        if let Some(line_id) = input.line_id {
            match lines.get(&line_id) {
                Some(l) if l.subject_id == input.subject_id => {
                    if l.computed {
                        v.add(
                            &format!("{prefix}.line_id"),
                            format!("内訳「{}」は計算式で反映するため直接入力できません", l.name),
                        );
                    }
                }
                _ => v.add(&format!("{prefix}.line_id"), "この施策・科目の内訳を指定してください"),
            }
        }
        validate_cell(
            &mut v,
            &prefix,
            &mut input.target_month,
            input.is_provisional,
            &mut input.provisional_reason,
            &s,
        );
        if !seen.insert(format!("{}/{}/{}", input.subject_id, input.line_id.unwrap_or(0), input.target_month)) {
            v.add(&prefix, "同じ科目・内訳・月が重複しています");
        }
        if let Some(amount) = &input.amount {
            amounts[i] = parse_amount(&mut v, &format!("{prefix}.amount"), &amount.0);
        }
    }
    v.check()?;

    let existing = calc::load_facts(&mut tx, scenario_id, activity_id).await?;
    for (i, input) in req.amounts.iter().enumerate() {
        let key = Key {
            subject_id: input.subject_id,
            line_id: input.line_id.unwrap_or(0),
            month: input.target_month.clone(),
        };
        let before = existing.get(&key);
        if input.amount.is_none() {
            if let Some(before) = before {
                tx.exec_drop("DELETE FROM budget_facts WHERE id = ?", (before.id,)).await?;
                rec.delete(&mut tx, "budget_facts", before.id, before).await?;
            }
            continue;
        }
        let mut after = Fact {
            id: 0,
            scenario_id,
            activity_id,
            subject_id: input.subject_id,
            line_id: input.line_id,
            target_month: input.target_month.clone(),
            amount: amounts[i].clone(),
            source: "manual".into(),
            is_provisional: input.is_provisional,
            provisional_reason: input.provisional_reason.clone(),
        };
        match before {
            Some(before) => {
                after.id = before.id;
                if before.amount == after.amount
                    && before.source == after.source
                    && before.is_provisional == after.is_provisional
                    && before.provisional_reason == after.provisional_reason
                {
                    continue;
                }
                calc::update_fact(&mut tx, &after).await?;
                rec.update(&mut tx, "budget_facts", before.id, before, &after).await?;
            }
            None => {
                after.id = calc::insert_fact(&mut tx, &after).await?;
                rec.insert(&mut tx, "budget_facts", after.id, &after).await?;
            }
        }
    }
    mark_edited(&mut tx, &rec, scenario_id, activity_id).await?;
    tx.commit().await?;
    respond_values(&st, &u, scenario_id, activity_id).await
}

/// 金額（円、整数）を検証する。
pub fn parse_amount(v: &mut Validator, field: &str, n: &str) -> String {
    // budget_facts.amount（DECIMAL(18,0)）に入る絶対値の上限
    let max: BigInt = num_traits::pow(BigInt::from(10), 18) - 1;
    match formula::parse_int(n) {
        None => v.add(field, "金額は円単位の整数で入力してください"),
        Some(x) if x.abs() > max => v.add(field, "金額が大きすぎます"),
        Some(x) => return x.to_string(),
    }
    String::new()
}

/// 閲覧制限のある科目の金額を入力できるか（FP&A のみ）を確かめる（docs/plan.md「2.17」）。
async fn check_restricted_input(tx: &mut Tx, u: &User, subject_id: i64) -> AppResult<()> {
    if u.is_admin() || !visibility::is_restricted(tx, subject_id).await? {
        return Ok(());
    }
    Err(AppError::new(StatusCode::FORBIDDEN, "forbidden", "閲覧制限のある科目の金額は FP&A のみが入力できます"))
}

// --- 想定条件 ---

/// PUT /api/scenarios/{id}/activities/{aid}/condition。
/// 想定条件は今回の見込の説明に統合したため（docs/plan.md「2.18」）、廃止を示す 410 を返す。
pub async fn put_condition(_: CurrentUser) -> AppError {
    gone("想定条件は「今回の見込の説明」に統合しました。説明に書いてください")
}

/// 変更理由が空のときに記録する理由を返す（docs/plan.md「2.18」）。
/// 作成中のシナリオの数値入力だけ、理由を省略できる。それ以外は空のまま返し、check_batch で必須のエラーになる。
async fn default_reason(st: &AppState, scenario_id: i64, reason: &str) -> AppResult<String> {
    if !reason.trim().is_empty() {
        return Ok(reason.to_string());
    }
    let mut conn = st.db.get_conn().await?;
    let s = find_scenario(&mut conn, scenario_id, "").await?;
    if !s.is_active {
        return Ok(reason.to_string());
    }
    Ok(format!("{}の見込更新", s.name))
}

/// 一括更新の件数と変更理由を確認する。
fn check_batch(n: usize, reason: &str) -> AppResult<()> {
    if n == 0 {
        return Err(validation1("values", "更新する値を指定してください"));
    }
    if n > MAX_ITEMS_PER_REQUEST {
        return Err(validation1("values", format!("一度に更新できるのは{MAX_ITEMS_PER_REQUEST}件までです")));
    }
    activity::require_reason(reason)
}

/// DECIMAL の文字列から末尾の不要な 0 を取り除く（例: "12.500000" → "12.5"）。
pub fn trim_decimal(s: &str) -> RawNum {
    let mut s = s.to_string();
    if s.contains('.') {
        s = s.trim_end_matches('0').trim_end_matches('.').to_string();
    }
    if s.is_empty() || s == "-" {
        s = "0".into();
    }
    RawNum(s)
}

/// 有理数を Go の big.Rat.RatString と同じ形（整数なら "12"、そうでなければ "1/3"）にする。
pub fn rat_string(x: &Rat) -> String {
    if x.is_integer() { x.to_integer().to_string() } else { format!("{}/{}", x.numer(), x.denom()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimals() {
        assert_eq!(trim_decimal("12.500000").0, "12.5");
        assert_eq!(trim_decimal("-0.000000").0, "-0");
        assert_eq!(trim_decimal("").0, "0");
        let mut v = Validator::new();
        assert_eq!(parse_driver_value(&mut v, "x", "1000.5"), "1000.500000");
        assert_eq!(parse_driver_value(&mut v, "y", "0.1234567"), "");
        assert_eq!(parse_amount(&mut v, "z", "1.5"), "");
        assert_eq!(parse_amount(&mut v, "a", "-1200"), "-1200");
        assert_eq!(v.0.len(), 2);
        assert!(Rat::from_integer(0.into()).is_integer());
    }
}
