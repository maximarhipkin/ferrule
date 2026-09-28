//! A service account's signed assertion (RFC 7523): RS256 over
//! `{iss, scope, aud, iat, exp}`, exchanged at Google's token endpoint.

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use ring::signature::{RsaKeyPair, RSA_PKCS1_SHA256};
use serde_json::json;

/// The DER inside a `PRIVATE KEY` PEM.
pub fn pkcs8_der(pem: &str) -> Result<Vec<u8>, String> {
    let begin = concat!("-----BEGIN ", "PRIVATE KEY-----");
    let end = concat!("-----END ", "PRIVATE KEY-----");
    let pem = pem.replace("\\n", "\n");
    let start = pem
        .find(begin)
        .ok_or("the private key isn't in the file (is this the JSON key Google downloaded?)")?;
    let rest = &pem[start + begin.len()..];
    let stop = rest.find(end).ok_or("the private key is cut short")?;
    let body: String = rest[..stop]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    STANDARD
        .decode(body)
        .map_err(|_| "the private key is damaged".to_string())
}

pub struct Signer {
    key: RsaKeyPair,
}

impl Signer {
    pub fn new(der: &[u8]) -> Result<Self, String> {
        RsaKeyPair::from_pkcs8(der)
            .map(|key| Self { key })
            .map_err(|_| "the private key isn't an RSA key ferrule can use".to_string())
    }

    /// `header.claims.signature`, valid for an hour from `now`.
    pub fn assertion(&self, iss: &str, scope: &str, aud: &str, now: u64) -> Result<String, String> {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let claims = URL_SAFE_NO_PAD.encode(
            json!({ "iss": iss, "scope": scope, "aud": aud, "iat": now, "exp": now + 3600 })
                .to_string(),
        );
        let input = format!("{header}.{claims}");
        let mut sig = vec![0u8; self.key.public().modulus_len()];
        self.key
            .sign(
                &RSA_PKCS1_SHA256,
                &ring::rand::SystemRandom::new(),
                input.as_bytes(),
                &mut sig,
            )
            .map_err(|_| "signing failed".to_string())?;
        Ok(format!("{input}.{}", URL_SAFE_NO_PAD.encode(sig)))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A throwaway 2048-bit key made for these tests; it signs nothing
    /// real. Its PEM armour is put together at run time.
    pub const TEST_KEY_BODY: &str = include_str!("testdata/rsa-2048.b64");

    pub fn test_pem() -> String {
        format!(
            "-----BEGIN {k}-----\n{TEST_KEY_BODY}-----END {k}-----\n",
            k = "PRIVATE KEY"
        )
    }

    #[test]
    fn an_assertion_verifies_with_the_public_key_and_says_what_it_should() {
        let der = pkcs8_der(&test_pem()).unwrap();
        let signer = Signer::new(&der).unwrap();
        let jwt = signer
            .assertion("sa@p.iam.gserviceaccount.com", "s1 s2", "https://aud", 1000)
            .unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let claims: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["exp"], 4600);
        assert_eq!(claims["scope"], "s1 s2");
        let public = ring::signature::UnparsedPublicKey::new(
            &ring::signature::RSA_PKCS1_2048_8192_SHA256,
            signer.key.public().as_ref().to_vec(),
        );
        public
            .verify(
                format!("{}.{}", parts[0], parts[1]).as_bytes(),
                &URL_SAFE_NO_PAD.decode(parts[2]).unwrap(),
            )
            .unwrap();
        // JSON-escaped newlines, as in the downloaded file.
        assert!(pkcs8_der(&test_pem().replace('\n', "\\n")).is_ok());
        assert!(pkcs8_der("not a key").is_err());
    }
}
