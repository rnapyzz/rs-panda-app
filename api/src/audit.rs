//! 変更セット（change_sets）と監査ログ（audit_logs）を記録する。
//!
//! 更新処理と同じトランザクションの中で begin を呼び、変更したレコードごとに insert / update / delete を呼ぶ。

use mysql_async::prelude::*;
use serde::Serialize;

use crate::AppResult;
use crate::db::{self, Tx, Value};
use crate::error::not_found;

/// 1つの変更セットに監査ログを記録する。
#[derive(Clone, Copy, Debug)]
pub struct Recorder {
    pub change_set_id: i64,
}

/// 変更セットを作成する。reason が空の場合は NULL で保存する。
/// scenario_id はシナリオに紐づかない変更（マスタ変更など）では None。シナリオがなければ 404 にする
/// （ハンドラーがシナリオを読む前に変更セットを作っても、外部キーのエラーで 500 にならないように）。
pub async fn begin(tx: &mut Tx, user_id: i64, scenario_id: Option<i64>, reason: &str) -> AppResult<Recorder> {
    let r = tx
        .exec_drop(
            "INSERT INTO change_sets (user_id, scenario_id, reason) VALUES (?, ?, ?)",
            p![user_id, scenario_id, db::null_str(reason.trim())],
        )
        .await;
    if scenario_id.is_some() && matches!(&r, Err(e) if db::err_no(e) == db::ERR_NO_REFERENCED_ROW) {
        return Err(not_found("シナリオが見つかりません"));
    }
    r?;
    Ok(Recorder { change_set_id: db::last_id(tx) })
}

/// 値を監査ログ用の JSON にする。
pub fn to_json<T: Serialize + ?Sized>(v: &T) -> AppResult<String> {
    Ok(serde_json::to_string(v)?)
}

/// 監査ログの1件（many でまとめて記録する）。action は insert / update / delete。
#[derive(Clone, Debug)]
pub struct Change {
    pub table: &'static str,
    pub id: i64,
    pub action: &'static str,
    pub before: Option<String>,
    pub after: Option<String>,
}

/// many で1回の INSERT にまとめる件数。
const MANY_BATCH: usize = 500;

impl Recorder {
    /// レコードの作成を記録する。
    pub async fn insert<T: Serialize + ?Sized>(&self, tx: &mut Tx, table: &str, id: i64, after: &T) -> AppResult<()> {
        self.record(tx, table, id, "insert", None, Some(to_json(after)?)).await
    }

    /// レコードの更新を記録する。
    pub async fn update<B: Serialize + ?Sized, A: Serialize + ?Sized>(
        &self,
        tx: &mut Tx,
        table: &str,
        id: i64,
        before: &B,
        after: &A,
    ) -> AppResult<()> {
        self.record(tx, table, id, "update", Some(to_json(before)?), Some(to_json(after)?)).await
    }

    /// レコードの削除を記録する。
    pub async fn delete<T: Serialize + ?Sized>(&self, tx: &mut Tx, table: &str, id: i64, before: &T) -> AppResult<()> {
        self.record(tx, table, id, "delete", Some(to_json(before)?), None).await
    }

    /// 変更前後の JSON（記録済みの文字列。None は NULL）をそのまま記録する。
    pub async fn record(
        &self,
        tx: &mut Tx,
        table: &str,
        id: i64,
        action: &str,
        before: Option<String>,
        after: Option<String>,
    ) -> AppResult<()> {
        tx.exec_drop(
            "INSERT INTO audit_logs (change_set_id, table_name, record_id, action, before_json, after_json) VALUES (?, ?, ?, ?, ?, ?)",
            p![self.change_set_id, table, id, action, before, after],
        )
        .await?;
        Ok(())
    }

    /// 監査ログをまとめて記録する（件数の多い取込で、1件ずつ INSERT しないため。I-15）。
    pub async fn many(&self, tx: &mut Tx, changes: &[Change]) -> AppResult<()> {
        for chunk in changes.chunks(MANY_BATCH) {
            let mut sql = String::from(
                "INSERT INTO audit_logs (change_set_id, table_name, record_id, action, before_json, after_json) VALUES ",
            );
            let mut args: Vec<Value> = Vec::with_capacity(chunk.len() * 6);
            for (i, c) in chunk.iter().enumerate() {
                if i > 0 {
                    sql.push(',');
                }
                sql.push_str("(?, ?, ?, ?, ?, ?)");
                args.extend([
                    Value::from(self.change_set_id),
                    Value::from(c.table),
                    Value::from(c.id),
                    Value::from(c.action),
                    Value::from(c.before.clone()),
                    Value::from(c.after.clone()),
                ]);
            }
            tx.exec_drop(sql, db::Params::Positional(args)).await?;
        }
        Ok(())
    }
}
