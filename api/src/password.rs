//! パスワードのハッシュ化と検証。
//!
//! PBKDF2-HMAC-SHA256 を使う。反復回数は OWASP の推奨値（600,000回）。
//! ハッシュは "pbkdf2-sha256$<反復回数>$<salt>$<hash>"（salt と hash は base64）の形式で保存する（Go 版と同じ）。

use std::sync::atomic::{AtomicU32, Ordering};

use base64::Engine;
use base64::engine::general_purpose::STANDARD_NO_PAD as B64;
use sha2::Sha256;
use subtle::ConstantTimeEq;

/// 新しく作るハッシュの反復回数。検証はハッシュに保存された回数で行う。
static ITERATIONS: AtomicU32 = AtomicU32::new(600_000);

/// テストを速くするために反復回数を下げる。本番コードから呼んではいけない。
pub fn set_iterations_for_testing(n: u32) {
    ITERATIONS.store(n, Ordering::Relaxed);
}

const SCHEME: &str = "pbkdf2-sha256";
const SALT_LEN: usize = 16;
const KEY_LEN: usize = 32;

/// パスワードの最小文字数。
pub const MIN_LENGTH: usize = 12;
/// パスワードの最大文字数。
pub const MAX_LENGTH: usize = 128;

/// 保存されたハッシュの形式が不正。
#[derive(Debug, PartialEq, Eq)]
pub struct InvalidHash;

/// パスワードの要件を満たしているかを検証する。
pub fn validate(pw: &str) -> Result<(), String> {
    let n = pw.chars().count();
    if n < MIN_LENGTH {
        return Err(format!("パスワードは{MIN_LENGTH}文字以上にしてください"));
    }
    if n > MAX_LENGTH {
        return Err(format!("パスワードは{MAX_LENGTH}文字以下にしてください"));
    }
    Ok(())
}

fn derive(pw: &str, salt: &[u8], iter: u32, len: usize) -> Vec<u8> {
    let mut key = vec![0u8; len];
    pbkdf2::pbkdf2_hmac::<Sha256>(pw.as_bytes(), salt, iter, &mut key);
    key
}

/// パスワードをハッシュ化する（CPU を使うので、非同期の処理からは hash_async を使う）。
pub fn hash(pw: &str) -> String {
    let mut salt = [0u8; SALT_LEN];
    rand::fill(&mut salt);
    let iter = ITERATIONS.load(Ordering::Relaxed);
    let key = derive(pw, &salt, iter, KEY_LEN);
    format!("{SCHEME}${iter}${}${}", B64.encode(salt), B64.encode(key))
}

/// パスワードがハッシュと一致するかを返す。
pub fn verify(pw: &str, encoded: &str) -> Result<bool, InvalidHash> {
    let parts: Vec<&str> = encoded.split('$').collect();
    if parts.len() != 4 || parts[0] != SCHEME {
        return Err(InvalidHash);
    }
    let iter: u32 = match parts[1].parse() {
        Ok(n) if n > 0 => n,
        _ => return Err(InvalidHash),
    };
    let salt = B64.decode(parts[2]).map_err(|_| InvalidHash)?;
    let want = B64.decode(parts[3]).map_err(|_| InvalidHash)?;
    let got = derive(pw, &salt, iter, want.len());
    Ok(got.ct_eq(&want).into())
}

/// hash を別スレッドで行う。
pub async fn hash_async(pw: String) -> String {
    tokio::task::spawn_blocking(move || hash(&pw)).await.expect("hash task")
}

/// verify を別スレッドで行う。
pub async fn verify_async(pw: String, encoded: String) -> Result<bool, InvalidHash> {
    tokio::task::spawn_blocking(move || verify(&pw, &encoded)).await.expect("verify task")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_and_verify() {
        set_iterations_for_testing(1000);
        let h = hash("correct horse battery");
        assert_eq!(verify("correct horse battery", &h), Ok(true));
        assert_eq!(verify("wrong password!!", &h), Ok(false));
        assert_ne!(hash("same password 123"), hash("same password 123"), "同じパスワードから同じハッシュが生成された");
    }

    #[test]
    fn verify_invalid_hash() {
        for h in ["", "plain", "bcrypt$1$a$b", "pbkdf2-sha256$x$a$b", "pbkdf2-sha256$1$!!$b"] {
            assert_eq!(verify("whatever", h), Err(InvalidHash), "{h}");
        }
    }

    #[test]
    fn validate_length() {
        assert!(validate("short").is_err());
        assert!(validate("パスワードは十二文字以上ですよ").is_ok());
    }

    #[test]
    fn compatible_with_go_hash() {
        // Go 版（crypto/pbkdf2）で作ったハッシュをそのまま検証できる
        let salt = B64.decode("AAECAwQFBgcICQoLDA0ODw").unwrap();
        let key = derive("correct horse battery", &salt, 1000, KEY_LEN);
        let h = format!("{SCHEME}$1000$AAECAwQFBgcICQoLDA0ODw${}", B64.encode(&key));
        assert_eq!(verify("correct horse battery", &h), Ok(true));
    }
}
