//! ホーム（docs/plan.md「2.10 現場担当の動線」）。シナリオでの施策ごとの状態と、基準・前回見込との差を返す。

use std::collections::{BTreeSet, HashMap};

use axum::extract::State;
use axum::response::Response;
use chrono::NaiveDateTime;
use futures::future::try_join_all;
use mysql_async::prelude::*;
use num_bigint::BigInt;
use num_traits::{Signed, Zero};
use serde::{Serialize, Serializer};

use super::notes::{split_causes, status_of};
use super::{SCENARIO_SELECT, Scenario, find_scenario, scenario_where};
use crate::auth::{CurrentUser, ROLE_MANAGER, ROLE_MEMBER, User};
use crate::db::{Params, Value};
use crate::error::bad_request;
use crate::http::{self, Id, Query};
use crate::{AppResult, AppState, jst, visibility};

/// 施策（別名 alias）が更新の対象であることを表す SQL の条件（docs/plan.md「2.10」）。プレースホルダーの引数はシナリオ ID が1つ。
///
/// 施策のステータスが「完了」「中止」でない施策。ただし「完了」「中止」でも、そのシナリオの計画値の月（決算確定月より後）に
/// 0 でない金額が残っている施策は対象にする。ホームの一覧・件数、締切の通知、シナリオの切り替えの注意で同じ条件を使う。
pub fn target_condition(alias: &str) -> String {
    format!(
        "({alias}.status NOT IN ('completed', 'cancelled') OR EXISTS (
            SELECT 1 FROM budget_facts tbf JOIN scenarios tsc ON tsc.id = tbf.scenario_id
            WHERE tbf.scenario_id = ? AND tbf.activity_id = {alias}.id AND tbf.amount <> 0
              AND (tsc.actual_through IS NULL OR tbf.target_month > tsc.actual_through)))"
    )
}

/// 年間の収益・費用（円、文字列）。
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct PlTotals {
    pub revenue: String,
    pub expense: String,
}

impl Default for PlTotals {
    fn default() -> Self {
        PlTotals { revenue: "0".into(), expense: "0".into() }
    }
}

/// 比較に使うシナリオ。
#[derive(Clone, Debug, Serialize)]
pub struct ScenarioRef {
    pub id: i64,
    pub name: String,
    pub plan_role: Option<String>,
    pub actual_through: Option<String>,
}

impl From<Scenario> for ScenarioRef {
    fn from(s: Scenario) -> Self {
        ScenarioRef { id: s.id, name: s.name, plan_role: s.plan_role, actual_through: s.actual_through }
    }
}

/// 新しく実績になった月の、前回見込の計画値と実績の差（見込の当たり具合）。
#[derive(Clone, Debug, Serialize)]
pub struct AccuracyRow {
    pub plan: PlTotals,
    pub actual: PlTotals,
    /// 科目 × 月の差の絶対値の合計 ÷ 計画値の絶対値の合計（%、小数1桁）。計画値がなければ null
    #[serde(serialize_with = "ser_rate")]
    pub rate: Option<f64>,
    /// 差が大きい（20% 以上、または計画値がないのに実績がある）か
    pub large: bool,
}

/// 率を Go の encoding/json と同じ形（20 は "20"、12.5 は "12.5"）で書き出す。
fn ser_rate<S: Serializer>(r: &Option<f64>, s: S) -> Result<S::Ok, S::Error> {
    match r {
        Some(r) => http::RawNum(format!("{r}")).serialize(s),
        None => s.serialize_none(),
    }
}

/// ホームの1行。
#[derive(Serialize)]
struct ActivityStatus {
    activity_id: i64,
    code: String,
    name: String,
    unit_id: i64,
    owner_user_id: Option<i64>,
    status: String,
    has_explanation: bool,
    explanation: String,
    causes: Vec<String>,
    /// 説明へのコメントの件数（消したものを除く、2.24）
    comment_count: i64,
    is_priority: bool,
    is_watched: bool,
    #[serde(serialize_with = "jst::ser_opt")]
    last_edited_at: Option<NaiveDateTime>,
    #[serde(serialize_with = "jst::ser_opt")]
    completed_at: Option<NaiveDateTime>,
    current: PlTotals,
    base: Option<PlTotals>,
    /// 期初計画（マネージャーのサービスの状況）
    initial: Option<PlTotals>,
    /// 修正計画
    revised: Option<PlTotals>,
    previous: Option<PlTotals>,
    accuracy: Option<AccuracyRow>,
}

