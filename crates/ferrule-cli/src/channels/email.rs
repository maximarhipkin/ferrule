//! M39 §5: email on the CLI side: the adapter's settings from
//! `[gateway.email]` (or M37's Gmail connection), and the dashboard card.

use super::card::{Field, Kind, Settings, Spec, Step};
use super::settings::Email;
use crate::config_follow::secret_value;
use anyhow::{anyhow, bail, Result};
use ferrule_gateway::channels::email::{self as em, EmailConfig, Server};
use ferrule_gateway::channels::files::Inbox;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

/// Without IDLE: how often to look, when `poll_secs` isn't set.
pub const DEFAULT_POLL_SECS: u64 = 60;

/// Where the last UID seen and the threads are kept.
pub fn state_dir() -> Option<PathBuf> {
    crate::config::data_dir()
        .ok()
        .map(|d| d.join("gateway").join("email"))
}

/// IMAP and SMTP: host and port.
pub type Servers = ((&'static str, u16), (&'static str, u16));

/// The servers of the providers everyone asks about, by the address's
/// domain: IMAP and SMTP, host and port.
pub fn provider(address: &str) -> Option<Servers> {
    let domain = address.rsplit_once('@')?.1.to_ascii_lowercase();
    Some(match domain.as_str() {
        "gmail.com" | "googlemail.com" => (("imap.gmail.com", 993), ("smtp.gmail.com", 465)),
        "yahoo.com" | "ymail.com" => (("imap.mail.yahoo.com", 993), ("smtp.mail.yahoo.com", 465)),
        "icloud.com" | "me.com" | "mac.com" => {
            (("imap.mail.me.com", 993), ("smtp.mail.me.com", 587))
        }
        "fastmail.com" | "fastmail.fm" => (("imap.fastmail.com", 993), ("smtp.fastmail.com", 465)),
        _ => return None,
    })
}

/// Whether the receiving server is one known to stamp
/// `Authentication-Results` on every mail (so `require_auth_results`
/// defaults on).
fn stamps_auth_results(imap_host: &str) -> bool {
    let h = imap_host.to_ascii_lowercase();
    [
        "imap.gmail.com",
        "imap.mail.yahoo.com",
        "imap.mail.me.com",
        "imap.fastmail.com",
    ]
    .contains(&h.as_str())
}

/// A Google app password as Google shows it (four groups of four letters)
/// without its spaces; anything else as typed.
pub fn app_password(typed: &str) -> String {
    let joined: String = typed.chars().filter(|c| !c.is_whitespace()).collect();
    if typed.contains(' ') && joined.len() == 16 && joined.chars().all(|c| c.is_ascii_alphabetic())
    {
        joined
    } else {
        typed.trim().to_string()
    }
}

/// The address and password M37's connection `name` holds (a Gmail app
/// password).
pub fn connection(name: &str) -> Result<(String, String)> {
    let store = ferrule_connections::Store::new(&crate::secrets::private_dir()?);
    let records = store.load()?;
    let Some(record) = records.iter().find(|r| r.name == name) else {
        bail!("[gateway.email] use_connection = \"{name}\", but there's no connection by that name — connect Gmail (the dashboard's Connections, or `ferrule connections setup gmail`), or give address and password_env instead");
    };
    if record.service.native.as_deref() != Some("gmail") {
        bail!("[gateway.email] use_connection = \"{name}\" isn't a Gmail app-password connection");
    }
    let secret = store.open(record)?;
    let get = |k: &str| {
        secret
            .fields
            .get(k)
            .filter(|v| !v.is_empty())
            .cloned()
            .ok_or_else(|| anyhow!("the connection `{name}` has no {k}; connect Gmail again"))
    };
    Ok((get("email")?, get("app_password")?))
}

