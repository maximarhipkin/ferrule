//! The Claude plan's credential, as far as ferrule ever holds one.
//!
//! - A setup-token the user pasted (`claude setup-token`, Anthropic's own
//!   flow): sealed in `private/plans/claude-code.json` under AAD
//!   `ferrule-plan:claude-code`, with the day it was pasted in the clear so
//!   doctor can warn before the year is up.
//! - An exported `CLAUDE_CODE_OAUTH_TOKEN`: moved out of ferrule's own
//!   environment at startup ([`take_exported`]), so hooks, MCP servers and
//!   commands, which inherit it, never see it.
//! - Claude Code's own login: nothing here. Ferrule never reads claude's
//!   credentials file or keychain entry.
//!
//! Whichever it is, the token only ever goes into the `claude` child's
//! environment (`claude::env`). Nothing in ferrule sends it anywhere.

use anyhow::{bail, Context, Result};
use ferrule_connections::seal::{write_private, Sealer};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const AAD: &str = "ferrule-plan:claude-code";
/// The variable claude reads a setup-token from.
pub const TOKEN_VAR: &str = "CLAUDE_CODE_OAUTH_TOKEN";
/// A setup-token lasts a year.
pub const TOKEN_LIFETIME: u64 = 365 * 24 * 3600;
/// Doctor warns this long before it runs out.
pub const WARN_BEFORE: u64 = 30 * 24 * 3600;

/// A token held in memory. Never printed.
#[derive(Clone, PartialEq)]
pub struct Token(String);

impl Token {
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }
    /// For the child's environment, and nothing else.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(…)")
    }
}

/// Which credential a turn runs on: the first that exists of an exported
/// token, a pasted one, and claude's own login.
#[derive(Debug, Clone, PartialEq)]
pub enum Credential {
    /// `CLAUDE_CODE_OAUTH_TOKEN` was set when ferrule started.
    Exported(Token),
    /// A pasted setup-token, and when it was pasted.
    Pasted { token: Token, at: u64 },
    /// Neither: claude uses its own login in the engine's config dir, if
    /// it has one.
    ClaudeLogin,
}

impl Credential {
    pub fn token(&self) -> Option<&Token> {
        match self {
            Credential::Exported(t) | Credential::Pasted { token: t, .. } => Some(t),
            Credential::ClaudeLogin => None,
        }
    }

    /// What doctor and status call it.
    pub fn describe(&self) -> &'static str {
        match self {
            Credential::Exported(_) => "CLAUDE_CODE_OAUTH_TOKEN from the environment",
            Credential::Pasted { .. } => "a setup-token pasted into `ferrule login claude --token`",
            Credential::ClaudeLogin => "Claude Code's own login",
        }
    }
}

static EXPORTED: OnceLock<Mutex<Option<Token>>> = OnceLock::new();

fn exported() -> &'static Mutex<Option<Token>> {
    EXPORTED.get_or_init(Mutex::default)
}

/// Move `CLAUDE_CODE_OAUTH_TOKEN` from the process environment into
/// memory. Call once, early, before any thread spawns a child; later calls
/// keep what the first one found.
pub fn take_exported() {
    let mut held = exported().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(v) = std::env::var_os(TOKEN_VAR) {
        if held.is_none() {
            let v = v.to_string_lossy().trim().to_string();
            if !v.is_empty() {
                *held = Some(Token(v));
            }
        }
        // SAFETY (edition 2021: safe fn): called at startup, before the
        // runtime starts threads that read the environment.
        std::env::remove_var(TOKEN_VAR);
    }
}

/// The exported token [`take_exported`] kept, or the variable itself when
/// nothing took it (tests, a library caller).
pub fn exported_token() -> Option<Token> {
    let held = exported().lock().unwrap_or_else(|e| e.into_inner()).clone();
    held.or_else(|| {
        std::env::var(TOKEN_VAR)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .map(Token)
    })
}

/// What a setup-token looks like; anything else is refused at paste time.
pub fn check_setup_token(token: &str) -> Result<()> {
    let t = token.trim();
    if t.starts_with("sk-ant-api") {
        bail!("that is an Anthropic API key, not a setup-token; add it as an API-key provider instead");
    }
    if !t.starts_with("sk-ant-oat") {
        bail!("that doesn't look like a setup-token (they start with `sk-ant-oat`); run `claude setup-token` and paste what it prints");
    }
    if t.chars().any(char::is_whitespace) {
        bail!("the setup-token has spaces or line breaks in it; paste it as one line");
    }
    Ok(())
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    /// When it was pasted (unix seconds).
    pub pasted_at: u64,
}

