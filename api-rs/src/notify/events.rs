//! 通知の設定と、4つの通知（更新の開始・締切の前・締切の超過・実績の反映）。

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use chrono::{Datelike, NaiveDate, Weekday};
use mysql_async::prelude::*;
use serde::Serialize;

use super::recipients::{ScenarioInfo, date_label, group_by, list_activities, mention, mention_list};
use super::{
    KIND_ACTUALS_REFLECTED, KIND_DEADLINE_OVERDUE, KIND_DEADLINE_REMINDER, KIND_UPDATE_STARTED, KINDS, Message, Note,
    Service,
};
use crate::AppResult;

/// 通知の設定（notification_settings）。
#[derive(Clone, Debug, Serialize)]
pub struct Settings {
    pub enabled_kinds: BTreeMap<String, bool>,
    pub reminder_days: Vec<i64>,
    /// HH:MM
    pub send_time: String,
}

impl Settings {
    fn enabled(&self, kind: &str) -> bool {
        self.enabled_kinds.get(kind).copied().unwrap_or(true)
    }
}

impl Service {
    pub async fn load_settings(&self) -> AppResult<Settings> {
        let mut conn = self.0.db.get_conn().await?;
        let (enabled, days, send_time): (String, String, String) = conn
            .query_first(
                "SELECT CAST(enabled_kinds AS CHAR), reminder_days, TIME_FORMAT(send_time, '%H:%i') FROM notification_settings WHERE id = 1",
            )
            .await?
            .ok_or_else(|| anyhow::anyhow!("notification_settings がありません"))?;
        let mut enabled_kinds: BTreeMap<String, bool> = serde_json::from_str(&enabled)?;
        for k in KINDS {
            enabled_kinds.entry(k.to_string()).or_insert(true);
        }
        let reminder_days = days.split(',').filter_map(|p| p.trim().parse::<i64>().ok()).filter(|n| *n > 0).collect();
        Ok(Settings { enabled_kinds, reminder_days, send_time })
    }

    /// 「更新の開始」を送る。シナリオが作成中・締切あり・ロックされていないときだけ、シナリオごとに1回。
    /// 締切の保存や作成中の指定の後に呼ぶ。失敗はログに残すだけにする（保存は失敗させない）。
    pub async fn update_started(&self, scenario_id: i64) {
        if let Err(e) = self.try_update_started(scenario_id).await {
            tracing::error!(scenario_id, error = ?e, "notify update_started");
        }
    }

    async fn try_update_started(&self, scenario_id: i64) -> AppResult<()> {
        if !self.load_settings().await?.enabled(KIND_UPDATE_STARTED) {
            return Ok(());
        }
        let Some(sc) = self.find_scenario(scenario_id).await? else { return Ok(()) };
        let Some(deadline) = sc.deadline.filter(|_| sc.active && !sc.locked) else { return Ok(()) };
        let mut conn = self.0.db.get_conn().await?;
        let sent: i64 = conn
            .exec_first(
                "SELECT COUNT(*) FROM notification_runs WHERE kind = ? AND scenario_id = ?",
                (KIND_UPDATE_STARTED, sc.id),
            )
            .await?
            .unwrap_or(0);
        drop(conn);
        if sent > 0 {
            return Ok(());
        }
        let (items, people) = self.load_activities(sc.id).await?;
        let groups = group_by(&items, |it| it.assignee);
        let title = format!("見込の更新が始まりました（{}）", sc.name);
        let notes = groups
            .iter()
            .map(|(id, items)| Note {
                user_id: *id,
                title: title.clone(),
                body: format!(
                    "締切は {} です。担当の施策（{}件）を更新し、今回の見込の説明を書いて「説明して完了」を押してください。\n{}",
                    date_label(deadline),
                    items.len(),
                    list_activities(items)
                ),
            })
            .collect();
        let slack_text = format!(
            "【{}】見込の更新が始まりました。締切は {} です。\n対象: {}{}",
            sc.name,
            date_label(deadline),
            mention_list(&groups, &people),
            self.link("/", "ホームを開く")
        );
        self.deliver(Message {
            kind: KIND_UPDATE_STARTED,
            scenario_id: sc.id,
            run_date: self.today(),
            notes,
            slack_text,
        })
        .await
    }

