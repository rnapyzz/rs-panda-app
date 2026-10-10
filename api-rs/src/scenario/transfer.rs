//! 計画値の CSV（金額の直接入力・ドライバー値）の出力と取込（docs/plan.md「6.3」）。

use std::collections::{BTreeMap, HashMap, HashSet};

use axum::extract::State;
use axum::extract::multipart::{Multipart, MultipartRejection};
use axum::response::Response;
use mysql_async::prelude::*;
use serde::Serialize;

use super::notes::mark_edited;
use super::values::{DriverValueRecord, parse_amount, parse_driver_value, rat_string, trim_decimal};
use super::{Scenario, can_edit_scenario, find_scenario};
use crate::activity::{self, Summary};
use crate::audit::Recorder;
use crate::auth::{CurrentUser, User};
use crate::calc::{self, Fact, Key};
use crate::csvio::{self, RowErrors};
use crate::db::{self, Params, Tx, Value};
use crate::error::{bad_request, conflict, too_large};
use crate::formula::{self, Rat};
use crate::http::{self, Id, Query, Validator};
use crate::{AppResult, AppState, audit, visibility};

const MAX_PLAN_CSV_BYTES: usize = 5 << 20; // 5MB
const MAX_PLAN_CSV_ROWS: usize = 5_000;
/// 「値を消す」セルの書き方（空欄は「変えない」）
const DELETE_MARK: &str = "-";

const AMOUNT_KEY_COLUMNS: [&str; 3] = ["activity_code", "subject_code", "line_name"];
const AMOUNT_REF_COLUMNS: [&str; 3] = ["activity_name", "subject_name", "line_type"];
const DRIVER_KEY_COLUMNS: [&str; 2] = ["activity_code", "driver_code"];
const DRIVER_REF_COLUMNS: [&str; 3] = ["activity_name", "driver_name", "unit"];

// --- 出力 ---

/// シナリオと、ユニットでの絞り込み（unit_id）の条件を読む。
async fn export_filter(st: &AppState, id: i64, q: &Query) -> AppResult<(Scenario, String, Vec<Value>)> {
    let mut conn = st.db.get_conn().await?;
    let s = find_scenario(&mut conn, id, "").await?;
    let unit = q.get("unit_id");
    if !unit.is_empty() {
        match unit.parse::<i64>() {
            Ok(unit_id) if unit_id > 0 => return Ok((s, " AND a.unit_id = ?".into(), vec![unit_id.into()])),
            _ => return Err(bad_request("unit_id が正しくありません")),
        }
    }
    Ok((s, String::new(), Vec::new()))
}

/// 出力する内訳（ID, 名前, 計算式で反映するか）。
type ExportLine = (i64, String, bool);

