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
        let state = AppState::new(Inner {
            db: pool.clone(),
            auth,
            cookie_secure: false,
            trust_proxy: false,
            oidc,
            notifier: None,
        });
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

    /// GET して、状態コード・ヘッダー・本文（バイト列）を返す。
    pub async fn get_bytes(&self, path: &str) -> (StatusCode, reqwest::header::HeaderMap, Vec<u8>) {
        let res = self.http.get(format!("{}{}", self.base, path)).send().await.unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        (status, headers, res.bytes().await.unwrap().to_vec())
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

/// 施策のテストに使うユニットとユーザー（Go 版の activityFixture）。
/// unit1 の担当は manager1、unit2 の担当は manager2。
pub struct ActivityFixture {
    pub env: TestEnv,
    pub admin: Client,
    pub unit1: i64,
    pub unit2: i64,
    pub manager1_id: i64,
    pub member_id: i64,
    pub manager1: Client,
    pub manager2: Client,
    pub member: Client,
    pub member2: Client,
    pub viewer: Client,
}

impl ActivityFixture {
    pub async fn new() -> Option<ActivityFixture> {
        let (admin, env) = TestEnv::admin_client().await?;
        let manager1_id = env.create_user("manager1@example.com", "manager").await;
        let manager2_id = env.create_user("manager2@example.com", "manager").await;
        let member_id = env.create_user("member@example.com", "member").await;
        env.create_user("member2@example.com", "member").await;
        env.create_user("viewer@example.com", "viewer").await;

        let seg = admin.must_create("/api/segments", json!({"name": "XXX事業"})).await;
        let org = admin.must_create("/api/organizations", json!({"name": "BBB部"})).await;
        let unit1 = admin
            .must_create(
                "/api/units",
                json!({"name": "CCC課", "segment_id": seg, "organization_id": org, "owner_user_id": manager1_id}),
            )
            .await;
        let unit2 = admin
            .must_create(
                "/api/units",
                json!({"name": "DDD課", "segment_id": seg, "organization_id": org, "owner_user_id": manager2_id}),
            )
            .await;

        let login = |email: &'static str| {
            let c = env.client();
            async move {
                c.login(email).await;
                c
            }
        };
        let manager1 = login("manager1@example.com").await;
        let manager2 = login("manager2@example.com").await;
        let member = login("member@example.com").await;
        let member2 = login("member2@example.com").await;
        let viewer = login("viewer@example.com").await;
        Some(ActivityFixture {
            env,
            admin,
            unit1,
            unit2,
            manager1_id,
            member_id,
            manager1,
            manager2,
            member,
            member2,
            viewer,
        })
    }
}

/// 施策の作成・更新リクエストを作る。overrides で項目を上書きする。
pub fn activity_body(unit: i64, code: &str, overrides: Value) -> Value {
    let mut b = json!({
        "unit_id": unit,
        "code": code,
        "name": format!("施策 {code}"),
        "activity_type": "recurring",
        "status": "in_progress",
    });
    if let Value::Object(o) = overrides {
        for (k, v) in o {
            b[k] = v;
        }
    }
    b
}

/// 計算式で金額を算出する内訳を持つ施策（formula）と直接入力だけの施策（manual）を持つ（Go 版の scenarioFixture）。
pub struct ScenarioFixture {
    pub f: ActivityFixture,
    pub sales: i64,
    pub cost: i64,
    /// 実績の取込に使う会計科目（4110 → 受託売上、8110 → 外注費）
    pub sales_account: i64,
    pub cost_account: i64,
    /// 施策（どちらも担当者は member）
    pub formula_act: i64,
    pub manual_act: i64,
    /// formula_act のドライバー
    pub price_id: i64,
    pub volume_id: i64,
    /// formula_act の売上の内訳（計算式で反映する）
    pub sales_line: i64,
    /// 2026年度のシナリオ（作成中）
    pub budget: i64,
}

impl std::ops::Deref for ScenarioFixture {
    type Target = ActivityFixture;
    fn deref(&self) -> &ActivityFixture {
        &self.f
    }
}

