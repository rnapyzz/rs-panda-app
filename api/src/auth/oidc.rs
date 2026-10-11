//! SSO（OIDC）のログイン（docs/architecture.md「SSO（OIDC）」）。
//! 認可コードフロー（PKCE・state・nonce 付き）で ID トークンを受け取り、署名と内容を検証する。
//! 事前に登録したユーザーだけがログインできる（メールアドレスで照合し、初回に sub を記録して以後も照合する）。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rsa::{BigUint, Pkcs1v15Sign, RsaPublicKey};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::config::OidcConfig;

const DISCOVERY_TTL: Duration = Duration::from_secs(3600);
const JWKS_TTL: Duration = Duration::from_secs(3600);
const CLOCK_SKEW_SECS: i64 = 120;

/// ID トークンの検証の失敗。ログイン画面に返す理由（login_error）に対応する。
#[derive(Debug)]
pub enum OidcError {
    /// アプリに登録されていない（メールアドレスが未確認を含む）
    NotRegistered,
    /// 許可していないドメインのアカウント
    DomainNotAllowed,
    Other(anyhow::Error),
}

impl<E: Into<anyhow::Error>> From<E> for OidcError {
    fn from(e: E) -> Self {
        OidcError::Other(e.into())
    }
}

#[derive(Clone, Debug, Deserialize)]
struct Discovery {
    #[serde(default)]
    issuer: String,
    #[serde(default)]
    authorization_endpoint: String,
    #[serde(default)]
    token_endpoint: String,
    #[serde(default)]
    jwks_uri: String,
}

#[derive(Default)]
struct Cache {
    discovery: Option<(Discovery, Instant)>,
    keys: HashMap<String, RsaPublicKey>,
    keys_at: Option<Instant>,
}

/// IdP の設定（discovery）と公開鍵（JWKS）をキャッシュし、ID トークンを検証する。
pub struct Oidc {
    cfg: OidcConfig,
    client: reqwest::Client,
    cache: Mutex<Cache>,
}

/// ログインの開始から戻りまでの検証値（短命の Cookie に入れる）。
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct AuthState {
    #[serde(rename = "s")]
    pub state: String,
    #[serde(rename = "n")]
    pub nonce: String,
    #[serde(rename = "v")]
    pub verifier: String,
    #[serde(rename = "r")]
    pub return_to: String,
}

impl AuthState {
    /// state・nonce・PKCE の検証値を作る。return_to はログイン後に戻るパス（アプリ内の相対パスだけ）。
    pub fn new(return_to: &str) -> AuthState {
        let return_to = if !return_to.starts_with('/') || return_to.starts_with("//") || return_to.starts_with("/api/")
        {
            "/".to_string()
        } else {
            return_to.to_string()
        };
        AuthState {
            state: super::random_string(),
            nonce: super::random_string(),
            verifier: super::random_string(),
            return_to,
        }
    }
}

/// ID トークンの内容のうち、使うもの。
#[derive(Debug, Default, Deserialize)]
pub struct IdClaims {
    #[serde(default)]
    pub iss: String,
    #[serde(default)]
    pub sub: String,
    #[serde(default)]
    pub aud: Value, // 文字列か配列
    #[serde(default)]
    pub exp: i64,
    #[serde(default)]
    pub iat: i64,
    #[serde(default)]
    pub nonce: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub email_verified: Value, // true か "true"
    #[serde(default)]
    pub hd: String,
}

impl Oidc {
    /// IdP の設定は最初に使うときに読む（起動時に IdP へ接続できなくてもよい）。
    pub fn new(mut cfg: OidcConfig) -> Oidc {
        if cfg.issuer.is_empty() {
            cfg.issuer = "https://accounts.google.com".into();
        }
        let client = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().expect("http client");
        Oidc { cfg, client, cache: Mutex::new(Cache::default()) }
    }

    pub fn config(&self) -> &OidcConfig {
        &self.cfg
    }