#[derive(Serialize)]
struct ActivityStatusResponse {
    scenario: Scenario,
    scope: String,
    base: Option<ScenarioRef>,
    initial: Option<ScenarioRef>,
    revised: Option<ScenarioRef>,
    previous: Option<ScenarioRef>,
    /// 前回見込より新しく実績になった月（実績のお知らせ）
    new_actual_months: Vec<String>,
    items: Vec<ActivityStatus>,
    /// 閲覧制限のある科目を除いた金額か（docs/plan.md「2.17」）
    restricted_hidden: bool,
}

type StatusRow = (
    i64,
    String,
    String,
    i64,
    Option<i64>,
    bool,
    bool,
    String,
    String,
    Option<NaiveDateTime>,
    Option<NaiveDateTime>,
    i64,
);

/// GET /api/scenarios/{id}/activity-status?scope=mine|units|all。
///
/// scope の既定は、現場担当は mine（担当施策）、マネージャーは units（所管ユニットの施策と担当施策）、それ以外は all。
/// 基準は同じ年度の修正計画（なければ期初計画、なければ最初に作ったシナリオ）、前回見込はシナリオの前回見込。
pub async fn activity_statuses(
    State(st): State<AppState>,
    CurrentUser(u): CurrentUser,
    Id(id): Id,
    q: Query,
) -> AppResult<Response> {
    let mut scope = q.get("scope").to_string();
    if scope.is_empty() {
        scope = default_scope(&u).into();
    }
    if !["mine", "units", "all"].contains(&scope.as_str()) {
        return Err(bad_request("scope は mine / units / all のいずれかを指定してください"));
    }

    let mut conn = st.db.get_conn().await?;
    let s = find_scenario(&mut conn, id, "").await?;
    let restricted_hidden = visibility::hidden(&mut conn, &u).await?;
    let base = base_scenario_of(&mut conn, s.fiscal_year).await?;
    let initial = role_scenario_of(&mut conn, s.fiscal_year, "initial").await?;
    let revised = role_scenario_of(&mut conn, s.fiscal_year, "revised").await?;
    let mut previous = None;
    let mut new_actual_months = Vec::new();
    if let Some(pid) = s.previous_scenario_id {
        let p = find_scenario(&mut conn, pid, "").await?;
        // 前回見込の決算確定月より後で、今回の決算確定月以前の月が、新しく実績になった月
        new_actual_months =
            s.actual_months().into_iter().filter(|m| p.actual_through.as_ref().is_none_or(|t| m > t)).collect();
        previous = Some(ScenarioRef::from(p));
    }

    // 対象の施策（更新の対象、docs/plan.md「2.10」）
    let mut filter = format!(" WHERE {}", target_condition("a"));
    let mut args: Vec<Value> = vec![id.into(), id.into(), u.id.into(), id.into()];
    match scope.as_str() {
        "mine" => {
            filter.push_str(" AND a.owner_user_id = ?");
            args.push(u.id.into());
        }
        "units" => {
            filter.push_str(" AND (un.owner_user_id = ? OR a.owner_user_id = ?)");
            args.extend([u.id.into(), u.id.into()]);
        }
        _ => {}
    }
    let rows: Vec<StatusRow> = conn
        .exec(
            format!(
                "SELECT a.id, a.code, a.name, a.unit_id, a.owner_user_id, a.is_priority, w.user_id IS NOT NULL,
                        COALESCE(n.explanation, ''), COALESCE(n.causes, ''), n.last_edited_at, n.completed_at,
                        (SELECT COUNT(*) FROM activity_scenario_comments c WHERE c.scenario_id = ? AND c.activity_id = a.id AND c.deleted_at IS NULL)
                 FROM activities a
                 JOIN units un ON un.id = a.unit_id
                 LEFT JOIN activity_scenario_notes n ON n.activity_id = a.id AND n.scenario_id = ?
                 LEFT JOIN activity_watches w ON w.activity_id = a.id AND w.user_id = ?{filter}
                 ORDER BY a.code"
            ),
            Params::Positional(args),
        )
        .await?;
    drop(conn);
    let mut items: Vec<ActivityStatus> = rows
        .into_iter()
        .map(
            |(
                activity_id,
                code,
                name,
                unit_id,
                owner_user_id,
                is_priority,
                is_watched,
                explanation,
                causes,
                edited,
                completed,
                comment_count,
            )| {
                ActivityStatus {
                    activity_id,
                    code,
                    name,
                    unit_id,
                    owner_user_id,
                    status: status_of(completed, edited).into(),
                    has_explanation: !explanation.is_empty(),
                    explanation,
                    causes: split_causes(&causes),
                    comment_count,
                    is_priority,
                    is_watched,
                    last_edited_at: edited,
                    completed_at: completed,
                    current: PlTotals::default(),
                    base: None,
                    initial: None,
                    revised: None,
                    previous: None,
                    accuracy: None,
                }
            },
        )
        .collect();

    // 年間の収益・費用（決算確定月以前は実績）。シナリオごとの集計は互いに関係しないので、並行して読む（I-15）
    let targets: Vec<(usize, i64)> = [
        Some(s.id),
        base.as_ref().map(|b| b.id),
        initial.as_ref().map(|b| b.id),
        revised.as_ref().map(|b| b.id),
        previous.as_ref().map(|b| b.id),
    ]
    .into_iter()
    .enumerate()
    .filter_map(|(i, id)| id.map(|id| (i, id)))
    .collect();
    let results = try_join_all(targets.iter().map(|&(_, sid)| {
        let (st, u) = (st.clone(), u.clone());
        async move {
            let mut conn = st.db.get_conn().await?;
            annual_totals(&mut conn, &u, sid, &[]).await
        }
    }))
    .await?;
    for ((slot, _), totals) in targets.iter().zip(results) {
        for it in &mut items {
            let v = totals.get(&it.activity_id).cloned().unwrap_or_default();
            match slot {
                0 => it.current = v,
                1 => it.base = Some(v),
                2 => it.initial = Some(v),
                3 => it.revised = Some(v),
                _ => it.previous = Some(v),
            }
        }
    }

    // 見込の当たり具合（新しく実績になった月の、前回見込の計画値と実績）
    if let Some(p) = &previous
        && !new_actual_months.is_empty()
    {
        let mut conn = st.db.get_conn().await?;
        let acc = accuracy_of(&mut conn, &u, s.id, p.id, &new_actual_months).await?;
        for it in &mut items {
            it.accuracy = acc.get(&it.activity_id).cloned();
        }
    }
    Ok(http::ok(&ActivityStatusResponse {
        scenario: s,
        scope,
        base,
        initial,
        revised,
        previous,
        new_actual_months,
        items,
        restricted_hidden,
    }))
}

