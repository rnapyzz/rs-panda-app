//! リスク画面（docs/plan.md「2.8 確度とリスク」「2.9 リスク画面」）。
//! 施策ごとに、楽観・基準（加重見込）・悲観・満額、段階別の売上、内訳、比較シナリオとの差、客観的なシグナル（警告）を返す。
//!
//! 施策の一覧は1回だけ読み、シナリオごとの金額は DB から読んだ行を施策ごとに合計する（基準・比較・複製元の版で同じ処理）。
//! 互いに関係しない読み込みは、別々のコネクションで並行して行う（I-15）。

use std::collections::{BTreeMap, HashMap, HashSet};

use axum::extract::State;
use axum::response::Response;
use chrono::{Duration, NaiveDate, NaiveDateTime};
use mysql_async::prelude::*;
use mysql_async::{Params, Pool};
use num_bigint::BigInt;
use num_traits::Zero;
use serde::{Serialize, Serializer};

use super::measure::{HIGH_RATE, JOINS};
use super::placeholders;
use crate::auth::{CurrentUser, User};
use crate::db::Value;
use crate::error::{bad_request, validation1};
use crate::formula::{Rat, parse_decimal, round_half_up};
use crate::http::{self, Query, RawNum};
use crate::{AppResult, AppState, calc, jst, visibility};

/// 「期日が近い」とみなすマイルストーンの日数
const UPCOMING_DAYS: i64 = 30;
// 警告のしきい値（docs/plan.md「2.8」の固定値）
/// 下方修正: 比較シナリオからの低下の率（%）
const DOWNWARD_RATE_PCT: i64 = 10;
/// 下方修正: 低下の金額の下限（円）
const DOWNWARD_MIN_YEN: i64 = 100_000;
/// 見込の当たり具合: 差の率（%）
const ACCURACY_RATE_PCT: i64 = 20;
/// 見込の当たり具合: 直近の決算確定済みの月数
const ACCURACY_MONTHS: usize = 3;
/// 連続の下方修正: 複製元をたどる版の数（今回を含む）
const CONSECUTIVE_DEPTH: usize = 3;

/// 指標。合計は FULL〜ACTUAL の添字で持つ。
const FULL: usize = 0;
const WEIGHTED: usize = 1;
const OPTIMISTIC: usize = 2;
const PESSIMISTIC: usize = 3;
const ACTUAL: usize = 4;

/// 収益と費用の合計（円、文字列）。
#[derive(Clone, Debug, Default, Serialize)]
struct Pl {
    revenue: String,
    expense: String,
}

/// 合計の単位（1円の 1/10,000）。標準の確率は小数4桁（DECIMAL(5,4)）なので、加重見込もこの単位で割り切れ、
/// 整数のまま誤差なく合計できる（有理数の足し算は1回ごとに約分するため、行数が多いと遅い）。
const SCALE: i128 = 10_000;

/// SCALE 単位の金額を、円未満を四捨五入（0 から遠い方へ）して円にする。
fn round_scaled(x: i128) -> i128 {
    let q = (x.abs() * 2 + SCALE) / (SCALE * 2);
    if x < 0 { -q } else { q }
}

/// 収益と費用の合計（SCALE 単位。加重見込の端数を最後に丸める）。
#[derive(Clone, Debug, Default)]
struct PlSum {
    revenue: i128,
    expense: i128,
}

impl PlSum {
    fn add(&mut self, category: &str, v: i128) {
        if category == "revenue" {
            self.revenue += v;
        } else {
            self.expense += v;
        }
    }

    fn profit(&self) -> i128 {
        self.revenue - self.expense
    }

    fn pl(&self) -> Pl {
        Pl { revenue: round_scaled(self.revenue).to_string(), expense: round_scaled(self.expense).to_string() }
    }
}

/// 率（%、小数1桁）を Go の encoding/json と同じ形（50 は "50"、12.5 は "12.5"）で書き出す。
fn ser_rate<S: Serializer>(tenths: &BigInt, s: S) -> Result<S::Ok, S::Error> {
    let v = tenths.to_string().parse::<f64>().unwrap_or(0.0) / 10.0;
    RawNum(format!("{v}")).serialize(s)
}

fn ser_rate_opt<S: Serializer>(tenths: &Option<BigInt>, s: S) -> Result<S::Ok, S::Error> {
    match tenths {
        Some(t) => ser_rate(t, s),
        None => s.serialize_none(),
    }
}

