//! データベースのマイグレーションを実行する。
//!
//! 使い方:
//!
//!   migrate up      未適用のマイグレーションを適用する（デフォルト）
//!   migrate status  適用状況を表示する

use std::time::Duration;

use anyhow::{Context, bail};
use fpanda_api::config::Config;
use fpanda_api::{db, migrate};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().init();
    if let Err(e) = tokio::time::timeout(Duration::from_secs(300), run())
        .await
        .unwrap_or_else(|_| Err(anyhow::anyhow!("timed out")))
    {
        tracing::error!(error = format!("{e:#}"), "migrate failed");
        std::process::exit(1);
    }
}

async fn run() -> anyhow::Result<()> {
    let cmd = std::env::args().nth(1).unwrap_or_else(|| "up".into());
    let cfg = Config::load();
    let mut conn = mysql_async::Conn::new(db::opts(&cfg.db, Some(&cfg.db.name))).await.context("connect database")?;

    let result = match cmd.as_str() {
        "up" => migrate::up(&mut conn).await.map(|_| ()),
        "status" => {
            let (all, applied) = migrate::status(&mut conn).await?;
            for m in all {
                let state = if applied.contains(m.version) { "applied" } else { "pending" };
                println!("{state:<8} {}", m.version);
            }
            Ok(())
        }
        other => bail!("unknown command {other:?} (use: up, status)"),
    };
    let _ = conn.disconnect().await;
    result
}
