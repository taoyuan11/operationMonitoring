use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::VerifyingKey;

const PUBLIC_KEY_ENV: &str = "OM_UPDATE_PUBLIC_KEY";
const KEY_ID_ENV: &str = "OM_UPDATE_PUBLIC_KEY_ID";

fn main() {
    println!("cargo:rerun-if-env-changed={PUBLIC_KEY_ENV}");
    println!("cargo:rerun-if-env-changed={KEY_ID_ENV}");

    let public_key = std::env::var(PUBLIC_KEY_ENV).ok();
    let key_id = std::env::var(KEY_ID_ENV).ok();
    match (public_key, key_id) {
        (None, None) => {}
        (Some(public_key), Some(key_id)) => validate_update_key(&public_key, &key_id),
        _ => panic!("{PUBLIC_KEY_ENV} and {KEY_ID_ENV} must be set together"),
    }
}

fn validate_update_key(encoded_key: &str, key_id: &str) {
    let key_id = key_id.trim();
    assert!(
        !key_id.is_empty()
            && key_id.len() <= 64
            && key_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')),
        "{KEY_ID_ENV} is invalid"
    );

    let key_bytes = STANDARD
        .decode(encoded_key.trim())
        .unwrap_or_else(|_| panic!("{PUBLIC_KEY_ENV} must be valid Base64"));
    let key_bytes: [u8; 32] = key_bytes
        .try_into()
        .unwrap_or_else(|_| panic!("{PUBLIC_KEY_ENV} must decode to 32 bytes"));
    VerifyingKey::from_bytes(&key_bytes)
        .unwrap_or_else(|_| panic!("{PUBLIC_KEY_ENV} must be a valid Ed25519 public key"));
}
