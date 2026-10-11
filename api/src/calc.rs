//! 内訳（activity_lines）の計算式とドライバー値（driver_values）から金額（budget_facts）を算出して保存する。
//!
//! 再計算するのは、計算式で反映する（formula_enabled）内訳だけ。算出した金額はその内訳の金額として保存する。
//! ロック済みのシナリオと実績シナリオは再計算しない（確定した値を変えないため）。
//! 計算に必要なドライバー値がそろわない月は金額を持たない（既存の値は削除する）。

use std::collections::{BTreeSet, HashMap, HashSet};

use mysql_async::prelude::*;
use serde::Serialize;

use crate::AppResult;
use crate::audit::Recorder;
use crate::db::{self, Tx};
use crate::error::validation1;
use crate::formula::{self, EvalError, Expr, Rat};

/// budget_facts の1行。監査ログにもこの形で記録する。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Fact {
    pub id: i64,
    pub scenario_id: i64,
    pub activity_id: i64,
    pub subject_id: i64,
    /// 内訳。None は科目への直接入力
    pub line_id: Option<i64>,
    /// YYYY-MM
    pub target_month: String,
    pub amount: String,
    pub source: String,
    pub is_provisional: bool,
    pub provisional_reason: String,
}

/// シナリオ×施策の中で金額を特定するキー（科目・内訳・月）。line_id が 0 なら科目への直接入力。
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Key {
    pub subject_id: i64,
    pub line_id: i64,
    pub month: String,
}

impl Fact {
    /// 金額のキーを返す。
    pub fn key(&self) -> Key {
        Key { subject_id: self.subject_id, line_id: self.line_id.unwrap_or(0), month: self.target_month.clone() }
    }
}

/// 会計年度（4月開始）の12か月を YYYY-MM で返す。
pub fn fiscal_months(fiscal_year: i32) -> Vec<String> {
    (0..12)
        .map(|i| {
            let (mut y, mut m) = (fiscal_year, 4 + i);
            if m > 12 {
                m -= 12;
                y += 1;
            }
            format!("{y:04}-{m:02}")
        })
        .collect()
}

/// 年度の月のうち、決算確定月（YYYY-MM、空なら未設定）より後の月を返す。
pub fn plan_months(fiscal_year: i32, actual_through: &str) -> Vec<String> {
    let months = fiscal_months(fiscal_year);
    if actual_through.is_empty() {
        return months;
    }
    months.into_iter().filter(|m| m.as_str() > actual_through).collect()
}

/// シナリオの計算式で反映する内訳を持つすべての施策の金額を再計算する。決算確定月を変更したときに呼ぶ。
pub async fn recalculate_scenario(tx: &mut Tx, rec: &Recorder, scenario_id: i64) -> AppResult<()> {
    let ids: Vec<i64> = tx
        .exec("SELECT DISTINCT activity_id FROM activity_lines WHERE formula_enabled ORDER BY activity_id", ())
        .await?;
    for id in ids {
        recalculate(tx, rec, scenario_id, id).await?;
    }
    Ok(())
}

/// 施策の金額を、再計算の対象となるすべてのシナリオで再計算する。計算式・確度・算出方式を変更したときに呼ぶ。
pub async fn recalculate_activity(tx: &mut Tx, rec: &Recorder, activity_id: i64) -> AppResult<()> {
    let ids: Vec<i64> = tx.exec("SELECT id FROM scenarios WHERE NOT is_locked ORDER BY id FOR SHARE", ()).await?;
    for id in ids {
        recalculate(tx, rec, id, activity_id).await?;
    }
    Ok(())
}

struct DriverValue {
    value: Rat,
    provisional: bool,
}

struct ComputedLine {
    id: i64,
    subject_id: i64,
    name: String,
    expr: Expr,
}