/// num × 100 ÷ den を、0.1% 単位に四捨五入した値（10 倍した整数）。
fn rate_tenths(num: &Rat, den: &Rat) -> BigInt {
    round_half_up(&(num * Rat::from_integer(1000.into()) / den))
}

#[derive(Clone, Debug, Serialize)]
struct RiskMilestone {
    name: String,
    due_date: String,
    status: String,
    /// overdue（期日超過）/ delayed（遅延）/ upcoming（期日が近い。注意で、警告には数えない）
    risk: &'static str,
}

/// 内訳（line_id が null は科目への直接入力）の、期間の満額。
#[derive(Clone, Debug, Serialize)]
struct RiskLine {
    line_id: Option<i64>,
    subject_id: i64,
    name: String,
    /// 実際に使う段階（内訳の上書き、なければ施策）
    confidence_level: String,
    outlook: String,
    amount: String,
}

#[derive(Clone, Debug, Default, Serialize)]
struct Postponed {
    count: i64,
    days: i64,
}

#[derive(Clone, Debug, Serialize)]
struct Downward {
    diff: String,
    #[serde(serialize_with = "ser_rate")]
    rate: BigInt,
}

/// 客観的なシグナル（docs/plan.md「2.8」）。
#[derive(Clone, Debug, Default, Serialize)]
struct Warnings {
    milestones: Vec<RiskMilestone>,
    /// 比較シナリオの作成以降に、期日を後ろにずらした回数と日数
    postponed: Postponed,
    /// 下方修正（年間の加重見込の利益の、比較シナリオからの低下）。しきい値を超えたときだけ
    downward: Option<Downward>,
    /// 複製元をたどった直近の見込で、加重見込の利益が2回続けて下がった
    consecutive: bool,
    /// 見込の当たり具合（直近の決算確定済みの月の、比較シナリオの計画値と実績の差の率）。しきい値を超えたときだけ
    #[serde(serialize_with = "ser_rate_opt")]
    accuracy: Option<BigInt>,
}

/// 施策ごとのリスク情報。
#[derive(Clone, Debug, Serialize)]
struct RiskActivity {
    id: i64,
    code: String,
    name: String,
    unit_id: i64,
    owner_user_id: Option<i64>,
    activity_type: String,
    status: String,
    confidence_level: String,
    assumptions: String,
    // 期間の楽観・基準（加重見込）・悲観・満額
    full: Pl,
    weighted: Pl,
    optimistic: Pl,
    pessimistic: Pl,
    /// 期間のうち、実績の月の収益・費用
    actual: Pl,
    /// 期間の売上（満額）を「実績・段階のコード・ダウンサイド」に分けたもの
    revenue_by_level: BTreeMap<String, String>,
    /// 月ごとの売上（満額）を、revenue_by_level と同じキーに分けたもの（施策の一覧の「確度の推移」。docs/plan.md「2.22」）
    revenue_by_month: BTreeMap<String, BTreeMap<String, String>>,
    /// 比較シナリオの、期間の加重見込
    compare: Option<Pl>,
    lines: Vec<RiskLine>,
    warnings: Warnings,
    /// 警告の数（マイルストーンの期日が近いは数えない）
    warning_count: usize,
    /// 「段階に対して状況が悪い」（確度の高い段階で、警告が1つ以上）
    bad_for_level: bool,
    /// シナリオごとの今回の見込の説明（scenario / compare）
    conditions: BTreeMap<&'static str, String>,
    /// 基準シナリオの説明へのコメントの件数（消したものを除く、docs/plan.md「2.24」）
    comment_count: i64,
}

#[derive(Clone, Debug, Serialize)]
struct RiskScenario {
    id: i64,
    name: String,
    plan_role: Option<String>,
    actual_through: Option<String>,
    #[serde(rename = "base_scenario_id")]
    base_id: Option<i64>,
    #[serde(skip)]
    fiscal_year: i32,
    #[serde(skip)]
    created_at: NaiveDateTime,
}
from_row!(RiskScenario { id, name, plan_role, actual_through, base_id, fiscal_year, created_at });

#[derive(Clone, Debug, Serialize)]
struct RiskLevel {
    code: String,
    name: String,
    rate: RawNum,
    /// 確度の高い段階（悲観で計上する）
    high: bool,
}

#[derive(Serialize)]
struct RiskResponse {
    fiscal_year: i32,
    today: String,
    period: String,
    months: Vec<String>,
    scenario: RiskScenario,
    compare: Option<RiskScenario>,
    levels: Vec<RiskLevel>,
    activities: Vec<RiskActivity>,
    /// 閲覧制限のある科目を除いた金額か（docs/plan.md「2.17」）
    restricted_hidden: bool,
}

