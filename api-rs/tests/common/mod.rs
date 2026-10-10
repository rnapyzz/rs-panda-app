//! 統合テストの共通処理（Go 版の testutil と server_test のヘルパー）。
//!
//! テストごとに専用のデータベースを作ってマイグレーションを適用し、API サーバーを起動する。
//! 接続先は TEST_DB_HOST / TEST_DB_PORT / TEST_DB_USER / TEST_DB_PASSWORD。
//! TEST_DB_HOST が未設定ならテストをスキップする（`let Some(env) = TestEnv::start().await else { return };`）。

#![allow(dead_code)]

use std::net::SocketAddr;
use std::time::Duration;

use fpanda_api::auth::AuthService;
use fpanda_api::auth::oidc::Oidc;
use fpanda_api::config::{DbConfig, OidcConfig};
use fpanda_api::state::Inner;
use fpanda_api::{AppState, db, migrate, password, server};
use mysql_async::prelude::*;
use reqwest::StatusCode;
use serde_json::{Value, json};

pub const TEST_PASSWORD: &str = "test-password-123";

fn getenv(key: &str, fallback: &str) -> String {
    std::env::var(key).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| fallback.into())
}

/// テスト用の DB とサーバー。drop するとデータベースを削除する。
pub struct TestEnv {
    pub db: db::Pool,
    pub base: String,
    pub admin_id: i64,
    db_cfg: DbConfig,
    name: String,
}

#[derive(Default)]
pub struct Options {
    /// SSO を有効にする（パスワードでのログインは FP&A だけ）
    pub oidc: Option<OidcConfig>,
}

impl TestEnv {
    pub async fn start() -> Option<TestEnv> {
        TestEnv::start_with(Options::default()).await
    }

    pub async fn start_with(opts: Options) -> Option<TestEnv> {
        let host = getenv("TEST_DB_HOST", "");
        if host.is_empty() {
            eprintln!("TEST_DB_HOST が未設定のため DB を使うテストをスキップします");
            return None;
        }
        // テストで作るユーザーのパスワードハッシュは反復回数を下げて、テスト時間を短くする。
        password::set_iterations_for_testing(1000);

        let db_cfg = DbConfig {
            host,
            port: getenv("TEST_DB_PORT", "3306").parse().unwrap(),
            user: getenv("TEST_DB_USER", "root"),
            password: getenv("TEST_DB_PASSWORD", "root"),
            name: String::new(),
        };
        let mut suffix = [0u8; 6];
        rand::fill(&mut suffix);
        let name = format!("fpanda_test_{}", hex::encode(suffix));

        let mut admin = mysql_async::Conn::new(db::opts(&db_cfg, None)).await.expect("connect test db");
        admin
            .query_drop(format!("CREATE DATABASE {name} CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci"))
            .await
            .expect("create test database");
        admin.disconnect().await.ok();

        let mut conn = mysql_async::Conn::new(db::opts(&db_cfg, Some(&name))).await.unwrap();
        migrate::up(&mut conn).await.expect("migrate");
        conn.disconnect().await.ok();

        let pool = mysql_async::Pool::new(db::opts(&db_cfg, Some(&name)));
        let mut auth = AuthService::new(pool.clone(), Duration::from_secs(3600));
        let oidc = opts.oidc.map(|cfg| {
            auth.restrict_password_login(&[fpanda_api::auth::ROLE_FPA_ADMIN]);
            Oidc::new(cfg)
        });
        let state = AppState::new(Inner { db: pool.clone(), auth, cookie_secure: false, trust_proxy: false, oidc });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, server::router(state).into_make_service_with_connect_info::<SocketAddr>())
                .await
                .unwrap();
        });
        Some(TestEnv { db: pool, base: format!("http://{addr}"), admin_id: 0, db_cfg, name })
    }

    /// FP&A 管理者（admin@example.com）でログイン済みのクライアントを返す。
    pub async fn admin_client() -> Option<(Client, TestEnv)> {
        let mut env = TestEnv::start().await?;
        env.admin_id = env.create_user("admin@example.com", "fpa_admin").await;
        let c = env.client();
        c.login("admin@example.com").await;
        Some((c, env))
    }

    pub fn client(&self) -> Client {
        Client::new(&self.base)
    }

    pub async fn create_user(&self, email: &str, role: &str) -> i64 {
        let hash = password::hash(TEST_PASSWORD);
        let mut conn = self.db.get_conn().await.unwrap();
        conn.exec_drop(
            "INSERT INTO users (name, email, password_hash, role) VALUES (?, ?, ?, ?)",
            (format!("テスト {role}"), email, hash, role),
        )
        .await
        .unwrap();
        conn.last_insert_id().unwrap() as i64
    }

    pub async fn exec(&self, sql: &str, params: impl Into<mysql_async::Params> + Send) {
        let mut conn = self.db.get_conn().await.unwrap();
        conn.exec_drop(sql, params).await.unwrap();
    }

    pub async fn query<T: FromRow + Send + 'static>(
        &self,
        sql: &str,
        params: impl Into<mysql_async::Params> + Send,
    ) -> Vec<T> {
        let mut conn = self.db.get_conn().await.unwrap();
        conn.exec(sql, params).await.unwrap()
    }

    pub async fn query_one<T: FromRow + Send + 'static>(
        &self,
        sql: &str,
        params: impl Into<mysql_async::Params> + Send,
    ) -> T {
        self.query(sql, params).await.into_iter().next().expect("no rows")
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let cfg = self.db_cfg.clone();
        let name = self.name.clone();
        // 非同期の drop はできないので、別スレッドの実行環境でデータベースを削除する
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async {
                if let Ok(mut c) = mysql_async::Conn::new(db::opts(&cfg, None)).await {
                    if let Err(e) = c.query_drop(format!("DROP DATABASE {name}")).await {
                        eprintln!("drop test database: {e}");
                    }
                    c.disconnect().await.ok();
                }
            });
        })
        .join()
        .ok();
    }
}

