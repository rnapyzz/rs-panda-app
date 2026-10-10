//! ログイン失敗の制限（docs/architecture.md「パスワードでのログイン」）。
//! 同じメールアドレス、または同じ IP からの失敗が THROTTLE_WINDOW に THROTTLE_LIMIT 回に達したら、ログインを止める。

use chrono::Duration;
use mysql_async::prelude::*;

use super::AuthService;
use crate::{AppResult, jst};

const THROTTLE_LIMIT: i64 = 5;
const THROTTLE_WINDOW_MINUTES: i64 = 15;
/// ログインの失敗の記録を残す期間（時間）
const ATTEMPTS_KEEP_HOURS: i64 = 24;

impl AuthService {
    /// メールアドレスか IP の失敗が上限に達しているかを返す。
    pub(super) async fn throttled(&self, email: &str, ip: &str) -> AppResult<bool> {
        let since = jst::now() - Duration::minutes(THROTTLE_WINDOW_MINUTES);
        let mut conn = self.db.get_conn().await?;
        let (by_email, by_ip): (i64, i64) = conn
            .exec_first(
                "SELECT (SELECT COUNT(*) FROM login_attempts WHERE email = ? AND attempted_at > ?),
                        (SELECT COUNT(*) FROM login_attempts WHERE ip = ? AND attempted_at > ?)",
                p![email, since, ip, since],
            )
            .await?
            .unwrap_or((0, 0));
        Ok(by_email >= THROTTLE_LIMIT || by_ip >= THROTTLE_LIMIT)
    }

    pub(super) async fn record_failure(&self, email: &str, ip: &str) -> AppResult<()> {
        let mut conn = self.db.get_conn().await?;
        conn.exec_drop(
            "INSERT INTO login_attempts (email, ip, attempted_at) VALUES (?, ?, ?)",
            p![email, ip, jst::now()],
        )
        .await?;
        Ok(())
    }

    pub(super) async fn clear_failures(&self, email: &str) -> AppResult<()> {
        let mut conn = self.db.get_conn().await?;
        conn.exec_drop("DELETE FROM login_attempts WHERE email = ?", (email,)).await?;
        Ok(())
    }

    /// 古いログインの失敗の記録を消す。
    pub(super) async fn delete_old_attempts(&self) -> AppResult<()> {
        let mut conn = self.db.get_conn().await?;
        conn.exec_drop(
            "DELETE FROM login_attempts WHERE attempted_at <= ?",
            (jst::now() - Duration::hours(ATTEMPTS_KEEP_HOURS),),
        )
        .await?;
        Ok(())
    }
}