    /// 「実績の反映」を送る。months は新しく実績になった月、activity_ids は前回見込との差が大きい施策。
    /// 決算確定月の保存の後に呼ぶ。失敗はログに残すだけにする。
    pub async fn actuals_reflected(&self, scenario_id: i64, months: Vec<String>, activity_ids: Vec<i64>) {
        if let Err(e) = self.try_actuals_reflected(scenario_id, &months, &activity_ids).await {
            tracing::error!(scenario_id, error = ?e, "notify actuals_reflected");
        }
    }

    async fn try_actuals_reflected(&self, scenario_id: i64, months: &[String], activity_ids: &[i64]) -> AppResult<()> {
        if months.is_empty() || activity_ids.is_empty() || !self.load_settings().await?.enabled(KIND_ACTUALS_REFLECTED)
        {
            return Ok(());
        }
        let Some(sc) = self.find_scenario(scenario_id).await?.filter(|sc| sc.active && !sc.locked) else {
            return Ok(());
        };
        let (mut items, people) = self.load_activities(sc.id).await?;
        items.retain(|it| activity_ids.contains(&it.id));
        let groups = group_by(&items, |it| it.assignee);
        let label = months_label(months);
        let notes = groups
            .iter()
            .map(|(id, items)| Note {
                user_id: *id,
                title: format!("{label}の実績が反映されました（{}）", sc.name),
                body: format!(
                    "前回の見込との差が大きい施策があります（差が前回の見込の 20% 以上）。見込を見直してください。\n{}",
                    list_activities(items)
                ),
            })
            .collect();
        let slack_text = format!(
            "【{}】{label}の実績が反映されました。前回の見込との差が大きい施策: {}{}",
            sc.name,
            mention_list(&groups, &people),
            self.link("/", "ホームを開く")
        );
        self.deliver(Message {
            kind: KIND_ACTUALS_REFLECTED,
            scenario_id: sc.id,
            run_date: self.today(),
            notes,
            slack_text,
        })
        .await
    }

    /// 1分ごとに run_due を呼ぶ（サーバーが止まるまで続ける）。
    pub async fn run(self) {
        let mut ticker = tokio::time::interval(Duration::from_secs(60));
        loop {
            ticker.tick().await;
            if let Err(e) = self.run_due().await {
                tracing::error!(error = ?e, "notify run");
            }
        }
    }

    /// 送信時刻を過ぎていれば、今日の「締切の前」「締切の超過」を送る（土日は送らない）。
    /// 送信の記録で二重送信を防ぐので、何度呼んでもよい。古いお知らせの削除もここで行う。
    pub async fn run_due(&self) -> AppResult<()> {
        let now = self.now();
        let st = self.load_settings().await?;
        if now.format("%H:%M").to_string() < st.send_time {
            return Ok(());
        }
        self.cleanup().await?;
        let today = now.date();
        if is_weekend(today) {
            return Ok(());
        }
        for sc in self.scenarios_with_deadline().await? {
            let Some(deadline) = sc.deadline else { continue };
            if today > deadline {
                if st.enabled(KIND_DEADLINE_OVERDUE) {
                    self.deadline_overdue(&sc, deadline, today).await?;
                }
            } else if st.enabled(KIND_DEADLINE_REMINDER)
                && st.reminder_days.iter().any(|d| reminder_date(deadline, *d) == today)
            {
                self.deadline_reminder(&sc, deadline, today).await?;
            }
        }
        Ok(())
    }

    async fn deadline_reminder(&self, sc: &ScenarioInfo, deadline: NaiveDate, today: NaiveDate) -> AppResult<()> {
        let (mut items, people) = self.load_activities(sc.id).await?;
        items.retain(|it| !it.completed);
        let groups = group_by(&items, |it| it.assignee);
        let left = (deadline - today).num_days();
        let when = if left == 1 { "明日".to_string() } else { format!("あと{left}日") };
        let notes = groups
            .iter()
            .map(|(id, items)| Note {
                user_id: *id,
                title: format!("締切まで{when}です（{}）", sc.name),
                body: format!(
                    "締切は {} です。まだ完了していない施策（{}件）を更新してください。\n{}",
                    date_label(deadline),
                    items.len(),
                    list_activities(items)
                ),
            })
            .collect();
        let slack_text = format!(
            "【{}】見込の更新の締切まで{when}です（{}）。未完了: {}{}",
            sc.name,
            date_label(deadline),
            mention_list(&groups, &people),
            self.link("/", "ホームを開く")
        );
        self.deliver(Message { kind: KIND_DEADLINE_REMINDER, scenario_id: sc.id, run_date: today, notes, slack_text })
            .await
    }

