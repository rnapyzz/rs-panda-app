//! お知らせ・通知の設定・送信の記録・催促の API。

use std::collections::BTreeMap;
use std::sync::LazyLock;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use chrono::NaiveDateTime;
use mysql_async::prelude::*;
use regex::Regex;
use serde::{Deserialize, Serialize};

use super::events::Settings;
use super::recipients::{ActivityItem, date_label, list_activities, mention};
use super::{KIND_MANUAL_REMINDER, KINDS, Note, Service, insert_notes};
use crate::auth::{AdminUser, CurrentUser};
use crate::error::{AppError, conflict, not_found, validation1};
use crate::http::{self, Id, Json, Query, Validator};
use crate::{AppResult, AppState, audit, db, jst};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/notifications", get(list_notifications))
        .route("/api/notifications/read-all", post(read_all))
        .route("/api/notifications/{id}/read", post(read_one))
        .route("/api/notification-settings", get(get_settings).put(put_settings))
        .route("/api/notification-settings/test", post(test_slack))
        .route("/api/notification-runs", get(list_runs))
        .route("/api/scenarios/{id}/reminders", get(list_reminders).post(send_reminder))
}

/// limit（既定 def、上限 max。正しくなければ既定）。
fn limit_of(q: &Query, def: i64, max: i64) -> i64 {
    match q.get("limit").parse::<i64>() {
        Ok(n) if n > 0 => n.min(max),
        _ => def,
    }
}

/// アプリ内のお知らせ。
#[derive(Serialize)]
struct Notification {
    id: i64,
    kind: String,
    scenario_id: Option<i64>,
    title: String,
    body: String,
    link: String,
    #[serde(serialize_with = "jst::ser")]
    created_at: NaiveDateTime,
    #[serde(serialize_with = "jst::ser_opt")]
    read_at: Option<NaiveDateTime>,
}
from_row!(Notification { id, kind, scenario_id, title, body, link, created_at, read_at });

/// GET /api/notifications?limit=。自分宛てのお知らせ（新しい順）と未読の数。
async fn list_notifications(State(st): State<AppState>, CurrentUser(u): CurrentUser, q: Query) -> AppResult<Response> {
    let mut conn = st.db.get_conn().await?;
    let items: Vec<Notification> = conn
        .exec(
            "SELECT id, kind, scenario_id, title, body, link, created_at, read_at
             FROM notifications WHERE user_id = ? ORDER BY created_at DESC, id DESC LIMIT ?",
            (u.id, limit_of(&q, 30, 100)),
        )
        .await?;
    let unread =
        db::count(&mut conn, "SELECT COUNT(*) FROM notifications WHERE user_id = ? AND read_at IS NULL", p![u.id])
            .await?;
    Ok(http::ok(&serde_json::json!({"items": items, "unread": unread})))
}

/// POST /api/notifications/{id}/read。自分宛てのお知らせを既読にする。
async fn read_one(State(st): State<AppState>, CurrentUser(u): CurrentUser, Id(id): Id) -> AppResult<Response> {
    let mut conn = st.db.get_conn().await?;
    if db::count(&mut conn, "SELECT COUNT(*) FROM notifications WHERE id = ? AND user_id = ?", p![id, u.id]).await? == 0
    {
        return Err(not_found("お知らせが見つかりません"));
    }
    conn.exec_drop(
        "UPDATE notifications SET read_at = COALESCE(read_at, NOW()) WHERE id = ? AND user_id = ?",
        (id, u.id),
    )
    .await?;
    Ok(http::no_content())
}

/// POST /api/notifications/read-all
async fn read_all(State(st): State<AppState>, CurrentUser(u): CurrentUser) -> AppResult<Response> {
    let mut conn = st.db.get_conn().await?;
    conn.exec_drop("UPDATE notifications SET read_at = NOW() WHERE user_id = ? AND read_at IS NULL", (u.id,)).await?;
    Ok(http::no_content())
}

#[derive(Serialize)]
struct SettingsResponse {
    #[serde(flatten)]
    settings: Settings,
    slack_configured: bool,
}

