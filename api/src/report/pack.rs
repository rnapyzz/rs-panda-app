//! 報告資料（docs/plan.md「2.23」）のデータの読み込み。シートの組み立ては pack_sheets で行う。

use std::collections::{HashMap, HashSet};

use axum::extract::State;
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use chrono::NaiveDateTime;
use mysql_async::Conn;
use mysql_async::prelude::*;
use num_bigint::BigInt;

use super::pack_sheets::{self, GRAINS, SHEETS};
use super::{
    Amounts, id_args, load_actuals, load_amounts, load_scenarios, load_unallocated, parse_big, parse_measure,
    placeholders, scenario_ids,
};
use crate::auth::{CurrentUser, User};
use crate::error::{AppError, bad_request, validation1};
use crate::http::Query;
use crate::scenario::target_condition;
use crate::{AppResult, AppState, calc, jst, visibility, xlsx};

/// 報告資料の系列（シナリオ、または実績）。
pub struct PackSeries {
    pub label: String,
    /// シナリオの決算確定月（実績の系列は None）
    pub actual_through: Option<String>,
}

/// 科目体系の1つの科目。
pub struct SubjectNode {
    pub id: i64,
    pub name: String,
    /// revenue / expense
    pub category: String,
    pub children: Vec<SubjectNode>,
}

pub struct PackUnit {
    pub id: i64,
    pub name: String,
    pub segment_id: i64,
    pub archived: bool,
}

/// セグメント・組織の1つのノード（ルートは parent_id 0）。
pub struct PackNode {
    pub id: i64,
    pub parent_id: i64,
    pub name: String,
}

pub struct PackActivity {
    pub id: i64,
    pub unit_id: i64,
    pub code: String,
    pub name: String,
}

/// 変動の説明の1行。差は年間の利益（今回 − 比較先）。比較先のシナリオがなければ None。
pub struct NoteRow {
    pub code: String,
    pub name: String,
    pub unit: String,
    pub owner: String,
    pub status: &'static str,
    pub current: BigInt,
    pub base_diff: BigInt,
    pub previous_diff: Option<BigInt>,
    pub causes: Vec<String>,
    pub explanation: String,
}

/// 変動の説明のシート。作成中のシナリオがなければ None にする。
pub struct PackNotes {
    pub scenario: String,
    pub base: String,
    pub previous: String,
    pub rows: Vec<NoteRow>,
}

/// 報告資料を作るのに使うデータ。
pub struct PackInput {
    pub fiscal_year: i32,
    pub measure: String,
    pub grain: String,
    pub months: Vec<String>,
    pub series: Vec<PackSeries>,
    /// values[i] は系列 i の、ユニット（施策）× 科目 × 月の金額。範囲で絞り込み済み。ユニット ID 0 は未割当の実績
    pub values: Vec<Amounts>,
    /// ルートの科目（並び順）
    pub subjects: Vec<SubjectNode>,
    /// 範囲のユニット（コード順）
    pub units: Vec<PackUnit>,
    /// セグメント（並び順）
    pub segments: Vec<PackNode>,
    /// 施策（コード順）。ユニット別 P/L に施策を含めるときだけ
    pub activities: Vec<PackActivity>,
    pub with_activities: bool,
    /// 全社（未割当の実績を含む）
    pub all_units: bool,
    pub scope_label: String,
    pub sheets: Vec<&'static str>,
    pub restricted_hidden: bool,
    pub user_name: String,
    pub now: NaiveDateTime,
    pub notes: Option<PackNotes>,
}

/// 範囲。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    All,
    Segment(i64),
    Organization(i64),
    Unit(i64),
}

fn parse_scope(q: &Query) -> Result<Scope, AppError> {
    let mut scope = Scope::All;
    for kind in ["segment", "organization", "unit"] {
        let s = q.get(&format!("{kind}_id"));
        if s.is_empty() {
            continue;
        }
        if scope != Scope::All {
            return Err(bad_request("範囲は segment_id / organization_id / unit_id のどれか1つで指定してください"));
        }
        let id = match s.parse::<i64>() {
            Ok(id) if id > 0 => id,
            _ => return Err(bad_request(format!("{kind}_id は数値で指定してください"))),
        };
        scope = match kind {
            "segment" => Scope::Segment(id),
            "organization" => Scope::Organization(id),
            _ => Scope::Unit(id),
        };
    }
    Ok(scope)
}

