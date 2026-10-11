//! ユーザーを作成する。最初の FP&A 管理者を作るために使う。
//!
//! パスワードは標準入力の1行目から読み込む（コマンドライン引数に残さないため）。
//!
//!   printf '%s' "$PASSWORD" | createuser -email admin@example.com -name 管理者 -role fpa_admin

use std::io::BufRead;

use anyhow::{anyhow, bail};
use fpanda_api::auth::{ROLE_FPA_ADMIN, normalize_email, valid_role};
use fpanda_api::config::Config;
use fpanda_api::{db, password};
use mysql_async::prelude::*;

const USAGE: &str = "使い方: createuser -email <メールアドレス> -name <氏名> [-role fpa_admin|manager|member|viewer]";

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

/// Go の flag パッケージと同じく、-name value・--name value・-name=value を受け付ける。
fn parse_args() -> anyhow::Result<(String, String, String)> {
    let (mut email, mut name, mut role) = (String::new(), String::new(), ROLE_FPA_ADMIN.to_string());
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let flag = arg.trim_start_matches('-');
        let (key, inline) = match flag.split_once('=') {
            Some((k, v)) => (k.to_string(), Some(v.to_string())),
            None => (flag.to_string(), None),
        };
        let mut value =
            || inline.clone().or_else(|| args.next()).ok_or_else(|| anyhow!("-{key} に値がありません\n{USAGE}"));
        match key.as_str() {
            "email" => email = value()?,
            "name" => name = value()?,
            "role" => role = value()?,
            _ => bail!("不明な引数です: {arg}\n{USAGE}"),
        }
    }
    Ok((email, name, role))
}

async fn run() -> anyhow::Result<()> {
    let (email, name, role) = parse_args()?;
    if email.is_empty() || name.is_empty() {
        eprintln!("{USAGE}");
        bail!("-email と -name は必須です");
    }
    if !valid_role(&role) {
        bail!("不明なロールです: {role}");
    }

    let mut pw = String::new();
    std::io::stdin().lock().read_line(&mut pw)?;
    if pw.is_empty() {
        bail!("標準入力からパスワードを読み込めませんでした");
    }
    let pw = pw.trim_end_matches(['\r', '\n']).to_string();
    password::validate(&pw).map_err(|e| anyhow!(e))?;
    let hash = password::hash_async(pw).await;

    let cfg = Config::load();
    let pool = db::pool(&cfg.db);
    let mut conn = pool.get_conn().await?;
    let email = normalize_email(&email);
    let r = conn
        .exec_drop(
            "INSERT INTO users (name, email, password_hash, role) VALUES (?, ?, ?, ?)",
            (name.trim(), email.clone(), hash, role.clone()),
        )
        .await;
    if db::is_dup(&r) {
        bail!("このメールアドレスは登録済みです: {email}");
    }
    r?;
    let id = conn.last_insert_id().unwrap_or(0);
    drop(conn);
    pool.disconnect().await?;
    println!("ユーザーを作成しました: id={id} email={email} role={role}");
    Ok(())
}