/// The adapter's settings from `e`, with the password found by `secret`
/// and a connection's address and password by `conn`.
pub fn resolve(
    e: &Email,
    secret: impl Fn(&str) -> Option<String>,
    conn: impl Fn(&str) -> Result<(String, String)>,
) -> Result<EmailConfig> {
    let (address, password) = match (&e.use_connection, &e.password_env) {
        (_, Some(env)) => {
            let address = e.address.clone().ok_or_else(|| {
                anyhow!("[gateway.email] needs address — run `ferrule setup` → Email")
            })?;
            let password = secret(env).ok_or_else(|| {
                anyhow!("`{env}` isn't set (needed by [gateway.email] password_env) — run `ferrule setup` → Email, or the dashboard's Email card")
            })?;
            (address, app_password(&password))
        }
        (Some(name), None) => {
            let (address, password) = conn(name)?;
            (e.address.clone().unwrap_or(address), password)
        }
        (None, None) => bail!("[gateway.email] needs password_env (or use_connection = \"gmail\") — run `ferrule setup` → Email"),
    };
    if !address.contains('@') {
        bail!("[gateway.email] address `{address}` isn't an email address");
    }
    let known = provider(&address);
    let imap_host = e
        .imap_host
        .clone()
        .or_else(|| known.map(|k| k.0 .0.to_string()))
        .ok_or_else(|| anyhow!("[gateway.email] needs imap_host for {address} (your provider's IMAP server, like imap.example.com)"))?;
    let smtp_host = e
        .smtp_host
        .clone()
        .or_else(|| known.map(|k| k.1 .0.to_string()))
        .ok_or_else(|| anyhow!("[gateway.email] needs smtp_host for {address} (your provider's SMTP server, like smtp.example.com)"))?;
    let known_port = |host: &str, pick: fn(Servers) -> (&'static str, u16)| {
        known
            .map(pick)
            .filter(|(h, _)| h.eq_ignore_ascii_case(host))
            .map(|(_, p)| p)
    };
    let imap_port = e
        .imap_port
        .or_else(|| known_port(&imap_host, |k| k.0))
        .unwrap_or(993);
    let smtp_port = e
        .smtp_port
        .or_else(|| known_port(&smtp_host, |k| k.1))
        .unwrap_or(465);
    let require_auth = e
        .require_auth_results
        .unwrap_or_else(|| stamps_auth_results(&imap_host));
    Ok(EmailConfig {
        username: e.username.clone().unwrap_or_else(|| address.clone()),
        address,
        password,
        imap: Server::new(&imap_host, imap_port),
        smtp: Server::new(&smtp_host, smtp_port),
        require_auth,
        poll: Duration::from_secs(e.poll_secs.unwrap_or(DEFAULT_POLL_SECS).clamp(10, 240)),
        state_dir: None,
        inbox: None,
        instance: crate::instance::label(crate::instance::current().as_deref()).to_string(),
    })
}

/// The adapter's settings. `workspace`: where files people send are saved;
/// `None` (`ferrule tasks run-now`): nothing is taken in, only sent.
pub fn config(e: &Email, workspace: Option<&Path>) -> Result<EmailConfig> {
    let mut cfg = resolve(e, secret_value, connection)?;
    cfg.state_dir = state_dir();
    cfg.inbox = workspace.map(|ws| Inbox::new(ws, e.max_file_mb));
    Ok(cfg)
}

fn probe(s: Settings) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>> {
    Box::pin(async move {
        let e: Email = s.read()?;
        let cfg = resolve(&e, |env| s.secrets.get(env).cloned(), connection)
            .map_err(|e| e.to_string())?;
        let require = cfg.require_auth;
        let mut said = em::probe(cfg).await?.summary();
        if !require {
            said.push_str(" · the server's Authentication-Results aren't required: approvals by mail still need them");
        }
        if e.allowed_senders.is_empty() {
            said.push_str(" · no one is allowed yet: add your address under Allowed senders");
        }
        Ok(said)
    })
}

