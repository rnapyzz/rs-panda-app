//! 締切と通知（docs/plan.md「2.13 締切と通知」）。
//!
//! 通知の種類は4つ（更新の開始・締切の前・締切の超過・実績の反映）。宛先ごとにアプリ内のお知らせ（notifications）を作り、
//! Slack の共有チャンネルにも1回の投稿にまとめて送る（Incoming Webhook。環境変数 SLACK_WEBHOOK_URL が空なら送らない）。
//! 送信の記録（notification_runs）を（種類・シナリオ・日付）で一意に残し、同じ通知を同じ日に二重に送らない。
//! FP&A がホームから送る催促（docs/plan.md「2.20」）もここで扱う。

mod api;
mod events;
mod recipients;

use std::sync::Arc;
use std::time::Duration;

use chrono::{NaiveDate, NaiveDateTime};
use mysql_async::prelude::*;

pub use api::routes;

use crate::db::{self, Params, Pool, Value};
use crate::{AppResult, jst};

// 通知の種類。
pub const KIND_UPDATE_STARTED: &str = "update_started";
pub const KIND_DEADLINE_REMINDER: &str = "deadline_reminder";
pub const KIND_DEADLINE_OVERDUE: &str = "deadline_overdue";
pub const KIND_ACTUALS_REFLECTED: &str = "actuals_reflected";
/// FP&A がホームから送る催促（docs/plan.md「2.20」）。設定の有効・無効の対象ではない
pub const KIND_MANUAL_REMINDER: &str = "manual_reminder";

/// 設定で有効・無効を切り替える種類。
pub const KINDS: [&str; 4] =
    [KIND_UPDATE_STARTED, KIND_DEADLINE_REMINDER, KIND_DEADLINE_OVERDUE, KIND_ACTUALS_REFLECTED];

/// 同じ日のうちに Slack へ送る回数の上限
const MAX_SLACK_ATTEMPTS: i64 = 3;
/// 失敗した Slack の送信を送り直すまでの間隔（秒）
const SLACK_RETRY_AFTER_SECS: i64 = 600;
const SLACK_TIMEOUT: Duration = Duration::from_secs(10);
/// アプリ内のお知らせを残す日数
const KEEP_DAYS: i64 = 180;

/// 今の日時（日本時間）を返す時計。
pub type Clock = Box<dyn Fn() -> NaiveDateTime + Send + Sync>;

/// Service の設定。
#[derive(Default)]
pub struct Options {
    /// Slack の Incoming Webhook の URL。空なら Slack には送らない
    pub slack_webhook_url: String,
    /// Slack の本文に載せるアプリの URL（例: https://fpanda.example.com）。空ならリンクを載せない
    pub base_url: String,
    /// テスト用の時計。None なら今の日本時間
    pub now: Option<Clock>,
}

struct Inner {
    db: Pool,
    slack: String,
    base_url: String,
    client: reqwest::Client,
    now: Clock,
}

/// 通知を作って送る。複製しても同じものを指す。
#[derive(Clone)]
pub struct Service(Arc<Inner>);

/// 1回の通知の内容。宛先ごとのお知らせと、Slack の本文。
struct Message {
    kind: &'static str,
    scenario_id: i64,
    run_date: NaiveDate,
    notes: Vec<Note>,
    slack_text: String,
}

/// 1人へのお知らせ。
struct Note {
    user_id: i64,
    title: String,
    body: String,
}

/// お知らせのリンク（どの通知もホーム）。
const LINK: &str = "/";

impl Service {
    pub fn new(db: Pool, o: Options) -> Service {
        let client = reqwest::Client::builder().timeout(SLACK_TIMEOUT).build().unwrap_or_default();
        Service(Arc::new(Inner {
            db,
            slack: o.slack_webhook_url,
            base_url: o.base_url.trim_end_matches('/').to_string(),
            client,
            now: o.now.unwrap_or_else(|| Box::new(jst::now)),
        }))
    }

    fn now(&self) -> NaiveDateTime {
        (self.0.now)()
    }

    fn today(&self) -> NaiveDate {
        self.now().date()
    }

    /// Slack の Webhook URL が設定されているか。
    pub fn slack_configured(&self) -> bool {
        !self.0.slack.is_empty()
    }

    /// Slack の本文に載せるリンク（<URL|ラベル>）。BaseURL がなければ空。
    fn link(&self, path: &str, label: &str) -> String {
        if self.0.base_url.is_empty() { String::new() } else { format!(" <{}{path}|{label}>", self.0.base_url) }
    }

