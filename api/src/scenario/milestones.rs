//! ホームのマイルストーン（docs/plan.md「2.11 マネージャーの動線」の ③）。

use axum::extract::State;
use axum::response::Response;
use chrono::{Duration, NaiveDateTime};
use mysql_async::prelude::*;
use serde::Serialize;

use super::find_scenario;
use crate::auth::CurrentUser;
use crate::db::{Params, Value};
use crate::error::bad_request;
use crate::http::{self, Id, Query};
use crate::{AppResult, AppState, jst};

/// 「期日が近い」とみなす日数
const UPCOMING_DAYS: i64 = 30;
/// 1回に指定できる施策の数
const MAX_MILESTONE_ACTIVITIES: usize = 200;

/// 期日の後ろ倒し（前回見込の作成以降）。
#[derive(Serialize)]
struct Postponement {
    count: i64,
    days: i64,
}

/// 完了していないマイルストーンと注意。
#[derive(Serialize)]
struct HomeMilestone {
    id: i64,
    activity_id: i64,
    name: String,
    due_date: String,
    status: String,
    /// 期日超過
    overdue: bool,
    /// 状態が遅延
    delayed: bool,
    /// 30日以内に期日
    upcoming: bool,
    postponed: Postponement,
}

#[derive(Serialize)]
struct HomeMilestonesResponse {
    today: String,
    /// 後ろ倒しを数え始める日時（前回見込の作成日時。前回見込がなければシナリオの作成日時）
    #[serde(serialize_with = "jst::ser")]
    since: NaiveDateTime,
    items: Vec<HomeMilestone>,
}

/// GET /api/scenarios/{id}/milestones?activity_ids=1,2,3。
/// 指定した施策の、完了していないマイルストーンを期日の順に返す。期日超過・遅延・期日が近い・後ろ倒しの注意を付ける。
/// 後ろ倒しは、監査ログのうち期日を後ろにずらした変更を、前回見込の作成以降について数える。
pub async fn milestones(State(st): State<AppState>, _: CurrentUser, Id(id): Id, q: Query) -> AppResult<Response> {
    let mut ids: Vec<i64> = Vec::new();
    for p in q.get("activity_ids").split(',') {
        let p = p.trim();
        if p.is_empty() {
            continue;
        }
        ids.push(p.parse().map_err(|_| bad_request("activity_ids は施策 ID をカンマ区切りで指定してください"))?);
    }
    if ids.len() > MAX_MILESTONE_ACTIVITIES {
        return Err(bad_request(format!("activity_ids は {MAX_MILESTONE_ACTIVITIES} 件までです")));
    }

    let mut conn = st.db.get_conn().await?;
    let s = find_scenario(&mut conn, id, "").await?;
    let since = match s.previous_scenario_id {
        Some(p) => find_scenario(&mut conn, p, "").await?.created_at,
        None => s.created_at,
    };
    let today = jst::today();
    let mut resp = HomeMilestonesResponse { today: today.format("%Y-%m-%d").to_string(), since, items: Vec::new() };
    if ids.is_empty() {
        return Ok(http::ok(&resp));
    }

    let placeholders = vec!["?"; ids.len()].join(",");
    let mut args: Vec<Value> = vec![since.into()];
    args.extend(ids.iter().map(|&i| Value::from(i)));
    // 後ろ倒し: 期日を後ろにずらした更新の回数と、ずらした日数の合計
    let rows: Vec<(i64, i64, String, String, String, i64, i64)> = conn
        .exec(
            format!(
                "SELECT m.id, m.activity_id, m.name, DATE_FORMAT(m.due_date, '%Y-%m-%d'), m.status,
                        COUNT(p.id), CAST(COALESCE(SUM(p.days), 0) AS SIGNED)
                 FROM activity_milestones m
                 LEFT JOIN (
                     SELECT a.id, a.record_id,
                            DATEDIFF(JSON_UNQUOTE(JSON_EXTRACT(a.after_json, '$.due_date')), JSON_UNQUOTE(JSON_EXTRACT(a.before_json, '$.due_date'))) AS days
                     FROM audit_logs a
                     WHERE a.table_name = 'activity_milestones' AND a.action = 'update' AND a.created_at >= ?
                       AND JSON_UNQUOTE(JSON_EXTRACT(a.after_json, '$.due_date')) > JSON_UNQUOTE(JSON_EXTRACT(a.before_json, '$.due_date'))
                 ) p ON p.record_id = m.id
                 WHERE m.status <> 'completed' AND m.activity_id IN ({placeholders})
                 GROUP BY m.id, m.activity_id, m.name, m.due_date, m.status
                 ORDER BY m.due_date, m.id"
            ),
            Params::Positional(args),
        )
        .await?;
    let soon = (today + Duration::days(UPCOMING_DAYS)).format("%Y-%m-%d").to_string();
    for (id, activity_id, name, due_date, status, count, days) in rows {
        let overdue = due_date < resp.today;
        resp.items.push(HomeMilestone {
            id,
            activity_id,
            upcoming: !overdue && due_date <= soon,
            delayed: status == "delayed",
            overdue,
            name,
            due_date,
            status,
            postponed: Postponement { count, days },
        });
    }
    Ok(http::ok(&resp))
}
