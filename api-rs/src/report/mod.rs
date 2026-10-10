//! 予実比較・リスク・報告資料の API。参照はログインユーザー全員。
//!
//! GET /api/reports/comparison は、複数のシナリオ（と実績）の金額を、
//! ユニット × 科目 × 月（またはユニット内の施策 × 科目 × 月）で集計して返す。
//! 階層（セグメント・組織）での集計や利益の計算は画面側で行う。

mod measure;
mod pack;
mod pack_sheets;
mod risk;

use std::collections::{BTreeMap, HashMap};

use axum::Router;
use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use mysql_async::prelude::*;
use mysql_async::{Conn, Params};
use num_bigint::BigInt;
use serde::Serialize;

use crate::auth::{CurrentUser, User};
use crate::db::Value;
use crate::error::{AppError, bad_request, validation1};
use crate::http::{self, Query};
use crate::{AppResult, AppState, calc, visibility};

pub use measure::MEASURES;

const MAX_SCENARIOS: usize = 4;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/reports/comparison", get(comparison))
        .route("/api/reports/risk", get(risk::risk))
        .route("/api/reports/pack.xlsx", get(pack::pack))
}

/// 比較する系列（シナリオ、または実績）。
#[derive(Clone, Debug, Serialize)]
struct Series {
    key: String,
    label: String,
    /// scenario / actual
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    scenario_id: Option<i64>,
    /// シナリオの決算確定月（YYYY-MM）。この月以前は実績
    #[serde(skip_serializing_if = "Option::is_none")]
    actual_through: Option<String>,
}

/// 1つの集計単位（ユニットまたは施策）× 科目 × 月の金額。values は系列の key → 金額（円、文字列）。
/// unit_id が null の行は未割当の実績（docs/plan.md「2.12」）。ユニットを指定しないときだけ返す。
#[derive(Clone, Debug, Serialize)]
struct Row {
    unit_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    activity_id: Option<i64>,
    subject_id: i64,
    month: String,
    values: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct ComparisonResponse {
    fiscal_year: i32,
    measure: String,
    months: Vec<String>,
    series: Vec<Series>,
    rows: Vec<Row>,
    /// 閲覧制限のある科目を除いた金額か（docs/plan.md「2.17」）
    restricted_hidden: bool,
}

/// 集計の行のキー（ユニット, 施策, 科目, 月）。ユニット・施策の 0 はなし（未割当・ユニット単位）。
/// 並べるとユニット・施策・科目・月の順になる。
type RowKey = (i64, i64, i64, String);

/// 系列ごとの金額。
type Amounts = HashMap<RowKey, BigInt>;

#[derive(Clone, Debug)]
struct ScenarioInfo {
    name: String,
    actual_through: Option<String>,
    fiscal_year: i32,
}

/// カンマ区切りの ID を読む（重なりは除き、順番は保つ）。
fn parse_ids(s: &str) -> Option<Vec<i64>> {
    let mut ids = Vec::new();
    for p in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        match p.parse::<i64>() {
            Ok(id) if id > 0 => {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
            _ => return None,
        }
    }
    Some(ids)
}

/// scenario_ids を読み、件数を確かめる。empty_msg は1つもないとき、too_many_msg は多すぎるときのメッセージ。
fn scenario_ids(q: &Query, empty_msg: &str, too_many: &str) -> Result<Vec<i64>, AppError> {
    let ids = parse_ids(q.get("scenario_ids"))
        .ok_or_else(|| bad_request("scenario_ids はシナリオ ID をカンマ区切りで指定してください"))?;
    if ids.is_empty() {
        return Err(validation1("scenario_ids", empty_msg));
    }
    if ids.len() > MAX_SCENARIOS {
        return Err(validation1("scenario_ids", format!("{too_many}{MAX_SCENARIOS}つまでです")));
    }
    Ok(ids)
}

/// measure を読む（既定は full）。
fn parse_measure(q: &Query) -> Result<String, AppError> {
    let m = match q.get("measure") {
        "" => "full",
        m => m,
    };
    if !MEASURES.contains(&m) {
        return Err(bad_request("measure は full / weighted / optimistic / pessimistic のいずれかを指定してください"));
    }
    Ok(m.to_string())
}

/// シナリオをまとめて読み、すべて同じ年度であることを確かめる。年度を返す。
async fn load_scenarios(conn: &mut Conn, ids: &[i64]) -> AppResult<(HashMap<i64, ScenarioInfo>, i32)> {
    let rows: Vec<(i64, String, Option<String>, i32)> = conn
        .exec(
            format!(
                "SELECT id, name, DATE_FORMAT(actual_through, '%Y-%m'), fiscal_year FROM scenarios WHERE id IN ({})",
                placeholders(ids.len())
            ),
            Params::Positional(ids.iter().map(|id| Value::from(*id)).collect()),
        )
        .await?;
    let infos: HashMap<i64, ScenarioInfo> = rows
        .into_iter()
        .map(|(id, name, actual_through, fiscal_year)| (id, ScenarioInfo { name, actual_through, fiscal_year }))
        .collect();
    if let Some(missing) = ids.iter().find(|id| !infos.contains_key(id)) {
        return Err(validation1("scenario_ids", format!("シナリオ {missing} が見つかりません")));
    }
    let fiscal_year = infos[&ids[0]].fiscal_year;
    if ids.iter().any(|id| infos[id].fiscal_year != fiscal_year) {
        return Err(validation1("scenario_ids", "同じ年度のシナリオを選んでください"));
    }
    Ok((infos, fiscal_year))
}

fn placeholders(n: usize) -> String {
    vec!["?"; n].join(",")
}

fn id_args(ids: &[i64]) -> Vec<Value> {
    ids.iter().map(|id| Value::from(*id)).collect()
}

fn parse_big(s: &str) -> AppResult<BigInt> {
    s.parse().map_err(|_| anyhow::anyhow!("invalid amount {s:?}").into())
}

/// GET /api/reports/comparison。
///
/// クエリパラメーター:
///   - scenario_ids: 比較するシナリオ ID（カンマ区切り、最大4つ）。先頭が差異の基準
///   - include_actual=true: 実績データ（取込済みの月）を系列に加える
///   - unit_id: 指定すると、そのユニットの施策ごとに集計する（指定しなければユニットごと）
///   - measure: full（満額、既定）/ weighted / optimistic / pessimistic
///
/// シナリオの金額は、決算確定月以前の月は実績、それより後の月は計画値。すべてのシナリオは同じ年度でなければならない。
async fn comparison(State(st): State<AppState>, CurrentUser(u): CurrentUser, q: Query) -> AppResult<Response> {
    let ids = scenario_ids(&q, "比較するシナリオを選んでください", "比較できるシナリオは")?;
    let include_actual = q.get("include_actual") == "true";
    let measure = parse_measure(&q)?;
    let unit_id = match q.get("unit_id") {
        "" => None,
        s => Some(s.parse::<i64>().map_err(|_| bad_request("unit_id は数値で指定してください"))?),
    };

    let mut conn = st.db.get_conn().await?;
    let (infos, fiscal_year) = load_scenarios(&mut conn, &ids).await?;
    let months = calc::fiscal_months(fiscal_year);
    let restricted_hidden = visibility::hidden(&mut conn, &u).await?;
    let mut series: Vec<Series> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| Series {
            key: format!("s{}", i + 1),
            label: infos[id].name.clone(),
            kind: "scenario",
            scenario_id: Some(*id),
            actual_through: infos[id].actual_through.clone(),
        })
        .collect();
    if include_actual {
        series.push(Series {
            key: "actual".into(),
            label: "実績".into(),
            kind: "actual",
            scenario_id: None,
            actual_through: None,
        });
    }