fn default_scope(u: &User) -> &'static str {
    match u.role.as_str() {
        ROLE_MEMBER => "mine",
        ROLE_MANAGER => "units",
        _ => "all",
    }
}

/// 年度の基準シナリオ（修正計画、なければ期初計画、なければ最初に作ったシナリオ）を返す。
pub async fn base_scenario_of<Q: Queryable>(q: &mut Q, fiscal_year: i32) -> AppResult<Option<ScenarioRef>> {
    Ok(scenario_where(
        q,
        "WHERE fiscal_year = ? ORDER BY CASE plan_role WHEN 'revised' THEN 0 WHEN 'initial' THEN 1 ELSE 2 END, id LIMIT 1",
        p![fiscal_year],
    )
    .await?
    .map(ScenarioRef::from))
}

/// 年度でエイリアス（initial / revised）を持つシナリオを返す（なければ None）。
pub async fn role_scenario_of<Q: Queryable>(q: &mut Q, fiscal_year: i32, role: &str) -> AppResult<Option<ScenarioRef>> {
    let s: Option<Scenario> = q
        .exec_first(format!("{SCENARIO_SELECT} WHERE fiscal_year = ? AND plan_role = ?"), p![fiscal_year, role])
        .await?;
    Ok(s.map(ScenarioRef::from))
}

