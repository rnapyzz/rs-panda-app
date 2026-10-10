//! ロック済みのシナリオの実績と、今の実績の食い違い（docs/plan.md「2.14」）と、実績を最新にする API。
//! ロック済みのシナリオはロック時の実績（scenario_actuals）を固定しているので、後から会計側の修正を取り込むとずれる。

use std::collections::{BTreeMap, HashMap, HashSet};

use axum::extract::State;
use axum::response::Response;
use mysql_async::prelude::*;
use num_bigint::BigInt;
use serde::{Deserialize, Serialize};

use crate::auth::AdminUser;
use crate::db::{self, Params, Value};
use crate::error::{conflict, not_found, validation1};
use crate::http::{self, Id, Json};
use crate::{AppResult, AppState, audit, calc};

/// 1か月の食い違い。差は「今の実績 − シナリオに保存した実績」（未割当を含む）。
/// changed は施策 × 科目で金額が違う件数（施策の間の付け替えだけなら、収益・費用の差は 0 でも changed は 1 以上）。
#[derive(Clone, Debug, Serialize)]
pub struct DriftMonth {
    pub month: String,
    pub revenue: String,
    pub expense: String,
    pub changed: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ScenarioDrift {
    pub scenario_id: i64,
    pub name: String,
    pub fiscal_year: i32,
    pub actual_through: String,
    pub months: Vec<DriftMonth>,
}

/// ロック済みのシナリオごとの食い違いを返す。scenario_id（0 はすべて）を指定するとそのシナリオだけ、
/// months を指定するとその月だけを比べる。食い違いのないシナリオは返さない。
pub async fn load_drift<Q: Queryable>(q: &mut Q, scenario_id: i64, months: &[String]) -> AppResult<Vec<ScenarioDrift>> {
    let pairs = drift_candidates(q, scenario_id, months).await?;
    if pairs.is_empty() {
        return Ok(Vec::new());
    }
    // 食い違う候補（シナリオ × 月）だけを、施策 × 科目の単位で比べる
    let cond = vec!["(?, ?)"; pairs.len()].join(",");
    let mut args: Vec<Value> = Vec::new();
    for (id, m) in &pairs {
        args.push((*id).into());
        args.push(format!("{m}-01").into());
    }
    let args: Vec<Value> = args.iter().cloned().chain(args.iter().cloned()).collect();
    let rows: Vec<(i64, String, i32, String, String, String, String)> = q
        .exec(
            format!(
                "SELECT x.sid, s.name, s.fiscal_year, DATE_FORMAT(s.actual_through, '%Y-%m'), x.m, sub.category, CAST(SUM(x.amt) AS CHAR)
                 FROM (
                     SELECT sa.scenario_id AS sid, DATE_FORMAT(sa.target_month, '%Y-%m') AS m, sa.activity_key AS akey, sa.subject_id AS subject_id, -sa.amount AS amt
                     FROM scenario_actuals sa JOIN scenarios s ON s.id = sa.scenario_id
                     WHERE s.is_locked AND (s.id, sa.target_month) IN ({cond})
                     UNION ALL
                     SELECT s.id, DATE_FORMAT(af.target_month, '%Y-%m'), af.activity_key, af.subject_id, af.amount
                     FROM scenarios s
                     JOIN actual_facts af ON af.target_month BETWEEN MAKEDATE(s.fiscal_year, 1) + INTERVAL 3 MONTH AND s.actual_through
                     WHERE s.is_locked AND s.actual_through IS NOT NULL AND (s.id, af.target_month) IN ({cond})
                 ) x
                 JOIN scenarios s ON s.id = x.sid
                 JOIN subjects sub ON sub.id = x.subject_id
                 GROUP BY x.sid, s.name, s.fiscal_year, s.actual_through, x.m, x.akey, x.subject_id, sub.category
                 HAVING SUM(x.amt) <> 0
                 ORDER BY x.sid, x.m"
            ),
            Params::Positional(args),
        )
        .await?;

    struct MonthSum {
        revenue: BigInt,
        expense: BigInt,
        changed: i64,
    }
    let mut order: Vec<i64> = Vec::new();
    let mut by_scenario: HashMap<i64, ScenarioDrift> = HashMap::new();
    let mut sums: HashMap<i64, BTreeMap<String, MonthSum>> = HashMap::new();
    for (id, name, fiscal_year, actual_through, month, category, amount) in rows {
        by_scenario.entry(id).or_insert_with(|| {
            order.push(id);
            ScenarioDrift { scenario_id: id, name, fiscal_year, actual_through, months: Vec::new() }
        });
        let ms = sums.entry(id).or_default().entry(month).or_insert_with(|| MonthSum {
            revenue: BigInt::default(),
            expense: BigInt::default(),
            changed: 0,
        });
        let v: BigInt = amount.parse().unwrap_or_default();
        if category == "revenue" {
            ms.revenue += v;
        } else {
            ms.expense += v;
        }
        ms.changed += 1;
    }
    Ok(order
        .into_iter()
        .map(|id| {
            let mut d = by_scenario.remove(&id).unwrap();
            for (m, s) in sums.remove(&id).unwrap_or_default() {
                d.months.push(DriftMonth {
                    month: m,
                    revenue: s.revenue.to_string(),
                    expense: s.expense.to_string(),
                    changed: s.changed,
                });
            }
            d
        })
        .collect())
}

/// 年度のロック済みのシナリオのうち、実績が今の実績と食い違うものの名前を返す（シナリオの切り替えの注意に使う）。
pub async fn drifted_scenarios<Q: Queryable>(q: &mut Q, fiscal_year: i32) -> AppResult<Vec<String>> {
    Ok(load_drift(q, 0, &[]).await?.into_iter().filter(|d| d.fiscal_year == fiscal_year).map(|d| d.name).collect())
}

/// GET /api/scenarios/actual-drift。FP&A のみ。
pub async fn list_drift(State(st): State<AppState>, _: AdminUser) -> AppResult<Response> {
    let mut conn = st.db.get_conn().await?;
    Ok(http::list(&load_drift(&mut conn, 0, &[]).await?))
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct RefreshRequest {
    reason: String,
    dry_run: bool,
}

#[derive(Serialize)]
struct RefreshResult {
    dry_run: bool,
    drift: ScenarioDrift,
    /// 入れ替えた後に保存した実績の件数
    saved: u64,
}

/// 実績を入れ替えたことの監査ログ（行ごとの監査ログは actual_facts に残っている）。
#[derive(Serialize)]
struct RefreshRecord<'a> {
    id: i64,
    name: &'a str,
    refreshed_actuals: u64,
    actual_drift: &'a [DriftMonth],
}

/// POST /api/scenarios/{id}/refresh-actuals。FP&A のみ。
/// ロック済みのシナリオに保存した決算確定月以前の実績（未割当を含む）を、今の実績に入れ替える。ロックは外れない。
/// dry_run なら保存せず、月ごとの差だけを返す。
pub async fn refresh_actuals(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    Id(id): Id,
    Json(req): Json<RefreshRequest>,
) -> AppResult<Response> {
    if !req.dry_run && req.reason.trim().is_empty() {
        return Err(validation1("reason", "実績を最新にするには変更理由の入力が必要です"));
    }
    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, Some(id), &req.reason).await?;
    let row: Option<(String, i32, bool, Option<String>)> = tx
        .exec_first(
            "SELECT name, fiscal_year, is_locked, DATE_FORMAT(actual_through, '%Y-%m') FROM scenarios WHERE id = ? FOR UPDATE",
            (id,),
        )
        .await?;
    let Some((name, fy, locked, through)) = row else {
        return Err(not_found("シナリオが見つかりません"));
    };
    let Some(through) = through.filter(|_| locked) else {
        return Err(conflict("実績を最新にできるのは、決算確定月のあるロック済みのシナリオだけです"));
    };
    let Some(drift) = load_drift(&mut tx, id, &[]).await?.into_iter().next() else {
        return Err(conflict(format!("「{name}」の実績は最新の実績と同じです")));
    };
    let mut result = RefreshResult { dry_run: req.dry_run, drift, saved: 0 };
    if req.dry_run {
        tx.rollback().await?;
        return Ok(http::ok(&result));
    }
    tx.exec_drop("DELETE FROM scenario_actuals WHERE scenario_id = ?", (id,)).await?;
    tx.exec_drop(
        "INSERT INTO scenario_actuals (scenario_id, activity_id, subject_id, target_month, amount)
         SELECT ?, activity_id, subject_id, target_month, amount FROM actual_facts
         WHERE target_month BETWEEN ? AND ?",
        (id, format!("{}-01", calc::fiscal_months(fy)[0]), format!("{through}-01")),
    )
    .await?;
    result.saved = tx.affected_rows();
    // 入れ替えた件数と月ごとの差を記録する
    let after = audit::to_json(&RefreshRecord {
        id,
        name: &name,
        refreshed_actuals: result.saved,
        actual_drift: &result.drift.months,
    })?;
    rec.record(&mut tx, "scenarios", id, "update", None, Some(after)).await?;
    tx.commit().await?;
    Ok(http::ok(&result))
}

