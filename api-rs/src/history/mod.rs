//! 変更履歴（変更セットと監査ログ）を参照する API。
//!
//! FP&A と経営陣・レビュアーはすべての履歴を見られる（数値の根拠と変更理由を確認できるようにするため）。
//! それ以外のロールは、activity_id で指定した、自分が編集できる施策の履歴だけを見られる（docs/plan.md「2.17」）。
//! 名前は必要な ID の分だけを読む（表全体は読まない）。

mod label;

use std::collections::{BTreeMap, BTreeSet, HashMap};

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use chrono::{Duration, NaiveDateTime};
use mysql_async::prelude::*;
use mysql_async::{Conn, Params};
use serde::Serialize;
use serde_json::value::RawValue;

use crate::auth::{CurrentUser, ROLE_FPA_ADMIN, ROLE_VIEWER, User};
use crate::db::Value;
use crate::error::{AppError, bad_request, not_found};
use crate::http::{self, Id, Query};
use crate::{AppResult, AppState, activity, jst};

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 200;
/// 一覧に表示する、変更セットごとの施策の数の上限
const MAX_ACTIVITIES_PER_SET: usize = 5;

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/change-sets", get(list)).route("/api/change-sets/{id}", get(get_one))
}

#[derive(Clone, Debug, Serialize)]
struct Ref {
    id: i64,
    name: String,
}

#[derive(Clone, Debug, Serialize)]
struct ActivityRef {
    id: i64,
    code: String,
    name: String,
}

/// 変更セット。tables・activities は一覧だけで埋める（詳細では null）。
#[derive(Clone, Debug, Serialize)]
struct ChangeSet {
    id: i64,
    #[serde(serialize_with = "jst::ser")]
    created_at: NaiveDateTime,
    user: Ref,
    scenario: Option<Ref>,
    reason: String,
    changes: i64,
    tables: Option<BTreeMap<String, i64>>,
    activities: Option<Vec<ActivityRef>>,
    /// activities に含めきれなかった施策の数
    more_activities: usize,
}

type ChangeSetRow = (i64, NaiveDateTime, i64, String, Option<i64>, Option<String>, String, i64);

fn change_set_of((id, created_at, uid, uname, sid, sname, reason, changes): ChangeSetRow) -> ChangeSet {
    ChangeSet {
        id,
        created_at,
        user: Ref { id: uid, name: uname },
        scenario: sid.map(|id| Ref { id, name: sname.unwrap_or_default() }),
        reason,
        changes,
        tables: None,
        activities: None,
        more_activities: 0,
    }
}

/// 記録に activity_id を持つテーブル。
const ACTIVITY_TABLES: [&str; 9] = [
    "activity_external_codes",
    "activity_milestones",
    "activity_drivers",
    "activity_lines",
    "activity_formulas",
    "budget_facts",
    "actual_facts",
    "scenario_conditions",
    "activity_scenario_notes",
];

/// 監査ログ a が施策 ? に関係するかを判定する SQL 条件。引数を3つ取る。
fn activity_filter() -> String {
    format!(
        "(
        (a.table_name = 'activities' AND a.record_id = ?)
        OR (a.table_name IN ({})
            AND CAST(COALESCE(JSON_EXTRACT(a.after_json, '$.activity_id'), JSON_EXTRACT(a.before_json, '$.activity_id')) AS UNSIGNED) = ?)
        OR (a.table_name = 'driver_values'
            AND CAST(COALESCE(JSON_EXTRACT(a.after_json, '$.activity_driver_id'), JSON_EXTRACT(a.before_json, '$.activity_driver_id')) AS UNSIGNED)
                IN (SELECT id FROM activity_drivers WHERE activity_id = ?))
    )",
        ACTIVITY_TABLES.iter().map(|t| format!("'{t}'")).collect::<Vec<_>>().join(", ")
    )
}

/// 監査ログ a が閲覧制限のある科目の金額・内訳（docs/plan.md「2.17」）かを判定する SQL 条件。
const RESTRICTED_LOG: &str = "(a.table_name IN ('budget_facts', 'actual_facts', 'scenario_actuals', 'activity_lines')
    AND CAST(COALESCE(JSON_EXTRACT(a.after_json, '$.subject_id'), JSON_EXTRACT(a.before_json, '$.subject_id')) AS UNSIGNED)
        IN (SELECT id FROM subjects WHERE is_restricted))";

