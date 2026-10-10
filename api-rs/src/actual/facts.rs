//! 実績（actual_facts）は、明細（actual_entries）を施策 × 科目 × 月で合計したもの。施策が NULL の行は未割当。
//! 明細を変えたら sync_facts で合計を合わせる。合計は差分だけを更新し、行ごとの監査ログを残す。

use std::collections::{BTreeSet, HashMap, HashSet};

use mysql_async::prelude::*;
use serde::Serialize;

use super::{FactCounts, amount_fits, month_args, parse_amount, placeholders};
use crate::AppResult;
use crate::audit::{Change, Recorder, to_json};
use crate::db::{Params, Tx, Value};
use crate::error::validation1;

/// 合計（actual_facts）をまとめて書く件数。
const FACT_BATCH: usize = 1000;

/// 実績の行のキー（月, 施策。0 は未割当, 科目）。並べると月・施策・科目の順になる。
type FactKey = (String, i64, i64);

/// 実績（actual_facts）の1行。監査ログにも使う。
#[derive(Clone, Debug, Serialize)]
struct ActualFact {
    id: i64,
    activity_id: Option<i64>,
    subject_id: i64,
    target_month: String,
    amount: String,
}

/// 「(?, ?), (?, ?)」の形の VALUES を作る。
fn rows_of(n: usize, cols: usize) -> String {
    vec![format!("({})", placeholders(cols)); n].join(",")
}

/// 指定月の実績を明細の合計に合わせる。
pub async fn sync_facts(tx: &mut Tx, rec: &Recorder, months: &[String], c: &mut FactCounts) -> AppResult<()> {
    if months.is_empty() {
        return Ok(());
    }
    let ph = placeholders(months.len());
    let rows: Vec<(i64, Option<i64>, i64, String, String)> = tx
        .exec(
            format!(
                "SELECT id, activity_id, subject_id, DATE_FORMAT(target_month, '%Y-%m'), CAST(amount AS CHAR)
                 FROM actual_facts WHERE target_month IN ({ph}) FOR UPDATE"
            ),
            Params::Positional(month_args(months)),
        )
        .await?;
    let existing: HashMap<FactKey, ActualFact> = rows
        .into_iter()
        .map(|(id, activity_id, subject_id, target_month, amount)| {
            (
                (target_month.clone(), activity_id.unwrap_or(0), subject_id),
                ActualFact { id, activity_id, subject_id, target_month, amount },
            )
        })
        .collect();

    let rows: Vec<(i64, i64, String, String)> = tx
        .exec(
            format!(
                "SELECT COALESCE(activity_id, 0), subject_id, DATE_FORMAT(target_month, '%Y-%m'), CAST(SUM(amount) AS CHAR)
                 FROM actual_entries WHERE target_month IN ({ph})
                 GROUP BY activity_id, subject_id, target_month"
            ),
            Params::Positional(month_args(months)),
        )
        .await?;
    let mut want: HashMap<FactKey, String> = HashMap::new();
    for (activity, subject, month, amount) in rows {
        if !parse_amount(&amount).is_some_and(|v| amount_fits(&v)) {
            return Err(validation1("file", format!("{month} の合計の金額が大きすぎます")));
        }
        want.insert((month, activity, subject), amount);
    }
    c.facts += want.len();

    let keys: BTreeSet<&FactKey> = existing.keys().chain(want.keys()).collect();

    // 変更をまとめて書く（件数の多い取込で、1件ずつ SQL を送らないため。I-15）
    let mut deletes: Vec<&ActualFact> = Vec::new();
    let mut inserts: Vec<ActualFact> = Vec::new();
    let mut updates: Vec<(&ActualFact, ActualFact)> = Vec::new();
    for k in keys {
        match (existing.get(k), want.get(k)) {
            (Some(before), None) => deletes.push(before),
            (None, Some(amount)) => inserts.push(ActualFact {
                id: 0,
                activity_id: (k.1 != 0).then_some(k.1),
                subject_id: k.2,
                target_month: k.0.clone(),
                amount: amount.clone(),
            }),
            (Some(before), Some(amount)) if &before.amount == amount => c.unchanged += 1,
            (Some(before), Some(amount)) => {
                updates.push((before, ActualFact { amount: amount.clone(), ..before.clone() }));
            }
            (None, None) => {}
        }
    }
    let mut changes: Vec<Change> = Vec::new();
    for batch in deletes.chunks(FACT_BATCH) {
        let mut args: Vec<Value> = Vec::with_capacity(batch.len());
        for f in batch {
            args.push(f.id.into());
            changes.push(Change {
                table: "actual_facts",
                id: f.id,
                action: "delete",
                before: Some(to_json(f)?),
                after: None,
            });
        }
        tx.exec_drop(
            format!("DELETE FROM actual_facts WHERE id IN ({})", placeholders(batch.len())),
            Params::Positional(args),
        )
        .await?;
    }
    for batch in updates.chunks(FACT_BATCH) {
        // 主キーが重なる行を入れ、金額だけを更新する（1件ずつの UPDATE の代わり）
        let mut args: Vec<Value> = Vec::with_capacity(batch.len() * 5);
        for (before, after) in batch {
            args.extend([
                after.id.into(),
                after.activity_id.into(),
                after.subject_id.into(),
                format!("{}-01", after.target_month).into(),
                after.amount.clone().into(),
            ]);
            changes.push(Change {
                table: "actual_facts",
                id: after.id,
                action: "update",
                before: Some(to_json(before)?),
                after: Some(to_json(after)?),
            });
        }
        tx.exec_drop(
            format!(
                "INSERT INTO actual_facts (id, activity_id, subject_id, target_month, amount) VALUES {} AS new ON DUPLICATE KEY UPDATE amount = new.amount",
                rows_of(batch.len(), 5)
            ),
            Params::Positional(args),
        )
        .await?;
    }
    for batch in inserts.chunks(FACT_BATCH) {
        let mut args: Vec<Value> = Vec::with_capacity(batch.len() * 4);
        for f in batch {
            args.extend([
                f.activity_id.into(),
                f.subject_id.into(),
                format!("{}-01", f.target_month).into(),
                f.amount.clone().into(),
            ]);
        }
        tx.exec_drop(
            format!(
                "INSERT INTO actual_facts (activity_id, subject_id, target_month, amount) VALUES {}",
                rows_of(batch.len(), 4)
            ),
            Params::Positional(args),
        )
        .await?;
    }
    if !inserts.is_empty() {
        // 追加した行の ID は、キー（施策・科目・月）で読み直す（監査ログに残すため）
        let rows: Vec<(i64, i64, i64, String)> = tx
            .exec(
                format!(
                    "SELECT id, activity_key, subject_id, DATE_FORMAT(target_month, '%Y-%m')
                     FROM actual_facts WHERE target_month IN ({ph})"
                ),
                Params::Positional(month_args(months)),
            )
            .await?;
        let ids: HashMap<FactKey, i64> = rows.into_iter().map(|(id, a, s, m)| ((m, a, s), id)).collect();
        for f in &mut inserts {
            f.id = ids.get(&(f.target_month.clone(), f.activity_id.unwrap_or(0), f.subject_id)).copied().unwrap_or(0);
            changes.push(Change {
                table: "actual_facts",
                id: f.id,
                action: "insert",
                before: None,
                after: Some(to_json(f)?),
            });
        }
    }
    c.deleted += deletes.len();
    c.inserted += inserts.len();
    c.updated += updates.len();
    rec.many(tx, &changes).await
}