/// 出すシートを読む（既定はすべて。重なりは除き、指定の順）。
fn parse_sheets(q: &Query) -> Result<Vec<&'static str>, AppError> {
    let s = q.get("sheets");
    if s.is_empty() {
        return Ok(SHEETS.to_vec());
    }
    let mut out = Vec::new();
    for p in s.split(',').map(str::trim) {
        let Some(sheet) = SHEETS.iter().find(|x| **x == p) else {
            return Err(bad_request("sheets は pl / units / notes をカンマ区切りで指定してください"));
        };
        if !out.contains(sheet) {
            out.push(*sheet);
        }
    }
    Ok(out)
}

/// GET /api/reports/pack.xlsx（報告資料、docs/plan.md「2.23」）。
///
/// クエリパラメーター:
///   - scenario_ids・include_actual・measure: 予実比較と同じ
///   - grain: month / quarter / half / year（全社 P/L の列の期間）
///   - segment_id / organization_id / unit_id: 範囲（どれか1つ。指定しなければ全社で、未割当の実績を含む）
///   - sheets: 出すシート（pl,units,notes のカンマ区切り。既定はすべて）
///   - activities=true: ユニット別 P/L に施策を含める
pub async fn pack(State(st): State<AppState>, CurrentUser(u): CurrentUser, q: Query) -> AppResult<Response> {
    let ids = scenario_ids(&q, "シナリオを選んでください", "選べるシナリオは")?;
    let measure = parse_measure(&q)?;
    let grain = match q.get("grain") {
        "" => "month",
        g => g,
    };
    if !GRAINS.contains(&grain) {
        return Err(bad_request("grain は month / quarter / half / year のいずれかを指定してください"));
    }
    let sheets = parse_sheets(&q)?;
    let scope = parse_scope(&q)?;

    let mut conn = st.db.get_conn().await?;
    let (infos, fiscal_year) = load_scenarios(&mut conn, &ids).await?;
    let mut series: Vec<PackSeries> = ids
        .iter()
        .map(|id| PackSeries { label: infos[id].name.clone(), actual_through: infos[id].actual_through.clone() })
        .collect();
    let include_actual = q.get("include_actual") == "true";
    if include_actual {
        series.push(PackSeries { label: "実績".into(), actual_through: None });
    }
    let months = calc::fiscal_months(fiscal_year);
    let restricted_hidden = visibility::hidden(&mut conn, &u).await?;

    // マスタと範囲
    let subjects = load_subjects(&mut conn, &u).await?;
    let segments = load_tree(&mut conn, "segments").await?;
    let units = load_units(&mut conn).await?;
    let (units, in_scope, scope_label) = apply_scope(&mut conn, units, &segments, scope).await?;
    let all_units = scope == Scope::All;

    // 金額（予実比較と同じ読み込み）。範囲のユニットに絞る
    let with_activities = q.get("activities") == "true";
    let by_activity = with_activities && sheets.contains(&"units");
    let unit_id = match scope {
        Scope::Unit(id) => Some(id),
        _ => None,
    };
    let keep = |src: Amounts| -> Amounts { src.into_iter().filter(|(k, _)| in_scope.contains(&k.0)).collect() };
    let mut amounts = load_amounts(&mut conn, &u, &ids, unit_id, by_activity, &measure).await?;
    let mut values: Vec<Amounts> = ids.iter().map(|id| keep(amounts.remove(id).unwrap_or_default())).collect();
    if include_actual {
        values.push(keep(load_actuals(&mut conn, &u, &months, unit_id, by_activity).await?));
    }
    if all_units {
        let mut unallocated = load_unallocated(&mut conn, &u, &ids, &months, include_actual).await?;
        for (i, id) in ids.iter().enumerate() {
            values[i].extend(keep(unallocated.remove(id).unwrap_or_default()));
        }
        if include_actual {
            let last = values.len() - 1;
            values[last].extend(keep(unallocated.remove(&0).unwrap_or_default()));
        }
    }
    let activities = if by_activity { load_activities(&mut conn).await? } else { Vec::new() };
    let notes = if sheets.contains(&"notes") { load_notes(&mut conn, &u, fiscal_year, &in_scope).await? } else { None };

    let input = PackInput {
        fiscal_year,
        measure,
        grain: grain.to_string(),
        months,
        series,
        values,
        subjects,
        units,
        segments,
        activities,
        with_activities,
        all_units,
        scope_label,
        sheets,
        restricted_hidden,
        user_name: u.name.clone(),
        now: jst::now(),
        notes,
    };
    let body = xlsx::write(&pack_sheets::build(&input))?;
    let date = input.now.format("%Y%m%d");
    let disposition = format!(
        "attachment; filename=\"report_{fiscal_year}_{date}.xlsx\"; filename*=UTF-8''{}",
        percent_encode(&format!("報告資料_{fiscal_year}_{date}.xlsx"))
    );
    let mut res = body.into_response();
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
    );
    h.insert(header::CONTENT_DISPOSITION, HeaderValue::from_str(&disposition).map_err(anyhow::Error::from)?);
    Ok(res)
}

