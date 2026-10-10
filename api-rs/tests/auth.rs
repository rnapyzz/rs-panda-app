//! 認証（パスワード・SSO・ログイン失敗の制限・CSRF）の統合テスト。Go 版の auth_test.go・sso_test.go。

mod common;

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use common::*;
use fpanda_api::config::OidcConfig;
use reqwest::StatusCode;
use rsa::traits::PublicKeyParts;
use rsa::{Pkcs1v15Sign, RsaPrivateKey};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

#[tokio::test]
async fn login_and_me() {
    let Some(env) = TestEnv::start().await else { return };
    env.create_user("admin@example.com", "fpa_admin").await;
    let c = env.client();

    assert_eq!(c.get("/api/auth/me").await.0, StatusCode::UNAUTHORIZED, "未ログインの /me");

    // メールアドレスは大文字・前後空白を正規化して照合する
    let (status, body) =
        c.post("/api/auth/login", json!({"email": "  Admin@Example.com ", "password": TEST_PASSWORD})).await;
    assert_eq!(status, StatusCode::OK, "login: {body}");

    let body = c.must_get("/api/auth/me").await;
    let u = &body["user"];
    assert_eq!((u["email"].as_str(), u["role"].as_str()), (Some("admin@example.com"), Some("fpa_admin")));
    assert!(u.get("password_hash").is_none(), "レスポンスに password_hash が含まれている");
}

#[tokio::test]
async fn login_failures() {
    let Some(env) = TestEnv::start().await else { return };
    let id = env.create_user("member@example.com", "member").await;
    let c = env.client();

    let cases = [
        (
            "パスワード誤り",
            json!({"email": "member@example.com", "password": "wrong-password-1"}),
            StatusCode::UNAUTHORIZED,
            false,
        ),
        (
            "未登録のメールアドレス",
            json!({"email": "nobody@example.com", "password": TEST_PASSWORD}),
            StatusCode::UNAUTHORIZED,
            false,
        ),
        ("未入力", json!({"email": "", "password": ""}), StatusCode::UNPROCESSABLE_ENTITY, false),
        (
            "無効化されたユーザー",
            json!({"email": "member@example.com", "password": TEST_PASSWORD}),
            StatusCode::UNAUTHORIZED,
            true,
        ),
    ];
    for (name, body, want, deactivate) in cases {
        if deactivate {
            env.exec("UPDATE users SET is_active = FALSE WHERE id = ?", (id,)).await;
        }
        let (status, res) = c.post("/api/auth/login", body).await;
        assert_eq!(status, want, "{name}: {res}");
    }
}

#[tokio::test]
async fn logout_invalidates_session() {
    let Some(env) = TestEnv::start().await else { return };
    env.create_user("viewer@example.com", "viewer").await;
    let c = env.client();
    let (status, _) =
        c.post("/api/auth/login", json!({"email": "viewer@example.com", "password": TEST_PASSWORD})).await;
    assert_eq!(status, StatusCode::OK);

    // ログアウト前の Cookie を保存しておき、ログアウト後に再送しても使えないことを確認する
    let token: String = {
        let res = env
            .client()
            .http
            .post(format!("{}/api/auth/login", env.base))
            .json(&json!({"email": "viewer@example.com", "password": TEST_PASSWORD}))
            .send()
            .await
            .unwrap();
        let sc = res.headers().get("set-cookie").unwrap().to_str().unwrap().to_string();
        assert!(sc.contains("HttpOnly") && sc.contains("SameSite=Lax") && sc.contains("Path=/"), "{sc}");
        sc.split(';').next().unwrap().to_string()
    };
    let old = reqwest::Client::new();
    let me = |cookie: String| {
        let old = old.clone();
        let url = format!("{}/api/auth/me", env.base);
        async move { old.get(url).header("Cookie", cookie).send().await.unwrap().status() }
    };
    assert_eq!(me(token.clone()).await, StatusCode::OK);

    // ログアウトはそのクライアント（Cookie）のセッションを削除する
    let res = old.post(format!("{}/api/auth/logout", env.base)).header("Cookie", token.clone()).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    assert!(res.headers().get("set-cookie").unwrap().to_str().unwrap().contains("Max-Age=0"));
    assert_eq!(me(token).await, StatusCode::UNAUTHORIZED, "古い Cookie での /me");

    assert_eq!(c.post("/api/auth/logout", Value::Null).await.0, StatusCode::NO_CONTENT);
    assert_eq!(c.get("/api/auth/me").await.0, StatusCode::UNAUTHORIZED, "ログアウト後の /me");
}