async fn find_scenario(pool: &Pool, id: i64) -> AppResult<Option<RiskScenario>> {
    let mut conn = pool.get_conn().await?;
    Ok(conn
        .exec_first(
            "SELECT id, name, plan_role, DATE_FORMAT(actual_through, '%Y-%m'), base_scenario_id, fiscal_year, created_at FROM scenarios WHERE id = ?",
            (id,),
        )
        .await?)
}

fn parse_opt_id(q: &Query, name: &str) -> AppResult<Option<i64>> {
    match q.get(name) {
        "" => Ok(None),
        s => s.parse().map(Some).map_err(|_| bad_request(format!("{name} は数値で指定してください"))),
    }
}

/// GET /api/reports/risk?scenario_id=&compare_id=&period=year|remaining
pub async fn risk(State(st): State<AppState>, CurrentUser(u): CurrentUser, q: Query) -> AppResult<Response> {
    let sid = parse_opt_id(&q, "scenario_id")?;
    let Some(sid) = sid else {
        return Err(validation1("scenario_id", "基準のシナリオを選んでください"));
    };
    let cid = parse_opt_id(&q, "compare_id")?;
    let period = match q.get("period") {
        "" => "year",
        p => p,
    };
    if period != "year" && period != "remaining" {
        return Err(bad_request("period は year / remaining のいずれかを指定してください"));
    }

    let pool = &st.db;
    let s = find_scenario(pool, sid)
        .await?
        .ok_or_else(|| validation1("scenario_id", format!("シナリオ {sid} が見つかりません")))?;
    let compare = match cid {
        None => None,
        Some(cid) => {
            let c = find_scenario(pool, cid)
                .await?
                .ok_or_else(|| validation1("compare_id", format!("シナリオ {cid} が見つかりません")))?;
            if c.fiscal_year != s.fiscal_year {
                return Err(validation1("compare_id", "基準と同じ年度のシナリオを選んでください"));
            }
            Some(c)
        }
    };
    let today = jst::today();
    let all_months = calc::fiscal_months(s.fiscal_year);
    let through = s.actual_through.clone().unwrap_or_default();
    let months: Vec<String> =
        if period == "remaining" { all_months.iter().filter(|m| **m > through).cloned().collect() } else { all_months };
    let in_period: HashSet<String> = months.iter().cloned().collect();

    let mut conn = pool.get_conn().await?;
    let restricted_hidden = visibility::hidden(&mut conn, &u).await?;
    let levels = load_levels(&mut conn).await?;
    let mut acts = load_activities(&mut conn, &levels).await?;
    drop(conn);

    // 互いに関係しない読み込みを並行して行う
    let compare_ref = compare.as_ref();
    let (rows, lines, milestones, conditions, chain, compare_rows, accuracy, postponed) = tokio::try_join!(
        amount_rows(pool, &u, s.id),
        fetch_lines(pool, &u, s.id, &months),
        fetch_milestones(pool, today),
        fetch_conditions(pool, s.id, compare_ref.map(|c| c.id)),
        consecutive_chain(pool, &u, &s),
        async {
            match compare_ref {
                Some(c) => amount_rows(pool, &u, c.id).await.map(Some),
                None => Ok(None),
            }
        },
        async {
            match compare_ref {
                Some(c) => fetch_accuracy(pool, &u, &s, c).await,
                None => Ok(HashMap::new()),
            }
        },
        async {
            match compare_ref {
                Some(c) => fetch_postponed(pool, c.created_at).await,
                None => Ok(HashMap::new()),
            }
        },
    )?;

    let totals = accumulate(&rows, &in_period);
    let compare_totals = compare_rows.map(|r| accumulate(&r, &in_period));
    let (notes, compare_notes, comment_counts) = conditions;
    let mut lines = lines;
    let mut milestones = milestones;
    let empty = Totals::default();
    for a in &mut acts {
        let t = totals.get(&a.activity.id).unwrap_or(&empty);
        let r = &mut a.activity;
        r.full = t.sums[FULL].pl();
        r.weighted = t.sums[WEIGHTED].pl();
        r.optimistic = t.sums[OPTIMISTIC].pl();
        r.pessimistic = t.sums[PESSIMISTIC].pl();
        r.actual = t.sums[ACTUAL].pl();
        r.revenue_by_level = t.by_level.iter().map(|(k, v)| (k.clone(), v.to_string())).collect();
        r.revenue_by_month = t
            .by_month
            .iter()
            .map(|(m, by)| (m.clone(), by.iter().map(|(k, v)| (k.clone(), v.to_string())).collect()))
            .collect();
        r.lines = direct_lines_last(lines.remove(&r.id).unwrap_or_default(), &r.confidence_level);
        r.warnings.milestones = milestones.remove(&r.id).unwrap_or_default();
        if let Some(n) = notes.get(&r.id) {
            r.conditions.insert("scenario", n.clone());
        }
        if let Some(n) = compare_notes.get(&r.id) {
            r.conditions.insert("compare", n.clone());
        }
        r.comment_count = comment_counts.get(&r.id).copied().unwrap_or(0);
        let profit = t.year[WEIGHTED].profit();
        if let Some(ct) = &compare_totals {
            let c = ct.get(&r.id).unwrap_or(&empty);
            r.compare = Some(c.sums[WEIGHTED].pl());
            r.warnings.downward = downward(profit, c.year[WEIGHTED].profit());
            r.warnings.accuracy = accuracy.get(&r.id).cloned();
            r.warnings.postponed = postponed.get(&r.id).cloned().unwrap_or_default();
        }
        // 連続の下方修正: 今回 < 1つ前 < 2つ前
        if chain.len() == CONSECUTIVE_DEPTH - 1 {
            let p1 = chain[0].get(&r.id).copied().unwrap_or(0);
            let p2 = chain[1].get(&r.id).copied().unwrap_or(0);
            r.warnings.consecutive = profit < p1 && p1 < p2;
        }
        let w = &r.warnings;
        r.warning_count = usize::from(w.milestones.iter().any(|m| m.risk != "upcoming"))
            + usize::from(w.postponed.count > 0)
            + usize::from(w.downward.is_some())
            + usize::from(w.consecutive)
            + usize::from(w.accuracy.is_some());
        r.bad_for_level = (a.high || t.high) && r.warning_count > 0;
    }

    Ok(http::ok(&RiskResponse {
        fiscal_year: s.fiscal_year,
        today: today.format("%Y-%m-%d").to_string(),
        period: period.to_string(),
        months,
        scenario: s,
        compare,
        levels,
        activities: acts.into_iter().map(|a| a.activity).collect(),
        restricted_hidden,
    }))
}

