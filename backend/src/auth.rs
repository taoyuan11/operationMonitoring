use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
    time::Duration,
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
use tokio::sync::watch;
use tracing::warn;

use crate::{
    error::{AppError, AppResult},
    state::{AdminSession, AppState},
    utils::now_ts,
};

pub const SESSION_COOKIE: &str = "om_session";
pub const SECURE_SESSION_COOKIE: &str = "__Secure-om_session";
pub const SESSION_MAX_AGE: i64 = 7 * 24 * 3600;
pub const ENROLLMENT_MAX_AGE: i64 = 10 * 60;
const SESSION_REVALIDATE_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Clone, Debug)]
pub struct AdminPrincipal {
    pub user_id: String,
    pub username: String,
    pub(crate) session_token: String,
    pub(crate) device_id: String,
    session_guard: AdminSessionGuard,
}

impl AdminPrincipal {
    pub fn session_guard(&self) -> AdminSessionGuard {
        self.session_guard.clone()
    }
}

#[derive(Clone, Debug)]
pub struct AdminSessionGuard {
    token: String,
    user_id: String,
    device_id: String,
    expires_at: i64,
    revoked_rx: watch::Receiver<bool>,
}

impl AdminSessionGuard {
    pub async fn wait_until_invalid(mut self, state: AppState) {
        if !admin_session_is_valid(&state, &self).await {
            return;
        }

        let mut interval = tokio::time::interval(SESSION_REVALIDATE_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval.tick().await;
        loop {
            tokio::select! {
                changed = self.revoked_rx.changed() => {
                    if changed.is_err() || *self.revoked_rx.borrow() {
                        return;
                    }
                }
                _ = interval.tick() => {
                    if !admin_session_is_valid(&state, &self).await {
                        return;
                    }
                }
            }
        }
    }
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

#[cfg(test)]
pub fn verify_totp(secret: &[u8], code: &str, timestamp: i64) -> bool {
    verify_totp_counter(secret, code, timestamp).is_some()
}

pub fn verify_totp_counter(secret: &[u8], code: &str, timestamp: i64) -> Option<i64> {
    let code = code.trim();
    if code.len() != 6 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let counter = timestamp.max(0) / 30;
    [0_i64, -1, 1].into_iter().find_map(|offset| {
        let candidate_counter = counter.saturating_add(offset).max(0);
        let candidate = totp_code(secret, candidate_counter as u64);
        bool::from(candidate.as_bytes().ct_eq(code.as_bytes())).then_some(candidate_counter)
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
        sessions.retain(|_, session| {
            let active = session.expires_at > now;
            if !active {
                session.revoked_tx.send_replace(true);
            }
            active
        });
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
        revoke_session(state, &token).await;
        return Err(AppError::unauthorized());
    }

    let session_guard = AdminSessionGuard {
        token: token.clone(),
        user_id: session.user_id.clone(),
        device_id: session.device_id.clone(),
        expires_at: session.expires_at,
        revoked_rx: session.revoked_tx.subscribe(),
    };
    Ok(AdminPrincipal {
        user_id: session.user_id,
        username: session.username,
        session_token: token,
        device_id: session.device_id,
        session_guard,
    })
}

pub async fn insert_session(
    state: &AppState,
    token: String,
    user_id: String,
    username: String,
    device_id: String,
    login_totp_counter: Option<i64>,
) {
    let (revoked_tx, _) = watch::channel(false);
    state.sessions.write().await.insert(
        token,
        AdminSession {
            user_id,
            username,
            device_id,
            login_totp_counter,
            expires_at: now_ts() + SESSION_MAX_AGE,
            revoked_tx,
        },
    );
}

pub async fn revoke_session(state: &AppState, token: &str) {
    if let Some(session) = state.sessions.write().await.remove(token) {
        session.revoked_tx.send_replace(true);
    }
}

pub async fn revoke_user_sessions(state: &AppState, user_id: &str) {
    revoke_matching_sessions(state, |session| session.user_id == user_id).await;
}

pub async fn revoke_device_sessions(state: &AppState, device_id: &str) {
    revoke_matching_sessions(state, |session| session.device_id == device_id).await;
}

async fn revoke_matching_sessions(state: &AppState, predicate: impl Fn(&AdminSession) -> bool) {
    let mut sessions = state.sessions.write().await;
    revoke_matching_session_entries(&mut sessions, predicate);
}

fn revoke_matching_session_entries(
    sessions: &mut std::collections::HashMap<String, AdminSession>,
    predicate: impl Fn(&AdminSession) -> bool,
) {
    sessions.retain(|_, session| {
        let revoked = predicate(session);
        if revoked {
            session.revoked_tx.send_replace(true);
        }
        !revoked
    });
}

async fn admin_session_is_valid(state: &AppState, guard: &AdminSessionGuard) -> bool {
    if *guard.revoked_rx.borrow() || guard.expires_at <= now_ts() {
        revoke_session(state, &guard.token).await;
        return false;
    }
    let present = state
        .sessions
        .read()
        .await
        .get(&guard.token)
        .is_some_and(|session| {
            session.user_id == guard.user_id
                && session.device_id == guard.device_id
                && session.expires_at == guard.expires_at
                && !*session.revoked_tx.borrow()
        });
    if !present {
        return false;
    }

    let valid = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT TRUE
        FROM admin_users u
        JOIN authenticator_devices d ON d.user_id = u.id
        WHERE u.id = $1 AND u.enabled = 1 AND d.id = $2
        "#,
    )
    .bind(&guard.user_id)
    .bind(&guard.device_id)
    .fetch_optional(&state.db)
    .await;
    match valid {
        Ok(Some(true)) => true,
        Ok(_) => {
            revoke_session(state, &guard.token).await;
            false
        }
        Err(error) => {
            warn!(?error, "failed to revalidate privileged WebSocket session");
            revoke_session(state, &guard.token).await;
            false
        }
    }
}

pub fn session_token(headers: &HeaderMap) -> Option<String> {
    session_tokens(headers).into_iter().next()
}

pub fn session_tokens(headers: &HeaderMap) -> Vec<String> {
    let mut tokens = Vec::with_capacity(2);
    let Some(cookie) = headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
    else {
        return tokens;
    };
    let mut secure_token = None;
    let mut http_token = None;
    for part in cookie.split(';') {
        let Some((name, value)) = part.trim().split_once('=') else {
            continue;
        };
        if name == SECURE_SESSION_COOKIE && !value.is_empty() {
            secure_token = Some(value.to_string());
        }
        if name == SESSION_COOKIE && !value.is_empty() {
            http_token = Some(value.to_string());
        }
    }
    if let Some(token) = secure_token {
        tokens.push(token);
    }
    if let Some(token) = http_token {
        if !tokens.iter().any(|known| known == &token) {
            tokens.push(token);
        }
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secure_session_cookie_takes_precedence_over_http_cookie() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            "om_session=http-token; __Secure-om_session=https-token"
                .parse()
                .unwrap(),
        );
        assert_eq!(session_token(&headers).as_deref(), Some("https-token"));
        assert_eq!(
            session_tokens(&headers),
            ["https-token".to_string(), "http-token".to_string()]
        );

        headers.insert(header::COOKIE, "om_session=http-token".parse().unwrap());
        assert_eq!(session_token(&headers).as_deref(), Some("http-token"));
    }

    #[test]
    fn revoking_a_session_notifies_existing_guards() {
        let (revoked_tx, revoked_rx) = watch::channel(false);
        let mut sessions = std::collections::HashMap::from([(
            "session-1".to_string(),
            AdminSession {
                user_id: "user-1".to_string(),
                username: "admin".to_string(),
                device_id: "device-1".to_string(),
                login_totp_counter: None,
                expires_at: i64::MAX,
                revoked_tx,
            },
        )]);

        revoke_matching_session_entries(&mut sessions, |session| session.user_id == "user-1");

        assert!(sessions.is_empty());
        assert!(*revoked_rx.borrow());
    }

    #[test]
    fn verifies_standard_totp_vector_and_adjacent_window() {
        let secret = b"12345678901234567890";
        assert!(verify_totp(secret, &"94287082"[2..], 59));
        let code = totp_code(secret, 2);
        assert!(verify_totp(secret, &code, 59));
        assert_eq!(verify_totp_counter(secret, &code, 59), Some(2));
        assert!(!verify_totp(secret, "not-a-code", 59));
        assert_eq!(verify_totp_counter(secret, "not-a-code", 59), None);
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