#[derive(Serialize, Deserialize)]
struct File {
    v: u32,
    meta: Meta,
    sealed: String,
}

/// The pasted setup-token at rest.
pub struct TokenStore {
    dir: PathBuf,
    key_path: PathBuf,
}

impl TokenStore {
    /// The store under ferrule's `private/` directory.
    pub fn new(private: &Path) -> Self {
        Self {
            dir: private.join("plans"),
            key_path: private.join("connections.key"),
        }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join("claude-code.json")
    }

    fn file(&self) -> Result<Option<File>> {
        match std::fs::read(self.path()) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).with_context(|| {
                format!(
                    "{} doesn't parse; run `ferrule logout claude` and paste the token again",
                    self.path().display()
                )
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", self.path().display())),
        }
    }

    /// When the token was pasted, without opening it.
    pub fn meta(&self) -> Result<Option<Meta>> {
        Ok(self.file()?.map(|f| f.meta))
    }

    pub fn load(&self) -> Result<Option<(Token, Meta)>> {
        let Some(file) = self.file()? else {
            return Ok(None);
        };
        let Some(sealer) = Sealer::load(&self.key_path)? else {
            bail!(
                "the key that seals the Claude setup-token is missing ({}); run `ferrule login claude --token` again",
                self.key_path.display()
            );
        };
        let plain = sealer.open(AAD, &file.sealed).context(
            "the Claude setup-token can't be opened; run `ferrule login claude --token` again",
        )?;
        let token = String::from_utf8(plain).context("the sealed setup-token isn't text")?;
        Ok(Some((Token(token), file.meta)))
    }

    pub fn save(&self, token: &str, now: u64) -> Result<()> {
        check_setup_token(token)?;
        let sealer = Sealer::load_or_create(&self.key_path)?;
        let file = File {
            v: 1,
            meta: Meta { pasted_at: now },
            sealed: sealer.seal(AAD, token.trim().as_bytes()),
        };
        write_private(&self.path(), &serde_json::to_vec_pretty(&file)?)
    }

    /// `true` when there was something to delete.
    pub fn delete(&self) -> Result<bool> {
        match std::fs::remove_file(self.path()) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e).with_context(|| format!("removing {}", self.path().display())),
        }
    }

    /// The active credential: exported, then pasted, then claude's own.
    pub fn credential(&self) -> Result<Credential> {
        if let Some(t) = exported_token() {
            return Ok(Credential::Exported(t));
        }
        Ok(match self.load()? {
            Some((token, meta)) => Credential::Pasted {
                token,
                at: meta.pasted_at,
            },
            None => Credential::ClaudeLogin,
        })
    }
}

/// Where a pasted token stands on `now`: fine, due soon (days left), or
/// past its year.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Age {
    Fine { days_left: u64 },
    Soon { days_left: u64 },
    Expired,
}

pub fn age(pasted_at: u64, now: u64) -> Age {
    let ends = pasted_at + TOKEN_LIFETIME;
    if now >= ends {
        return Age::Expired;
    }
    let days_left = (ends - now) / 86_400;
    if ends - now <= WARN_BEFORE {
        Age::Soon { days_left }
    } else {
        Age::Fine { days_left }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pasted_token_is_sealed_and_its_age_is_known_without_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::new(dir.path());
        assert!(store.load().unwrap().is_none());
        assert!(store.save("not a token", 1).is_err());
        assert!(store.save("sk-ant-api03-xyz", 1).is_err());
        let token = "sk-ant-oat01-SECRETSECRET";
        store.save(&format!("  {token}\n"), 1_000).unwrap();
        let raw = std::fs::read_to_string(store.path()).unwrap();
        assert!(!raw.contains("SECRETSECRET"), "sealed on disk");
        assert_eq!(store.meta().unwrap().unwrap().pasted_at, 1_000);
        let (t, _) = store.load().unwrap().unwrap();
        assert_eq!(t.expose(), token);
        assert_eq!(format!("{t:?}"), "Token(…)");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store.path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0);
        }
        assert!(store.delete().unwrap());
        assert!(!store.delete().unwrap());

        assert!(matches!(age(0, 10 * 86_400), Age::Fine { days_left: 355 }));
        assert!(matches!(
            age(0, TOKEN_LIFETIME - 5 * 86_400),
            Age::Soon { days_left: 5 }
        ));
        assert_eq!(age(0, TOKEN_LIFETIME), Age::Expired);
    }
}