/// GET /api/notification-settings
async fn get_settings(State(st): State<AppState>, _: CurrentUser) -> AppResult<Response> {
    let settings = st.notify.load_settings().await?;
    Ok(http::ok(&SettingsResponse { settings, slack_configured: st.notify.slack_configured() }))
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct SettingsRequest {
    enabled_kinds: BTreeMap<String, bool>,
    reminder_days: Vec<i64>,
    send_time: String,
    reason: String,
}

static TIME_PATTERN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([01][0-9]|2[0-3]):[0-5][0-9]$").unwrap());

/// PUT /api/notification-settings。FP&A のみ。変更は変更履歴に残す。
async fn put_settings(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    Json(req): Json<SettingsRequest>,
) -> AppResult<Response> {
    let mut v = Validator::new();
    let mut enabled = req.enabled_kinds.clone();
    if let Some(k) = enabled.keys().find(|k| !KINDS.contains(&k.as_str())) {
        v.add("enabled_kinds", format!("通知の種類 {k} はありません"));
    }
    for k in KINDS {
        enabled.entry(k.to_string()).or_insert(true);
    }
    // 重なりを除き、日数の多い順
    let mut days = req.reminder_days.clone();
    days.sort_unstable_by(|a, b| b.cmp(a));
    days.dedup();
    if days.len() > 5 {
        v.add("reminder_days", "締切の前の通知は5回までです");
    }
    if days.iter().any(|d| !(1..=14).contains(d)) {
        v.add("reminder_days", "締切の前の日数は 1〜14 日で指定してください");
    }
    if !TIME_PATTERN.is_match(&req.send_time) {
        v.add("send_time", "送信時刻は HH:MM（例: 09:00）で指定してください");
    }
    v.check()?;

    let before = st.notify.load_settings().await?;
    let after = Settings { enabled_kinds: enabled, reminder_days: days, send_time: req.send_time.clone() };
    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    tx.exec_drop(
        "UPDATE notification_settings SET enabled_kinds = ?, reminder_days = ?, send_time = ? WHERE id = 1",
        (
            serde_json::to_string(&after.enabled_kinds)?,
            after.reminder_days.iter().map(|d| d.to_string()).collect::<Vec<_>>().join(","),
            format!("{}:00", after.send_time),
        ),
    )
    .await?;
    rec.update(&mut tx, "notification_settings", 1, &before, &after).await?;
    tx.commit().await?;
    Ok(http::ok(&SettingsResponse { settings: after, slack_configured: st.notify.slack_configured() }))
}

/// POST /api/notification-settings/test。Slack にテスト送信する。
async fn test_slack(State(st): State<AppState>, AdminUser(u): AdminUser) -> AppResult<Response> {
    if !st.notify.slack_configured() {
        return Err(conflict("Slack の Webhook URL（環境変数 SLACK_WEBHOOK_URL）が設定されていません"));
    }
    st.notify
        .post_slack(&format!("F-Panda からのテスト送信です（{} さんが送信）。", u.name))
        .await
        .map_err(|e| AppError::new(StatusCode::BAD_GATEWAY, "slack_failed", e))?;
    Ok(http::no_content())
}

#[derive(Serialize)]
struct Run {
    id: i64,
    kind: String,
    scenario_id: i64,
    scenario_name: String,
    run_date: String,
    recipients: i64,
    slack_status: String,
    slack_attempts: i64,
    slack_error: String,
    #[serde(serialize_with = "jst::ser")]
    created_at: NaiveDateTime,
}
from_row!(Run {
    id,
    kind,
    scenario_id,
    scenario_name,
    run_date,
    recipients,
    slack_status,
    slack_attempts,
    slack_error,
    created_at
});

/// GET /api/notification-runs?limit=。FP&A のみ。送信の記録（新しい順）。
async fn list_runs(State(st): State<AppState>, _: AdminUser, q: Query) -> AppResult<Response> {
    let mut conn = st.db.get_conn().await?;
    let items: Vec<Run> = conn
        .exec(
            "SELECT n.id, n.kind, n.scenario_id, s.name, DATE_FORMAT(n.run_date, '%Y-%m-%d'), n.recipients, n.slack_status,
                    n.slack_attempts, COALESCE(n.slack_error, ''), n.created_at
             FROM notification_runs n JOIN scenarios s ON s.id = n.scenario_id
             ORDER BY n.created_at DESC, n.id DESC LIMIT ?",
            (limit_of(&q, 50, 200),),
        )
        .await?;
    Ok(http::list(&items))
}

// --- 催促（docs/plan.md「2.20」） ---

impl Service {
    /// 今日（日本時間）の0時から今までの秒数。DB の NOW() と比べて「今日送ったか」を判定する（時計のずれを避ける）。
    fn since_today(&self) -> i64 {
        let now = self.now();
        (now - now.date().and_hms_opt(0, 0, 0).unwrap_or(now)).num_seconds()
    }

    /// シナリオで今日催促した人の ID。
    async fn reminded_today<Q: Queryable>(&self, q: &mut Q, scenario_id: i64) -> AppResult<Vec<i64>> {
        Ok(q.exec(
            "SELECT DISTINCT user_id FROM notifications
             WHERE kind = ? AND scenario_id = ? AND created_at >= NOW() - INTERVAL ? SECOND ORDER BY user_id",
            (KIND_MANUAL_REMINDER, scenario_id, self.since_today()),
        )
        .await?)
    }
}

/// GET /api/scenarios/{id}/reminders。今日催促した人。
async fn list_reminders(State(st): State<AppState>, _: AdminUser, Id(id): Id) -> AppResult<Response> {
    let mut conn = st.db.get_conn().await?;
    let ids = st.notify.reminded_today(&mut conn, id).await?;
    Ok(http::ok(&serde_json::json!({"user_ids": ids})))
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ReminderRequest {
    user_id: i64,
}

/// POST /api/scenarios/{id}/reminders（user_id）。
/// 作成中のシナリオで、その人が担当する未完了の施策の一覧を、お知らせと Slack（メンション）で送る。同じ人には1日1回まで。
/// 同時に押されても二重に送らないよう、シナリオの行をロックしてから確かめて登録する。
async fn send_reminder(
    State(st): State<AppState>,
    AdminUser(_): AdminUser,
    Id(id): Id,
    Json(req): Json<ReminderRequest>,
) -> AppResult<Response> {
    let svc = &st.notify;
    let mut tx = db::begin(&st.db).await?;
    let sc: Option<(String, bool, bool, Option<chrono::NaiveDate>)> = tx
        .exec_first("SELECT name, is_active, is_locked, update_deadline FROM scenarios WHERE id = ? FOR UPDATE", (id,))
        .await?;
    let Some((name, active, locked, deadline)) = sc else {
        return Err(not_found("シナリオが見つかりません"));
    };
    if !active || locked {
        return Err(conflict("催促できるのは、今回の見込（作成中のシナリオ）だけです"));
    }
    if svc.reminded_today(&mut tx, id).await?.contains(&req.user_id) {
        return Err(conflict("この人には今日すでに催促しています（1日1回まで）"));
    }
    let (items, people) = svc.load_activities(id).await?;
    let open: Vec<ActivityItem> = items.into_iter().filter(|it| it.owner == req.user_id && !it.completed).collect();
    let Some(person) = people.get(&req.user_id).filter(|_| !open.is_empty()) else {
        return Err(validation1("user_id", "この人が担当する未完了の施策はありません"));
    };
    let deadline = deadline.map(|d| format!("締切は {} です。", date_label(d))).unwrap_or_default();
    let note = Note {
        user_id: req.user_id,
        title: format!("見込の更新をお願いします（{name}）"),
        body: format!(
            "{deadline}未完了の施策（{}件）を更新し、今回の見込の説明を書いて「説明して完了」を押してください。\n{}",
            open.len(),
            list_activities(&open)
        ),
    };
    insert_notes(&mut tx, KIND_MANUAL_REMINDER, Some(id), &[note]).await?;
    tx.commit().await?;

    let slack = if svc.slack_configured() {
        let text = format!(
            "【{name}】{} さん、見込の更新をお願いします。{deadline}未完了 {}件{}",
            mention(person),
            open.len(),
            svc.link("/", "ホームを開く")
        );
        match svc.post_slack(&text).await {
            Ok(()) => "sent",
            Err(e) => {
                tracing::error!(scenario_id = id, user_id = req.user_id, error = e, "notify manual_reminder slack");
                "failed"
            }
        }
    } else {
        "off"
    };
    Ok(http::ok(&serde_json::json!({"user_id": req.user_id, "activities": open.len(), "slack": slack})))
}