    /// 送信の記録を作り、お知らせを登録して Slack に送る。
    /// 同じ（種類・シナリオ・日付）の記録があれば、お知らせは作らず、失敗した Slack の送信だけを送り直す。
    async fn deliver(&self, m: Message) -> AppResult<()> {
        if m.notes.is_empty() {
            return Ok(());
        }
        let mut conn = self.0.db.get_conn().await?;
        // 前回の送信からの秒数は DB の時計で測る
        let existing: Option<(i64, String, i64, i64)> = conn
            .exec_first(
                "SELECT id, slack_status, slack_attempts, TIMESTAMPDIFF(SECOND, updated_at, NOW())
                 FROM notification_runs WHERE kind = ? AND scenario_id = ? AND run_date = ?",
                (m.kind, m.scenario_id, m.run_date),
            )
            .await?;
        if let Some((run_id, status, attempts, waited)) = existing {
            if status == "failed"
                && self.slack_configured()
                && attempts < MAX_SLACK_ATTEMPTS
                && waited >= SLACK_RETRY_AFTER_SECS
            {
                self.send_run(run_id, attempts).await?;
            }
            return Ok(());
        }
        drop(conn);

        let mut tx = db::begin(&self.0.db).await?;
        let r = tx
            .exec_drop(
                "INSERT INTO notification_runs (kind, scenario_id, run_date, recipients, slack_text) VALUES (?, ?, ?, ?, ?)",
                (m.kind, m.scenario_id, m.run_date, m.notes.len(), &m.slack_text),
            )
            .await;
        if db::is_dup(&r) {
            return Ok(()); // ほかの API サーバーが先に送った
        }
        r?;
        let run_id = db::last_id(&tx);
        insert_notes(&mut tx, m.kind, Some(m.scenario_id), &m.notes).await?;
        tx.commit().await?;
        if self.slack_configured() {
            self.send_run(run_id, 0).await?;
        }
        Ok(())
    }

    /// 記録の Slack の本文を送り、結果を記録する。
    async fn send_run(&self, run_id: i64, attempts: i64) -> AppResult<()> {
        let mut conn = self.0.db.get_conn().await?;
        let text: Option<String> =
            conn.exec_first("SELECT COALESCE(slack_text, '') FROM notification_runs WHERE id = ?", (run_id,)).await?;
        let (status, error) = match self.post_slack(&text.unwrap_or_default()).await {
            Ok(()) => ("sent", None),
            Err(e) => {
                tracing::error!(run_id, error = e, "slack notification failed");
                ("failed", Some(e))
            }
        };
        conn.exec_drop(
            "UPDATE notification_runs SET slack_status = ?, slack_attempts = ?, slack_error = ? WHERE id = ?",
            (status, attempts + 1, error, run_id),
        )
        .await?;
        Ok(())
    }

    /// Incoming Webhook に本文を送る。失敗したら理由を返す。
    pub async fn post_slack(&self, text: &str) -> Result<(), String> {
        if !self.slack_configured() {
            return Err("Slack の Webhook URL が設定されていません".into());
        }
        let res = self
            .0
            .client
            .post(&self.0.slack)
            .json(&serde_json::json!({"text": text}))
            .send()
            .await
            .map_err(|e| format!("Slack に送れませんでした: {e}"))?;
        let status = res.status();
        if status != reqwest::StatusCode::OK {
            let body = res.text().await.unwrap_or_default();
            let body: String = body.chars().take(500).collect();
            return Err(format!("Slack が {} を返しました: {}", status.as_u16(), body.trim()));
        }
        Ok(())
    }

    /// 古いお知らせを消す。
    async fn cleanup(&self) -> AppResult<()> {
        let mut conn = self.0.db.get_conn().await?;
        conn.exec_drop(
            "DELETE FROM notifications WHERE created_at < ?",
            (self.now() - chrono::Duration::days(KEEP_DAYS),),
        )
        .await?;
        Ok(())
    }
}

/// お知らせをまとめて登録する。
async fn insert_notes<Q: Queryable>(q: &mut Q, kind: &str, scenario_id: Option<i64>, notes: &[Note]) -> AppResult<()> {
    for chunk in notes.chunks(500) {
        let mut args: Vec<Value> = Vec::with_capacity(chunk.len() * 6);
        for n in chunk {
            args.extend([
                n.user_id.into(),
                kind.into(),
                scenario_id.into(),
                n.title.clone().into(),
                n.body.clone().into(),
                LINK.into(),
            ]);
        }
        q.exec_drop(
            format!(
                "INSERT INTO notifications (user_id, kind, scenario_id, title, body, link) VALUES {}",
                vec!["(?, ?, ?, ?, ?, ?)"; chunk.len()].join(",")
            ),
            Params::Positional(args),
        )
        .await?;
    }
    Ok(())
}
