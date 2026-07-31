use std::{fs, path::Path};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signer, SigningKey};

use crate::models::AgentUpdateOffer;

const UPDATE_SIGNATURE_DOMAIN: &str = "operation-monitoring-agent-update-v1";

#[derive(Clone)]
pub struct UpdateSigner {
    key_id: String,
    signing_key: SigningKey,
}

impl UpdateSigner {
    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    pub fn public_key_base64(&self) -> String {
        STANDARD.encode(self.signing_key.verifying_key().to_bytes())
    }

    pub fn sign_offer(&self, offer: &mut AgentUpdateOffer) -> Result<()> {
        let payload = update_signature_payload(offer)?;
        let signature = self.signing_key.sign(payload.as_bytes());
        offer.signature_key_id = Some(self.key_id.clone());
        offer.signature = Some(STANDARD.encode(signature.to_bytes()));
        Ok(())
    }
}

pub fn load_update_signer(path: Option<&Path>, key_id: &str) -> Result<Option<UpdateSigner>> {
    let Some(path) = path else {
        return Ok(None);
    };
    if !valid_signature_key_id(key_id) {
        bail!("OM_UPDATE_SIGNING_KEY_ID is invalid");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(path)
            .with_context(|| format!("failed to inspect update signing key {}", path.display()))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            bail!(
                "update signing key file {} must not be accessible by group or other users",
                path.display()
            );
        }
    }
    let encoded = fs::read_to_string(path)
        .with_context(|| format!("failed to read update signing key {}", path.display()))?;
    let bytes = STANDARD
        .decode(encoded.trim())
        .context("OM_UPDATE_SIGNING_KEY_FILE must contain a Base64-encoded 32-byte Ed25519 key")?;
    let bytes: [u8; 32] = bytes.try_into().map_err(|_| {
        anyhow::anyhow!(
            "OM_UPDATE_SIGNING_KEY_FILE must contain a Base64-encoded 32-byte Ed25519 key"
        )
    })?;
    Ok(Some(UpdateSigner {
        key_id: key_id.to_string(),
        signing_key: SigningKey::from_bytes(&bytes),
    }))
}

fn valid_signature_key_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn update_signature_payload(offer: &AgentUpdateOffer) -> Result<String> {
    let target_os = offer
        .target_os
        .as_deref()
        .context("signed update offer is missing target_os")?;
    for (name, value) in [
        ("version", offer.version.as_str()),
        ("target_os", target_os),
        ("package_type", offer.package_type.as_str()),
        ("native_arch", offer.native_arch.as_str()),
        ("sha256", offer.sha256.as_str()),
    ] {
        if value.is_empty()
            || value
                .bytes()
                .any(|byte| matches!(byte, b'\0' | b'\r' | b'\n'))
        {
            bail!("update signature field {name} is invalid");
        }
    }
    Ok(format!(
        "{UPDATE_SIGNATURE_DOMAIN}\nversion={}\ntarget_os={target_os}\npackage_type={}\nnative_arch={}\nsize_bytes={}\nsha256={}\n",
        offer.version,
        offer.package_type,
        offer.native_arch,
        offer.size_bytes,
        offer.sha256.to_ascii_lowercase(),
    ))
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    use super::*;

    fn offer() -> AgentUpdateOffer {
        AgentUpdateOffer {
            release_id: "release-1".to_string(),
            version: "1.2.3".to_string(),
            artifact_id: "artifact-1".to_string(),
            download_url: "/api/agent/update/artifacts/artifact-1/download".to_string(),
            sha256: "A".repeat(64),
            size_bytes: 42,
            package_type: "standalone".to_string(),
            native_arch: "x86_64".to_string(),
            target_os: Some("linux".to_string()),
            signature_key_id: None,
            signature: None,
            retry_count: 0,
        }
    }

    #[test]
    fn signs_the_agent_canonical_payload() {
        let signer = UpdateSigner {
            key_id: "release-v1".to_string(),
            signing_key: SigningKey::from_bytes(&[7_u8; 32]),
        };
        let mut offer = offer();
        assert_eq!(
            update_signature_payload(&offer).unwrap(),
            format!(
                "operation-monitoring-agent-update-v1\nversion=1.2.3\ntarget_os=linux\npackage_type=standalone\nnative_arch=x86_64\nsize_bytes=42\nsha256={}\n",
                "a".repeat(64)
            )
        );
        signer.sign_offer(&mut offer).unwrap();
        assert_eq!(offer.signature_key_id.as_deref(), Some("release-v1"));
        let signature = Signature::from_slice(
            &STANDARD
                .decode(offer.signature.as_deref().unwrap())
                .unwrap(),
        )
        .unwrap();
        let verifying_key = VerifyingKey::from_bytes(
            &STANDARD
                .decode(signer.public_key_base64())
                .unwrap()
                .try_into()
                .unwrap(),
        )
        .unwrap();
        verifying_key
            .verify(
                update_signature_payload(&offer).unwrap().as_bytes(),
                &signature,
            )
            .unwrap();

        offer.size_bytes += 1;
        assert!(
            verifying_key
                .verify(
                    update_signature_payload(&offer).unwrap().as_bytes(),
                    &signature,
                )
                .is_err()
        );
    }
}