/// Cookie を保持する HTTP クライアント。
pub struct Client {
    pub base: String,
    pub http: reqwest::Client,
}

impl Client {
    pub fn new(base: &str) -> Client {
        let http =
            reqwest::Client::builder().cookie_store(true).redirect(reqwest::redirect::Policy::none()).build().unwrap();
        Client { base: base.into(), http }
    }

    pub async fn do_req(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let m = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
        let mut req = self.http.request(m, format!("{}{}", self.base, path)).header("Content-Type", "application/json");
        if let Some(b) = body {
            req = req.body(b.to_string());
        }
        let res = req.send().await.unwrap();
        let status = res.status();
        let bytes = res.bytes().await.unwrap();
        let v = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or_else(|_| {
                panic!("{method} {path}: invalid JSON response {:?}", String::from_utf8_lossy(&bytes))
            })
        };
        (status, v)
    }

    pub async fn get(&self, path: &str) -> (StatusCode, Value) {
        self.do_req("GET", path, None).await
    }

    pub async fn post(&self, path: &str, body: Value) -> (StatusCode, Value) {
        self.do_req("POST", path, Some(body)).await
    }

    pub async fn put(&self, path: &str, body: Value) -> (StatusCode, Value) {
        self.do_req("PUT", path, Some(body)).await
    }

    pub async fn delete(&self, path: &str) -> (StatusCode, Value) {
        self.do_req("DELETE", path, None).await
    }

    pub async fn login(&self, email: &str) {
        let (status, body) = self.post("/api/auth/login", json!({"email": email, "password": TEST_PASSWORD})).await;
        assert_eq!(status, StatusCode::OK, "login {email}: {body}");
    }

    /// POST して 201 を確認し、作成された ID を返す。
    pub async fn must_create(&self, path: &str, body: Value) -> i64 {
        let (status, res) = self.post(path, body).await;
        assert_eq!(status, StatusCode::CREATED, "POST {path}: {res}");
        res["id"].as_i64().unwrap()
    }

    pub async fn must_get(&self, path: &str) -> Value {
        let (status, res) = self.get(path).await;
        assert_eq!(status, StatusCode::OK, "GET {path}: {res}");
        res
    }

    /// CSV を multipart/form-data で送る。reason・csv が空ならその項目を送らない。
    pub async fn upload(&self, path: &str, csv: &str, reason: &str) -> (StatusCode, Value) {
        let mut form = reqwest::multipart::Form::new();
        if !reason.is_empty() {
            form = form.text("reason", reason.to_string());
        }
        if !csv.is_empty() {
            form = form.part("file", reqwest::multipart::Part::bytes(csv.as_bytes().to_vec()).file_name("data.csv"));
        }
        let res = self.http.post(format!("{}{}", self.base, path)).multipart(form).send().await.unwrap();
        let status = res.status();
        (status, res.json().await.unwrap_or(Value::Null))
    }

    /// エクスポートの CSV を取得する（BOM を除いた本文を返す）。
    pub async fn download(&self, path: &str) -> String {
        let res = self.http.get(format!("{}{}", self.base, path)).send().await.unwrap();
        let status = res.status();
        let cd = res.headers().get("content-disposition").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let body = res.text().await.unwrap();
        assert_eq!(status, StatusCode::OK, "GET {path}: {body}");
        assert!(body.starts_with('\u{feff}'), "GET {path}: BOM がない");
        assert!(cd.contains("attachment"), "GET {path}: Content-Disposition = {cd:?}");
        body.trim_start_matches('\u{feff}').to_string()
    }
}

pub fn error_code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or("")
}

pub fn detail<'a>(body: &'a Value, field: &str) -> &'a str {
    body["error"]["details"][field].as_str().unwrap_or("")
}

pub fn items(body: &Value) -> &Vec<Value> {
    body["items"].as_array().expect("items")
}

pub fn result_counts(body: &Value) -> String {
    format!("inserted={} updated={} unchanged={}", body["inserted"], body["updated"], body["unchanged"])
}

pub fn row_errors(body: &Value) -> Vec<String> {
    body["error"]["rows"]
        .as_array()
        .map(|rows| rows.iter().map(|r| format!("{}:{}", r["line"], r["message"].as_str().unwrap_or(""))).collect())
        .unwrap_or_default()
}
