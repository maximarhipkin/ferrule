//! The API-key form's decryption, the twin of `encryptKey` in
//! `relay/worker.js`: ECDH P-256 with a one-time key held in memory, HKDF-
//! SHA256 (salt = the slot id, info = "ferrule key form v1"), AES-256-GCM
//! with the slot id as associated data.

use crate::seal::{b64, unb64};
use anyhow::{anyhow, bail, Context, Result};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use ring::agreement::{self, EphemeralPrivateKey, UnparsedPublicKey, ECDH_P256};
use ring::hkdf;
use ring::rand::SystemRandom;
use serde_json::Value;

const INFO: &[u8] = b"ferrule key form v1";

/// One form's private key; used once.
pub struct KeyForm {
    private: EphemeralPrivateKey,
    /// Raw uncompressed P-256 point, base64url: goes in the link.
    pub public: String,
}

impl KeyForm {
    pub fn new() -> Result<Self> {
        let rng = SystemRandom::new();
        let private =
            EphemeralPrivateKey::generate(&ECDH_P256, &rng).map_err(|_| anyhow!("making a key"))?;
        let public = b64(private
            .compute_public_key()
            .map_err(|_| anyhow!("making a key"))?
            .as_ref());
        Ok(Self { private, public })
    }

    /// The key typed into the form, from its envelope `{v, epk, iv, ct}`.
    pub fn open(self, slot_id: &str, envelope: &Value) -> Result<String> {
        if envelope["v"] != 1 {
            bail!("an envelope of another version");
        }
        let field = |k: &str| {
            envelope[k]
                .as_str()
                .ok_or_else(|| anyhow!("no {k}"))
                .and_then(unb64)
        };
        let (epk, iv, mut ct) = (field("epk")?, field("iv")?, field("ct")?);
        let peer = UnparsedPublicKey::new(&ECDH_P256, epk);
        let key = agreement::agree_ephemeral(self.private, &peer, |shared| {
            let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, slot_id.as_bytes()).extract(shared);
            let okm = prk.expand(&[INFO], &AES_256_GCM).map_err(|_| ())?;
            Ok::<_, ()>(UnboundKey::from(okm))
        })
        .map_err(|_| anyhow!("the form's key doesn't fit"))?
        .map_err(|_| anyhow!("deriving the key"))?;
        let nonce = Nonce::try_assume_unique_for_key(&iv).map_err(|_| anyhow!("bad iv"))?;
        let plain = LessSafeKey::new(key)
            .open_in_place(nonce, Aad::from(slot_id.as_bytes()), &mut ct)
            .map_err(|_| anyhow!("the envelope doesn't open (edited, or for another slot)"))?;
        let text = String::from_utf8(plain.to_vec()).context("the key isn't text")?;
        let text = text.trim().to_string();
        if text.is_empty() || text.len() > 4096 || text.chars().any(char::is_control) {
            bail!("that doesn't look like an API key");
        }
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Encrypts with the Worker's own `encryptKey` in node, when node is
    /// there (CI has it), so the two sides can't drift apart.
    fn node_encrypt(public: &str, slot: &str, secret: &str) -> Option<Value> {
        let worker = concat!(env!("CARGO_MANIFEST_DIR"), "/../../relay/worker.js");
        let url = format!("file://{}", worker.replace('\\', "/"));
        let url = if cfg!(windows) {
            format!("file:///{}", worker.replace('\\', "/"))
        } else {
            url
        };
        let script = format!(
            "import({url:?}).then(async m => console.log(JSON.stringify(await m.encryptKey({public:?}, {slot:?}, {secret:?}))))"
        );
        let out = Command::new("node")
            .args(["--input-type=module", "-e", &script])
            .output()
            .ok()?;
        if !out.status.success() {
            panic!("node: {}", String::from_utf8_lossy(&out.stderr));
        }
        serde_json::from_slice(&out.stdout).ok()
    }

    #[test]
    fn a_key_the_browser_encrypted_opens_here_once_and_only_for_its_slot() {
        let slot = crate::relay::Slot::new();
        let form = KeyForm::new().unwrap();
        let Some(env) = node_encrypt(&form.public, &slot.id, "  lin_api_SECRET  ") else {
            eprintln!("node isn't installed; skipping the cross-check");
            return;
        };
        assert!(!env.to_string().contains("lin_api_SECRET"));
        assert_eq!(form.open(&slot.id, &env).unwrap(), "lin_api_SECRET");

        let form = KeyForm::new().unwrap();
        let env = node_encrypt(&form.public, &slot.id, "k").unwrap();
        let other = crate::relay::Slot::new();
        assert!(form.open(&other.id, &env).is_err(), "bound to the slot");

        let form = KeyForm::new().unwrap();
        let mut env = node_encrypt(&form.public, &slot.id, "k").unwrap();
        env["ct"] = b64(b"xxxxxxxxxxxxxxxxxxxxxxxxx").into();
        assert!(form.open(&slot.id, &env).is_err(), "edited");
    }
}
