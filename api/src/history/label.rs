//! 監査ログの対象を人が読める名前にする。名前は今のマスタから引き、削除済みのものは記録（before/after）の値を使う。

use std::collections::{BTreeSet, HashMap};

use mysql_async::prelude::*;
use mysql_async::{Conn, Params};
use serde_json::{Map, Value as Json};

use super::{ids_of, placeholders};
use crate::AppResult;

/// ラベルを作るための監査ログの1件（記録は変更後、なければ変更前）。
pub struct Record {
    table: String,
    record_id: i64,
    rec: Map<String, Json>,
}

impl Record {
    pub fn new(table: &str, record_id: i64, before: &str, after: &str) -> Record {
        let object = |s: &str| match serde_json::from_str::<Json>(s) {
            Ok(Json::Object(m)) => Some(m),
            _ => None,
        };
        Record {
            table: table.to_string(),
            record_id,
            rec: object(after).or_else(|| object(before)).unwrap_or_default(),
        }
    }

    fn str(&self, key: &str) -> &str {
        self.rec.get(key).and_then(Json::as_str).unwrap_or("")
    }

    fn int(&self, key: &str) -> Option<i64> {
        self.rec.get(key).and_then(Json::as_i64)
    }
}

/// 改称前のテーブル名も含む、マスタのテーブルの呼び方。
fn table_noun(table: &str) -> Option<&'static str> {
    Some(match table {
        "scenarios" => "シナリオ",
        // 改称前（functions）の監査ログも「ユニット」と呼ぶ
        "units" | "functions" => "ユニット",
        "segments" => "セグメント",
        "organizations" => "組織",
        "subjects" => "科目",
        "confidence_levels" => "確度の段階",
        "gl_accounts" => "会計科目",
        "notification_settings" => "通知の設定",
        "org_change_plans" => "組織変更の予約",
        "users" => "ユーザー",
        _ => return None,
    })
}

#[derive(Default)]
pub struct Labeler {
    activities: HashMap<i64, String>,
    subjects: HashMap<i64, String>,
    lines: HashMap<i64, String>,
    /// ドライバー → （名前, 施策）
    drivers: HashMap<i64, (String, i64)>,
}

/// id IN (...) で名前を引く。
async fn names(conn: &mut Conn, table: &str, ids: &BTreeSet<i64>) -> AppResult<HashMap<i64, String>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<(i64, String)> = conn
        .exec(
            format!("SELECT id, name FROM {table} WHERE id IN ({})", placeholders(ids.len())),
            Params::Positional(ids_of(ids)),
        )
        .await?;
    Ok(rows.into_iter().collect())
}

impl Labeler {
    /// ラベルに使う名前を、監査ログが参照する ID の分だけ読む。
    pub async fn load(conn: &mut Conn, records: &[Record]) -> AppResult<Labeler> {
        let mut activity_ids = BTreeSet::new();
        let mut subject_ids = BTreeSet::new();
        let mut line_ids = BTreeSet::new();
        let mut driver_ids = BTreeSet::new();
        for r in records {
            if r.table == "activities" {
                activity_ids.insert(r.record_id);
            }
            activity_ids.extend(r.int("activity_id"));
            subject_ids.extend(r.int("subject_id"));
            line_ids.extend(r.int("line_id"));
            if r.table == "driver_values" {
                driver_ids.extend(r.int("activity_driver_id"));
            }
        }
        let mut l = Labeler::default();
        if !driver_ids.is_empty() {
            let rows: Vec<(i64, String, i64)> = conn
                .exec(
                    format!(
                        "SELECT id, name, activity_id FROM activity_drivers WHERE id IN ({})",
                        placeholders(driver_ids.len())
                    ),
                    Params::Positional(ids_of(&driver_ids)),
                )
                .await?;
            for (id, name, activity_id) in rows {
                activity_ids.insert(activity_id);
                l.drivers.insert(id, (name, activity_id));
            }
        }
        l.activities = names(conn, "activities", &activity_ids).await?;
        l.subjects = names(conn, "subjects", &subject_ids).await?;
        l.lines = names(conn, "activity_lines", &line_ids).await?;
        Ok(l)
    }

    fn activity_name(&self, id: i64) -> String {
        self.activities.get(&id).cloned().unwrap_or_else(|| format!("（削除された施策 #{id}）"))
    }

    fn activity(&self, r: &Record) -> String {
        r.int("activity_id").map(|id| self.activity_name(id)).unwrap_or_default()
    }

    fn line_name(&self, id: i64) -> String {
        match self.lines.get(&id) {
            Some(name) => format!("内訳「{name}」"),
            None => format!("（削除された内訳 #{id}）"),
        }
    }