/// GET /api/scenarios/{id}/amounts/export。
/// 1行が施策 × 科目 × 内訳（内訳名が空は科目への直接入力）で、月を横に並べる。実績の月は実績を出す。
/// ユーザーが見られない科目（閲覧制限）の行は出さない。
pub async fn export_amounts(
    State(st): State<AppState>,
    CurrentUser(u): CurrentUser,
    Id(id): Id,
    q: Query,
) -> AppResult<Response> {
    let (s, cond, args) = export_filter(&st, id, &q).await?;
    let months = calc::fiscal_months(s.fiscal_year);
    let mut conn = st.db.get_conn().await?;
    let acts: Vec<(i64, String, String)> = conn
        .exec(
            format!("SELECT a.id, a.code, a.name FROM activities a WHERE 1 = 1{cond} ORDER BY a.code"),
            Params::Positional(args.clone()),
        )
        .await?;
    let subjects: Vec<(i64, String, String)> = conn
        .exec(
            format!(
                "SELECT id, code, name FROM subjects WHERE 1 = 1{} ORDER BY category, sort_order, code",
                visibility::subject_filter(&u, "id")
            ),
            (),
        )
        .await?;

    // 施策 → 科目 → 内訳（表示順）
    let line_rows: Vec<(i64, i64, i64, String, bool)> = conn
        .exec(
            format!(
                "SELECT l.id, l.activity_id, l.subject_id, l.name, l.formula_enabled
                 FROM activity_lines l JOIN activities a ON a.id = l.activity_id
                 WHERE 1 = 1{cond} ORDER BY l.sort_order, l.id"
            ),
            Params::Positional(args.clone()),
        )
        .await?;
    let mut lines: HashMap<(i64, i64), Vec<ExportLine>> = HashMap::new();
    let mut line_ids: HashSet<i64> = HashSet::new();
    for (lid, aid, sid, name, formula) in line_rows {
        lines.entry((aid, sid)).or_default().push((lid, name, formula));
        line_ids.insert(lid);
    }

    // 金額: (施策, 科目, 内訳（0 は直接入力）) → 月 → 金額
    let mut vargs: Vec<Value> = vec![s.id.into()];
    vargs.extend(args);
    let value_rows: Vec<(i64, i64, Option<i64>, String, String)> = conn
        .exec(
            format!(
                "SELECT v.activity_id, v.subject_id, v.line_id, DATE_FORMAT(v.target_month, '%Y-%m'), v.amount
                 FROM scenario_amounts v JOIN activities a ON a.id = v.activity_id
                 WHERE v.scenario_id = ?{cond}"
            ),
            Params::Positional(vargs),
        )
        .await?;
    let mut values: HashMap<(i64, i64, i64), HashMap<String, Rat>> = HashMap::new();
    for (aid, sid, line_id, month, amount) in value_rows {
        let lid = line_id.filter(|l| line_ids.contains(l)).unwrap_or(0);
        let x = formula::parse_rat(&amount).ok_or_else(|| anyhow::anyhow!("金額を読めません: {amount}"))?;
        *values.entry((aid, sid, lid)).or_default().entry(month).or_insert_with(|| Rat::from_integer(0.into())) += x;
    }

    let cells = |m: Option<&HashMap<String, Rat>>| -> Vec<String> {
        months.iter().map(|month| m.and_then(|m| m.get(month)).map(rat_string).unwrap_or_default()).collect()
    };
    let mut header: Vec<&str> =
        vec!["activity_code", "activity_name", "subject_code", "subject_name", "line_name", "line_type"];
    header.extend(months.iter().map(String::as_str));
    let mut out: Vec<Vec<String>> = Vec::new();
    for (aid, acode, aname) in &acts {
        for (sid, scode, sname) in &subjects {
            if let Some(direct) = values.get(&(*aid, *sid, 0)).filter(|d| !d.is_empty()) {
                let mut row =
                    vec![acode.clone(), aname.clone(), scode.clone(), sname.clone(), String::new(), "manual".into()];
                row.extend(cells(Some(direct)));
                out.push(row);
            }
            for (lid, lname, formula) in lines.get(&(*aid, *sid)).map(Vec::as_slice).unwrap_or(&[]) {
                let typ = if *formula { "formula" } else { "manual" };
                let mut row =
                    vec![acode.clone(), aname.clone(), scode.clone(), sname.clone(), lname.clone(), typ.into()];
                row.extend(cells(values.get(&(*aid, *sid, *lid))));
                out.push(row);
            }
        }
    }
    csvio::write_csv("amounts", &header, &out)
}