/// ユーザーが見られる変更履歴の範囲。None はすべて、Some は施策（自分が編集できるもの）の履歴だけ。
async fn scope_of(conn: &mut Conn, u: &User, q: &Query) -> AppResult<Option<i64>> {
    if u.role == ROLE_FPA_ADMIN || u.role == ROLE_VIEWER {
        return Ok(None);
    }
    let s = q.get("activity_id");
    if s.is_empty() {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "変更履歴は FP&A と経営陣が見られます。施策の画面から、編集できる施策の履歴を開いてください",
        ));
    }
    let id: i64 = s.parse().map_err(|_| bad_request("activity_id は数値で指定してください"))?;
    if !activity::load(conn, u, id).await?.can_edit {
        return Err(AppError::new(StatusCode::FORBIDDEN, "forbidden", "編集できる施策の変更履歴だけを見られます"));
    }
    Ok(Some(id))
}

const CHANGE_SET_SELECT: &str = "SELECT c.id, c.created_at, u.id, u.name, s.id, s.name, COALESCE(c.reason, ''),
       (SELECT COUNT(*) FROM audit_logs a WHERE a.change_set_id = c.id)
    FROM change_sets c
    JOIN users u ON u.id = c.user_id
    LEFT JOIN scenarios s ON s.id = c.scenario_id";

/// GET /api/change-sets。新しい順に返す。
///
/// クエリパラメーター:
///   - scenario_id / activity_id / user_id: 絞り込み
///   - reason: with（理由あり）/ without（理由なし）
///   - from / to: 期間（YYYY-MM-DD、to の日を含む）
///   - before_id: この ID より古いものを返す（続きの取得）
///   - limit: 件数（既定50、最大200）
async fn list(State(st): State<AppState>, CurrentUser(u): CurrentUser, q: Query) -> AppResult<Response> {
    let mut conn = st.db.get_conn().await?;
    let scope = scope_of(&mut conn, &u, &q).await?;

    // 監査ログが1件もない変更セット（変更がなかった操作）は表示しない
    let mut conds = vec!["EXISTS (SELECT 1 FROM audit_logs a WHERE a.change_set_id = c.id)".to_string()];
    let mut args: Vec<Value> = Vec::new();
    for (param, cond) in [("scenario_id", "c.scenario_id = ?"), ("user_id", "c.user_id = ?"), ("before_id", "c.id < ?")]
    {
        let s = q.get(param);
        if s.is_empty() {
            continue;
        }
        let id: i64 = s.parse().map_err(|_| bad_request(format!("{param} は数値で指定してください")))?;
        conds.push(cond.into());
        args.push(id.into());
    }
    let s = q.get("activity_id");
    if !s.is_empty() {
        let id: i64 = s.parse().map_err(|_| bad_request("activity_id は数値で指定してください"))?;
        let mut cond =
            format!("EXISTS (SELECT 1 FROM audit_logs a WHERE a.change_set_id = c.id AND {}", activity_filter());
        if scope.is_some() {
            // 閲覧制限のある科目の変更だけの変更セットは出さない
            cond.push_str(&format!(" AND NOT {RESTRICTED_LOG}"));
        }
        conds.push(format!("{cond})"));
        args.extend([id.into(), id.into(), id.into()]);
    }
    match q.get("reason") {
        "" => {}
        "with" => conds.push("c.reason IS NOT NULL AND c.reason <> ''".into()),
        "without" => conds.push("(c.reason IS NULL OR c.reason = '')".into()),
        _ => return Err(bad_request("reason は with または without を指定してください")),
    }
    for (param, op) in [("from", ">="), ("to", "<")] {
        let s = q.get(param);
        if s.is_empty() {
            continue;
        }
        let d = jst::parse_date(s).ok_or_else(|| bad_request(format!("{param} は YYYY-MM-DD で指定してください")))?;
        // to の日を含める
        let d = if param == "to" { d + Duration::days(1) } else { d };
        conds.push(format!("c.created_at {op} ?"));
        args.push(d.into());
    }
    let limit = match q.get("limit") {
        "" => DEFAULT_LIMIT,
        s => match s.parse::<i64>() {
            Ok(n) if n > 0 => n.min(MAX_LIMIT),
            _ => return Err(bad_request("limit は正の整数で指定してください")),
        },
    };
    args.push((limit + 1).into());
    let rows: Vec<ChangeSetRow> = conn
        .exec(
            format!("{CHANGE_SET_SELECT} WHERE {} ORDER BY c.id DESC LIMIT ?", conds.join(" AND ")),
            Params::Positional(args),
        )
        .await?;
    let has_more = rows.len() as i64 > limit;
    let mut items: Vec<ChangeSet> = rows.into_iter().take(limit as usize).map(change_set_of).collect();
    fill_summaries(&mut conn, &mut items).await?;
    Ok(http::ok(&serde_json::json!({"items": items, "has_more": has_more})))
}