    fn subject_name(&self, r: &Record) -> String {
        let id = r.int("subject_id").unwrap_or(0);
        self.subjects.get(&id).cloned().unwrap_or_else(|| format!("科目 #{id}"))
    }

    pub fn label(&self, r: &Record) -> String {
        let activity = || self.activity(r);
        match r.table.as_str() {
            "activities" => {
                format!("施策 {}", self.activities.get(&r.record_id).map(String::as_str).unwrap_or(r.str("name")))
            }
            "activity_external_codes" => format!("{} / 外部コード「{}」", activity(), r.str("code")),
            "activity_milestones" => format!("{} / マイルストーン「{}」", activity(), r.str("name")),
            "activity_drivers" => format!("{} / ドライバー「{}」", activity(), r.str("name")),
            "activity_lines" => format!("{} / {} / 内訳「{}」", activity(), self.subject_name(r), r.str("name")),
            // 内訳に置き換える前の計算式（改称前の監査ログ用）
            "activity_formulas" => format!("{} / 計算式（{}）", activity(), self.subject_name(r)),
            "budget_facts" => match r.int("line_id") {
                Some(line) => format!(
                    "{} / {} / {} / {}",
                    activity(),
                    self.subject_name(r),
                    self.line_name(line),
                    r.str("target_month")
                ),
                None => format!("{} / {} / {}", activity(), self.subject_name(r), r.str("target_month")),
            },
            "actual_facts" => {
                let who = if r.int("activity_id").is_some() { activity() } else { "未割当".into() };
                format!("{who} / {} / {}（実績）", self.subject_name(r), r.str("target_month"))
            }
            "fiscal_year_closings" => format!("年度の締め {}年度", r.record_id),
            "allocation_rules" => {
                let dept = match r.str("department_code") {
                    "" => "全部門",
                    d => d,
                };
                format!(
                    "割当ルール {} {} × {dept} → {}",
                    r.str("gl_account_code"),
                    r.str("gl_account_name"),
                    activity()
                )
            }
            "driver_values" => {
                let id = r.int("activity_driver_id").unwrap_or(0);
                match self.drivers.get(&id) {
                    Some((name, aid)) => format!("{} / {name} / {}", self.activity_name(*aid), r.str("target_month")),
                    None => format!("（削除されたドライバー #{id}） / {}", r.str("target_month")),
                }
            }
            "scenario_conditions" => format!("{} / 想定条件", activity()),
            "activity_scenario_notes" => format!("{} / 差異の説明・更新の完了", activity()),
            table => match table_noun(table) {
                Some(noun) if !r.str("name").is_empty() => format!("{noun} {}", r.str("name")),
                Some(noun) if table == "users" => format!("{noun} #{}（パスワード）", r.record_id),
                Some(noun) => format!("{noun} #{}", r.record_id),
                None => format!("{table} #{}", r.record_id),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_without_names() {
        let l = Labeler::default();
        let rec = |table: &str, id: i64, after: &str| Record::new(table, id, "null", after);
        assert_eq!(l.label(&rec("activities", 3, r#"{"name": "新規"}"#)), "施策 新規");
        assert_eq!(
            l.label(&rec(
                "budget_facts",
                1,
                r#"{"activity_id": 9, "subject_id": 2, "line_id": 5, "target_month": "2026-10"}"#
            )),
            "（削除された施策 #9） / 科目 #2 / （削除された内訳 #5） / 2026-10"
        );
        assert_eq!(
            l.label(&rec("actual_facts", 1, r#"{"activity_id": null, "subject_id": 2, "target_month": "2026-04"}"#)),
            "未割当 / 科目 #2 / 2026-04（実績）"
        );
        assert_eq!(l.label(&rec("users", 7, r#"{"password": "changed"}"#)), "ユーザー #7（パスワード）");
        assert_eq!(l.label(&rec("functions", 2, r#"{"name": "営業課"}"#)), "ユニット 営業課");
        assert_eq!(l.label(&rec("fiscal_year_closings", 2026, "{}")), "年度の締め 2026年度");
        assert_eq!(
            l.label(&rec(
                "allocation_rules",
                1,
                r#"{"gl_account_code": "6110", "gl_account_name": "給与", "department_code": null, "activity_id": 4}"#
            )),
            "割当ルール 6110 給与 × 全部門 → （削除された施策 #4）"
        );
        assert_eq!(l.label(&rec("mystery", 1, "{}")), "mystery #1");
        // 変更後がなければ（削除）、変更前の記録を使う
        assert_eq!(
            l.label(&Record::new("activity_milestones", 1, r#"{"activity_id": 1, "name": "検収"}"#, "null")),
            "（削除された施策 #1） / マイルストーン「検収」"
        );
    }
}