/// GET /api/scenarios/{id}/driver-values/export。1行が施策 × ドライバーで、月を横に並べる。
pub async fn export_driver_values(
    State(st): State<AppState>,
    _: CurrentUser,
    Id(id): Id,
    q: Query,
) -> AppResult<Response> {
    let (s, cond, args) = export_filter(&st, id, &q).await?;
    let months = calc::fiscal_months(s.fiscal_year);
    let mut conn = st.db.get_conn().await?;
    let mut vargs: Vec<Value> = vec![s.id.into()];
    vargs.extend(args.clone());
    let rows: Vec<(i64, String, String)> = conn
        .exec(
            format!(
                "SELECT v.activity_driver_id, DATE_FORMAT(v.target_month, '%Y-%m'), v.value
                 FROM driver_values v
                 JOIN activity_drivers d ON d.id = v.activity_driver_id
                 JOIN activities a ON a.id = d.activity_id
                 WHERE v.scenario_id = ?{cond}"
            ),
            Params::Positional(vargs),
        )
        .await?;
    let mut values: HashMap<i64, HashMap<String, String>> = HashMap::new();
    for (did, month, value) in rows {
        values.entry(did).or_default().insert(month, trim_decimal(&value).0);
    }
    let drivers: Vec<(i64, String, String, String, String, String)> = conn
        .exec(
            format!(
                "SELECT d.id, a.code, a.name, d.code, d.name, COALESCE(d.unit, '')
                 FROM activity_drivers d JOIN activities a ON a.id = d.activity_id
                 WHERE 1 = 1{cond} ORDER BY a.code, d.sort_order, d.id"
            ),
            Params::Positional(args),
        )
        .await?;
    let mut header: Vec<&str> = vec!["activity_code", "activity_name", "driver_code", "driver_name", "unit"];
    header.extend(months.iter().map(String::as_str));
    let out: Vec<Vec<String>> = drivers
        .into_iter()
        .map(|(did, a, b, c, d, e)| {
            let mut row = vec![a, b, c, d, e];
            row.extend(months.iter().map(|m| values.get(&did).and_then(|v| v.get(m)).cloned().unwrap_or_default()));
            row
        })
        .collect();
    csvio::write_csv("driver-values", &header, &out)
}

// --- 取込 ---

#[derive(Serialize)]
struct PlanImportActivity {
    activity_id: i64,
    code: String,
    name: String,
    changed: usize,
}

#[derive(Serialize)]
struct PlanImportWarning {
    /// actual_month（実績の月の値）か formula_line（計算式で反映する内訳の値）
    kind: &'static str,
    count: usize,
}

/// 取込の結果。件数はセルの数。
#[derive(Serialize)]
struct PlanImportResult {
    dry_run: bool,
    /// amounts / driver_values
    kind: &'static str,
    rows: usize,
    inserted: usize,
    updated: usize,
    deleted: usize,
    unchanged: usize,
    activities: Vec<PlanImportActivity>,
    warnings: Vec<PlanImportWarning>,
}

/// 取り込む1セルの変更。value が None なら削除。
#[derive(Clone)]
struct CellOp {
    subject_id: i64,
    /// 金額の内訳（None は科目への直接入力）
    line_id: Option<i64>,
    /// ドライバー値
    driver_id: i64,
    month: String,
    value: Option<String>,
}

/// 取込の途中の状態（行の検証と、施策ごとの変更）。
struct PlanImport {
    u: User,
    s: Scenario,
    months: Vec<String>,
    errs: RowErrors,
    result: PlanImportResult,
    /// 施策コード → ID
    acts: HashMap<String, i64>,
    summary: HashMap<i64, Summary>,
    ops: BTreeMap<i64, Vec<CellOp>>,
    warnings: HashMap<&'static str, usize>,
}

