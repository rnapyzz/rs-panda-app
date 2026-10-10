//! 明細を施策に割り当てる（docs/plan.md「2.12」の割当の順番）。

use std::collections::HashMap;

use mysql_async::prelude::*;

use crate::AppResult;
use crate::db::Tx;

/// 割当の根拠（actual_entries.allocated_by）。
pub const BY_ACTIVITY_CODE: &str = "activity_code"; // 箱の ID が施策コード
pub const BY_EXTERNAL_CODE: &str = "external_code"; // 箱の ID が外部コード
pub const BY_RULE: &str = "rule"; // 割当ルール（会計科目 × 部門）
pub const BY_MANUAL: &str = "manual"; // 未割当の一覧から選んだ
pub const BY_UNALLOCATED: &str = "unallocated"; // 未割当

pub const ALLOCATION_KINDS: [&str; 5] = [BY_ACTIVITY_CODE, BY_EXTERNAL_CODE, BY_RULE, BY_MANUAL, BY_UNALLOCATED];

#[derive(Default)]
pub struct Allocator {
    /// 施策コード → 施策
    pub activities: HashMap<String, i64>,
    /// 外部コード → 施策
    pub externals: HashMap<String, i64>,
    /// （会計科目, 部門。"" は全部門）→ 施策
    pub rules: HashMap<(i64, String), i64>,
}

impl Allocator {
    /// 箱の ID（施策コード → 外部コード）→ 部門まで一致する割当ルール → 部門が空のルール の順に施策を探す。
    /// どれにも当たらなければ未割当（None）。
    pub fn allocate(&self, account_id: i64, dept: &str, box_code: &str) -> (Option<i64>, &'static str) {
        if !box_code.is_empty() {
            if let Some(&id) = self.activities.get(box_code) {
                return (Some(id), BY_ACTIVITY_CODE);
            }
            if let Some(&id) = self.externals.get(box_code) {
                return (Some(id), BY_EXTERNAL_CODE);
            }
        }
        if !dept.is_empty()
            && let Some(&id) = self.rules.get(&(account_id, dept.to_string()))
        {
            return (Some(id), BY_RULE);
        }
        if let Some(&id) = self.rules.get(&(account_id, String::new())) {
            return (Some(id), BY_RULE);
        }
        (None, BY_UNALLOCATED)
    }
}

pub async fn load_allocator(tx: &mut Tx) -> AppResult<Allocator> {
    let rules: Vec<(i64, String, i64)> =
        tx.query("SELECT gl_account_id, department_key, activity_id FROM allocation_rules").await?;
    Ok(Allocator {
        activities: code_index(tx, "SELECT code, id FROM activities").await?,
        externals: code_index(tx, "SELECT code, activity_id FROM activity_external_codes").await?,
        rules: rules.into_iter().map(|(a, d, id)| ((a, d), id)).collect(),
    })
}

/// コードと ID を返すクエリから code → id の対応表を作る。
pub async fn code_index(tx: &mut Tx, sql: &str) -> AppResult<HashMap<String, i64>> {
    let rows: Vec<(String, i64)> = tx.query(sql).await?;
    Ok(rows.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocate_order() {
        let a = Allocator {
            activities: HashMap::from([("ACT-1".into(), 1)]),
            externals: HashMap::from([("P-1".into(), 2)]),
            rules: HashMap::from([
                ((10, String::new()), 3), // 会計科目 10 は全部門で施策 3
                ((10, "D2".into()), 4),   // ただし D2 は施策 4
            ]),
        };
        let cases = [
            (10, "D2", "ACT-1", Some(1), BY_ACTIVITY_CODE), // 箱の ID がルールより優先
            (10, "D2", "P-1", Some(2), BY_EXTERNAL_CODE),
            (10, "D2", "UNKNOWN", Some(4), BY_RULE), // 登録されていない箱の ID はルールへ
            (10, "D1", "", Some(3), BY_RULE),        // 部門のルールがなければ全部門のルール
            (10, "", "", Some(3), BY_RULE),
            (20, "D2", "", None, BY_UNALLOCATED),
        ];
        for (account, dept, box_code, want, by) in cases {
            assert_eq!(a.allocate(account, dept, box_code), (want, by), "{account} {dept} {box_code}");
        }
    }
}
