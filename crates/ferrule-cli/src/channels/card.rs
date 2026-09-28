//! M39 §9: a channel's dashboard card, described once: its fields, its
//! guide with direct links, and its Test. Save and remove are the same for
//! every channel. Save writes the secrets to `secrets.env` and the rest to
//! `[gateway.<channel>]`. Remove takes the table out again and forgets the
//! secrets nothing else reads.

use crate::config::Config;
use crate::connections_setup::Place;
use anyhow::{anyhow, Result};
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

/// What a field holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    /// Write-only. The config key names the env var (`token_env`); the
    /// value goes to `secrets.env` under that name, `env` unless the
    /// config already names another.
    Secret {
        env: &'static str,
    },
    /// Ids, one per line or comma-separated.
    List,
    Number,
    /// One of these words.
    Choice(&'static [&'static str]),
}

#[derive(Debug, Clone, Copy)]
pub struct Field {
    /// The key in `[gateway.<channel>]`.
    pub key: &'static str,
    pub label: &'static str,
    pub hint: &'static str,
    pub kind: Kind,
    pub optional: bool,
}

/// One step of the guide; `url` goes straight to the page it names.
#[derive(Debug, Clone, Copy)]
pub struct Step {
    pub text: &'static str,
    pub url: Option<&'static str>,
}

/// What Test gets: the card's table as it would be saved, and the secrets'
/// values by env name (typed now, else stored).
#[derive(Debug, Clone, Default)]
pub struct Settings {
    pub table: toml::Table,
    pub secrets: BTreeMap<String, String>,
    /// The instance's relay Worker and its key, when one is deployed (a
    /// channel whose webhooks come through it tests that part too).
    pub relay: Option<(String, String)>,
}

impl Settings {
    /// The value of the secret the config key `key` names.
    pub fn secret(&self, key: &str) -> Option<&str> {
        let env = self.table.get(key)?.as_str()?;
        self.secrets.get(env).map(String::as_str)
    }

    /// The table read as the channel's settings.
    pub fn read<T: serde::de::DeserializeOwned>(&self) -> Result<T, String> {
        toml::Value::Table(self.table.clone())
            .try_into()
            .map_err(|e: toml::de::Error| e.message().to_string())
    }
}

/// Test: `Ok` with what it found ("+972 50… (Shop)"), `Err` saying why in
/// words.
pub type Probe = fn(Settings) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>>;

pub struct Spec {
    pub name: &'static str,
    pub fields: &'static [Field],
    pub guide: &'static [Step],
    pub probe: Probe,
    /// Checks the table reads as the channel's settings (serde's
    /// `deny_unknown_fields`, the required keys).
    pub check: fn(&toml::Table) -> Result<(), String>,
}

/// The channels with a form on the dashboard; each channel's part adds
/// its own. Telegram, Discord and Slack keep `ferrule setup`'s flow.
pub const CARDS: &[&Spec] = &[&super::whatsapp::SPEC];

/// `name`'s card.
pub fn spec(name: &str) -> Option<&'static Spec> {
    CARDS.iter().copied().find(|s| s.name == name)
}

/// An SVG path (24×24, stroked) for each channel's card. Plain shapes,
/// not the vendors' marks.
pub fn icon(name: &str) -> &'static str {
    match name {
        "telegram" => "M21 4 3 11l6 2 2 6 3-4 5 4z M9 13l8-6",
        "discord" => {
            "M7 7c3-1.5 7-1.5 10 0l2 9c-1.5 1.5-3.5 2-5 2l-1-2m-2 0-1 2c-1.5 0-3.5-.5-5-2z"
        }
        "slack" => "M9 3v8M15 13v8M3 15h8M13 9h8",
        "whatsapp" => "M4 20l1.5-4.5A8 8 0 1 1 8.5 18.5z M9 9c0 3 3 6 6 6",
        "matrix" => "M4 4v16h2M20 4v16h-2M9 10v5M9 11c0-1 3-1 3 0v4M12 11c0-1 3-1 3 0v4",
        "email" => "M3 6h18v12H3z M3 6l9 7 9-7",
        "signal" => "M12 3a9 9 0 1 0 0 18c-1.5 0-3-.4-4.3-1L4 21l1-3.7A9 9 0 0 1 12 3z",
        "mattermost" => "M12 3a9 9 0 1 0 6 2.3M15 7l-3 7-2-3",
        "http" => "M8 7l-5 5 5 5M16 7l5 5-5 5M14 4l-4 16",
        _ => "M4 12h16",
    }
}