/// RFC 5987 の形（英数字と -._~ 以外のバイトを %XX）にする。
fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// 科目体系（見られる科目だけ）。親が見られない科目は、ルートとして出す。
async fn load_subjects(conn: &mut Conn, u: &User) -> AppResult<Vec<SubjectNode>> {
    let rows: Vec<(i64, Option<i64>, String, String)> = conn
        .query(format!(
            "SELECT id, parent_id, name, category FROM subjects WHERE TRUE{} ORDER BY sort_order, code",
            visibility::subject_filter(u, "id")
        ))
        .await?;
    let visible: HashSet<i64> = rows.iter().map(|r| r.0).collect();
    let mut children: HashMap<i64, Vec<(i64, String, String)>> = HashMap::new();
    let mut roots = Vec::new();
    for (id, parent, name, category) in rows {
        match parent.filter(|p| visible.contains(p)) {
            Some(p) => children.entry(p).or_default().push((id, name, category)),
            None => roots.push((id, name, category)),
        }
    }
    fn build(
        (id, name, category): (i64, String, String),
        children: &mut HashMap<i64, Vec<(i64, String, String)>>,
    ) -> SubjectNode {
        let kids = children.remove(&id).unwrap_or_default();
        SubjectNode { id, name, category, children: kids.into_iter().map(|c| build(c, children)).collect() }
    }
    Ok(roots.into_iter().map(|r| build(r, &mut children)).collect())
}

/// セグメントか組織の階層（並び順）。
async fn load_tree(conn: &mut Conn, table: &str) -> AppResult<Vec<PackNode>> {
    let rows: Vec<(i64, i64, String)> =
        conn.query(format!("SELECT id, COALESCE(parent_id, 0), name FROM {table} ORDER BY sort_order, id")).await?;
    Ok(rows.into_iter().map(|(id, parent_id, name)| PackNode { id, parent_id, name }).collect())
}

async fn load_units(conn: &mut Conn) -> AppResult<Vec<PackUnit>> {
    let rows: Vec<(i64, String, i64, bool)> =
        conn.query("SELECT id, name, segment_id, is_archived FROM units ORDER BY code").await?;
    Ok(rows.into_iter().map(|(id, name, segment_id, archived)| PackUnit { id, name, segment_id, archived }).collect())
}

/// ノードとその子孫の ID。
fn descendants(nodes: &[PackNode], root: i64) -> HashSet<i64> {
    let mut under = HashSet::from([root]);
    loop {
        let before = under.len();
        for n in nodes {
            if under.contains(&n.parent_id) {
                under.insert(n.id);
            }
        }
        if under.len() == before {
            return under;
        }
    }
}