fn placeholders(n: usize) -> String {
    vec!["?"; n].join(",")
}

fn ids_of<'a>(ids: impl IntoIterator<Item = &'a i64>) -> Vec<Value> {
    ids.into_iter().map(|id| Value::from(*id)).collect()
}

/// 監査ログの（変更セット, テーブル, レコード, 施策, ドライバー）。
type LogRef = (i64, String, i64, Option<i64>, Option<i64>);

/// 変更セットごとに、テーブル別の件数と関係する施策を埋める。
async fn fill_summaries(conn: &mut Conn, items: &mut [ChangeSet]) -> AppResult<()> {
    if items.is_empty() {
        return Ok(());
    }
    let cs_ids: Vec<i64> = items.iter().map(|c| c.id).collect();
    let rows: Vec<LogRef> = conn
        .exec(
            format!(
                "SELECT a.change_set_id, a.table_name, a.record_id,
                        CAST(COALESCE(JSON_EXTRACT(a.after_json, '$.activity_id'), JSON_EXTRACT(a.before_json, '$.activity_id')) AS UNSIGNED),
                        CAST(COALESCE(JSON_EXTRACT(a.after_json, '$.activity_driver_id'), JSON_EXTRACT(a.before_json, '$.activity_driver_id')) AS UNSIGNED)
                 FROM audit_logs a WHERE a.change_set_id IN ({})",
                placeholders(cs_ids.len())
            ),
            Params::Positional(ids_of(&cs_ids)),
        )
        .await?;
    let mut tables: HashMap<i64, BTreeMap<String, i64>> = HashMap::new();
    let mut activity_sets: HashMap<i64, BTreeSet<i64>> = HashMap::new();
    let mut driver_sets: HashMap<i64, BTreeSet<i64>> = HashMap::new();
    for (cs, table, record_id, activity_id, driver_id) in rows {
        *tables.entry(cs).or_default().entry(table.clone()).or_default() += 1;
        if table == "activities" {
            activity_sets.entry(cs).or_default().insert(record_id);
        } else if table == "driver_values" {
            if let Some(d) = driver_id {
                driver_sets.entry(cs).or_default().insert(d);
            }
        } else if let Some(a) = activity_id.filter(|_| ACTIVITY_TABLES.contains(&table.as_str())) {
            activity_sets.entry(cs).or_default().insert(a);
        }
    }

    // ドライバー → 施策（使うドライバーだけ）
    let drivers: BTreeSet<i64> = driver_sets.values().flatten().copied().collect();
    if !drivers.is_empty() {
        let rows: Vec<(i64, i64)> = conn
            .exec(
                format!("SELECT id, activity_id FROM activity_drivers WHERE id IN ({})", placeholders(drivers.len())),
                Params::Positional(ids_of(&drivers)),
            )
            .await?;
        let activity_of: HashMap<i64, i64> = rows.into_iter().collect();
        for (cs, ds) in &driver_sets {
            for d in ds {
                if let Some(a) = activity_of.get(d) {
                    activity_sets.entry(*cs).or_default().insert(*a);
                }
            }
        }
    }

    // 施策の名前（使う施策だけ）
    let activity_ids: BTreeSet<i64> = activity_sets.values().flatten().copied().collect();
    let mut refs: HashMap<i64, ActivityRef> = HashMap::new();
    if !activity_ids.is_empty() {
        let rows: Vec<(i64, String, String)> = conn
            .exec(
                format!("SELECT id, code, name FROM activities WHERE id IN ({})", placeholders(activity_ids.len())),
                Params::Positional(ids_of(&activity_ids)),
            )
            .await?;
        refs = rows.into_iter().map(|(id, code, name)| (id, ActivityRef { id, code, name })).collect();
    }
    for cs in items.iter_mut() {
        cs.tables = Some(tables.remove(&cs.id).unwrap_or_default());
        let mut list: Vec<ActivityRef> = activity_sets
            .remove(&cs.id)
            .unwrap_or_default()
            .into_iter()
            .map(|id| {
                refs.get(&id).cloned().unwrap_or_else(|| ActivityRef {
                    id,
                    code: String::new(),
                    name: format!("（削除された施策 #{id}）"),
                })
            })
            .collect();
        list.sort_by(|a, b| a.code.cmp(&b.code).then(a.id.cmp(&b.id)));
        if list.len() > MAX_ACTIVITIES_PER_SET {
            cs.more_activities = list.len() - MAX_ACTIVITIES_PER_SET;
            list.truncate(MAX_ACTIVITIES_PER_SET);
        }
        cs.activities = Some(list);
    }
    Ok(())
}