async fn load_levels(conn: &mut mysql_async::Conn) -> AppResult<Vec<RiskLevel>> {
    let rows: Vec<(String, String, String, bool)> = conn
        .query(format!("SELECT code, name, CAST(rate AS CHAR), rate >= {HIGH_RATE} FROM confidence_levels ORDER BY sort_order, code"))
        .await?;
    Ok(rows
        .into_iter()
        .map(|(code, name, rate, high)| {
            let rate =
                if rate.contains('.') { rate.trim_end_matches('0').trim_end_matches('.').to_string() } else { rate };
            RiskLevel { code, name, rate: RawNum(rate), high }
        })
        .collect())
}

/// 施策と、その段階が確度の高い段階か。
struct ActivityEntry {
    activity: RiskActivity,
    high: bool,
}

type ActivityRow = (i64, String, String, i64, Option<i64>, String, String, String, String);

async fn load_activities(conn: &mut mysql_async::Conn, levels: &[RiskLevel]) -> AppResult<Vec<ActivityEntry>> {
    let high: HashSet<&str> = levels.iter().filter(|l| l.high).map(|l| l.code.as_str()).collect();
    let rows: Vec<ActivityRow> = conn
        .query(
            "SELECT id, code, name, unit_id, owner_user_id, activity_type, status, confidence_level, COALESCE(assumptions, '')
             FROM activities ORDER BY code",
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(|(id, code, name, unit_id, owner_user_id, activity_type, status, confidence_level, assumptions)| {
            ActivityEntry {
                high: high.contains(confidence_level.as_str()),
                activity: RiskActivity {
                    id,
                    code,
                    name,
                    unit_id,
                    owner_user_id,
                    activity_type,
                    status,
                    confidence_level,
                    assumptions,
                    full: Pl::default(),
                    weighted: Pl::default(),
                    optimistic: Pl::default(),
                    pessimistic: Pl::default(),
                    actual: Pl::default(),
                    revenue_by_level: BTreeMap::new(),
                    revenue_by_month: BTreeMap::new(),
                    compare: None,
                    lines: Vec::new(),
                    warnings: Warnings::default(),
                    warning_count: 0,
                    bad_for_level: false,
                    conditions: BTreeMap::new(),
                    comment_count: 0,
                },
            }
        })
        .collect())
}