/// 範囲に入るユニットか。
type UnitFilter = Box<dyn Fn(&PackUnit) -> bool + Send>;

/// 範囲のユニットに絞り、範囲の名前を付ける。返り値は（範囲のユニット、範囲のユニット ID（全社なら未割当の 0 を含む）、範囲の名前）。
async fn apply_scope(
    conn: &mut Conn,
    units: Vec<PackUnit>,
    segments: &[PackNode],
    scope: Scope,
) -> AppResult<(Vec<PackUnit>, HashSet<i64>, String)> {
    let (field, label, name, matched): (&str, &str, Option<String>, UnitFilter) = match scope {
        Scope::All => {
            let mut ids: HashSet<i64> = units.iter().map(|un| un.id).collect();
            ids.insert(0);
            return Ok((units, ids, "全社".into()));
        }
        Scope::Unit(id) => {
            let name = units.iter().find(|un| un.id == id).map(|un| un.name.clone());
            ("unit_id", "ユニット", name, Box::new(move |un: &PackUnit| un.id == id))
        }
        Scope::Segment(id) => {
            let name = segments.iter().find(|n| n.id == id).map(|n| n.name.clone());
            let under = descendants(segments, id);
            ("segment_id", "セグメント", name, Box::new(move |un: &PackUnit| under.contains(&un.segment_id)))
        }
        Scope::Organization(id) => {
            let orgs = load_tree(conn, "organizations").await?;
            let name = orgs.iter().find(|n| n.id == id).map(|n| n.name.clone());
            let under = descendants(&orgs, id);
            // ユニットの組織は units から読む（PackUnit は持たない）
            let rows: Vec<(i64, i64)> = conn.query("SELECT id, organization_id FROM units").await?;
            let ids: HashSet<i64> = rows.into_iter().filter(|(_, o)| under.contains(o)).map(|(id, _)| id).collect();
            ("organization_id", "組織", name, Box::new(move |un: &PackUnit| ids.contains(&un.id)))
        }
    };
    let Some(name) = name else {
        return Err(validation1(field, "範囲が見つかりません"));
    };
    let units: Vec<PackUnit> = units.into_iter().filter(|un| matched(un)).collect();
    let ids = units.iter().map(|un| un.id).collect();
    Ok((units, ids, format!("{label}: {name}")))
}

async fn load_activities(conn: &mut Conn) -> AppResult<Vec<PackActivity>> {
    let rows: Vec<(i64, i64, String, String)> =
        conn.query("SELECT id, unit_id, code, name FROM activities ORDER BY code").await?;
    Ok(rows.into_iter().map(|(id, unit_id, code, name)| PackActivity { id, unit_id, code, name }).collect())
}

type NoteDbRow = (i64, i64, String, String, String, String, String, String, bool, bool);