/// 月ごとの実績の合計（会計システムとの突合用）。未割当を含む。
#[derive(Clone, Debug, Serialize)]
pub struct MonthTotals {
    pub month: String,
    pub revenue: String,
    pub expense: String,
    pub unallocated_revenue: String,
    pub unallocated_expense: String,
}

pub async fn monthly_totals(tx: &mut Tx, months: &[String]) -> AppResult<Vec<MonthTotals>> {
    let mut out = Vec::with_capacity(months.len());
    for m in months {
        let r: Option<(String, String, String, String)> = tx
            .exec_first(
                "SELECT CAST(COALESCE(SUM(CASE WHEN s.category = 'revenue' THEN f.amount END), 0) AS CHAR),
                        CAST(COALESCE(SUM(CASE WHEN s.category = 'expense' THEN f.amount END), 0) AS CHAR),
                        CAST(COALESCE(SUM(CASE WHEN s.category = 'revenue' AND f.activity_id IS NULL THEN f.amount END), 0) AS CHAR),
                        CAST(COALESCE(SUM(CASE WHEN s.category = 'expense' AND f.activity_id IS NULL THEN f.amount END), 0) AS CHAR)
                 FROM actual_facts f JOIN subjects s ON s.id = f.subject_id
                 WHERE f.target_month = ?",
                (format!("{m}-01"),),
            )
            .await?;
        let (revenue, expense, unallocated_revenue, unallocated_expense) = r.unwrap_or_default();
        out.push(MonthTotals { month: m.clone(), revenue, expense, unallocated_revenue, unallocated_expense });
    }
    Ok(out)
}

/// 指定した月のうち、明細がある月を返す。
pub async fn entry_months(tx: &mut Tx, months: &[String]) -> AppResult<HashSet<String>> {
    if months.is_empty() {
        return Ok(HashSet::new());
    }
    let rows: Vec<String> = tx
        .exec(
            format!(
                "SELECT DISTINCT DATE_FORMAT(target_month, '%Y-%m') FROM actual_entries WHERE target_month IN ({})",
                placeholders(months.len())
            ),
            Params::Positional(month_args(months)),
        )
        .await?;
    Ok(rows.into_iter().collect())
}
