//! 通知の対象のシナリオ・施策・宛先と、本文の組み立て。

use std::collections::{BTreeMap, HashMap};

use chrono::{Datelike, NaiveDate, Weekday};
use mysql_async::prelude::*;

use super::Service;
use crate::AppResult;
use crate::scenario::target_condition;

/// 通知の対象のシナリオ。
#[derive(Clone, Debug)]
pub struct ScenarioInfo {
    pub id: i64,
    pub name: String,
    pub active: bool,
    pub locked: bool,
    pub deadline: Option<NaiveDate>,
}
from_row!(ScenarioInfo { id, name, active, locked, deadline });

const SCENARIO_SELECT: &str = "SELECT id, name, is_active, is_locked, update_deadline FROM scenarios";

/// 通知に載せる施策。
#[derive(Clone, Debug)]
pub struct ActivityItem {
    pub id: i64,
    pub code: String,
    pub name: String,
    /// 担当者（いなければユニットのマネージャー）。0 は宛先なし
    pub assignee: i64,
    /// 施策の担当者（有効なユーザー）。0 はなし
    pub owner: i64,
    /// ユニットのマネージャー（有効なユーザー）。0 はなし
    pub manager: i64,
    pub completed: bool,
}
from_row!(ActivityItem { id, code, name, assignee, owner, manager, completed });

/// 宛先の人。
#[derive(Clone, Debug, Default)]
pub struct Person {
    pub name: String,
    pub slack_id: String,
}

impl Service {
    pub(super) async fn find_scenario(&self, id: i64) -> AppResult<Option<ScenarioInfo>> {
        let mut conn = self.0.db.get_conn().await?;
        Ok(conn.exec_first(format!("{SCENARIO_SELECT} WHERE id = ?"), (id,)).await?)
    }

    /// 作成中で、締切があり、ロックされていないシナリオ。
    pub(super) async fn scenarios_with_deadline(&self) -> AppResult<Vec<ScenarioInfo>> {
        let mut conn = self.0.db.get_conn().await?;
        Ok(conn
            .query(format!(
                "{SCENARIO_SELECT} WHERE is_active AND NOT is_locked AND update_deadline IS NOT NULL ORDER BY id"
            ))
            .await?)
    }

    /// シナリオの更新の対象の施策（docs/plan.md「2.10」）と更新の状態、有効なユーザーを返す。無効なユーザーは宛先にしない。
    pub(super) async fn load_activities(
        &self,
        scenario_id: i64,
    ) -> AppResult<(Vec<ActivityItem>, HashMap<i64, Person>)> {
        let mut conn = self.0.db.get_conn().await?;
        let items: Vec<ActivityItem> = conn
            .exec(
                format!(
                    "SELECT a.id, a.code, a.name,
                            COALESCE(IF(ou.is_active, ou.id, NULL), IF(mu.is_active, mu.id, NULL), 0),
                            COALESCE(IF(ou.is_active, ou.id, NULL), 0),
                            COALESCE(IF(mu.is_active, mu.id, NULL), 0),
                            n.completed_at IS NOT NULL
                     FROM activities a
                     JOIN units un ON un.id = a.unit_id
                     LEFT JOIN users ou ON ou.id = a.owner_user_id
                     LEFT JOIN users mu ON mu.id = un.owner_user_id
                     LEFT JOIN activity_scenario_notes n ON n.activity_id = a.id AND n.scenario_id = ?
                     WHERE {}
                     ORDER BY a.code",
                    target_condition("a")
                ),
                (scenario_id, scenario_id),
            )
            .await?;
        let rows: Vec<(i64, String, String)> =
            conn.query("SELECT id, name, COALESCE(slack_user_id, '') FROM users WHERE is_active").await?;
        let people = rows.into_iter().map(|(id, name, slack_id)| (id, Person { name, slack_id })).collect();
        Ok((items, people))
    }
}

/// 施策を宛先ごとにまとめる（宛先の ID の順）。宛先が 0 の施策は除く。
pub fn group_by(items: &[ActivityItem], key: impl Fn(&ActivityItem) -> i64) -> BTreeMap<i64, Vec<ActivityItem>> {
    let mut out: BTreeMap<i64, Vec<ActivityItem>> = BTreeMap::new();
    for it in items {
        let k = key(it);
        if k != 0 {
            out.entry(k).or_default().push(it.clone());
        }
    }
    out
}

/// お知らせの本文用に施策を並べる（10件まで）。
pub fn list_activities(items: &[ActivityItem]) -> String {
    let mut lines: Vec<String> = items.iter().take(10).map(|it| format!("・{}（{}）", it.name, it.code)).collect();
    if items.len() > 10 {
        lines.push(format!("ほか {} 件", items.len() - 10));
    }
    lines.join("\n")
}

/// Slack での呼び方。メンバー ID があればメンションし、なければ名前。
pub fn mention(p: &Person) -> String {
    if p.slack_id.is_empty() { p.name.clone() } else { format!("<@{}>", p.slack_id) }
}

/// 「@佐藤（2件）、@鈴木（1件）」の形にする。
pub fn mention_list(groups: &BTreeMap<i64, Vec<ActivityItem>>, people: &HashMap<i64, Person>) -> String {
    groups
        .iter()
        .map(|(id, items)| format!("{}（{}件）", mention(people.get(id).unwrap_or(&Person::default())), items.len()))
        .collect::<Vec<_>>()
        .join("、")
}

/// 「10/15（水）」の形にする。
pub fn date_label(d: NaiveDate) -> String {
    let w = match d.weekday() {
        Weekday::Sun => "日",
        Weekday::Mon => "月",
        Weekday::Tue => "火",
        Weekday::Wed => "水",
        Weekday::Thu => "木",
        Weekday::Fri => "金",
        Weekday::Sat => "土",
    };
    format!("{}/{}（{w}）", d.month(), d.day())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(code: &str) -> ActivityItem {
        ActivityItem {
            id: 1,
            code: code.into(),
            name: format!("施策 {code}"),
            assignee: 0,
            owner: 0,
            manager: 0,
            completed: false,
        }
    }

    #[test]
    fn labels() {
        assert_eq!(date_label(NaiveDate::from_ymd_opt(2026, 10, 15).unwrap()), "10/15（木）");
        let items: Vec<ActivityItem> = (1..=12).map(|i| item(&format!("A-{i}"))).collect();
        let text = list_activities(&items);
        assert!(text.starts_with("・施策 A-1（A-1）\n") && text.ends_with("ほか 2 件"), "{text}");
        assert_eq!(mention(&Person { name: "佐藤".into(), slack_id: String::new() }), "佐藤");
        assert_eq!(mention(&Person { name: "佐藤".into(), slack_id: "U1".into() }), "<@U1>");
    }
}
