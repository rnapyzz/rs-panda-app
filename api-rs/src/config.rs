//! 環境変数からアプリケーション設定を読み込む。

use std::time::Duration;

/// アプリケーション設定。
#[derive(Clone, Debug)]
pub struct Config {
    pub http_addr: String,
    pub db: DbConfig,
    /// ログインセッションの有効期間。
    pub session_ttl: Duration,
    /// true のとき、セッション Cookie に Secure 属性を付ける（HTTPS 環境で有効にする）。
    pub cookie_secure: bool,
    /// 通知を送る Slack の Incoming Webhook の URL（秘密情報）。空なら Slack には送らない。
    pub slack_webhook_url: String,
    /// 通知の本文に載せるアプリの URL（例: https://fpanda.example.com）。
    pub app_base_url: String,
    /// true のとき、接続元の IP を X-Forwarded-For から読む（ALB の後ろで動かす本番用）。
    pub trust_proxy: bool,
    /// SSO の設定。client_id が空なら SSO は無効。
    pub oidc: OidcConfig,
}

/// SSO（OIDC）の設定。
#[derive(Clone, Debug, Default)]
pub struct OidcConfig {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    pub redirect_url: String,
    pub allowed_domains: Vec<String>,
}

/// MySQL の接続設定。
#[derive(Clone, Debug)]
pub struct DbConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub name: String,
}

impl Config {
    /// 環境変数から設定を読み込む。未設定の項目は開発用のデフォルト値を使う。
    pub fn load() -> Config {
        Config {
            http_addr: getenv("HTTP_ADDR", ":8080"),
            session_ttl: get_duration("SESSION_TTL", Duration::from_secs(12 * 3600)),
            cookie_secure: getenv("COOKIE_SECURE", "false") == "true",
            slack_webhook_url: getenv("SLACK_WEBHOOK_URL", ""),
            app_base_url: getenv("APP_BASE_URL", ""),
            trust_proxy: getenv("TRUST_PROXY", "false") == "true",
            oidc: OidcConfig {
                issuer: getenv("OIDC_ISSUER", "https://accounts.google.com"),
                client_id: getenv("OIDC_CLIENT_ID", ""),
                client_secret: getenv("OIDC_CLIENT_SECRET", ""),
                redirect_url: getenv("OIDC_REDIRECT_URL", ""),
                allowed_domains: split_list(&getenv("OIDC_ALLOWED_DOMAINS", "")),
            },
            db: DbConfig {
                host: getenv("DB_HOST", "localhost"),
                port: getenv("DB_PORT", "3306").parse().unwrap_or(3306),
                user: getenv("DB_USER", "fpanda"),
                password: getenv("DB_PASSWORD", "fpanda"),
                name: getenv("DB_NAME", "fpanda"),
            },
        }
    }

    /// 待ち受けるアドレス。Go 版と同じく ":8080" の形も受け付ける。
    pub fn listen_addr(&self) -> String {
        if self.http_addr.starts_with(':') { format!("0.0.0.0{}", self.http_addr) } else { self.http_addr.clone() }
    }
}

pub fn getenv(key: &str, fallback: &str) -> String {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => v,
        _ => fallback.to_string(),
    }
}

/// Go の time.ParseDuration と同じ形（例: 12h、90m、1h30m）を読む。読めないか 0 以下なら fallback。
fn get_duration(key: &str, fallback: Duration) -> Duration {
    parse_duration(&getenv(key, "")).filter(|d| !d.is_zero()).unwrap_or(fallback)
}

pub fn parse_duration(s: &str) -> Option<Duration> {
    if s.is_empty() {
        return None;
    }
    let mut total = 0f64;
    let mut rest = s;
    while !rest.is_empty() {
        let num_len = rest.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(rest.len());
        if num_len == 0 {
            return None;
        }
        let n: f64 = rest[..num_len].parse().ok()?;
        rest = &rest[num_len..];
        let unit_len = rest.find(|c: char| c.is_ascii_digit() || c == '.').unwrap_or(rest.len());
        let secs = match &rest[..unit_len] {
            "ns" => 1e-9,
            "us" | "µs" => 1e-6,
            "ms" => 1e-3,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            _ => return None,
        };
        rest = &rest[unit_len..];
        total += n * secs;
    }
    Some(Duration::from_secs_f64(total))
}

/// カンマ区切りの値を、前後の空白を除いた小文字の一覧にする（空の要素は除く）。
fn split_list(s: &str) -> Vec<String> {
    s.split(',').map(|p| p.trim().to_lowercase()).filter(|p| !p.is_empty()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_go_durations() {
        assert_eq!(parse_duration("12h"), Some(Duration::from_secs(43200)));
        assert_eq!(parse_duration("1h30m"), Some(Duration::from_secs(5400)));
        assert_eq!(parse_duration("abc"), None);
        assert_eq!(split_list(" A.com, ,b.com"), vec!["a.com", "b.com"]);
    }
}