/// シナリオの金額の1行（施策 × 区分 × 実績/計画 × 段階 × 見通し × 月）。
struct AmountRow {
    activity_id: i64,
    category: String,
    kind: String,
    level: String,
    /// 標準の確率（SCALE 倍した整数）
    rate: i128,
    high: bool,
    outlook: String,
    month: String,
    /// 円
    amount: i128,
}

type RawAmountRow = (i64, String, String, String, i64, bool, String, String, String);

/// シナリオの金額を、内訳の段階・見通しの種類ごとに読む。
async fn amount_rows(pool: &Pool, u: &User, scenario_id: i64) -> AppResult<Vec<AmountRow>> {
    let mut conn = pool.get_conn().await?;
    let rows: Vec<RawAmountRow> = conn
        .exec(
            format!(
                "SELECT b.activity_id, s.category, b.kind, cl.code, CAST(ROUND(cl.rate * {SCALE}) AS SIGNED), cl.rate >= {HIGH_RATE}, COALESCE(l.outlook, 'base'),
                        DATE_FORMAT(b.target_month, '%Y-%m'), CAST(SUM(b.amount) AS CHAR)
                 FROM scenario_amounts b JOIN subjects s ON s.id = b.subject_id{JOINS}
                 WHERE b.scenario_id = ?{}
                 GROUP BY b.activity_id, s.category, b.kind, cl.code, cl.rate, l.outlook, b.target_month",
                visibility::subject_filter(u, "b.subject_id")
            ),
            (scenario_id,),
        )
        .await?;
    rows.into_iter()
        .map(|(activity_id, category, kind, level, rate, high, outlook, month, amount)| {
            Ok(AmountRow {
                activity_id,
                category,
                kind,
                level,
                rate: rate.into(),
                high,
                outlook,
                month,
                amount: amount.parse().map_err(|_| anyhow::anyhow!("invalid amount {amount:?}"))?,
            })
        })
        .collect()
}

/// 施策ごとの合計。
#[derive(Default)]
struct Totals {
    /// 指標ごとの期間の合計
    sums: [PlSum; 5],
    /// 指標ごとの年間の合計（シグナルに使う）
    year: [PlSum; 5],
    /// 円
    by_level: BTreeMap<String, i128>,
    by_month: BTreeMap<String, BTreeMap<String, i128>>,
    /// 計画値に確度の高い段階の内訳がある
    high: bool,
}

/// 金額の行を、施策ごとの指標（docs/plan.md「2.8」）に合計する。
fn accumulate(rows: &[AmountRow], in_period: &HashSet<String>) -> HashMap<i64, Totals> {
    let mut out: HashMap<i64, Totals> = HashMap::new();
    for r in rows {
        let t = out.entry(r.activity_id).or_default();
        let v = r.amount * SCALE;
        let mut values: [i128; 5] = [v, v, v, v, 0];
        if r.kind == "actual" {
            values[ACTUAL] = v;
        } else {
            values[WEIGHTED] = r.amount * r.rate;
            if r.outlook == "downside" {
                values[OPTIMISTIC] = 0;
            } else if !r.high {
                values[PESSIMISTIC] = 0;
            }
            t.high |= r.high;
        }
        let in_p = in_period.contains(&r.month);
        for (i, x) in values.into_iter().enumerate() {
            t.year[i].add(&r.category, x);
            if in_p {
                t.sums[i].add(&r.category, x);
            }
        }
        if in_p && r.category == "revenue" {
            let key = if r.kind == "actual" {
                "actual"
            } else if r.outlook == "downside" {
                "downside"
            } else {
                &r.level
            };
            *t.by_level.entry(key.to_string()).or_default() += r.amount;
            *t.by_month.entry(r.month.clone()).or_default().entry(key.to_string()).or_default() += r.amount;
        }
    }
    out
}