impl ScenarioFixture {
    pub async fn new() -> Option<ScenarioFixture> {
        let f = ActivityFixture::new().await?;
        let a = &f.admin;
        let sales =
            a.must_create("/api/subjects", json!({"code": "4110", "name": "受託売上", "category": "revenue"})).await;
        let cost =
            a.must_create("/api/subjects", json!({"code": "8110", "name": "外注費", "category": "expense"})).await;
        let sales_account =
            a.must_create("/api/gl-accounts", json!({"code": "4110", "name": "受託売上", "subject_id": sales})).await;
        let cost_account =
            a.must_create("/api/gl-accounts", json!({"code": "8110", "name": "外注費", "subject_id": cost})).await;
        let formula_act = a
            .must_create(
                "/api/activities",
                activity_body(f.unit1, "SAAS-1", json!({"owner_user_id": f.member_id, "confidence_level": "C"})),
            )
            .await;
        let base = format!("/api/activities/{formula_act}");
        let price_id = a
            .must_create(
                &format!("{base}/drivers"),
                json!({"code": "unit_price", "name": "単価", "driver_kind": "value"}),
            )
            .await;
        let volume_id = a
            .must_create(&format!("{base}/drivers"), json!({"code": "volume", "name": "件数", "driver_kind": "kpi"}))
            .await;
        let sales_line = a
            .must_create(
                &format!("{base}/lines"),
                json!({"subject_id": sales, "name": "利用料", "expression": "unit_price * volume * 0.5", "formula_enabled": true, "reason": "算出式の設定"}),
            )
            .await;
        let manual_act = a
            .must_create("/api/activities", activity_body(f.unit1, "PRJ-1", json!({"owner_user_id": f.member_id})))
            .await;
        let budget = a
            .must_create(
                "/api/scenarios",
                json!({"name": "2026年度 当初予算", "fiscal_year": 2026, "plan_role": "initial"}),
            )
            .await;
        // 現場が入力できるのは作成中のシナリオだけ
        let (status, body) = a.post(&format!("/api/scenarios/{budget}/activate"), Value::Null).await;
        assert_eq!(status, StatusCode::OK, "作成中に指定: {body}");
        Some(ScenarioFixture {
            f,
            sales,
            cost,
            sales_account,
            cost_account,
            formula_act,
            manual_act,
            price_id,
            volume_id,
            sales_line,
            budget,
        })
    }

    pub fn values_path(&self, scenario_id: i64, activity_id: i64) -> String {
        format!("/api/scenarios/{scenario_id}/activities/{activity_id}")
    }

    /// 実績の金額を返す（なければ ""）。activity_id の 0 は未割当。
    pub async fn actual_amount(&self, activity_id: i64, subject_id: i64, month: &str) -> String {
        let rows: Vec<String> = self
            .env
            .query(
                "SELECT CAST(amount AS CHAR) FROM actual_facts WHERE activity_key = ? AND subject_id = ? AND target_month = ?",
                (activity_id, subject_id, format!("{month}-01")),
            )
            .await;
        rows.into_iter().next().unwrap_or_default()
    }

    /// PRJ-1（manual_act）の受託売上の実績を、実績 CSV の取込で入れる（その月の明細はこの1行に置き換わる）。
    pub async fn import_actual(&self, month: &str, amount: i64) {
        let csv = format!("target_month,box_code,account_code,amount\n{month},PRJ-1,4110,{amount}\n");
        let (status, body) = self.admin.upload("/api/actuals/import", &csv, &format!("{month} 実績取込")).await;
        assert_eq!(status, StatusCode::OK, "実績の取込: {body}");
    }
}

/// JSON の数値・文字列を文字列にする（数値は書かれたまま）。
pub fn num_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn cell_of(values: &Value, month: &str) -> (String, Value) {
    for c in values.as_array().into_iter().flatten() {
        if c["target_month"] == month {
            return (num_string(&c["amount"]), c.clone());
        }
    }
    (String::new(), Value::Null)
}

/// レスポンスから科目・月の金額（科目への直接入力）を取り出す。無ければ ""。
pub fn amount_of(body: &Value, subject_id: i64, month: &str) -> (String, Value) {
    for r in body["amounts"].as_array().into_iter().flatten() {
        if r["subject_id"] == json!(subject_id) {
            return cell_of(&r["values"], month);
        }
    }
    (String::new(), Value::Null)
}

/// 内訳の金額を返す。
pub fn line_amount_of(body: &Value, line_id: i64, month: &str) -> (String, Value) {
    for r in body["amounts"].as_array().into_iter().flatten() {
        for l in r["lines"].as_array().into_iter().flatten() {
            if l["id"] == json!(line_id) {
                return cell_of(&l["values"], month);
            }
        }
    }
    (String::new(), Value::Null)
}

pub fn counts(body: &Value) -> String {
    format!(
        "inserted={} updated={} deleted={} unchanged={}",
        body["inserted"], body["updated"], body["deleted"], body["unchanged"]
    )
}