/// シナリオの施策ごとの収益・費用の合計（months を指定すればその月だけ）を返す。
/// ユーザーが見られない科目（閲覧制限）は除く。
pub async fn annual_totals<Q: Queryable>(
    q: &mut Q,
    u: &User,
    scenario_id: i64,
    months: &[String],
) -> AppResult<HashMap<i64, PlTotals>> {
    let mut sql = format!(
        "SELECT b.activity_id,
                CAST(COALESCE(SUM(CASE WHEN s.category = 'revenue' THEN b.amount END), 0) AS CHAR),
                CAST(COALESCE(SUM(CASE WHEN s.category = 'expense' THEN b.amount END), 0) AS CHAR)
         FROM scenario_amounts b JOIN subjects s ON s.id = b.subject_id
         WHERE b.scenario_id = ?{}",
        visibility::subject_filter(u, "b.subject_id")
    );
    let mut args: Vec<Value> = vec![scenario_id.into()];
    if let (Some(first), Some(last)) = (months.first(), months.last()) {
        sql.push_str(" AND b.target_month BETWEEN ? AND ?");
        args.push(format!("{first}-01").into());
        args.push(format!("{last}-01").into());
    }
    sql.push_str(" GROUP BY b.activity_id");
    let rows: Vec<(i64, String, String)> = q.exec(sql, Params::Positional(args)).await?;
    Ok(rows.into_iter().map(|(id, revenue, expense)| (id, PlTotals { revenue, expense })).collect())
}

/// months（新しく実績になった月）について、施策ごとに前回見込の計画値と今回の実績を比べる。
/// 差の率は、科目 × 月の差の絶対値の合計 ÷ 前回見込の絶対値の合計（docs/plan.md「2.8」の見込の当たり具合）。
/// ユーザーが見られない科目（閲覧制限）は除く。
pub async fn accuracy_of<Q: Queryable>(
    q: &mut Q,
    u: &User,
    current_id: i64,
    previous_id: i64,
    months: &[String],
) -> AppResult<HashMap<i64, AccuracyRow>> {
    type K = (i64, i64, String);
    let (Some(first), Some(last)) = (months.first(), months.last()) else { return Ok(HashMap::new()) };
    let filter = visibility::subject_filter(u, "subject_id");
    let load = async |q: &mut Q, scenario_id: i64| -> AppResult<HashMap<K, BigInt>> {
        let rows: Vec<(i64, i64, String, String)> = q
            .exec(
                format!(
                    "SELECT activity_id, subject_id, DATE_FORMAT(target_month, '%Y-%m'), CAST(SUM(amount) AS CHAR)
                     FROM scenario_amounts WHERE scenario_id = ? AND target_month BETWEEN ? AND ?{filter}
                     GROUP BY activity_id, subject_id, target_month"
                ),
                p![scenario_id, format!("{first}-01"), format!("{last}-01")],
            )
            .await?;
        Ok(rows.into_iter().map(|(a, s, m, amount)| ((a, s, m), amount.parse().unwrap_or_default())).collect())
    };
    let actual = load(q, current_id).await?;
    let plan = load(q, previous_id).await?;
    let plan_totals = annual_totals(q, u, previous_id, months).await?;
    let actual_totals = annual_totals(q, u, current_id, months).await?;

    let mut diff_sum: HashMap<i64, BigInt> = HashMap::new();
    let mut plan_sum: HashMap<i64, BigInt> = HashMap::new();
    let keys: BTreeSet<&K> = actual.keys().chain(plan.keys()).collect();
    let zero = BigInt::zero();
    for k in keys {
        let a = actual.get(k).unwrap_or(&zero);
        let p = plan.get(k).unwrap_or(&zero);
        *diff_sum.entry(k.0).or_default() += (a - p).abs();
        *plan_sum.entry(k.0).or_default() += p.abs();
    }

    let mut out = HashMap::new();
    for (id, d) in diff_sum {
        let mut acc = AccuracyRow {
            plan: plan_totals.get(&id).cloned().unwrap_or_default(),
            actual: actual_totals.get(&id).cloned().unwrap_or_default(),
            rate: None,
            large: false,
        };
        let p = plan_sum.get(&id).cloned().unwrap_or_default();
        if p.is_zero() {
            acc.large = !d.is_zero();
        } else {
            // 0.1% 単位に四捨五入: floor(d × 1000 / p + 0.5) = floor((d × 2000 + p) / 2p)
            let tenths: BigInt = (&d * 2000 + &p) / (&p * 2);
            acc.rate = Some(tenths.to_string().parse::<f64>().unwrap_or(0.0) / 10.0);
            // 20% 以上: 差 × 5 ≥ 計画値
            acc.large = &d * 5 >= p;
        }
        out.insert(id, acc);
    }
    Ok(out)
}