/// 監査ログの1件。label は対象のレコードを人が読める形で表したもの。
#[derive(Serialize)]
pub struct Log {
    id: i64,
    table_name: String,
    record_id: i64,
    action: String,
    label: String,
    before: Box<RawValue>,
    after: Box<RawValue>,
}

/// GET /api/change-sets/{id}。変更セットと、含まれる監査ログをすべて返す。
/// FP&A と経営陣以外は activity_id の指定が必要で、その施策に関係する監査ログだけを返す（閲覧制限のある科目の金額は除く）。
async fn get_one(State(st): State<AppState>, CurrentUser(u): CurrentUser, Id(id): Id, q: Query) -> AppResult<Response> {
    let mut conn = st.db.get_conn().await?;
    let scope = scope_of(&mut conn, &u, &q).await?;
    let row: Option<ChangeSetRow> = conn.exec_first(format!("{CHANGE_SET_SELECT} WHERE c.id = ?"), (id,)).await?;
    let mut cs = change_set_of(row.ok_or_else(|| not_found("変更履歴が見つかりません"))?);

    let mut sql = "SELECT a.id, a.table_name, a.record_id, a.action, CAST(COALESCE(a.before_json, 'null') AS CHAR), CAST(COALESCE(a.after_json, 'null') AS CHAR)
                   FROM audit_logs a WHERE a.change_set_id = ?"
        .to_string();
    let mut args: Vec<Value> = vec![id.into()];
    if let Some(aid) = scope {
        sql.push_str(&format!(" AND {} AND NOT {RESTRICTED_LOG}", activity_filter()));
        args.extend([aid.into(), aid.into(), aid.into()]);
    }
    let rows: Vec<(i64, String, i64, String, String, String)> =
        conn.exec(format!("{sql} ORDER BY a.id"), Params::Positional(args)).await?;
    if scope.is_some() && rows.is_empty() {
        return Err(not_found("変更履歴が見つかりません"));
    }
    let records: Vec<label::Record> = rows.iter().map(|r| label::Record::new(&r.1, r.2, &r.4, &r.5)).collect();
    let labeler = label::Labeler::load(&mut conn, &records).await?;
    let mut logs = Vec::with_capacity(rows.len());
    for ((id, table_name, record_id, action, before, after), rec) in rows.into_iter().zip(&records) {
        logs.push(Log {
            id,
            label: labeler.label(rec),
            table_name,
            record_id,
            action,
            before: RawValue::from_string(before).map_err(anyhow::Error::from)?,
            after: RawValue::from_string(after).map_err(anyhow::Error::from)?,
        });
    }
    cs.changes = logs.len() as i64;
    Ok(http::ok(&serde_json::json!({"change_set": cs, "logs": logs})))
}