/// 行数・合計・行の内容のチェックサム。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Fingerprint {
    n: i64,
    sum: String,
    checksum: String,
}

/// ロック済みのシナリオの（シナリオ, 月）ごとに、保存した実績と今の実績の要約（行数・合計・行の内容のチェックサム）を比べ、
/// 違うものを返す（I-15）。今の実績はシナリオによらないので月ごとに1回だけ集計し、全シナリオ × 全行の突き合わせを避ける。
async fn drift_candidates<Q: Queryable>(
    q: &mut Q,
    scenario_id: i64,
    months: &[String],
) -> AppResult<Vec<(i64, String)>> {
    let mut filter = "s.is_locked AND s.actual_through IS NOT NULL".to_string();
    let mut args: Vec<Value> = Vec::new();
    if scenario_id != 0 {
        filter.push_str(" AND s.id = ?");
        args.push(scenario_id.into());
    }
    // 対象のシナリオと、その実績の月
    let spans: Vec<(i64, String, String)> = q
        .exec(
            format!(
                "SELECT s.id, DATE_FORMAT(MAKEDATE(s.fiscal_year, 1) + INTERVAL 3 MONTH, '%Y-%m'), DATE_FORMAT(s.actual_through, '%Y-%m')
                 FROM scenarios s WHERE {filter}"
            ),
            Params::Positional(args.clone()),
        )
        .await?;
    if spans.is_empty() {
        return Ok(Vec::new());
    }
    let month_filter: HashSet<&String> = months.iter().collect();

    // 保存した実績の要約（シナリオ × 月）
    let rows: Vec<(i64, String, i64, String, String)> = q
        .exec(
            format!(
                "SELECT sa.scenario_id, DATE_FORMAT(sa.target_month, '%Y-%m'), COUNT(*), CAST(SUM(sa.amount) AS CHAR),
                        CAST(SUM(CRC32(CONCAT_WS(',', sa.activity_key, sa.subject_id, sa.amount))) AS CHAR)
                 FROM scenario_actuals sa JOIN scenarios s ON s.id = sa.scenario_id
                 WHERE {filter}
                 GROUP BY sa.scenario_id, sa.target_month"
            ),
            Params::Positional(args),
        )
        .await?;
    let snap: HashMap<(i64, String), Fingerprint> =
        rows.into_iter().map(|(id, m, n, sum, checksum)| ((id, m), Fingerprint { n, sum, checksum })).collect();

    // 今の実績の要約（月）
    let rows: Vec<(String, i64, String, String)> = q
        .exec(
            "SELECT DATE_FORMAT(target_month, '%Y-%m'), COUNT(*), CAST(SUM(amount) AS CHAR),
                    CAST(SUM(CRC32(CONCAT_WS(',', activity_key, subject_id, amount))) AS CHAR)
             FROM actual_facts GROUP BY target_month",
            (),
        )
        .await?;
    let cur: HashMap<String, Fingerprint> =
        rows.into_iter().map(|(m, n, sum, checksum)| (m, Fingerprint { n, sum, checksum })).collect();

    let empty = Fingerprint::default();
    let mut out = Vec::new();
    for (id, from, to) in spans {
        let mut m = from;
        while m <= to {
            if month_filter.is_empty() || month_filter.contains(&m) {
                let key = (id, m.clone());
                if snap.get(&key).unwrap_or(&empty) != cur.get(&m).unwrap_or(&empty) {
                    out.push(key);
                }
            }
            m = next_month(&m);
        }
    }
    Ok(out)
}

/// "2026-12" → "2027-01"。
pub fn next_month(ym: &str) -> String {
    let parse = || -> Option<(i32, u32)> {
        let (y, m) = ym.split_once('-')?;
        Some((y.parse().ok()?, m.parse().ok()?))
    };
    match parse() {
        Some((y, m)) if (1..=12).contains(&m) && ym.len() == 7 => {
            if m == 12 {
                format!("{:04}-01", y + 1)
            } else {
                format!("{y:04}-{:02}", m + 1)
            }
        }
        _ => "9999-12".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_months() {
        assert_eq!(next_month("2026-12"), "2027-01");
        assert_eq!(next_month("2026-04"), "2026-05");
        assert_eq!(next_month("bad"), "9999-12");
    }
}
