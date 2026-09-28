//! M39 §4: Matrix on the CLI side: the adapter's settings from
//! `[gateway.matrix]` and the dashboard card.

use super::card::{Field, Kind, Settings, Spec, Step};
use super::settings::Matrix;
use crate::config_follow::secret_value;
use anyhow::{anyhow, bail, Result};
use ferrule_gateway::channels::files::Inbox;
use ferrule_gateway::channels::matrix::{self as mx, Login, MatrixConfig};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

/// Where the session and sync position are kept.
pub fn state_dir() -> Option<PathBuf> {
    crate::config::data_dir()
        .ok()
        .map(|d| d.join("gateway").join("matrix"))
}

/// How it logs in: a token, else a user and password.
pub fn login(m: &Matrix, secret: impl Fn(&str) -> Option<String>) -> Result<Login> {
    if let Some(env) = &m.access_token_env {
        return secret(env).map(Login::Token).ok_or_else(|| {
            anyhow!("`{env}` isn't set (needed by [gateway.matrix] access_token_env) — run `ferrule setup` → Matrix, or the dashboard's Matrix card")
        });
    }
    match (&m.user, &m.password_env) {
        (Some(user), Some(env)) => {
            let password = secret(env).ok_or_else(|| {
                anyhow!("`{env}` isn't set (needed by [gateway.matrix] password_env) — run `ferrule setup` → Matrix")
            })?;
            Ok(Login::Password {
                user: user.clone(),
                password,
            })
        }
        _ => bail!("[gateway.matrix] needs access_token_env, or user and password_env — run `ferrule setup` → Matrix"),
    }
}

/// The adapter's settings. `workspace`: where files people send are saved;
/// `None` (`ferrule tasks run-now`): nothing is taken in, only sent.
pub fn config(m: &Matrix, workspace: Option<&Path>) -> Result<MatrixConfig> {
    Ok(MatrixConfig {
        homeserver: m.homeserver.trim_end_matches('/').to_string(),
        login: login(m, secret_value)?,
        state_dir: state_dir(),
        inbox: workspace.map(|ws| Inbox::new(ws, m.max_file_mb)),
    })
}

fn probe(s: Settings) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>> {
    Box::pin(async move {
        let m: Matrix = s.read()?;
        let login = login(&m, |env| s.secrets.get(env).cloned()).map_err(|e| e.to_string())?;
        let p = mx::probe(MatrixConfig {
            homeserver: m.homeserver.trim_end_matches('/').to_string(),
            login,
            state_dir: None,
            inbox: None,
        })
        .await?;
        let mut said = p.summary();
        if !p.encrypted.is_empty() {
            said.push_str(
                " · it won't answer in encrypted rooms: make an unencrypted one and invite it",
            );
        }
        if m.allowed_users.is_empty() {
            said.push_str(" · no one is allowed yet: `ferrule setup` → Matrix pairs you, or list your @you:server");
        }
        Ok(said)
    })
}

fn check(t: &toml::Table) -> Result<(), String> {
    let m: Matrix = toml::Value::Table(t.clone())
        .try_into()
        .map_err(|e: toml::de::Error| e.message().to_string())?;
    if !(m.homeserver.starts_with("https://") || m.homeserver.starts_with("http://")) {
        return Err(format!(
            "the homeserver is a URL like https://matrix.example.org, not `{}`",
            m.homeserver
        ));
    }
    for u in &m.allowed_users {
        if !(u.starts_with('@') && u.contains(':')) {
            return Err(format!("`{u}` isn't a Matrix user id like @you:matrix.org"));
        }
    }
    for r in &m.allowed_rooms {
        if !(r.starts_with('!') && r.contains(':')) {
            return Err(format!(
                "`{r}` isn't a room id like !abc123:matrix.org (Element: Room settings → Advanced)"
            ));
        }
    }
    Ok(())
}

pub const SPEC: Spec = Spec {
    name: "matrix",
    fields: &[
        Field {
            key: "homeserver",
            label: "Homeserver",
            hint: "the client API's URL: https://matrix-client.matrix.org for matrix.org",
            kind: Kind::Text,
            optional: false,
        },
        Field {
            key: "access_token_env",
            label: "Access token",
            hint: "the bot account's token (syt_…); `ferrule setup` → Matrix can log in with a password for you",
            kind: Kind::Secret {
                env: "MATRIX_ACCESS_TOKEN",
            },
            optional: false,
        },
        Field {
            key: "allowed_users",
            label: "Allowed users",
            hint: "@you:matrix.org — whose DMs reach the agent",
            kind: Kind::List,
            optional: true,
        },
        Field {
            key: "allowed_rooms",
            label: "Allowed rooms",
            hint: "!abc123:matrix.org — rooms where a mention reaches it (Room settings → Advanced)",
            kind: Kind::List,
            optional: true,
        },
    ],
    guide: &[
        Step {
            text: "Register a separate account for the bot (any homeserver; matrix.org is fine)",
            url: Some("https://app.element.io/#/register"),
        },
        Step {
            text: "Signed in as the bot in Element: Settings → Help & About → Access token. Close the tab without logging out (logging out ends the token)",
            url: Some("https://app.element.io/#/login"),
        },
        Step {
            text: "Save and Test here, then invite the bot to a DM or an unencrypted room from your own account",
            url: None,
        },
        Step {
            text: "Encrypted rooms are refused: when creating a room, turn off \"Enable end-to-end encryption\"",
            url: None,
        },
    ],
    probe,
    check,
};

#[cfg(test)]
mod tests {
    use super::*;

    fn table(pairs: &[(&str, toml::Value)]) -> toml::Table {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn the_card_checks_urls_and_ids() {
        let ok = table(&[("homeserver", "https://m.org".into())]);
        assert!(check(&ok).is_ok());
        let bare = table(&[("homeserver", "m.org".into())]);
        assert!(check(&bare).unwrap_err().contains("a URL"));
        let mut who = ok.clone();
        who.insert("allowed_users".into(), vec!["max"].into());
        assert!(check(&who).unwrap_err().contains("@you:matrix.org"));
        let mut room = ok.clone();
        room.insert("allowed_rooms".into(), vec!["#general:m.org"].into());
        assert!(check(&room).unwrap_err().contains("room id"));
    }

    #[test]
    fn a_token_wins_over_a_password_and_a_missing_secret_is_named() {
        let m: Matrix = toml::from_str(
            "homeserver = \"https://m.org\"\naccess_token_env = \"T\"\nuser = \"@b:m.org\"\npassword_env = \"P\"",
        )
        .unwrap();
        let got = login(&m, |e| (e == "T").then(|| "tok".to_string())).unwrap();
        assert!(matches!(got, Login::Token(t) if t == "tok"));
        let e = login(&m, |_| None).unwrap_err().to_string();
        assert!(e.contains("`T` isn't set"), "{e}");
        let pw = Matrix {
            access_token_env: None,
            ..m
        };
        let got = login(&pw, |e| (e == "P").then(|| "pw".to_string())).unwrap();
        assert!(
            matches!(got, Login::Password { user, password } if user == "@b:m.org" && password == "pw")
        );
        let none = Matrix { user: None, ..pw };
        assert!(login(&none, |_| None).is_err());
    }
}