/// 内訳ごとの段階・見通しの種類と、期間の計画値の月の満額を読む。施策 → 内訳（科目・内訳の順）。
/// 2つ目の値は科目への直接入力の満額（科目 → 満額）で、direct_lines_last で科目ごとに1行にする。
async fn fetch_lines(
    pool: &Pool,
    u: &User,
    scenario_id: i64,
    months: &[String],
) -> AppResult<HashMap<i64, (Vec<RiskLine>, BTreeMap<i64, String>)>> {
    let mut conn = pool.get_conn().await?;
    // （施策, 科目, 内訳。0 は直接入力）→ 満額
    let mut amounts: HashMap<(i64, i64, i64), String> = HashMap::new();
    if !months.is_empty() {
        let mut args: Vec<Value> = vec![scenario_id.into()];
        args.extend(months.iter().map(|m| Value::from(format!("{m}-01"))));
        let rows: Vec<(i64, i64, i64, String)> = conn
            .exec(
                format!(
                    "SELECT activity_id, subject_id, COALESCE(line_id, 0), CAST(SUM(amount) AS CHAR)
                     FROM scenario_amounts WHERE scenario_id = ? AND kind = 'plan' AND target_month IN ({}){}
                     GROUP BY activity_id, subject_id, line_id",
                    placeholders(months.len()),
                    visibility::subject_filter(u, "subject_id")
                ),
                Params::Positional(args),
            )
            .await?;
        amounts = rows.into_iter().map(|(a, s, l, v)| ((a, s, l), v)).collect();
    }
    let rows: Vec<(i64, i64, i64, String, String, String)> = conn
        .query(format!(
            "SELECT l.id, l.activity_id, l.subject_id, l.name, COALESCE(l.confidence_level, a.confidence_level), l.outlook
             FROM activity_lines l JOIN activities a ON a.id = l.activity_id WHERE TRUE{} ORDER BY l.activity_id, l.subject_id, l.id",
            visibility::subject_filter(u, "l.subject_id")
        ))
        .await?;
    let mut out: HashMap<i64, (Vec<RiskLine>, BTreeMap<i64, String>)> = HashMap::new();
    for (id, aid, subject_id, name, confidence_level, outlook) in rows {
        let amount = amounts.get(&(aid, subject_id, id)).cloned().unwrap_or_else(|| "0".into());
        out.entry(aid).or_default().0.push(RiskLine {
            line_id: Some(id),
            subject_id,
            name,
            confidence_level,
            outlook,
            amount,
        });
    }
    for ((aid, subject, line), v) in amounts {
        if line == 0 {
            out.entry(aid).or_default().1.insert(subject, v);
        }
    }
    Ok(out)
}

/// 内訳に科目への直接入力（施策の段階・ベース）を足し、科目の順・内訳の順（直接入力は科目の最後）に並べる。
fn direct_lines_last((mut lines, direct): (Vec<RiskLine>, BTreeMap<i64, String>), level: &str) -> Vec<RiskLine> {
    for (subject_id, amount) in direct {
        lines.push(RiskLine {
            line_id: None,
            subject_id,
            name: "科目への直接入力".into(),
            confidence_level: level.to_string(),
            outlook: "base".into(),
            amount,
        });
    }
    lines.sort_by_key(|l| (l.subject_id, l.line_id.is_none(), l.line_id));
    lines
}

/// 加重見込の利益（SCALE 単位）が比較シナリオから 10% 以上かつ 10万円以上下がったら、その幅と率を返す。
fn downward(cur: i128, cmp: i128) -> Option<Downward> {
    let diff = cur - cmp;
    if diff >= 0 {
        return None;
    }
    let drop = -diff;
    if drop < i128::from(DOWNWARD_MIN_YEN) * SCALE {
        return None;
    }
    let base = cmp.abs();
    let rate = if base == 0 {
        BigInt::from(1000)
    } else {
        // 10% 未満なら下方修正としない: drop × 100 < base × 10
        if drop * 100 < base * i128::from(DOWNWARD_RATE_PCT) {
            return None;
        }
        rate_tenths(&Rat::from_integer(drop.into()), &Rat::from_integer(base.into()))
    };
    Some(Downward { diff: round_scaled(diff).to_string(), rate })
}