/// An allowlist entry: an address, or `@domain`.
pub fn sender_ok(s: &str) -> bool {
    match s.split_once('@') {
        Some((local, domain)) => {
            !domain.is_empty()
                && domain.contains('.')
                && !domain.contains('@')
                && !s.contains(char::is_whitespace)
                && (local.is_empty() || !local.contains(['<', '>']))
        }
        None => false,
    }
}

fn check(t: &toml::Table) -> Result<(), String> {
    let e: Email = toml::Value::Table(t.clone())
        .try_into()
        .map_err(|e: toml::de::Error| e.message().to_string())?;
    match (&e.address, &e.use_connection) {
        (Some(a), _) if !a.contains('@') => {
            return Err(format!("`{a}` isn't an email address"));
        }
        (None, None) => return Err("the address is missing".into()),
        _ => {}
    }
    if e.password_env.is_none() && e.use_connection.is_none() {
        return Err("the password is missing (an app password, not your normal one)".into());
    }
    if e.imap_host.is_none()
        && e.address.as_deref().and_then(provider).is_none()
        && e.use_connection.is_none()
    {
        return Err(
            "the IMAP server is missing: your provider's help pages name it (imap.example.com)"
                .into(),
        );
    }
    if e.smtp_host.is_none()
        && e.address.as_deref().and_then(provider).is_none()
        && e.use_connection.is_none()
    {
        return Err(
            "the SMTP server is missing: your provider's help pages name it (smtp.example.com)"
                .into(),
        );
    }
    for s in &e.allowed_senders {
        if !sender_ok(s) {
            return Err(format!(
                "`{s}` isn't an address (you@example.com) or a domain (@example.com)"
            ));
        }
    }
    Ok(())
}