#[tokio::test]
async fn deactivated_user_loses_session() {
    let Some(env) = TestEnv::start().await else { return };
    let id = env.create_user("manager@example.com", "manager").await;
    let c = env.client();
    c.login("manager@example.com").await;
    env.exec("UPDATE users SET is_active = FALSE WHERE id = ?", (id,)).await;
    assert_eq!(c.get("/api/auth/me").await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn cross_origin_request_rejected() {
    let Some(env) = TestEnv::start().await else { return };
    env.create_user("admin@example.com", "fpa_admin").await;
    let body = json!({"email": "admin@example.com", "password": TEST_PASSWORD});
    let http = reqwest::Client::new();
    let url = format!("{}/api/auth/login", env.base);

    let res = http.post(&url).header("Sec-Fetch-Site", "cross-site").json(&body).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN, "別オリジンからの POST");
    let res = http.post(&url).header("Origin", "http://evil.example.com").json(&body).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN, "別オリジンからの POST（Origin）");
    let res = http.post(&url).header("Origin", env.base.clone()).json(&body).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::OK, "同じオリジンからの POST");
    let res = http.post(&url).header("Sec-Fetch-Site", "same-origin").json(&body).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn unknown_api_path_returns_json_404() {
    let Some(env) = TestEnv::start().await else { return };
    let c = env.client();
    let (status, body) = c.get("/api/nope").await;
    assert_eq!((status, error_code(&body)), (StatusCode::NOT_FOUND, "not_found"));
    // 定義済みのパスでもメソッドが違えば同じ 404
    let (status, body) = c.do_req("DELETE", "/api/health", None).await;
    assert_eq!((status, error_code(&body)), (StatusCode::NOT_FOUND, "not_found"));
    let body = c.must_get("/api/health").await;
    assert_eq!(body, json!({"status": "ok", "database": "ok"}));
}

#[tokio::test]
async fn login_throttle() {
    let Some(env) = TestEnv::start().await else { return };
    env.create_user("member@example.com", "member").await;
    let c = env.client();
    for i in 0..5 {
        let (status, _) =
            c.post("/api/auth/login", json!({"email": "member@example.com", "password": "wrong-password-123"})).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{} 回目の失敗", i + 1);
    }
    // 5回失敗すると、正しいパスワードでも止める
    let (status, body) =
        c.post("/api/auth/login", json!({"email": "member@example.com", "password": TEST_PASSWORD})).await;
    assert_eq!((status, error_code(&body)), (StatusCode::TOO_MANY_REQUESTS, "too_many_attempts"));
    // SSO が無効な環境では、だれでもパスワードでログインできる
    let body = env.client().must_get("/api/auth/config").await;
    assert_eq!(body["sso"], json!(false));
    assert_eq!(body["password_roles"].as_array().unwrap().len(), 4);
}

// --- SSO ---

/// OIDC の IdP の代わり（discovery・JWKS・トークンエンドポイント）。次に発行する ID トークンの内容を claims で決める。
#[derive(Clone)]
struct FakeIdp {
    url: String,
    key: Arc<RsaPrivateKey>,
    claims: Arc<Mutex<Map<String, Value>>>,
    /// トークンの交換で受け取った PKCE の検証値
    verifier: Arc<Mutex<String>>,
}

