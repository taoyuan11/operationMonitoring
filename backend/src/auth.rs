use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
};

use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use anyhow::{Context, anyhow};
use axum::http::{HeaderMap, header};
use base64::{Engine, engine::general_purpose::STANDARD};
use data_encoding::BASE32_NOPAD;
use hmac::{Hmac, Mac};
use rand::{RngCore, rngs::OsRng};
use sha1::Sha1;
use subtle::ConstantTimeEq;

use crate::{
    error::{AppError, AppResult},
    state::{AdminSession, AppState},
    utils::now_ts,
};

pub const SESSION_COOKIE: &str = "om_session";
pub const SESSION_MAX_AGE: i64 = 7 * 24 * 3600;
pub const ENROLLMENT_MAX_AGE: i64 = 10 * 60;

#[derive(Clone, Debug)]
pub struct AdminPrincipal {
    pub user_id: String,
    pub username: String,
}

#[derive(Clone)]
pub struct AuthCipher(Aes256Gcm);

impl AuthCipher {
    pub fn from_key(key: &[u8]) -> anyhow::Result<Self> {
        if key.len() != 32 {
            return Err(anyhow!(
                "authentication secret key must contain exactly 32 bytes"
            ));
        }
        Ok(Self(Aes256Gcm::new_from_slice(key).map_err(|_| {
            anyhow!("failed to initialize authentication encryption")
        })?))
    }

    pub fn encrypt(&self, plaintext: &[u8]) -> anyhow::Result<String> {
        let mut nonce_bytes = [0_u8; 12];
        OsRng.fill_bytes(&mut nonce_bytes);
        let ciphertext = self
            .0
            .encrypt(Nonce::from_slice(&nonce_bytes), plaintext)
            .map_err(|_| anyhow!("failed to encrypt authenticator secret"))?;
        let mut encoded = nonce_bytes.to_vec();
        encoded.extend_from_slice(&ciphertext);
        Ok(STANDARD.encode(encoded))
    }

    pub fn decrypt(&self, encoded: &str) -> anyhow::Result<Vec<u8>> {
        let bytes = STANDARD
            .decode(encoded)
            .context("invalid encrypted authenticator secret")?;
        if bytes.len() <= 12 {
            return Err(anyhow!("invalid encrypted authenticator secret"));
        }
        self.0
            .decrypt(Nonce::from_slice(&bytes[..12]), &bytes[12..])
            .map_err(|_| anyhow!("unable to decrypt authenticator secret"))
    }
}

pub fn load_auth_cipher(
    configured_key: Option<&str>,
    key_file: &Path,
) -> anyhow::Result<AuthCipher> {
    let key = if let Some(configured) = configured_key {
        STANDARD
            .decode(configured.trim())
            .context("OM_AUTH_SECRET_KEY must be a base64-encoded 32-byte key")?
    } else {
        load_or_create_key_file(key_file)?
    };
    AuthCipher::from_key(&key)
}

fn load_or_create_key_file(path: &Path) -> anyhow::Result<Vec<u8>> {
    match fs::read_to_string(path) {
        Ok(value) => STANDARD
            .decode(value.trim())
            .with_context(|| format!("invalid authentication key file {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                fs::create_dir_all(parent).with_context(|| {
                    format!(
                        "failed to create authentication key directory {}",
                        parent.display()
                    )
                })?;
            }
            let mut key = [0_u8; 32];
            OsRng.fill_bytes(&mut key);
            let encoded = STANDARD.encode(key);
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(path) {
                Ok(mut file) => {
                    file.write_all(encoded.as_bytes()).with_context(|| {
                        format!("failed to write authentication key file {}", path.display())
                    })?;
                    Ok(key.to_vec())
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let value = fs::read_to_string(path).with_context(|| {
                        format!("failed to read authentication key file {}", path.display())
                    })?;
                    STANDARD.decode(value.trim()).with_context(|| {
                        format!("invalid authentication key file {}", path.display())
                    })
                }
                Err(error) => Err(error).with_context(|| {
                    format!(
                        "failed to create authentication key file {}",
                        path.display()
                    )
                }),
            }
        }
        Err(error) => Err(error)
            .with_context(|| format!("failed to read authentication key file {}", path.display())),
    }
}

pub fn generate_totp_secret() -> Vec<u8> {
    let mut secret = vec![0_u8; 20];
    OsRng.fill_bytes(&mut secret);
    secret
}