    let by_activity = unit_id.is_some();
    let amounts = load_amounts(&mut conn, &u, &ids, unit_id, by_activity, &measure).await?;
    let mut rows: BTreeMap<RowKey, Row> = BTreeMap::new();
    let mut put = |k: RowKey, series: &str, v: BigInt| {
        rows.entry(k.clone())
            .or_insert_with(|| Row {
                unit_id: (k.0 != 0).then_some(k.0),
                activity_id: (k.1 != 0).then_some(k.1),
                subject_id: k.2,
                month: k.3,
                values: BTreeMap::new(),
            })
            .values
            .insert(series.to_string(), v.to_string());
    };
    for (i, id) in ids.iter().enumerate() {
        for (k, v) in amounts.get(id).cloned().unwrap_or_default() {
            put(k, &format!("s{}", i + 1), v);
        }
    }
    if include_actual {
        for (k, v) in load_actuals(&mut conn, &u, &months, unit_id, by_activity).await? {
            put(k, "actual", v);
        }
    }
    // 全社の合計を会計と一致させるため、ユニットを指定しないときは未割当の実績も返す（ユニット ID 0 → null）
    if unit_id.is_none() {
        let mut unallocated = load_unallocated(&mut conn, &u, &ids, &months, include_actual).await?;
        for (i, id) in ids.iter().enumerate() {
            for (k, v) in unallocated.remove(id).unwrap_or_default() {
                put(k, &format!("s{}", i + 1), v);
            }
        }
        for (k, v) in unallocated.remove(&0).unwrap_or_default() {
            put(k, "actual", v);
        }
    }
    Ok(http::ok(&ComparisonResponse {
        fiscal_year,
        measure,
        months,
        series,
        rows: rows.into_values().collect(),
        restricted_hidden,
    }))
}