/// シナリオ×施策の、計算式で反映する内訳の金額を再計算し、差分を budget_facts に反映して監査ログに記録する。
pub async fn recalculate(tx: &mut Tx, rec: &Recorder, scenario_id: i64, activity_id: i64) -> AppResult<()> {
    let row: Option<(i32, bool, Option<String>)> = tx
        .exec_first(
            "SELECT fiscal_year, is_locked, DATE_FORMAT(actual_through, '%Y-%m') FROM scenarios WHERE id = ?",
            (scenario_id,),
        )
        .await?;
    let Some((fiscal_year, locked, through)) = row else {
        return Err(anyhow::anyhow!("scenario {scenario_id} not found").into());
    };
    if locked {
        return Ok(());
    }
    // 計算するのは計画値の月（決算確定月より後の月）だけ。実績の月の金額は変えない
    let months = plan_months(fiscal_year, through.as_deref().unwrap_or(""));

    let lines = load_computed_lines(tx, activity_id).await?;
    if lines.is_empty() {
        return Ok(());
    }
    let values = load_driver_values(tx, scenario_id, activity_id).await?;

    let mut want: HashMap<Key, Fact> = HashMap::new();
    let mut line_ids: HashSet<i64> = HashSet::new();
    for l in &lines {
        line_ids.insert(l.id);
        let idents = l.expr.idents();
        for month in &months {
            let mut vars = HashMap::new();
            let mut provisional = Vec::new();
            let complete = idents.iter().all(|id| match values.get(month).and_then(|m| m.get(id)) {
                Some(v) => {
                    vars.insert(id.clone(), v.value.clone());
                    if v.provisional {
                        provisional.push(id.clone());
                    }
                    true
                }
                None => false,
            });
            if !complete {
                continue;
            }
            let result = match l.expr.eval(&vars) {
                Ok(r) => r,
                Err(EvalError::DivisionByZero) => {
                    return Err(validation1(
                        "values",
                        format!("{month} の内訳「{}」の計算で 0 による割り算が発生します", l.name),
                    ));
                }
                Err(e) => return Err(anyhow::anyhow!(e).into()),
            };
            let mut f = Fact {
                scenario_id,
                activity_id,
                subject_id: l.subject_id,
                line_id: Some(l.id),
                target_month: month.clone(),
                amount: formula::round_half_up(&result).to_string(),
                source: "formula".into(),
                ..Default::default()
            };
            if !provisional.is_empty() {
                provisional.sort();
                f.is_provisional = true;
                f.provisional_reason = format!("仮の値のドライバーを含む: {}", provisional.join(", "));
            }
            want.insert(f.key(), f);
        }
    }

    let all = load_facts(tx, scenario_id, activity_id).await?;
    let existing: HashMap<Key, Fact> =
        all.into_iter().filter(|(k, _)| line_ids.contains(&k.line_id) && months.contains(&k.month)).collect();
    apply(tx, rec, &existing, want).await
}

/// existing を want に合わせて insert / update / delete する。
pub async fn apply(
    tx: &mut Tx,
    rec: &Recorder,
    existing: &HashMap<Key, Fact>,
    want: HashMap<Key, Fact>,
) -> AppResult<()> {
    // 監査ログの順序を安定させる（科目・内訳・月の順）
    let keys: BTreeSet<Key> = existing.keys().chain(want.keys()).cloned().collect();
    for k in keys {
        match (existing.get(&k), want.get(&k)) {
            (Some(before), None) => {
                tx.exec_drop("DELETE FROM budget_facts WHERE id = ?", (before.id,)).await?;
                rec.delete(tx, "budget_facts", before.id, before).await?;
            }
            (None, Some(after)) => {
                let mut after = after.clone();
                after.id = insert_fact(tx, &after).await?;
                rec.insert(tx, "budget_facts", after.id, &after).await?;
            }
            (Some(before), Some(after)) => {
                let mut after = after.clone();
                after.id = before.id;
                if *before == after {
                    continue;
                }
                update_fact(tx, &after).await?;
                rec.update(tx, "budget_facts", before.id, before, &after).await?;
            }
            (None, None) => {}
        }
    }
    Ok(())
}

/// budget_facts に1行追加する。
pub async fn insert_fact(tx: &mut Tx, f: &Fact) -> AppResult<i64> {
    tx.exec_drop(
        "INSERT INTO budget_facts (scenario_id, activity_id, subject_id, line_id, target_month, amount, source, is_provisional, provisional_reason)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        p![
            f.scenario_id,
            f.activity_id,
            f.subject_id,
            f.line_id,
            format!("{}-01", f.target_month),
            f.amount.clone(),
            f.source.clone(),
            f.is_provisional,
            db::null_str(&f.provisional_reason)
        ],
    )
    .await?;
    Ok(db::last_id(tx))
}

