//! M44: the panel's sign-in. The panel signs a short-lived token for one
//! bot and one user with the bot's secret (`FERRULE_PANEL_SECRET`); the
//! dashboard checks the signature, the bot, the expiry and that its nonce
//! wasn't used before, then opens a session like a login link does.
//!
//! `ferrule-panel.v1.` + b64(claims JSON) + `.` + b64(HMAC-SHA256 over
//! everything before the last dot).

use ferrule_connections::seal::{sha256_b64, unb64, write_private};
use ring::hmac;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

pub const PREFIX: &str = "ferrule-panel.v1.";
/// What the browser is told, whatever went wrong; the reason is only logged.
pub const REFUSED: &str = "that sign-in didn't work; open the bot again from the panel";
/// 5 minutes, plus 30 s of clock skew.
pub const MAX_AHEAD_SECS: u64 = 330;
pub const NONCE_CAP: usize = 10_000;

/// What checks a panel token: the shared secret and this bot's id.
#[derive(Clone)]
pub struct Key {
    secret: Vec<u8>,
    bot: String,
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Key").field("bot", &self.bot).finish()
    }
}

impl Key {
    pub fn new(secret: &[u8], bot: &str) -> Self {
        Self {
            secret: secret.to_vec(),
            bot: bot.to_string(),
        }
    }

    /// The key managed mode has: needs the mode on, a panel secret and a
    /// bot id.
    pub fn from_managed() -> Option<Self> {
        if !crate::managed::on() {
            return None;
        }
        let secret = crate::managed::panel_secret()?;
        let bot = crate::managed::state().bot_id.as_deref()?;
        Some(Self::new(secret, bot))
    }
}

#[derive(Debug, Deserialize)]
pub struct Claims {
    pub bot: String,
    pub user: String,
    pub exp: u64,
    pub nonce: String,
}

/// The claims of a good token. The `Err` is for the log only, never the
/// reply.
pub fn verify(token: &str, key: &Key, now: u64) -> Result<Claims, String> {
    let (claims_b64, mac_b64) = token
        .strip_prefix(PREFIX)
        .and_then(|t| t.split_once('.'))
        .ok_or("not a panel token")?;
    let mac = unb64(mac_b64).map_err(|_| "bad signature")?;
    hmac::verify(
        &hmac::Key::new(hmac::HMAC_SHA256, &key.secret),
        format!("{PREFIX}{claims_b64}").as_bytes(),
        &mac,
    )
    .map_err(|_| "bad signature")?;
    let claims: Claims = unb64(claims_b64)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or("malformed claims")?;
    if claims.bot != key.bot {
        return Err(format!(
            "a token for bot {}, and this is {}",
            claims.bot, key.bot
        ));
    }
    if claims.exp <= now {
        return Err(format!("expired {}s ago", now - claims.exp));
    }
    if claims.exp > now + MAX_AHEAD_SECS {
        return Err(format!(
            "expires {}s ahead, more than {MAX_AHEAD_SECS}",
            claims.exp - now
        ));
    }
    if !(16..=128).contains(&claims.nonce.chars().count()) {
        return Err("a nonce must be 16 to 128 characters".into());
    }
    if !(1..=128).contains(&claims.user.chars().count())
        || claims.user.chars().any(char::is_control)
    {
        return Err("a bad user id".into());
    }
    Ok(claims)
}

/// A signed token, for the tests (the panel is another program).
#[cfg(test)]
pub fn sign(claims: &serde_json::Value, secret: &[u8]) -> String {
    use ferrule_connections::seal::b64;
    let body = format!("{PREFIX}{}", b64(claims.to_string().as_bytes()));
    let mac = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, secret), body.as_bytes());
    format!("{body}.{}", b64(mac.as_ref()))
}

/// The nonces spent so far, on disk (sha256 of the nonce → its expiry) so
/// a restart doesn't make a token good again.
pub struct Nonces {
    path: PathBuf,
    cap: usize,
}

impl Nonces {
    pub fn at(path: PathBuf) -> Self {
        Self {
            path,
            cap: NONCE_CAP,
        }
    }