/// 見込の当たり具合: 基準の決算確定済みの直近3か月（比較シナリオでは計画値の月）で、比較シナリオの計画値と実績の差の率が 20% 以上の施策。
async fn fetch_accuracy(pool: &Pool, u: &User, s: &RiskScenario, c: &RiskScenario) -> AppResult<HashMap<i64, BigInt>> {
    let Some(through) = &s.actual_through else { return Ok(HashMap::new()) };
    // 比べるのは、基準では実績で、比較シナリオでは計画値の月だけ（比較シナリオでも実績の月は、計画値がないので比べない）
    let mut months: Vec<String> = calc::fiscal_months(s.fiscal_year)
        .into_iter()
        .filter(|m| m <= through && c.actual_through.as_ref().is_none_or(|ct| m > ct))
        .collect();
    if months.len() > ACCURACY_MONTHS {
        months.drain(..months.len() - ACCURACY_MONTHS);
    }
    if months.is_empty() {
        return Ok(HashMap::new());
    }
    let ph = placeholders(months.len());
    let filter = visibility::subject_filter(u, "subject_id");
    let month_args = || months.iter().map(|m| Value::from(format!("{m}-01")));
    let mut args: Vec<Value> = vec![c.id.into()];
    args.extend(month_args());
    args.push(s.id.into());
    args.extend(month_args());
    let mut conn = pool.get_conn().await?;
    // 比較シナリオの計画値（計画値の月だけ）と、基準の実績を、施策 × 科目 × 月で比べる
    let rows: Vec<(i64, String, String)> = conn
        .exec(
            format!(
                "SELECT activity_id, CAST(SUM(ABS(diff)) AS CHAR), CAST(SUM(ABS(plan)) AS CHAR)
                 FROM (
                     SELECT activity_id, subject_id, target_month, SUM(actual) - SUM(plan) AS diff, SUM(plan) AS plan
                     FROM (
                         SELECT activity_id, subject_id, target_month, amount AS plan, 0 AS actual
                         FROM scenario_amounts WHERE scenario_id = ? AND kind = 'plan' AND target_month IN ({ph}){filter}
                         UNION ALL
                         SELECT activity_id, subject_id, target_month, 0, amount
                         FROM scenario_amounts WHERE scenario_id = ? AND kind = 'actual' AND target_month IN ({ph}){filter}
                     ) x GROUP BY activity_id, subject_id, target_month
                 ) y GROUP BY activity_id"
            ),
            Params::Positional(args),
        )
        .await?;
    let mut out = HashMap::new();
    for (aid, d, p) in rows {
        let (Some(d), Some(p)) = (parse_decimal(&d), parse_decimal(&p)) else { continue };
        // 20% 以上: 差 × 100 ≥ 計画値 × 20
        if p.is_zero() || &d * Rat::from_integer(100.into()) < &p * Rat::from_integer(ACCURACY_RATE_PCT.into()) {
            continue;
        }
        out.insert(aid, rate_tenths(&d, &p));
    }
    Ok(out)
}

/// 比較シナリオの作成以降に期日を後ろにずらしたマイルストーンの変更の、施策ごとの回数と日数。
async fn fetch_postponed(pool: &Pool, since: NaiveDateTime) -> AppResult<HashMap<i64, Postponed>> {
    let mut conn = pool.get_conn().await?;
    let rows: Vec<(i64, i64, i64)> = conn
        .exec(
            "SELECT m.activity_id, COUNT(*),
                    CAST(COALESCE(SUM(DATEDIFF(JSON_UNQUOTE(JSON_EXTRACT(a.after_json, '$.due_date')), JSON_UNQUOTE(JSON_EXTRACT(a.before_json, '$.due_date')))), 0) AS SIGNED)
             FROM audit_logs a JOIN activity_milestones m ON m.id = a.record_id
             WHERE a.table_name = 'activity_milestones' AND a.action = 'update' AND a.created_at >= ?
               AND JSON_UNQUOTE(JSON_EXTRACT(a.after_json, '$.due_date')) > JSON_UNQUOTE(JSON_EXTRACT(a.before_json, '$.due_date'))
             GROUP BY m.activity_id",
            (since,),
        )
        .await?;
    Ok(rows.into_iter().map(|(aid, count, days)| (aid, Postponed { count, days })).collect())
}

/// 複製元をたどった直近の版（今回を除く、同じ年度）の、施策ごとの年間の加重見込の利益。版ごとの集計は並行して読む。
async fn consecutive_chain(pool: &Pool, u: &User, s: &RiskScenario) -> AppResult<Vec<HashMap<i64, i128>>> {
    let mut prevs: Vec<i64> = Vec::new();
    let mut base = s.base_id;
    while prevs.len() < CONSECUTIVE_DEPTH - 1
        && let Some(id) = base
    {
        let Some(prev) = find_scenario(pool, id).await? else { break };
        if prev.fiscal_year != s.fiscal_year {
            break;
        }
        prevs.push(prev.id);
        base = prev.base_id;
    }
    let none = HashSet::new();
    let totals = futures::future::try_join_all(prevs.iter().map(|id| amount_rows(pool, u, *id))).await?;
    Ok(totals
        .iter()
        .map(|rows| accumulate(rows, &none).into_iter().map(|(id, t)| (id, t.year[WEIGHTED].profit())).collect())
        .collect())
}