impl FakeIdp {
    async fn start() -> FakeIdp {
        let key = RsaPrivateKey::new(&mut rand_core::OsRng, 2048).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let idp = FakeIdp {
            url,
            key: Arc::new(key),
            claims: Arc::new(Mutex::new(Map::new())),
            verifier: Arc::new(Mutex::new(String::new())),
        };
        let app = Router::new()
            .route(
                "/.well-known/openid-configuration",
                get(|State(idp): State<FakeIdp>| async move {
                    Json(json!({
                        "issuer": idp.url,
                        "authorization_endpoint": format!("{}/authorize", idp.url),
                        "token_endpoint": format!("{}/token", idp.url),
                        "jwks_uri": format!("{}/jwks", idp.url),
                    }))
                }),
            )
            .route(
                "/jwks",
                get(|State(idp): State<FakeIdp>| async move {
                    Json(json!({"keys": [{
                        "kty": "RSA", "kid": "k1", "alg": "RS256",
                        "n": URL_SAFE_NO_PAD.encode(idp.key.n().to_bytes_be()),
                        "e": URL_SAFE_NO_PAD.encode(idp.key.e().to_bytes_be()),
                    }]}))
                }),
            )
            .route(
                "/token",
                post(|State(idp): State<FakeIdp>, Form(form): Form<std::collections::HashMap<String, String>>| async move {
                    *idp.verifier.lock().unwrap() = form.get("code_verifier").cloned().unwrap_or_default();
                    let claims = idp.claims.lock().unwrap().clone();
                    Json(json!({"id_token": idp.sign(&Value::Object(claims)), "token_type": "Bearer"}))
                }),
            )
            .with_state(idp.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        idp
    }

    fn sign(&self, claims: &Value) -> String {
        let enc = |v: &Value| URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap());
        let signing = format!("{}.{}", enc(&json!({"alg": "RS256", "kid": "k1", "typ": "JWT"})), enc(claims));
        let digest = Sha256::digest(signing.as_bytes());
        let sig = self.key.sign(Pkcs1v15Sign::new::<rsa::sha2::Sha256>(), &digest).unwrap();
        format!("{signing}.{}", URL_SAFE_NO_PAD.encode(sig))
    }
}

fn query_of(location: &str) -> std::collections::HashMap<String, String> {
    let q = location.split_once('?').map(|(_, q)| q).unwrap_or("");
    form_urlencoded::parse(q.as_bytes()).into_owned().collect()
}

fn location(res: &reqwest::Response) -> String {
    res.headers().get("location").and_then(|v| v.to_str().ok()).unwrap_or("").to_string()
}

/// SSO のログインを最後まで進め、コールバックのリダイレクト先を返す。claims の nonce はログインの開始で決まる。
async fn sso_login(env: &TestEnv, idp: &FakeIdp, claims: Value) -> (Client, String) {
    let c = env.client();
    let res = c.http.get(format!("{}/api/auth/oidc/login?return_to=/reports", env.base)).send().await.unwrap();
    let loc = location(&res);
    let q = query_of(&loc);
    assert!(
        res.status() == StatusCode::FOUND
            && loc.starts_with(&format!("{}/authorize", idp.url))
            && q["code_challenge_method"] == "S256"
            && q["hd"] == "example.com",
        "ログインの開始: status = {}, location = {loc}",
        res.status()
    );
    let now = chrono::Utc::now().timestamp();
    let mut full = json!({"iss": idp.url, "aud": "client-1", "exp": now + 3600, "iat": now, "nonce": q["nonce"], "email_verified": true});
    for (k, v) in claims.as_object().unwrap() {
        full[k] = v.clone();
    }
    *idp.claims.lock().unwrap() = full.as_object().unwrap().clone();

    let mut state = String::new();
    state.extend(form_urlencoded::byte_serialize(q["state"].as_bytes()));
    let res = c.http.get(format!("{}/api/auth/oidc/callback?code=abc&state={state}", env.base)).send().await.unwrap();
    // PKCE: トークンの交換で送った検証値が、ログインの開始で送った challenge と一致する
    let verifier = idp.verifier.lock().unwrap().clone();
    assert_eq!(
        URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
        q["code_challenge"],
        "PKCE の検証値が一致しない"
    );
    let loc = location(&res);
    (c, loc)
}