/// budget_facts の1行を更新する（科目・内訳・月は変えない）。
pub async fn update_fact(tx: &mut Tx, f: &Fact) -> AppResult<()> {
    tx.exec_drop(
        "UPDATE budget_facts SET amount = ?, source = ?, is_provisional = ?, provisional_reason = ? WHERE id = ?",
        p![f.amount.clone(), f.source.clone(), f.is_provisional, db::null_str(&f.provisional_reason), f.id],
    )
    .await?;
    Ok(())
}

type FactRow = (i64, i64, Option<i64>, String, String, String, bool, String);

/// シナリオ×施策の金額を（科目・内訳・月）ごとに行ロック付きで返す。
pub async fn load_facts(tx: &mut Tx, scenario_id: i64, activity_id: i64) -> AppResult<HashMap<Key, Fact>> {
    let rows: Vec<FactRow> = tx
        .exec(
            "SELECT id, subject_id, line_id, DATE_FORMAT(target_month, '%Y-%m'), amount, source, is_provisional, COALESCE(provisional_reason, '')
             FROM budget_facts WHERE scenario_id = ? AND activity_id = ? FOR UPDATE",
            (scenario_id, activity_id),
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(|(id, subject_id, line_id, target_month, amount, source, is_provisional, provisional_reason)| {
            let f = Fact {
                id,
                scenario_id,
                activity_id,
                subject_id,
                line_id,
                target_month,
                amount,
                source,
                is_provisional,
                provisional_reason,
            };
            (f.key(), f)
        })
        .collect())
}

/// 内訳の要約。
#[derive(Clone, Debug)]
pub struct LineInfo {
    pub subject_id: i64,
    pub name: String,
    /// 計算式で反映する内訳か（直接入力できない）
    pub computed: bool,
}

/// 施策の内訳を返す。
pub async fn lines(tx: &mut Tx, activity_id: i64) -> AppResult<HashMap<i64, LineInfo>> {
    let rows: Vec<(i64, i64, String, bool)> = tx
        .exec("SELECT id, subject_id, name, formula_enabled FROM activity_lines WHERE activity_id = ?", (activity_id,))
        .await?;
    Ok(rows.into_iter().map(|(id, subject_id, name, computed)| (id, LineInfo { subject_id, name, computed })).collect())
}

/// 計算式で反映する内訳を返す。
async fn load_computed_lines(tx: &mut Tx, activity_id: i64) -> AppResult<Vec<ComputedLine>> {
    let rows: Vec<(i64, i64, String, String)> = tx
        .exec(
            "SELECT id, subject_id, name, expression FROM activity_lines
             WHERE activity_id = ? AND formula_enabled AND expression IS NOT NULL ORDER BY id",
            (activity_id,),
        )
        .await?;
    rows.into_iter()
        .map(|(id, subject_id, name, src)| {
            let expr = Expr::parse(&src).map_err(|e| anyhow::anyhow!("activity {activity_id} line {id}: {e}"))?;
            Ok(ComputedLine { id, subject_id, name, expr })
        })
        .collect()
}

/// 月 → ドライバー code → 値 を返す。
async fn load_driver_values(
    tx: &mut Tx,
    scenario_id: i64,
    activity_id: i64,
) -> AppResult<HashMap<String, HashMap<String, DriverValue>>> {
    let rows: Vec<(String, String, String, bool)> = tx
        .exec(
            "SELECT d.code, DATE_FORMAT(v.target_month, '%Y-%m'), v.value, v.is_provisional
             FROM driver_values v JOIN activity_drivers d ON d.id = v.activity_driver_id
             WHERE v.scenario_id = ? AND d.activity_id = ?",
            (scenario_id, activity_id),
        )
        .await?;
    let mut out: HashMap<String, HashMap<String, DriverValue>> = HashMap::new();
    for (code, month, value, provisional) in rows {
        let value = formula::parse_decimal(&value).ok_or_else(|| anyhow::anyhow!("invalid driver value {value:?}"))?;
        out.entry(month).or_default().insert(code, DriverValue { value, provisional });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn months() {
        let m = fiscal_months(2026);
        assert_eq!((m[0].as_str(), m[11].as_str()), ("2026-04", "2027-03"));
        assert_eq!(plan_months(2026, "2026-09")[0], "2026-10");
        assert_eq!(plan_months(2026, "").len(), 12);
    }
}