/// POST /api/scenarios/{id}/plan-values/import（?dry_run=true で確認のみ）。
/// 金額かドライバー値の CSV（ヘッダーで判定）を取り込む。権限は数値の入力と同じで、施策ごとに判定する。
pub async fn import_plan_values(
    State(st): State<AppState>,
    CurrentUser(u): CurrentUser,
    Id(scenario_id): Id,
    q: Query,
    mp: Result<Multipart, MultipartRejection>,
) -> AppResult<Response> {
    let up = csvio::read_upload(&q, mp).await?;
    if up.data.len() > MAX_PLAN_CSV_BYTES {
        return Err(too_large("ファイルは 5MB 以下にしてください"));
    }
    let header = csvio::header(&up.data)?;
    let kind = if header.iter().any(|h| h == "driver_code") {
        "driver_values"
    } else if header.iter().any(|h| h == "subject_code") {
        "amounts"
    } else {
        return Err(bad_request(
            "金額の CSV（subject_code の列）か、ドライバー値の CSV（driver_code の列）を指定してください",
        ));
    };
    let s = {
        let mut conn = st.db.get_conn().await?;
        find_scenario(&mut conn, scenario_id, "").await?
    };
    let months = calc::fiscal_months(s.fiscal_year);
    let (keys, refs): (&[&str], &[&str]) = if kind == "driver_values" {
        (&DRIVER_KEY_COLUMNS, &DRIVER_REF_COLUMNS)
    } else {
        (&AMOUNT_KEY_COLUMNS, &AMOUNT_REF_COLUMNS)
    };
    let optional: Vec<&str> = refs.iter().copied().chain(months.iter().map(String::as_str)).collect();
    let rows = csvio::parse_with_optional(&up.data, keys, &optional)?;
    if rows.len() > MAX_PLAN_CSV_ROWS {
        return Err(bad_request("データ行は 5,000 行までです"));
    }

    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, Some(scenario_id), &up.reason).await?;
    let s = find_scenario(&mut tx, scenario_id, " FOR SHARE").await?;
    if s.is_locked {
        return Err(conflict("ロックされたシナリオには取り込めません"));
    }
    if !can_edit_scenario(&u, &s) {
        return Err(conflict("作成中ではないシナリオの数値は FP&A のみが入力できます"));
    }
    let acts = code_index(&mut tx, "SELECT code, id FROM activities").await?;
    let mut p = PlanImport {
        u: u.clone(),
        s,
        months,
        errs: RowErrors::default(),
        result: PlanImportResult {
            dry_run: up.dry_run,
            kind,
            rows: rows.len(),
            inserted: 0,
            updated: 0,
            deleted: 0,
            unchanged: 0,
            activities: Vec::new(),
            warnings: Vec::new(),
        },
        acts,
        summary: HashMap::new(),
        ops: BTreeMap::new(),
        warnings: HashMap::new(),
    };
    if kind == "amounts" {
        p.plan_amounts(&mut tx, &rows).await?;
    } else {
        p.plan_driver_values(&mut tx, &rows).await?;
    }
    p.errs.check()?;
    for k in ["actual_month", "formula_line"] {
        if let Some(&n) = p.warnings.get(k).filter(|n| **n > 0) {
            p.result.warnings.push(PlanImportWarning { kind: k, count: n });
        }
    }

    // 行ロックの順番をそろえる（BTreeMap なので ID の順）
    let ids: Vec<i64> = p.ops.keys().copied().collect();
    for id in ids {
        let a = activity::lock_for_edit(&mut tx, &u, id).await?;
        if kind == "amounts" {
            p.apply_amounts(&mut tx, &rec, id).await?;
        } else {
            p.apply_driver_values(&mut tx, &rec, id).await?;
        }
        mark_edited(&mut tx, &rec, scenario_id, id).await?;
        p.result.activities.push(PlanImportActivity {
            activity_id: id,
            code: a.code,
            name: a.name,
            changed: p.ops[&id].len(),
        });
    }
    p.result.activities.sort_by(|a, b| a.code.cmp(&b.code));
    if up.dry_run {
        tx.rollback().await?;
    } else {
        tx.commit().await?;
    }
    Ok(http::ok(&p.result))
}

/// コード → ID の対応を返す。
async fn code_index(tx: &mut Tx, sql: &str) -> AppResult<HashMap<String, i64>> {
    let rows: Vec<(String, i64)> = tx.exec(sql, ()).await?;
    Ok(rows.into_iter().collect())
}

/// セルの値が今の値から変わるかを返す（数値として比べる）。
fn changed_cell(cur: Option<&String>, value: &Option<String>) -> bool {
    match (value, cur) {
        (None, cur) => cur.is_some(),
        (Some(_), None) => true,
        (Some(v), Some(c)) => match (formula::parse_rat(c), formula::parse_rat(v)) {
            (Some(a), Some(b)) => a != b,
            _ => true,
        },
    }
}

impl PlanImport {
    /// 行の施策を返す。見つからなければ行のエラーにする。
    async fn activity_of(&mut self, tx: &mut Tx, row: &csvio::Row) -> AppResult<Option<Summary>> {
        let code = row.get("activity_code");
        let Some(&id) = self.acts.get(code) else {
            self.errs.add(row.line, format!("施策コード「{code}」が見つかりません"));
            return Ok(None);
        };
        if let Some(a) = self.summary.get(&id) {
            return Ok(Some(a.clone()));
        }
        let a = activity::load(tx, &self.u, id).await?;
        self.summary.insert(id, a.clone());
        Ok(Some(a))
    }

