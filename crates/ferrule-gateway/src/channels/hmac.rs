//! M39: HMAC-SHA256 on `ring` (no new crate), for WhatsApp's
//! `X-Hub-Signature-256` and the HTTP API's webhook signature.

use ring::{digest, hmac};

/// `sha256=<hex>` of HMAC-SHA256(`key`, `body`).
pub fn sign(key: &[u8], body: &[u8]) -> String {
    let k = hmac::Key::new(hmac::HMAC_SHA256, key);
    format!("sha256={}", hex(hmac::sign(&k, body).as_ref()))
}

/// Whether `header` (`sha256=<hex>`) is HMAC-SHA256(`key`, `body`), in
/// constant time.
pub fn verify(key: &[u8], body: &[u8], header: &str) -> bool {
    let Some(sig) = header.trim().strip_prefix("sha256=") else {
        return false;
    };
    let Some(sig) = unhex(sig) else {
        return false;
    };
    let k = hmac::Key::new(hmac::HMAC_SHA256, key);
    hmac::verify(&k, body, &sig).is_ok()
}

/// SHA-256 of `data`.
pub fn sha256(data: &[u8]) -> Vec<u8> {
    digest::digest(&digest::SHA256, data).as_ref().to_vec()
}

/// Lower-case hex.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Whether two strings are equal, in time that doesn't depend on where
/// they differ (a verify token, a key).
pub fn same(a: &str, b: &str) -> bool {
    let (a, b) = (sha256(a.as_bytes()), sha256(b.as_bytes()));
    a.iter().zip(&b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Unpadded base64url, the relay mailbox's `<box>`.
pub fn b64url(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_signature_matches_rfc_4231_and_verifies_only_itself() {
        // RFC 4231 test case 2.
        let s = sign(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            s,
            "sha256=5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert!(verify(b"Jefe", b"what do ya want for nothing?", &s));
        assert!(!verify(b"Jefe", b"what do ya want for nothing!", &s));
        assert!(!verify(b"jefe", b"what do ya want for nothing?", &s));
        assert!(!verify(b"Jefe", b"x", "sha256=zz"));
        assert!(!verify(b"Jefe", b"x", ""));
        assert!(same("abc", "abc") && !same("abc", "abd"));
        assert_eq!(b64url(&[0xfb, 0xff]), "-_8");
    }
}