/// The values a Save or a Test brings, by field key.
pub type Values = BTreeMap<String, String>;

/// `values` over the table `current` (the config's), checked: the table as
/// it would be saved, and the secrets typed now by env name.
pub fn merge(
    spec: &Spec,
    current: Option<&toml::Table>,
    values: &Values,
) -> Result<(toml::Table, BTreeMap<String, String>), String> {
    let mut table = current.cloned().unwrap_or_default();
    let mut secrets = BTreeMap::new();
    for f in spec.fields {
        let typed = values
            .get(f.key)
            .map(|v| v.trim())
            .filter(|v| !v.is_empty());
        let Some(v) = typed else {
            continue;
        };
        let value = match f.kind {
            Kind::Text => toml::Value::String(v.to_string()),
            Kind::Secret { env } => {
                if v.chars().any(char::is_whitespace) {
                    return Err(format!(
                        "{} has spaces or line breaks in it; paste it as one line. Nothing was saved.",
                        f.label
                    ));
                }
                let name = table
                    .get(f.key)
                    .and_then(toml::Value::as_str)
                    .unwrap_or(env)
                    .to_string();
                secrets.insert(name.clone(), v.to_string());
                toml::Value::String(name)
            }
            Kind::List => toml::Value::Array(
                v.split([',', '\n'])
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| toml::Value::String(s.to_string()))
                    .collect(),
            ),
            Kind::Number => toml::Value::Integer(
                v.parse()
                    .map_err(|_| format!("{} should be a whole number, not `{v}`", f.label))?,
            ),
            Kind::Choice(words) => {
                if !words.contains(&v) {
                    return Err(format!("{} is one of {}", f.label, words.join(", ")));
                }
                toml::Value::String(v.to_string())
            }
        };
        table.insert(f.key.to_string(), value);
    }
    for f in spec.fields {
        // A secret's env name is written even when nothing is typed: the
        // stored value (or the environment's) is then what it reads.
        if let Kind::Secret { env } = f.kind {
            if !f.optional && !table.contains_key(f.key) {
                table.insert(f.key.to_string(), toml::Value::String(env.to_string()));
            }
        }
        if !f.optional && !table.contains_key(f.key) {
            return Err(format!("{} is still empty. Nothing was saved.", f.label));
        }
    }
    (spec.check)(&table).map_err(|e| format!("{e}. Nothing was saved."))?;
    Ok((table, secrets))
}

/// The table `[gateway.<name>]` in `cfg_path`, as it is.
pub fn current(cfg_path: &std::path::Path, name: &str) -> Option<toml::Table> {
    let text = std::fs::read_to_string(cfg_path).ok()?;
    let doc: toml::Table = toml::from_str(&text).ok()?;
    doc.get("gateway")?.get(name)?.as_table().cloned()
}

/// `[connections] relay_url` in `cfg_path`.
fn relay_of(cfg_path: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(cfg_path).ok()?;
    let doc: toml::Table = toml::from_str(&text).ok()?;
    Some(
        doc.get("connections")?
            .get("relay_url")?
            .as_str()?
            .to_string(),
    )
}

/// Test's settings: the merged table, with each secret typed now or else
/// the stored one.
pub fn settings(spec: &Spec, place: &Place, values: &Values) -> Result<Settings, String> {
    gather(spec, place, values).map(|(s, _)| s)
}