    /// 行の月のセルを読む。空欄は「変えない」で返さず、"-" は削除（None）。形式の誤りは行のエラーにする。
    fn read_cells(
        &mut self,
        row: &csvio::Row,
        parse: fn(&mut Validator, &str, &str) -> String,
    ) -> HashMap<String, Option<String>> {
        let mut out = HashMap::new();
        for m in &self.months {
            let raw = row.get(m);
            match raw {
                "" => continue,
                DELETE_MARK => {
                    out.insert(m.clone(), None);
                    continue;
                }
                _ => {}
            }
            let mut v = Validator::new();
            let value = parse(&mut v, m, raw);
            if let Some(msg) = v.0.get(m) {
                self.errs.add(row.line, format!("{m} 列: {msg}（{raw}）"));
                continue;
            }
            out.insert(m.clone(), Some(value));
        }
        out
    }

    /// 行の変更を集める。変わらないセルは「変更なし」、実績の月や計算式の内訳は取り込まずに注意として数える。
    fn collect(
        &mut self,
        row: &csvio::Row,
        a: &Summary,
        cells: &HashMap<String, Option<String>>,
        current: impl Fn(&str) -> Option<String>,
        formula: bool,
        op: impl Fn(&str, Option<String>) -> CellOp,
    ) {
        let actual = self.s.actual_months();
        let mut ops = Vec::new();
        for m in &self.months {
            let Some(value) = cells.get(m) else { continue };
            if !changed_cell(current(m).as_ref(), value) {
                self.result.unchanged += 1;
            } else if actual.contains(m) {
                *self.warnings.entry("actual_month").or_default() += 1;
            } else if formula {
                *self.warnings.entry("formula_line").or_default() += 1;
            } else {
                ops.push(op(m, value.clone()));
            }
        }
        if ops.is_empty() {
            return;
        }
        if !a.can_edit {
            self.errs.add(row.line, format!("施策「{}」の数値を入力する権限がありません", a.code));
            return;
        }
        self.ops.entry(a.id).or_default().extend(ops);
    }

    // --- 金額 ---

    async fn plan_amounts(&mut self, tx: &mut Tx, rows: &[csvio::Row]) -> AppResult<()> {
        let subjects = code_index(tx, "SELECT code, id FROM subjects").await?;
        // 閲覧制限のある科目は FP&A のみが入力できる（docs/plan.md「2.17」）
        let restricted: HashSet<String> = if self.u.is_admin() {
            HashSet::new()
        } else {
            code_index(tx, "SELECT code, id FROM subjects WHERE is_restricted").await?.into_keys().collect()
        };
        let line_rows: Vec<(i64, i64, i64, String, bool)> =
            tx.exec("SELECT id, activity_id, subject_id, name, formula_enabled FROM activity_lines", ()).await?;
        let lines: HashMap<(i64, i64, String), (i64, bool)> =
            line_rows.into_iter().map(|(id, aid, sid, name, formula)| ((aid, sid, name), (id, formula))).collect();

        // 今の値（実績の月は実績、それ以外は計画値）
        let value_rows: Vec<(i64, i64, i64, String, String)> = tx
            .exec(
                "SELECT activity_id, subject_id, COALESCE(line_id, 0), DATE_FORMAT(target_month, '%Y-%m'), amount
                 FROM scenario_amounts WHERE scenario_id = ?",
                (self.s.id,),
            )
            .await?;
        let mut current: HashMap<(i64, i64, i64, String), String> = HashMap::new();
        for (aid, sid, lid, month, amount) in value_rows {
            let key = (aid, sid, lid, month);
            let v = match current.get(&key) {
                Some(cur) => rat_string(
                    &(formula::parse_rat(cur).unwrap_or_default() + formula::parse_rat(&amount).unwrap_or_default()),
                ),
                None => amount,
            };
            current.insert(key, v);
        }

        let mut seen: HashMap<(i64, i64, i64), usize> = HashMap::new();
        for row in rows {
            let a = self.activity_of(tx, row).await?;
            let subject_code = row.get("subject_code");
            let subject_id = subjects.get(subject_code).copied();
            if subject_id.is_none() {
                self.errs.add(row.line, format!("科目コード「{subject_code}」が見つかりません"));
            } else if restricted.contains(subject_code) {
                self.errs.add(row.line, format!("科目「{subject_code}」は閲覧制限があるため、FP&A のみが入力できます"));
                continue;
            }
            let mut line = (0i64, false);
            let line_name = row.get("line_name");
            if let (Some(a), Some(sid), false) = (&a, subject_id, line_name.is_empty()) {
                match lines.get(&(a.id, sid, line_name.to_string())) {
                    Some(&l) => line = l,
                    None => {
                        self.errs.add(
                            row.line,
                            format!(
                                "施策「{}」の科目「{subject_code}」に内訳「{line_name}」がありません。内訳は施策の画面で作ってください",
                                a.code
                            ),
                        );
                        continue;
                    }
                }
            }
            let cells = self.read_cells(row, parse_amount_cell);
            let (Some(a), Some(sid)) = (a, subject_id) else { continue };
            let key = (a.id, sid, line.0);
            if let Some(first) = seen.get(&key) {
                self.errs.add(row.line, format!("{first} 行目と同じ施策・科目・内訳の行です"));
                continue;
            }
            seen.insert(key, row.line);
            let line_id = (line.0 != 0).then_some(line.0);
            let aid = a.id;
            self.collect(
                row,
                &a,
                &cells,
                |m| current.get(&(aid, sid, line.0, m.to_string())).cloned(),
                line.1,
                |m, value| CellOp { subject_id: sid, line_id, driver_id: 0, month: m.to_string(), value },
            );
        }
        Ok(())
    }