/// シナリオごとに、ユニット（by_activity のときは施策）× 科目 × 月の合計を返す。unit_id を指定するとそのユニットだけ。
/// 金額は scenario_amounts ビュー（決算確定月以前は実績、それより後は計画値）から、指標（measure）で読む。
async fn load_amounts(
    conn: &mut Conn,
    u: &User,
    ids: &[i64],
    unit_id: Option<i64>,
    by_activity: bool,
    measure: &str,
) -> AppResult<HashMap<i64, Amounts>> {
    let mut args = id_args(ids);
    // ユニット単位では施策 ID を 0 とし、グループ化にも含めない
    let (activity_col, group_activity) = if by_activity { ("a.id", "a.id, ") } else { ("0", "") };
    let mut filter = String::new();
    if let Some(id) = unit_id {
        filter.push_str(" AND a.unit_id = ?");
        args.push(id.into());
    }
    let rows: Vec<(i64, i64, i64, i64, String, String)> = conn
        .exec(
            format!(
                "SELECT b.scenario_id, a.unit_id, {activity_col}, b.subject_id, DATE_FORMAT(b.target_month, '%Y-%m'), {}
                 FROM scenario_amounts b JOIN activities a ON a.id = b.activity_id{}
                 WHERE b.scenario_id IN ({}){filter}{}
                 GROUP BY b.scenario_id, a.unit_id, {group_activity}b.subject_id, b.target_month",
                measure::sum(measure),
                measure::JOINS,
                placeholders(ids.len()),
                visibility::subject_filter(u, "b.subject_id")
            ),
            Params::Positional(args),
        )
        .await?;
    let mut out: HashMap<i64, Amounts> = HashMap::new();
    for (sid, unit, activity, subject, month, amount) in rows {
        out.entry(sid).or_default().insert((unit, activity, subject, month), parse_big(&amount)?);
    }
    Ok(out)
}

/// 実績データの、ユニット（by_activity のときは施策）× 科目 × 月の合計を返す。unit_id を指定するとそのユニットだけ。
async fn load_actuals(
    conn: &mut Conn,
    u: &User,
    months: &[String],
    unit_id: Option<i64>,
    by_activity: bool,
) -> AppResult<Amounts> {
    let mut args: Vec<Value> =
        vec![format!("{}-01", months[0]).into(), format!("{}-01", months[months.len() - 1]).into()];
    let (activity_col, group_activity) = if by_activity { ("a.id", "a.id, ") } else { ("0", "") };
    let mut filter = String::new();
    if let Some(id) = unit_id {
        filter.push_str(" AND a.unit_id = ?");
        args.push(id.into());
    }
    let rows: Vec<(i64, i64, i64, String, String)> = conn
        .exec(
            format!(
                "SELECT a.unit_id, {activity_col}, f.subject_id, DATE_FORMAT(f.target_month, '%Y-%m'), CAST(SUM(f.amount) AS CHAR)
                 FROM actual_facts f JOIN activities a ON a.id = f.activity_id
                 WHERE f.target_month BETWEEN ? AND ?{filter}{}
                 GROUP BY a.unit_id, {group_activity}f.subject_id, f.target_month",
                visibility::subject_filter(u, "f.subject_id")
            ),
            Params::Positional(args),
        )
        .await?;
    rows.into_iter()
        .map(|(unit, activity, subject, month, amount)| Ok(((unit, activity, subject, month), parse_big(&amount)?)))
        .collect()
}

/// 未割当の実績の、科目 × 月の合計を返す（ユニット ID・施策 ID は 0）。
/// シナリオは決算確定月以前の月（scenario_unallocated ビュー）、キー 0 は実績データ（include_actual のとき）。
async fn load_unallocated(
    conn: &mut Conn,
    u: &User,
    ids: &[i64],
    months: &[String],
    include_actual: bool,
) -> AppResult<HashMap<i64, Amounts>> {
    let mut args = id_args(ids);
    let mut sql = format!(
        "SELECT scenario_id, subject_id, DATE_FORMAT(target_month, '%Y-%m'), CAST(SUM(amount) AS CHAR)
         FROM scenario_unallocated WHERE scenario_id IN ({}){}
         GROUP BY scenario_id, subject_id, target_month",
        placeholders(ids.len()),
        visibility::subject_filter(u, "subject_id")
    );
    if include_actual {
        sql.push_str(&format!(
            " UNION ALL
             SELECT 0, subject_id, DATE_FORMAT(target_month, '%Y-%m'), CAST(SUM(amount) AS CHAR)
             FROM actual_facts WHERE activity_id IS NULL AND target_month BETWEEN ? AND ?{}
             GROUP BY subject_id, target_month",
            visibility::subject_filter(u, "subject_id")
        ));
        args.push(format!("{}-01", months[0]).into());
        args.push(format!("{}-01", months[months.len() - 1]).into());
    }
    let rows: Vec<(i64, i64, String, String)> = conn.exec(sql, Params::Positional(args)).await?;
    let mut out: HashMap<i64, Amounts> = HashMap::new();
    for (sid, subject, month, amount) in rows {
        out.entry(sid).or_default().insert((0, 0, subject, month), parse_big(&amount)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids() {
        assert_eq!(parse_ids("3, 1,3,,2"), Some(vec![3, 1, 2]));
        assert_eq!(parse_ids(""), Some(vec![]));
        assert_eq!(parse_ids("1,x"), None);
        assert_eq!(parse_ids("0"), None);
    }
}