/// 変動の説明のシートのデータ。今回の見込は年度の作成中のシナリオ、
/// 目標は修正計画（なければ期初計画、なければ最初に作ったシナリオ。ホームと同じ）、前回の見込はシナリオの前回見込。
/// 対象の施策はホームと同じ（更新の対象）で、範囲のユニットに絞る。
async fn load_notes(
    conn: &mut Conn,
    u: &User,
    fiscal_year: i32,
    in_scope: &HashSet<i64>,
) -> AppResult<Option<PackNotes>> {
    let current: Option<(i64, String, Option<i64>)> = conn
        .exec_first(
            "SELECT id, name, previous_scenario_id FROM scenarios WHERE is_active AND fiscal_year = ?",
            (fiscal_year,),
        )
        .await?;
    let Some((current_id, scenario, previous_id)) = current else { return Ok(None) };
    let (base_id, base): (i64, String) = conn
        .exec_first(
            "SELECT id, name FROM scenarios WHERE fiscal_year = ?
             ORDER BY CASE plan_role WHEN 'revised' THEN 0 WHEN 'initial' THEN 1 ELSE 2 END, id LIMIT 1",
            (fiscal_year,),
        )
        .await?
        .ok_or_else(|| anyhow::anyhow!("no scenario in {fiscal_year}"))?;
    let mut ids = vec![current_id, base_id];
    let mut previous = String::new();
    if let Some(pid) = previous_id {
        previous = conn
            .exec_first("SELECT name FROM scenarios WHERE id = ?", (pid,))
            .await?
            .ok_or_else(|| anyhow::anyhow!("scenario {pid} not found"))?;
        ids.push(pid);
    }
    let profits = annual_profits(conn, u, &ids).await?;
    let profit_of = |sid: i64, aid: i64| profits.get(&(sid, aid)).cloned().unwrap_or_default();

    let rows: Vec<NoteDbRow> = conn
        .exec(
            format!(
                "SELECT a.id, a.unit_id, a.code, a.name, un.name, COALESCE(ow.name, ''),
                        COALESCE(n.explanation, ''), COALESCE(n.causes, ''), n.last_edited_at IS NOT NULL, n.completed_at IS NOT NULL
                 FROM activities a
                 JOIN units un ON un.id = a.unit_id
                 LEFT JOIN users ow ON ow.id = a.owner_user_id
                 LEFT JOIN activity_scenario_notes n ON n.activity_id = a.id AND n.scenario_id = ?
                 WHERE {}",
                target_condition("a")
            ),
            (current_id, current_id),
        )
        .await?;
    let mut out = Vec::new();
    for (id, unit_id, code, name, unit, owner, explanation, causes, edited, completed) in rows {
        if !in_scope.contains(&unit_id) {
            continue;
        }
        let current = profit_of(current_id, id);
        out.push(NoteRow {
            code,
            name,
            unit,
            owner,
            status: if completed {
                "completed"
            } else if edited {
                "in_progress"
            } else {
                "not_started"
            },
            base_diff: &current - profit_of(base_id, id),
            previous_diff: previous_id.map(|pid| &current - profit_of(pid, id)),
            current,
            causes: if causes.is_empty() { Vec::new() } else { causes.split(',').map(str::to_string).collect() },
            explanation,
        });
    }
    pack_sheets::sort_notes(&mut out);
    Ok(Some(PackNotes { scenario, base, previous, rows: out }))
}

/// （シナリオ, 施策）→ 年間の利益（満額、決算確定月以前は実績）。見られない科目は除く。
async fn annual_profits(conn: &mut Conn, u: &User, ids: &[i64]) -> AppResult<HashMap<(i64, i64), BigInt>> {
    let rows: Vec<(i64, i64, String)> = conn
        .exec(
            format!(
                "SELECT b.scenario_id, b.activity_id, CAST(SUM(CASE WHEN s.category = 'revenue' THEN b.amount ELSE -b.amount END) AS CHAR)
                 FROM scenario_amounts b JOIN subjects s ON s.id = b.subject_id
                 WHERE b.scenario_id IN ({}){}
                 GROUP BY b.scenario_id, b.activity_id",
                placeholders(ids.len()),
                visibility::subject_filter(u, "b.subject_id")
            ),
            mysql_async::Params::Positional(id_args(ids)),
        )
        .await?;
    rows.into_iter().map(|(sid, aid, v)| Ok(((sid, aid), parse_big(&v)?))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_filename() {
        assert_eq!(percent_encode("報告資料_2026.xlsx"), "%E5%A0%B1%E5%91%8A%E8%B3%87%E6%96%99_2026.xlsx");
    }

    #[test]
    fn scopes() {
        let q = Query::parse("segment_id=1&unit_id=2");
        assert!(parse_scope(&q).is_err());
        assert!(parse_scope(&Query::parse("unit_id=x")).is_err());
        assert!(parse_scope(&Query::parse("organization_id=3")).unwrap() == Scope::Organization(3));
        let nodes = vec![
            PackNode { id: 1, parent_id: 0, name: "a".into() },
            PackNode { id: 2, parent_id: 1, name: "b".into() },
            PackNode { id: 3, parent_id: 2, name: "c".into() },
            PackNode { id: 4, parent_id: 0, name: "d".into() },
        ];
        assert_eq!(descendants(&nodes, 2), HashSet::from([2, 3]));
    }
}