    /// 施策の金額を書き込む（画面の直接入力と同じく、source は manual、仮の値の印は外す）。
    async fn apply_amounts(&mut self, tx: &mut Tx, rec: &Recorder, activity_id: i64) -> AppResult<()> {
        let existing = calc::load_facts(tx, self.s.id, activity_id).await?;
        for op in self.ops[&activity_id].clone() {
            let key = Key { subject_id: op.subject_id, line_id: op.line_id.unwrap_or(0), month: op.month.clone() };
            let before = existing.get(&key);
            let Some(value) = op.value else {
                if let Some(before) = before {
                    tx.exec_drop("DELETE FROM budget_facts WHERE id = ?", (before.id,)).await?;
                    rec.delete(tx, "budget_facts", before.id, before).await?;
                    self.result.deleted += 1;
                }
                continue;
            };
            let mut after = Fact {
                id: 0,
                scenario_id: self.s.id,
                activity_id,
                subject_id: op.subject_id,
                line_id: op.line_id,
                target_month: op.month.clone(),
                amount: value,
                source: "manual".into(),
                is_provisional: false,
                provisional_reason: String::new(),
            };
            match before {
                Some(before) => {
                    after.id = before.id;
                    calc::update_fact(tx, &after).await?;
                    rec.update(tx, "budget_facts", before.id, before, &after).await?;
                    self.result.updated += 1;
                }
                None => {
                    after.id = calc::insert_fact(tx, &after).await?;
                    rec.insert(tx, "budget_facts", after.id, &after).await?;
                    self.result.inserted += 1;
                }
            }
        }
        Ok(())
    }

    // --- ドライバー値 ---