/// 完了していないマイルストーンのうち、期日超過・遅延・期日が近いもの（施策 → 期日の順）。
async fn fetch_milestones(pool: &Pool, today: NaiveDate) -> AppResult<HashMap<i64, Vec<RiskMilestone>>> {
    let mut conn = pool.get_conn().await?;
    let soon = today + Duration::days(UPCOMING_DAYS);
    let rows: Vec<(i64, String, String, String)> = conn
        .exec(
            "SELECT activity_id, name, DATE_FORMAT(due_date, '%Y-%m-%d'), status
             FROM activity_milestones
             WHERE status <> 'completed' AND (status = 'delayed' OR due_date <= ?)
             ORDER BY due_date, id",
            (soon,),
        )
        .await?;
    let today = today.format("%Y-%m-%d").to_string();
    let mut out: HashMap<i64, Vec<RiskMilestone>> = HashMap::new();
    for (aid, name, due_date, status) in rows {
        let risk = if due_date < today {
            "overdue"
        } else if status == "delayed" {
            "delayed"
        } else {
            "upcoming"
        };
        out.entry(aid).or_default().push(RiskMilestone { name, due_date, status, risk });
    }
    Ok(out)
}

/// 施策 → 説明（基準シナリオ）、施策 → 説明（比較シナリオ）、施策 → コメントの件数（基準シナリオ）。
type Conditions = (HashMap<i64, String>, HashMap<i64, String>, HashMap<i64, i64>);

/// 基準・比較シナリオの、今回の見込の説明（docs/plan.md「2.18」で想定条件を統合）と、基準シナリオのコメントの件数。
async fn fetch_conditions(pool: &Pool, scenario_id: i64, compare_id: Option<i64>) -> AppResult<Conditions> {
    let mut conn = pool.get_conn().await?;
    const NOTES: &str = "SELECT activity_id, explanation FROM activity_scenario_notes WHERE scenario_id = ? AND COALESCE(explanation, '') <> ''";
    let notes: Vec<(i64, String)> = conn.exec(NOTES, (scenario_id,)).await?;
    let compare_notes: Vec<(i64, String)> = match compare_id {
        Some(id) => conn.exec(NOTES, (id,)).await?,
        None => Vec::new(),
    };
    let counts: Vec<(i64, i64)> = conn
        .exec(
            "SELECT activity_id, COUNT(*) FROM activity_scenario_comments WHERE scenario_id = ? AND deleted_at IS NULL GROUP BY activity_id",
            (scenario_id,),
        )
        .await?;
    Ok((notes.into_iter().collect(), compare_notes.into_iter().collect(), counts.into_iter().collect()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rat(n: i64) -> Rat {
        Rat::from_integer(n.into())
    }

    /// 円を SCALE 単位にする。
    fn yen(n: i64) -> i128 {
        i128::from(n) * SCALE
    }

    #[test]
    fn downward_thresholds() {
        // 1,000,000 → 500,000 は 50% の低下
        let d = downward(yen(500_000), yen(1_000_000)).unwrap();
        assert_eq!((d.diff.as_str(), d.rate.to_string().as_str()), ("-500000", "500"));
        // 10万円未満・10% 未満は下方修正としない
        assert!(downward(yen(950_000), yen(1_000_000)).is_none());
        assert!(downward(yen(10_000_000 - 900_000), yen(10_000_000)).is_none());
        assert!(downward(yen(2_000_000), yen(1_000_000)).is_none());
        // 比較の利益が 0 なら 100%
        assert_eq!(downward(yen(-200_000), yen(0)).unwrap().rate, BigInt::from(1000));
        // ちょうど 10%
        assert!(downward(yen(9_000_000), yen(10_000_000)).is_some());
    }

    #[test]
    fn rounding() {
        // 円未満は四捨五入（0 から遠い方へ）。加重見込の 0.5 円も誤差なく扱える
        assert_eq!(round_scaled(12_345), 1);
        assert_eq!(round_scaled(15_000), 2);
        assert_eq!(round_scaled(-15_000), -2);
        assert_eq!(round_scaled(14_999), 1);
        assert_eq!(round_scaled(333 * 5_000), 167); // 333 円 × 0.5 = 166.5 円
    }

    #[test]
    fn rates() {
        assert_eq!(rate_tenths(&rat(1), &rat(3)).to_string(), "333");
        assert_eq!(rate_tenths(&rat(1), &rat(8)).to_string(), "125");
        let s = serde_json::to_string(&Downward { diff: "-1".into(), rate: BigInt::from(500) }).unwrap();
        assert_eq!(s, r#"{"diff":"-1","rate":50}"#);
        let s = serde_json::to_string(&Downward { diff: "-1".into(), rate: BigInt::from(125) }).unwrap();
        assert_eq!(s, r#"{"diff":"-1","rate":12.5}"#);
    }
}