/// The settings, and which secrets were typed now.
fn gather(
    spec: &Spec,
    place: &Place,
    values: &Values,
) -> Result<(Settings, BTreeMap<String, String>), String> {
    let now = current(&place.config, spec.name);
    let (table, typed) = merge(spec, now.as_ref(), values)?;
    let mut secrets = typed.clone();
    for f in spec.fields {
        let Kind::Secret { .. } = f.kind else {
            continue;
        };
        let env = table.get(f.key).and_then(toml::Value::as_str);
        if let Some(env) = env {
            if !secrets.contains_key(env) {
                if let Some(v) = place.get(env) {
                    secrets.insert(env.to_string(), v);
                }
            }
        }
        if !f.optional && env.is_none_or(|e| !secrets.contains_key(e)) {
            return Err(format!("paste the {}.", f.label.to_lowercase()));
        }
    }
    let relay = relay_of(&place.config).zip(place.get(ferrule_connections::relay::RELAY_KEY_ENV));
    Ok((
        Settings {
            table,
            secrets,
            relay,
        },
        typed,
    ))
}

/// Save: the secrets typed now first (so a config that names them never
/// points at nothing), then `[gateway.<name>]`, comments elsewhere kept.
pub fn save(spec: &Spec, place: &Place, values: &Values) -> Result<(), String> {
    let (s, typed) = gather(spec, place, values)?;
    for (env, v) in &typed {
        crate::secrets::set(&place.secrets, env, v)
            .map_err(|e| format!("the secrets file couldn't be written: {e:#}"))?;
    }
    write_table(place, spec.name, &s.table).map_err(|e| format!("{e:#}"))
}

fn write_table(place: &Place, name: &str, table: &toml::Table) -> Result<()> {
    let mut t = crate::setup::Target::load(place.config.clone())?;
    let tbl = crate::setup::table(t.root(), &["gateway", name])?;
    for (k, v) in table {
        crate::setup::put(tbl, k, edit_value(v)?);
    }
    t.save()
}

fn edit_value(v: &toml::Value) -> Result<toml_edit::Value> {
    Ok(match v {
        toml::Value::String(s) => s.as_str().into(),
        toml::Value::Integer(i) => (*i).into(),
        toml::Value::Boolean(b) => (*b).into(),
        toml::Value::Float(f) => (*f).into(),
        toml::Value::Array(a) => {
            let mut out = toml_edit::Array::new();
            for x in a {
                out.push(edit_value(x)?);
            }
            toml_edit::Value::Array(out)
        }
        other => return Err(anyhow!("can't write {other} into the config")),
    })
}