    async fn plan_driver_values(&mut self, tx: &mut Tx, rows: &[csvio::Row]) -> AppResult<()> {
        let driver_rows: Vec<(i64, i64, String)> =
            tx.exec("SELECT id, activity_id, code FROM activity_drivers", ()).await?;
        let drivers: HashMap<(i64, String), i64> =
            driver_rows.into_iter().map(|(id, aid, code)| ((aid, code), id)).collect();
        let value_rows: Vec<(i64, String, String)> = tx
            .exec(
                "SELECT activity_driver_id, DATE_FORMAT(target_month, '%Y-%m'), value FROM driver_values WHERE scenario_id = ?",
                (self.s.id,),
            )
            .await?;
        let current: HashMap<(i64, String), String> = value_rows.into_iter().map(|(d, m, v)| ((d, m), v)).collect();

        let mut seen: HashMap<i64, usize> = HashMap::new();
        for row in rows {
            let a = self.activity_of(tx, row).await?;
            let code = row.get("driver_code");
            let mut driver_id = 0;
            if let Some(a) = &a {
                driver_id = drivers.get(&(a.id, code.to_string())).copied().unwrap_or(0);
                if driver_id == 0 {
                    self.errs.add(
                        row.line,
                        format!(
                            "施策「{}」にドライバー「{code}」がありません。ドライバーは施策の画面で作ってください",
                            a.code
                        ),
                    );
                    continue;
                }
            }
            let cells = self.read_cells(row, parse_driver_cell);
            let Some(a) = a else { continue };
            if let Some(first) = seen.get(&driver_id) {
                self.errs.add(row.line, format!("{first} 行目と同じ施策・ドライバーの行です"));
                continue;
            }
            seen.insert(driver_id, row.line);
            self.collect(
                row,
                &a,
                &cells,
                |m| current.get(&(driver_id, m.to_string())).cloned(),
                false,
                |m, value| CellOp { subject_id: 0, line_id: None, driver_id, month: m.to_string(), value },
            );
        }
        Ok(())
    }

    /// 施策のドライバー値を書き込み、計算式の金額を計算し直す。仮の値の印は外す。
    async fn apply_driver_values(&mut self, tx: &mut Tx, rec: &Recorder, activity_id: i64) -> AppResult<()> {
        for op in self.ops[&activity_id].clone() {
            let row: Option<(i64, String, bool, String)> = tx
                .exec_first(
                    "SELECT id, value, is_provisional, COALESCE(provisional_reason, '')
                     FROM driver_values WHERE activity_driver_id = ? AND scenario_id = ? AND target_month = ? FOR UPDATE",
                    p![op.driver_id, self.s.id, format!("{}-01", op.month)],
                )
                .await?;
            let mut before = DriverValueRecord {
                scenario_id: self.s.id,
                activity_driver_id: op.driver_id,
                target_month: op.month.clone(),
                value: "0".into(),
                ..Default::default()
            };
            if let Some((id, v, p, r)) = &row {
                before.id = *id;
                before.value = trim_decimal(v).0;
                before.is_provisional = *p;
                before.provisional_reason = r.clone();
            }
            let Some(value) = op.value else {
                if row.is_some() {
                    tx.exec_drop("DELETE FROM driver_values WHERE id = ?", (before.id,)).await?;
                    rec.delete(tx, "driver_values", before.id, &before).await?;
                    self.result.deleted += 1;
                }
                continue;
            };
            let mut after = DriverValueRecord {
                id: 0,
                scenario_id: self.s.id,
                activity_driver_id: op.driver_id,
                target_month: op.month.clone(),
                value: trim_decimal(&value).0,
                is_provisional: false,
                provisional_reason: String::new(),
            };
            if row.is_some() {
                after.id = before.id;
                tx.exec_drop(
                    "UPDATE driver_values SET value = ?, is_provisional = FALSE, provisional_reason = NULL WHERE id = ?",
                    (value, before.id),
                )
                .await?;
                rec.update(tx, "driver_values", before.id, &before, &after).await?;
                self.result.updated += 1;
                continue;
            }
            tx.exec_drop(
                "INSERT INTO driver_values (activity_driver_id, scenario_id, target_month, value, is_provisional) VALUES (?, ?, ?, ?, FALSE)",
                p![op.driver_id, self.s.id, format!("{}-01", op.month), value],
            )
            .await?;
            after.id = db::last_id(tx);
            rec.insert(tx, "driver_values", after.id, &after).await?;
            self.result.inserted += 1;
        }
        calc::recalculate(tx, rec, self.s.id, activity_id).await
    }
}

fn parse_amount_cell(v: &mut Validator, field: &str, raw: &str) -> String {
    parse_amount(v, field, raw)
}

fn parse_driver_cell(v: &mut Validator, field: &str, raw: &str) -> String {
    parse_driver_value(v, field, raw)
}
