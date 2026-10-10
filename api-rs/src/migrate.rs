//! 埋め込まれた SQL ファイル（migrations/*.sql）を MySQL に順番に適用する。
//!
//! 適用済みのバージョンは schema_migrations テーブルに記録する。
//! MySQL の DDL はトランザクションで巻き戻せないため、ファイルの途中で失敗した場合は
//! 手動での復旧が必要になる。1ファイルの変更は小さく保つこと。

use std::collections::HashSet;

use anyhow::{Context, bail};
use mysql_async::Conn;
use mysql_async::prelude::*;

include!(concat!(env!("OUT_DIR"), "/migrations.rs"));

const LOCK_NAME: &str = "fpanda_schema_migrations";

/// 1つのマイグレーションファイル。
#[derive(Clone, Copy, Debug)]
pub struct Migration {
    /// ファイル名から拡張子を除いたもの（例: 0001_create_masters）
    pub version: &'static str,
    pub sql: &'static str,
}

/// 埋め込まれたマイグレーションをバージョン順に返す。
pub fn all() -> Vec<Migration> {
    MIGRATIONS.iter().map(|(v, s)| Migration { version: v, sql: s }).collect()
}

/// 未適用のマイグレーションを返す。
pub fn pending(all: &[Migration], applied: &HashSet<String>) -> Vec<Migration> {
    all.iter().filter(|m| !applied.contains(m.version)).copied().collect()
}

/// 未適用のマイグレーションをすべて適用する。
/// 複数プロセスから同時に実行されないよう、MySQL の名前付きロックを取得する（ロックはセッション単位なので同じコネクションで処理する）。
pub async fn up(conn: &mut Conn) -> anyhow::Result<usize> {
    let locked: Option<Option<i64>> =
        conn.exec_first("SELECT GET_LOCK(?, 30)", (LOCK_NAME,)).await.context("get lock")?;
    if locked.flatten() != Some(1) {
        bail!("get lock: timed out");
    }
    let result = up_locked(conn).await;
    let _ = conn.exec_drop("SELECT RELEASE_LOCK(?)", (LOCK_NAME,)).await;
    result
}

async fn up_locked(conn: &mut Conn) -> anyhow::Result<usize> {
    conn.query_drop(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version VARCHAR(255) NOT NULL PRIMARY KEY,
            applied_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
        )",
    )
    .await
    .context("create schema_migrations")?;

    let applied = applied_versions(conn).await?;
    let pending = pending(&all(), &applied);
    if pending.is_empty() {
        tracing::info!("no pending migrations");
        return Ok(0);
    }
    for m in &pending {
        tracing::info!(version = m.version, "applying migration");
        // ファイルは複数の SQL 文を含むため、テキストプロトコル（複数文の実行）で流す
        conn.query_drop(m.sql).await.with_context(|| format!("apply {}", m.version))?;
        conn.exec_drop("INSERT INTO schema_migrations (version) VALUES (?)", (m.version,))
            .await
            .with_context(|| format!("record {}", m.version))?;
    }
    tracing::info!(count = pending.len(), "migrations applied");
    Ok(pending.len())
}

/// 各マイグレーションの適用状況を返す。
pub async fn status(conn: &mut Conn) -> anyhow::Result<(Vec<Migration>, HashSet<String>)> {
    let exists: i64 = conn
        .query_first(
            "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = DATABASE() AND table_name = 'schema_migrations'",
        )
        .await?
        .unwrap_or(0);
    if exists == 0 {
        return Ok((all(), HashSet::new()));
    }
    Ok((all(), applied_versions(conn).await?))
}

async fn applied_versions(conn: &mut Conn) -> anyhow::Result<HashSet<String>> {
    let rows: Vec<String> =
        conn.query("SELECT version FROM schema_migrations").await.context("select schema_migrations")?;
    Ok(rows.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_in_order() {
        let all = all();
        assert!(all.len() >= 26);
        assert_eq!(all[0].version, "0001_create_masters");
        assert!(all.windows(2).all(|w| w[0].version < w[1].version));
        let applied: HashSet<String> = [all[0].version.to_string()].into();
        assert_eq!(pending(&all, &applied).len(), all.len() - 1);
    }
}