/// Remove: `[gateway.<name>]` out of the config, then each secret it named
/// that nothing left in the config reads. What was removed, for the reply.
pub fn remove(place: &Place, name: &str, cfg_before: &Config) -> Result<Vec<String>> {
    let envs = super::secret_envs_of(cfg_before, name);
    let mut t = crate::setup::Target::load(place.config.clone())?;
    crate::setup::table(t.root(), &["gateway"])?.remove(name);
    t.save()?;
    let after = t.config()?;
    let mut forgot = Vec::new();
    for env in envs {
        let used = super::reads_env(&after, &env)
            || after.providers.values().any(|p| p.api_key_env == env)
            || after.secrets.contains_key(&env);
        if !used {
            crate::secrets::remove(&place.secrets, &env)?;
            forgot.push(env);
        }
    }
    Ok(forgot)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(s: Settings) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>> {
        Box::pin(async move {
            match s.secret("token_env") {
                Some("good") => Ok("bot ok".into()),
                _ => Err("rejected".into()),
            }
        })
    }

    fn check(t: &toml::Table) -> Result<(), String> {
        toml::Value::Table(t.clone())
            .try_into::<super::super::settings::Mattermost>()
            .map(|_| ())
            .map_err(|e| e.message().to_string())
    }

    const SPEC: Spec = Spec {
        name: "mattermost",
        fields: &[
            Field {
                key: "server_url",
                label: "Server URL",
                hint: "",
                kind: Kind::Text,
                optional: false,
            },
            Field {
                key: "token_env",
                label: "Bot token",
                hint: "",
                kind: Kind::Secret {
                    env: "MATTERMOST_TOKEN",
                },
                optional: false,
            },
            Field {
                key: "allowed_users",
                label: "Allowed users",
                hint: "",
                kind: Kind::List,
                optional: true,
            },
        ],
        guide: &[],
        probe,
        check,
    };

    fn place() -> (tempfile::TempDir, Place) {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("ferrule.toml");
        std::fs::write(&config, "# mine\n[agent]\nstream = true\n").unwrap();
        let secrets = dir.path().join("secrets.env");
        (dir, Place { config, secrets })
    }

    fn vals(pairs: &[(&str, &str)]) -> Values {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn save_writes_the_secret_apart_and_the_table_beside_the_comments() {
        let (_d, place) = place();
        save(
            &SPEC,
            &place,
            &vals(&[
                ("server_url", "https://chat.example.com"),
                ("token_env", "abc123"),
                ("allowed_users", "u1, u2\nu3"),
            ]),
        )
        .unwrap();
        let text = std::fs::read_to_string(&place.config).unwrap();
        assert!(text.starts_with("# mine\n"), "{text}");
        assert!(
            !text.contains("abc123"),
            "the token stays out of the config"
        );
        let cfg: Config = toml::from_str(&text).unwrap();
        let m = cfg.gateway.mattermost.clone().unwrap();
        assert_eq!(m.server_url, "https://chat.example.com");
        assert_eq!(m.token_env, "MATTERMOST_TOKEN");
        assert_eq!(m.allowed_users, ["u1", "u2", "u3"]);
        assert_eq!(place.get("MATTERMOST_TOKEN").as_deref(), Some("abc123"));

        // A second save without the token keeps the stored one.
        save(&SPEC, &place, &vals(&[("allowed_users", "u9")])).unwrap();
        assert_eq!(place.get("MATTERMOST_TOKEN").as_deref(), Some("abc123"));
        let cfg: Config = toml::from_str(&std::fs::read_to_string(&place.config).unwrap()).unwrap();
        assert_eq!(
            cfg.gateway.mattermost.as_ref().unwrap().allowed_users,
            ["u9"]
        );

        let forgot = remove(&place, "mattermost", &cfg).unwrap();
        assert_eq!(forgot, ["MATTERMOST_TOKEN"]);
        assert!(place.get("MATTERMOST_TOKEN").is_none());
        let cfg: Config = toml::from_str(&std::fs::read_to_string(&place.config).unwrap()).unwrap();
        assert!(cfg.gateway.mattermost.is_none());
    }

    #[test]
    fn nothing_is_saved_while_something_is_missing_or_wrong() {
        let (_d, place) = place();
        let e = save(&SPEC, &place, &vals(&[("token_env", "abc")])).unwrap_err();
        assert!(e.contains("Server URL is still empty"), "{e}");
        let e = save(
            &SPEC,
            &place,
            &vals(&[("server_url", "https://x"), ("token_env", "a b")]),
        )
        .unwrap_err();
        assert!(e.contains("one line"), "{e}");
        let e = save(&SPEC, &place, &vals(&[("server_url", "https://x")])).unwrap_err();
        assert!(e.contains("paste the bot token"), "{e}");
        assert!(!std::fs::read_to_string(&place.config)
            .unwrap()
            .contains("mattermost"));
        assert!(place.get("MATTERMOST_TOKEN").is_none());
    }

    #[tokio::test]
    async fn test_uses_the_stored_secret_when_none_is_typed() {
        let (_d, place) = place();
        crate::secrets::set(&place.secrets, "MATTERMOST_TOKEN", "good").unwrap();
        let s = settings(&SPEC, &place, &vals(&[("server_url", "https://x")])).unwrap();
        assert_eq!((SPEC.probe)(s).await.unwrap(), "bot ok");
        let s = settings(
            &SPEC,
            &place,
            &vals(&[("server_url", "https://x"), ("token_env", "bad")]),
        )
        .unwrap();
        assert_eq!((SPEC.probe)(s).await.unwrap_err(), "rejected");
    }
}