#[tokio::test]
async fn sso_login_flow() {
    if std::env::var("TEST_DB_HOST").unwrap_or_default().is_empty() {
        return;
    }
    let idp = FakeIdp::start().await;
    let Some(env) = TestEnv::start_with(Options {
        oidc: Some(OidcConfig {
            issuer: idp.url.clone(),
            client_id: "client-1".into(),
            client_secret: "secret".into(),
            redirect_url: "http://app/api/auth/oidc/callback".into(),
            allowed_domains: vec!["example.com".into()],
        }),
    })
    .await
    else {
        return;
    };
    env.create_user("member@example.com", "member").await;
    env.create_user("admin@example.com", "fpa_admin").await;

    // 登録済みのユーザーはログインでき、元の画面へ戻る。初回は sub を記録する
    let (c, loc) =
        sso_login(&env, &idp, json!({"sub": "g-1", "email": "Member@example.com", "hd": "example.com"})).await;
    assert_eq!(loc, "/reports");
    assert_eq!(c.get("/api/auth/me").await.0, StatusCode::OK, "SSO でログインした後の /me");
    let sub: String = env.query_one("SELECT oidc_subject FROM users WHERE email = 'member@example.com'", ()).await;
    assert_eq!(sub, "g-1");

    let now = chrono::Utc::now().timestamp();
    let cases = [
        (
            "未登録",
            json!({"sub": "g-9", "email": "nobody@example.com", "hd": "example.com"}),
            "/?login_error=not_registered",
        ),
        (
            "別の sub（メールアドレスの付け替え）",
            json!({"sub": "g-2", "email": "member@example.com", "hd": "example.com"}),
            "/?login_error=not_registered",
        ),
        (
            "許可していないドメイン",
            json!({"sub": "g-3", "email": "admin@other.com", "hd": "other.com"}),
            "/?login_error=domain",
        ),
        (
            "メールアドレスが未確認",
            json!({"sub": "g-4", "email": "admin@example.com", "email_verified": false}),
            "/?login_error=not_registered",
        ),
        ("期限切れ", json!({"sub": "g-5", "email": "admin@example.com", "exp": now - 3600}), "/?login_error=failed"),
        (
            "宛先の違い",
            json!({"sub": "g-6", "email": "admin@example.com", "aud": "other-client"}),
            "/?login_error=failed",
        ),
    ];
    for (name, claims, want) in cases {
        let (_, loc) = sso_login(&env, &idp, claims).await;
        assert_eq!(loc, want, "{name}");
    }

    // state が違えば通さない
    let nc = env.client();
    nc.http.get(format!("{}/api/auth/oidc/login", env.base)).send().await.unwrap();
    let res = nc.http.get(format!("{}/api/auth/oidc/callback?code=abc&state=wrong", env.base)).send().await.unwrap();
    assert_eq!(location(&res), "/?login_error=failed", "state の違い");

    // SSO が有効な環境では、パスワードでログインできるのは FP&A だけ
    let member = env.client();
    let (status, body) =
        member.post("/api/auth/login", json!({"email": "member@example.com", "password": TEST_PASSWORD})).await;
    assert_eq!((status, error_code(&body)), (StatusCode::FORBIDDEN, "sso_required"), "担当者のパスワードでのログイン");
    env.client().login("admin@example.com").await;
    let (status, body) = member.get("/api/auth/config").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["sso"], json!(true));
    assert_eq!(body["password_roles"], json!(["fpa_admin"]));
}