    async fn get_json<T: DeserializeOwned>(&self, url: &str) -> anyhow::Result<T> {
        let res = self.client.get(url).send().await?;
        if res.status() != reqwest::StatusCode::OK {
            bail!("oidc: GET {url}: {}", res.status().as_u16());
        }
        let body = res.bytes().await?;
        if body.len() > 1 << 20 {
            bail!("oidc: GET {url}: response too large");
        }
        Ok(serde_json::from_slice(&body)?)
    }

    async fn load_discovery(&self) -> anyhow::Result<Discovery> {
        let mut cache = self.cache.lock().await;
        if let Some((d, at)) = &cache.discovery
            && at.elapsed() < DISCOVERY_TTL
        {
            return Ok(d.clone());
        }
        let d: Discovery = self
            .get_json(&format!("{}/.well-known/openid-configuration", self.cfg.issuer.trim_end_matches('/')))
            .await?;
        if d.issuer != self.cfg.issuer
            || d.authorization_endpoint.is_empty()
            || d.token_endpoint.is_empty()
            || d.jwks_uri.is_empty()
        {
            bail!("oidc: invalid discovery document (issuer {:?})", d.issuer);
        }
        cache.discovery = Some((d.clone(), Instant::now()));
        Ok(d)
    }

    /// kid の公開鍵を返す。見つからなければ JWKS を読み直す（鍵の更新に追随する）。
    async fn public_key(&self, jwks_uri: &str, kid: &str) -> anyhow::Result<RsaPublicKey> {
        {
            let cache = self.cache.lock().await;
            let fresh = cache.keys_at.is_some_and(|at| at.elapsed() < JWKS_TTL);
            if let (Some(k), true) = (cache.keys.get(kid), fresh) {
                return Ok(k.clone());
            }
        }
        #[derive(Deserialize)]
        struct Jwk {
            #[serde(default)]
            kty: String,
            #[serde(default)]
            kid: String,
            #[serde(default)]
            n: String,
            #[serde(default)]
            e: String,
        }
        #[derive(Deserialize)]
        struct JwkSet {
            #[serde(default)]
            keys: Vec<Jwk>,
        }
        let set: JwkSet = self.get_json(jwks_uri).await?;
        let mut keys = HashMap::new();
        for k in set.keys {
            if k.kty != "RSA" {
                continue;
            }
            let (Ok(n), Ok(e)) = (URL_SAFE_NO_PAD.decode(&k.n), URL_SAFE_NO_PAD.decode(&k.e)) else { continue };
            if e.is_empty() || e.len() > 4 {
                continue;
            }
            if let Ok(key) = RsaPublicKey::new(BigUint::from_bytes_be(&n), BigUint::from_bytes_be(&e)) {
                keys.insert(k.kid, key);
            }
        }
        let found = keys.get(kid).cloned();
        let mut cache = self.cache.lock().await;
        cache.keys = keys;
        cache.keys_at = Some(Instant::now());
        found.ok_or_else(|| anyhow!("oidc: unknown key id {kid:?}"))
    }

