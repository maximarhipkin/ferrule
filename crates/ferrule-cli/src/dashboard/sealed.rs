//! M52.1: a key typed into the page crosses the tunnel sealed. Cloudflare
//! terminates a quick tunnel's TLS, so a key the page POSTs as typed is
//! readable there. Instead the page asks for a one-time public key
//! (`POST /api/seal`), seals each secret field to it with M20's key-form
//! scheme (ECDH P-256, HKDF-SHA256, AES-256-GCM, the key id as salt and
//! associated data; `ferrule_connections::keyform`), and sends
//! `{"sealed": {kid, v, epk, iv, ct}}` in the field's place. Before any
//! handler runs, [`Seals::open_all`] swaps every envelope for its text, in
//! memory, so the handlers read the same strings they always did.
//!
//! Each key opens one envelope, only for the session it was made for,
//! within [`TTL`]. An envelope that doesn't open refuses the whole request:
//! nothing runs and nothing is saved. A plain value still works (scripts,
//! the chat's admin tool, a page served over plain HTTP, where browsers
//! have no WebCrypto). docs/m22-dashboard.md, "Threat model", has the
//! limit: whoever can rewrite the page's script can also take the key.

use super::auth;
use anyhow::Result;
use ferrule_connections::keyform::KeyForm;
use ferrule_connections::seal;
use serde_json::{json, Map, Value};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a key waits for its envelope. The page asks for one right
/// before it sends.
pub const TTL: Duration = Duration::from_secs(600);

/// Keys waiting at once, across sessions; the oldest goes first.
const MAX: usize = 64;

struct Pending {
    kid: String,
    /// The session's CSRF token: only the session that asked can use it.
    session: String,
    made: Instant,
    form: KeyForm,
}

#[derive(Default)]
pub struct Seals {
    pending: Mutex<Vec<Pending>>,
}

impl Seals {
    /// A one-time key for `session`: `{kid, public}`.
    pub fn issue(&self, session: &str) -> Result<Value> {
        let form = KeyForm::new()?;
        let kid = seal::b64(&seal::random::<16>());
        let out = json!({ "kid": kid, "public": form.public });
        let mut pending = self.pending.lock().unwrap();
        pending.retain(|p| p.made.elapsed() < TTL);
        while pending.len() >= MAX {
            pending.remove(0);
        }
        pending.push(Pending {
            kid,
            session: session.to_string(),
            made: Instant::now(),
            form,
        });
        Ok(out)
    }

    /// `body` with every `{"sealed": …}` in it opened, or why not.
    pub fn open_all(&self, session: &str, body: Value) -> Result<Value, String> {
        match body {
            Value::Object(map) if map.len() == 1 && map.contains_key("sealed") => {
                self.open(session, &map["sealed"]).map(Value::String)
            }
            Value::Object(map) => {
                let mut out = Map::with_capacity(map.len());
                for (k, v) in map {
                    out.insert(k, self.open_all(session, v)?);
                }
                Ok(Value::Object(out))
            }
            Value::Array(items) => items
                .into_iter()
                .map(|v| self.open_all(session, v))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array),
            other => Ok(other),
        }
    }

    fn open(&self, session: &str, envelope: &Value) -> Result<String, String> {
        const AGAIN: &str = "a key from this page couldn't be opened, so nothing was changed";
        let kid = envelope["kid"].as_str().unwrap_or_default();
        let found = {
            let mut pending = self.pending.lock().unwrap();
            pending
                .iter()
                .position(|p| p.kid == kid && auth::same(&p.session, session))
                .map(|at| pending.remove(at))
        };
        let Some(p) = found.filter(|p| p.made.elapsed() < TTL) else {
            tracing::warn!("dashboard: a sealed field came with no live key for this session");
            return Err(format!(
                "{AGAIN}: its one-time key expired or was used. Try again."
            ));
        };
        p.form.open_text(kid, envelope).map_err(|e| {
            // keyform's reasons name what failed, never the text.
            tracing::warn!("dashboard: a sealed field didn't open: {e}");
            format!("{AGAIN}: {e}. Reload the page and try again.")
        })
    }
}