    /// Records `nonce` as used. Fails closed: a file that can't be locked,
    /// read or written, or one that is full, refuses the sign-in.
    pub fn spend(&self, nonce: &str, exp: u64, now: u64) -> Result<(), String> {
        let _lock = crate::filewrite::Lock::take(&self.path)
            .map_err(|e| format!("the nonce file can't be locked: {e:#}"))?;
        let mut map: BTreeMap<String, u64> = match std::fs::read(&self.path) {
            Ok(b) => serde_json::from_slice(&b)
                .map_err(|e| format!("the nonce file can't be read: {e}"))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(format!("the nonce file can't be read: {e}")),
        };
        map.retain(|_, e| *e > now);
        let key = sha256_b64(nonce.as_bytes());
        if map.contains_key(&key) {
            return Err("the nonce was used already".into());
        }
        if map.len() >= self.cap {
            return Err(format!(
                "{} holds {} unexpired nonces; refusing panel sign-ins until some expire",
                self.path.display(),
                self.cap
            ));
        }
        map.insert(key, exp);
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("the nonce file can't be written: {e}"))?;
        }
        let bytes = serde_json::to_vec(&map).map_err(|e| e.to_string())?;
        write_private(&self.path, &bytes)
            .map_err(|e| format!("the nonce file can't be written: {e:#}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrule_connections::seal::b64;
    use serde_json::json;

    const SECRET: &[u8] = b"test-panel-secret-0123456789abcdef0123";

    fn claims(exp: u64) -> serde_json::Value {
        json!({ "bot": "b_test", "user": "u_1", "exp": exp, "nonce": "0123456789abcdef" })
    }

    #[test]
    fn a_good_token_verifies_and_each_defect_is_named() {
        let key = Key::new(SECRET, "b_test");
        let now = 1_000;
        let ok = verify(&sign(&claims(now + 60), SECRET), &key, now).unwrap();
        assert_eq!((ok.user.as_str(), ok.exp), ("u_1", now + 60));
        let why = |t: String| verify(&t, &key, now).unwrap_err();
        assert!(why("nope".into()).contains("not a panel token"));
        assert!(why(sign(&claims(now + 60), b"another secret")).contains("bad signature"));
        assert!(why(sign(&claims(now - 5), SECRET)).contains("expired 5s ago"));
        assert!(why(sign(&claims(now + 600), SECRET)).contains("600s ahead"));
        let mut other = claims(now + 60);
        other["bot"] = json!("b_other");
        assert!(why(sign(&other, SECRET)).contains("b_other"));
        let mut short = claims(now + 60);
        short["nonce"] = json!("short");
        assert!(why(sign(&short, SECRET)).contains("16 to 128"));
        let mut user = claims(now + 60);
        user["user"] = json!("a\nb");
        assert!(why(sign(&user, SECRET)).contains("bad user"));
        // Signed, but not claims.
        let body = format!("{PREFIX}{}", b64(b"[]"));
        let mac = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, SECRET), body.as_bytes());
        assert!(why(format!("{body}.{}", b64(mac.as_ref()))).contains("malformed"));
    }

    #[test]
    fn a_spent_nonce_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonces.json");
        Nonces::at(path.clone()).spend("n1", 100, 10).unwrap();
        let again = Nonces::at(path.clone());
        assert_eq!(
            again.spend("n1", 100, 10),
            Err("the nonce was used already".into())
        );
        // Once it has expired, the entry is pruned.
        again.spend("n2", 300, 101).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains(&sha256_b64(b"n1")), "{text}");
    }

    #[test]
    fn the_nonce_file_is_capped_and_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonces.json");
        let n = Nonces {
            path: path.clone(),
            cap: 2,
        };
        n.spend("a", 100, 10).unwrap();
        n.spend("b", 100, 10).unwrap();
        assert!(n
            .spend("c", 100, 10)
            .unwrap_err()
            .contains("unexpired nonces"));
        std::fs::write(&path, "not json").unwrap();
        assert!(n
            .spend("d", 100, 10)
            .unwrap_err()
            .starts_with("the nonce file can't be read"));
    }
}