    /// IdP の認可エンドポイントの URL を返す。
    pub async fn auth_url(&self, st: &AuthState) -> anyhow::Result<String> {
        let d = self.load_discovery().await?;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(st.verifier.as_bytes()));
        let mut q = form_urlencoded::Serializer::new(String::new());
        q.append_pair("client_id", &self.cfg.client_id)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");
        if self.cfg.allowed_domains.len() == 1 {
            // Google: アカウントの選択をそのドメインに絞る（検証はトークンで行う）
            q.append_pair("hd", &self.cfg.allowed_domains[0]);
        }
        q.append_pair("nonce", &st.nonce)
            .append_pair("prompt", "select_account")
            .append_pair("redirect_uri", &self.cfg.redirect_url)
            .append_pair("response_type", "code")
            .append_pair("scope", "openid email profile")
            .append_pair("state", &st.state);
        Ok(format!("{}?{}", d.authorization_endpoint, q.finish()))
    }

    /// 認可コードを ID トークンに交換し、検証した内容を返す。
    pub async fn exchange(&self, code: &str, st: &AuthState) -> Result<IdClaims, OidcError> {
        let d = self.load_discovery().await?;
        let res = self
            .client
            .post(&d.token_endpoint)
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", &self.cfg.redirect_url),
                ("client_id", &self.cfg.client_id),
                ("client_secret", &self.cfg.client_secret),
                ("code_verifier", &st.verifier),
            ])
            .send()
            .await?;
        if res.status() != reqwest::StatusCode::OK {
            let status = res.status().as_u16();
            let body = res.text().await.unwrap_or_default();
            let body: String = body.chars().take(500).collect();
            return Err(anyhow!("oidc: token endpoint: {status} {body}").into());
        }
        #[derive(Deserialize)]
        struct Token {
            #[serde(default)]
            id_token: String,
        }
        let tok: Token = serde_json::from_slice(&res.bytes().await?)?;
        self.verify(&d, &tok.id_token, &st.nonce).await
    }

    /// ID トークン（RS256 の JWT）の署名と内容を検証する。
    async fn verify(&self, d: &Discovery, token: &str, nonce: &str) -> Result<IdClaims, OidcError> {
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() != 3 {
            return Err(anyhow!("oidc: malformed id token").into());
        }
        #[derive(Deserialize)]
        struct Header {
            #[serde(default)]
            alg: String,
            #[serde(default)]
            kid: String,
        }
        let header: Header = decode_segment(parts[0])?;
        if header.alg != "RS256" {
            return Err(anyhow!("oidc: unsupported alg {:?}", header.alg).into());
        }
        let key = self.public_key(&d.jwks_uri, &header.kid).await?;
        let sig = URL_SAFE_NO_PAD.decode(parts[2])?;
        let digest = Sha256::digest(format!("{}.{}", parts[0], parts[1]).as_bytes());
        if key.verify(Pkcs1v15Sign::new::<rsa::sha2::Sha256>(), &digest, &sig).is_err() {
            return Err(anyhow!("oidc: invalid signature").into());
        }
        let c: IdClaims = decode_segment(parts[1])?;
        let now = chrono::Utc::now().timestamp();
        if c.iss != d.issuer {
            return Err(anyhow!("oidc: unexpected issuer {:?}", c.iss).into());
        }
        if !audience_contains(&c.aud, &self.cfg.client_id) {
            return Err(anyhow!("oidc: unexpected audience").into());
        }
        if now > c.exp + CLOCK_SKEW_SECS {
            return Err(anyhow!("oidc: id token expired").into());
        }
        if c.iat > now + CLOCK_SKEW_SECS {
            return Err(anyhow!("oidc: id token issued in the future").into());
        }
        if c.nonce != nonce {
            return Err(anyhow!("oidc: nonce mismatch").into());
        }
        if c.sub.is_empty() || c.email.is_empty() {
            return Err(anyhow!("oidc: missing sub or email").into());
        }
        if c.email_verified != Value::Bool(true) && c.email_verified != Value::String("true".into()) {
            return Err(OidcError::NotRegistered);
        }
        if !self.cfg.allowed_domains.is_empty() {
            let domain = if c.hd.is_empty() {
                c.email.rsplit_once('@').map(|(_, d)| d).unwrap_or(&c.email).to_string()
            } else {
                c.hd.clone()
            };
            if !self.cfg.allowed_domains.contains(&domain.to_lowercase()) {
                return Err(OidcError::DomainNotAllowed);
            }
        }
        Ok(c)
    }
}

fn decode_segment<T: DeserializeOwned>(seg: &str) -> anyhow::Result<T> {
    let b = URL_SAFE_NO_PAD.decode(seg).map_err(|e| anyhow!("oidc: malformed segment: {e}"))?;
    Ok(serde_json::from_slice(&b)?)
}

fn audience_contains(aud: &Value, client_id: &str) -> bool {
    match aud {
        Value::String(s) => s == client_id,
        Value::Array(a) => a.iter().any(|v| v.as_str() == Some(client_id)),
        _ => false,
    }
}