pub const SPEC: Spec = Spec {
    name: "email",
    fields: &[
        Field {
            key: "address",
            label: "Address",
            hint: "the mailbox the agent reads and sends from — best a separate one, like yourname.agent@gmail.com",
            kind: Kind::Text,
            optional: false,
        },
        Field {
            key: "password_env",
            label: "App password",
            hint: "an app password (Gmail: 16 letters, spaces are fine), not your normal password",
            kind: Kind::Secret {
                env: "EMAIL_PASSWORD",
            },
            optional: false,
        },
        Field {
            key: "allowed_senders",
            label: "Allowed senders",
            hint: "you@example.com, or @example.com for a whole domain — only their mail reaches the agent",
            kind: Kind::List,
            optional: true,
        },
        Field {
            key: "imap_host",
            label: "IMAP server",
            hint: "left empty for Gmail, Yahoo, iCloud and Fastmail; else your provider's (imap.example.com)",
            kind: Kind::Text,
            optional: true,
        },
        Field {
            key: "smtp_host",
            label: "SMTP server",
            hint: "left empty for those four; else your provider's (smtp.example.com)",
            kind: Kind::Text,
            optional: true,
        },
        Field {
            key: "imap_port",
            label: "IMAP port",
            hint: "993 (TLS) unless your provider says otherwise; 143 uses STARTTLS",
            kind: Kind::Number,
            optional: true,
        },
        Field {
            key: "smtp_port",
            label: "SMTP port",
            hint: "465 (TLS) or 587 (STARTTLS)",
            kind: Kind::Number,
            optional: true,
        },
    ],
    guide: &[
        Step {
            text: "Best: make a separate mailbox for the agent, so it never reads your own mail",
            url: Some("https://accounts.google.com/signup"),
        },
        Step {
            text: "Gmail: turn on 2-Step Verification, then make an app password (Google shows 16 letters)",
            url: Some("https://myaccount.google.com/apppasswords"),
        },
        Step {
            text: "Gmail: check IMAP is on (Settings → Forwarding and POP/IMAP; new accounts have it on)",
            url: Some("https://mail.google.com/mail/u/0/#settings/fwdandpop"),
        },
        Step {
            text: "Other providers: iCloud, Yahoo and Fastmail have app passwords too; a company server needs its IMAP and SMTP names",
            url: Some("https://support.apple.com/en-us/102654"),
        },
        Step {
            text: "Save and Test, then send the address a mail from an allowed sender",
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

    fn no_conn(_: &str) -> Result<(String, String)> {
        bail!("no connections here")
    }

    #[test]
    fn the_card_checks_addresses_servers_and_senders() {
        let ok = table(&[
            ("address", "bot@gmail.com".into()),
            ("password_env", "EMAIL_PASSWORD".into()),
        ]);
        assert!(check(&ok).is_ok());
        let mut other = ok.clone();
        other.insert("address".into(), "bot@example.org".into());
        assert!(check(&other).unwrap_err().contains("IMAP server"));
        other.insert("imap_host".into(), "imap.example.org".into());
        assert!(check(&other).unwrap_err().contains("SMTP server"));
        other.insert("smtp_host".into(), "smtp.example.org".into());
        assert!(check(&other).is_ok());
        let mut who = ok.clone();
        who.insert("allowed_senders".into(), vec!["max"].into());
        assert!(check(&who).unwrap_err().contains("@example.com"));
        who.insert(
            "allowed_senders".into(),
            vec!["max@example.com", "@example.org"].into(),
        );
        assert!(check(&who).is_ok());
        let nopw = table(&[("address", "bot@gmail.com".into())]);
        assert!(check(&nopw).unwrap_err().contains("app password"));
    }

    #[test]
    fn a_known_provider_fills_its_servers_and_asks_for_vouching() {
        let e: Email = toml::from_str("address = \"Bot@Gmail.com\"\npassword_env = \"P\"").unwrap();
        let got = resolve(
            &e,
            |k| (k == "P").then(|| "abcd efgh ijkl mnop".to_string()),
            no_conn,
        )
        .unwrap();
        assert_eq!(got.imap, Server::new("imap.gmail.com", 993));
        assert_eq!(got.smtp, Server::new("smtp.gmail.com", 465));
        assert_eq!(got.password, "abcdefghijklmnop", "Google's groups joined");
        assert_eq!(got.username, "Bot@Gmail.com");
        assert!(got.require_auth);
        assert_eq!(got.poll, Duration::from_secs(60));

        let own: Email = toml::from_str(
            "address = \"a@corp.example\"\npassword_env = \"P\"\nimap_host = \"mail.corp.example\"\nsmtp_host = \"mail.corp.example\"\nsmtp_port = 587\npoll_secs = 1",
        )
        .unwrap();
        let got = resolve(&own, |_| Some("pass word".into()), no_conn).unwrap();
        assert_eq!(got.imap.port, 993);
        assert_eq!(got.smtp, Server::new("mail.corp.example", 587));
        assert_eq!(got.password, "pass word", "a real password keeps its space");
        assert!(!got.require_auth, "unknown servers may not stamp it");
        assert_eq!(got.poll, Duration::from_secs(10));
        let e = resolve(&own, |_| None, no_conn).unwrap_err().to_string();
        assert!(e.contains("`P` isn't set"), "{e}");
    }

    #[test]
    fn a_gmail_connection_lends_its_address_and_password() {
        let e: Email = toml::from_str("use_connection = \"gmail\"").unwrap();
        let got = resolve(
            &e,
            |_| None,
            |name| {
                assert_eq!(name, "gmail");
                Ok(("me@gmail.com".into(), "abcdefghijklmnop".into()))
            },
        )
        .unwrap();
        assert_eq!(got.address, "me@gmail.com");
        assert_eq!(got.password, "abcdefghijklmnop");
        assert_eq!(got.imap.host, "imap.gmail.com");
        let e = resolve(&e, |_| None, no_conn).unwrap_err().to_string();
        assert!(e.contains("no connections"), "{e}");
    }
}