pub fn otpauth_uri(username: &str, secret: &[u8]) -> String {
    let issuer = "Operation Monitoring";
    let label = format!("{issuer}:{username}");
    format!(
        "otpauth://totp/{}?secret={}&issuer={}&algorithm=SHA1&digits=6&period=30",
        urlencoding::encode(&label),
        BASE32_NOPAD.encode(secret),
        urlencoding::encode(issuer),
    )
}

pub fn verify_totp(secret: &[u8], code: &str, timestamp: i64) -> bool {
    let code = code.trim();
    if code.len() != 6 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    let counter = timestamp.max(0) as u64 / 30;
    [-1_i64, 0, 1].into_iter().any(|offset| {
        let candidate_counter = if offset.is_negative() {
            counter.saturating_sub(offset.unsigned_abs())
        } else {
            counter.saturating_add(offset as u64)
        };
        let candidate = totp_code(secret, candidate_counter);
        candidate.as_bytes().ct_eq(code.as_bytes()).into()
    })
}

fn totp_code(secret: &[u8], counter: u64) -> String {
    let mut mac = <Hmac<Sha1> as Mac>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(&counter.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = (digest[19] & 0x0f) as usize;
    let binary = ((u32::from(digest[offset]) & 0x7f) << 24)
        | (u32::from(digest[offset + 1]) << 16)
        | (u32::from(digest[offset + 2]) << 8)
        | u32::from(digest[offset + 3]);
    format!("{:06}", binary % 1_000_000)
}

#[cfg(test)]
pub(crate) fn totp_code_at(secret: &[u8], timestamp: i64) -> String {
    totp_code(secret, timestamp.max(0) as u64 / 30)
}

pub fn validate_username(username: &str) -> AppResult<(String, String)> {
    let username = username.trim();
    let valid = (3..=32).contains(&username.len())
        && username
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if !valid {
        return Err(AppError::bad_request(
            "用户名需为 3–32 位字母、数字、点、下划线或连字符",
        ));
    }
    Ok((username.to_string(), username.to_ascii_lowercase()))
}

pub async fn require_admin(state: &AppState, headers: &HeaderMap) -> AppResult<AdminPrincipal> {
    let Some(token) = session_token(headers) else {
        return Err(AppError::unauthorized());
    };
    let now = now_ts();
    let session = {
        let mut sessions = state.sessions.write().await;
        sessions.retain(|_, session| session.expires_at > now);
        sessions.get(&token).cloned()
    };
    let Some(session) = session else {
        return Err(AppError::unauthorized());
    };

    let valid: Option<bool> = sqlx::query_scalar(
        r#"
        SELECT TRUE
        FROM admin_users u
        JOIN authenticator_devices d ON d.user_id = u.id
        WHERE u.id = $1 AND u.enabled = 1 AND d.id = $2
        "#,
    )
    .bind(&session.user_id)
    .bind(&session.device_id)
    .fetch_optional(&state.db)
    .await?;
    if valid.is_none() {
        state.sessions.write().await.remove(&token);
        return Err(AppError::unauthorized());
    }

    Ok(AdminPrincipal {
        user_id: session.user_id,
        username: session.username,
    })
}

pub async fn insert_session(
    state: &AppState,
    token: String,
    user_id: String,
    username: String,
    device_id: String,
) {
    state.sessions.write().await.insert(
        token,
        AdminSession {
            user_id,
            username,
            device_id,
            expires_at: now_ts() + SESSION_MAX_AGE,
        },
    );
}

pub fn session_token(headers: &HeaderMap) -> Option<String> {
    let cookie = headers.get(header::COOKIE)?.to_str().ok()?;
    cookie.split(';').find_map(|part| {
        let (name, value) = part.trim().split_once('=')?;
        (name == SESSION_COOKIE).then(|| value.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifies_standard_totp_vector_and_adjacent_window() {
        let secret = b"12345678901234567890";
        assert!(verify_totp(secret, &"94287082"[2..], 59));
        let code = totp_code(secret, 2);
        assert!(verify_totp(secret, &code, 59));
        assert!(!verify_totp(secret, "not-a-code", 59));
    }

    #[test]
    fn encrypts_authenticator_secrets_without_plaintext() {
        let cipher = AuthCipher::from_key(&[7_u8; 32]).expect("create cipher");
        let encrypted = cipher.encrypt(b"top-secret").expect("encrypt");
        assert!(!encrypted.contains("top-secret"));
        assert_eq!(cipher.decrypt(&encrypted).expect("decrypt"), b"top-secret");
    }

    #[test]
    fn validates_normalized_usernames() {
        assert_eq!(
            validate_username(" Admin.User ").expect("valid username"),
            ("Admin.User".to_string(), "admin.user".to_string())
        );
        assert!(validate_username("管理员").is_err());
    }
}