    /// 締切を過ぎた未完了の施策を、担当者とユニットのマネージャーに知らせる。
    /// 担当者でもありマネージャーでもある人には、1件のお知らせにまとめる。
    async fn deadline_overdue(&self, sc: &ScenarioInfo, deadline: NaiveDate, today: NaiveDate) -> AppResult<()> {
        let (mut items, people) = self.load_activities(sc.id).await?;
        items.retain(|it| !it.completed);
        let owned = group_by(&items, |it| it.assignee);
        let managed = group_by(&items, |it| it.manager);
        let over = (today - deadline).num_days();
        let title = format!("締切を{over}日過ぎています（{}）", sc.name);
        let recipients: BTreeSet<i64> = owned.keys().chain(managed.keys()).copied().collect();
        let notes = recipients
            .into_iter()
            .map(|id| {
                let mut parts = Vec::new();
                if let Some(items) = owned.get(&id) {
                    parts.push(format!("担当の未完了の施策（{}件）:\n{}", items.len(), list_activities(items)));
                }
                if let Some(items) = managed.get(&id) {
                    parts.push(format!("所管ユニットの未完了の施策（{}件）:\n{}", items.len(), list_activities(items)));
                }
                Note {
                    user_id: id,
                    title: title.clone(),
                    body: format!(
                        "締切（{}）を過ぎています。至急、更新を完了にしてください。\n{}",
                        date_label(deadline),
                        parts.join("\n")
                    ),
                }
            })
            .collect();
        let mut text = format!(
            "【{}】見込の更新の締切（{}）を{over}日過ぎています。未完了: {}",
            sc.name,
            date_label(deadline),
            mention_list(&owned, &people)
        );
        if !managed.is_empty() {
            let names: Vec<String> =
                managed.keys().map(|id| mention(people.get(id).unwrap_or(&Default::default()))).collect();
            text.push_str(&format!("\nユニットのマネージャー: {}", names.join("、")));
        }
        text.push_str(&self.link("/", "ホームを開く"));
        self.deliver(Message {
            kind: KIND_DEADLINE_OVERDUE,
            scenario_id: sc.id,
            run_date: today,
            notes,
            slack_text: text,
        })
        .await
    }
}

/// "2026-04" の一覧を「4月・5月」にする。
fn months_label(months: &[String]) -> String {
    months
        .iter()
        .map(|m| format!("{}月", m.get(5..).and_then(|s| s.parse::<u32>().ok()).unwrap_or(0)))
        .collect::<Vec<_>>()
        .join("・")
}

fn is_weekend(d: NaiveDate) -> bool {
    matches!(d.weekday(), Weekday::Sat | Weekday::Sun)
}

/// 締切の days 日前の送信日。土日に当たる場合は直前の金曜にする。
fn reminder_date(deadline: NaiveDate, days: i64) -> NaiveDate {
    let mut d = deadline - chrono::Duration::days(days);
    while is_weekend(d) {
        d -= chrono::Duration::days(1);
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn reminder_dates() {
        for (deadline, days, want) in [
            ("2026-10-15", 3, "2026-10-12"), // 木曜の締切の3日前は月曜
            ("2026-10-15", 1, "2026-10-14"),
            ("2026-10-19", 1, "2026-10-16"), // 月曜の締切の前日（日曜）は直前の金曜
            ("2026-10-20", 3, "2026-10-16"), // 火曜の締切の3日前（土曜）は直前の金曜
        ] {
            assert_eq!(reminder_date(d(deadline), days), d(want), "{deadline} {days}");
        }
    }

    #[test]
    fn month_labels() {
        assert_eq!(months_label(&["2026-04".into(), "2026-05".into()]), "4月・5月");
    }
}
