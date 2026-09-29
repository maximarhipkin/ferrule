//! M39 §8: the HTTP API on the CLI side: the adapter's settings from
//! `[gateway.http]`, its keys (`ferrule channels keys …` and the card's key
//! list) and the dashboard card.

use super::card::{Field, Kind, Settings, Spec, Step};
use super::settings::HttpApi;
use crate::config::Config;
use anyhow::{anyhow, bail, Result};
use clap::Subcommand;
use ferrule_connections::cloudflared::Cloudflared;
use ferrule_gateway::channels::files::Inbox;
use ferrule_gateway::channels::http::{clients, HttpConfig};
use serde_json::{json, Value};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

/// `<data>/gateway/http`: the keys (`clients.json`) and the gateway's
/// state for the API.
pub fn dir_in(data: &Path) -> PathBuf {
    data.join("gateway").join("http")
}

/// This instance's API dir.
pub fn dir() -> Result<PathBuf> {
    crate::config::data_dir_path()
        .map(|d| dir_in(&d))
        .ok_or_else(|| anyhow!("no data dir (HOME isn't set)"))
}

/// The adapter's settings. `workspace`: where files programs send are
/// saved; `None` (`ferrule tasks run-now`): nothing is taken in.
pub fn config(h: &HttpApi, cfg: &Config, workspace: Option<&Path>) -> Result<HttpConfig> {
    let tunnel = match h.public.as_deref() {
        None => None,
        Some("tunnel") => Some(tunnel_bin(&crate::dashboard::cloudflared(cfg))?),
        Some(other) => bail!("[gateway.http] public is \"tunnel\" or unset, not `{other}`"),
    };
    Ok(HttpConfig {
        dir: dir()?,
        port: h.port,
        requests_per_minute: h.requests_per_minute.max(1),
        tunnel,
        inbox: workspace.map(|ws| Inbox::new(ws, 1)),
    })
}

/// cloudflared for `public = "tunnel"`, when it's already here (setup and
/// the dashboard fetch it; the gateway doesn't download on start).
fn tunnel_bin(c: &Cloudflared) -> Result<PathBuf> {
    match c {
        Cloudflared::At(p) => Ok(p.clone()),
        Cloudflared::Fetch(p) if !c.needs_fetch() => Ok(p.clone()),
        Cloudflared::Fetch(_) => bail!(
            "[gateway.http] public = \"tunnel\" needs cloudflared, which isn't here yet: `ferrule setup` → HTTP API fetches it, or install it"
        ),
        _ => bail!(
            "[gateway.http] public = \"tunnel\" needs cloudflared: install it, or unset [connections] cloudflared = \"off\""
        ),
    }
}

/// What answers on 127.0.0.1:`port`.
#[derive(Debug, PartialEq)]
pub enum Listening {
    /// A ferrule gateway's API (a 401 with its realm).
    Ours,
    Free,
    /// Something else, in its words.
    Other(String),
}

/// Asks 127.0.0.1:`port` without a key: the gateway answers 401 with
/// `WWW-Authenticate: Bearer realm="ferrule"`.
pub async fn listening(port: u16) -> Listening {
    let http = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .expect("a reqwest client");
    match http
        .get(format!("http://127.0.0.1:{port}/v1/events"))
        .send()
        .await
    {
        Ok(r) => {
            let realm = r
                .headers()
                .get("www-authenticate")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.contains("realm=\"ferrule\""));
            if r.status() == 401 && realm {
                Listening::Ours
            } else {
                Listening::Other(format!("it answered {}", r.status()))
            }
        }
        Err(e) if e.is_connect() => Listening::Free,
        Err(e) => Listening::Other(e.to_string()),
    }
}

/// "3 keys" and so on, for the card and doctor.
pub fn keys_said(dir: &Path) -> String {
    match clients::load(dir) {
        Ok(c) if c.is_empty() => "no key yet".into(),
        Ok(c) => crate::setup::plural(c.len(), "key", "keys"),
        Err(e) => e,
    }
}

fn probe(s: Settings) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>> {
    Box::pin(async move {
        let h: HttpApi = s.read()?;
        let keys = dir().map(|d| keys_said(&d)).unwrap_or_default();
        match listening(h.port).await {
            Listening::Ours => Ok(format!("the API answers on 127.0.0.1:{} · {keys}", h.port)),
            Listening::Free => Ok(format!(
                "port {} is free: the API opens there when the gateway starts · {keys}",
                h.port
            )),
            Listening::Other(why) => Err(format!(
                "something else is on 127.0.0.1:{} ({why}): pick another port",
                h.port
            )),
        }
    })
}

fn check(t: &toml::Table) -> Result<(), String> {
    let h: HttpApi = toml::Value::Table(t.clone())
        .try_into()
        .map_err(|e: toml::de::Error| e.message().to_string())?;
    if h.port == 0 {
        return Err("the port is 1–65535".into());
    }
    if h.requests_per_minute == 0 {
        return Err("requests a minute is at least 1".into());
    }
    match h.public.as_deref() {
        None | Some("tunnel") => Ok(()),
        Some(other) => Err(format!("public is `tunnel` or empty, not `{other}`")),
    }
}

pub const SPEC: Spec = Spec {
    name: "http",
    fields: &[
        Field {
            key: "port",
            label: "Port",
            hint: "on 127.0.0.1; 8788 when empty",
            kind: Kind::Number,
            optional: true,
        },
        Field {
            key: "requests_per_minute",
            label: "Requests a minute, per key",
            hint: "30 when empty; over it a program gets 429 and Retry-After",
            kind: Kind::Number,
            optional: true,
        },
        Field {
            key: "public",
            label: "Public",
            hint: "tunnel: a Cloudflare quick tunnel to it, its URL in /status. Empty: this machine only",
            kind: Kind::Choice(&["tunnel"]),
            optional: true,
        },
    ],
    guide: &[
        Step {
            text: "Save and turn on, then restart the gateway: the API listens on 127.0.0.1 only",
            url: None,
        },
        Step {
            text: "Create a key below for each program (n8n, Zapier, a script); it's shown once",
            url: None,
        },
        Step {
            text: "POST /v1/messages with Authorization: Bearer <key> and {\"text\": \"…\"}; the answer comes back, or as SSE with Accept: text/event-stream",
            url: Some("https://github.com/maximarhipkin/ferrule/blob/main/docs/channels.md#http"),
        },
        Step {
            text: "From elsewhere: set Public to tunnel (cloudflared), or put your own reverse proxy in front",
            url: Some("https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/do-more-with-tunnels/trycloudflare/"),
        },
    ],
    probe,
    check,
};

/// The keys for the card: never a key or a secret, only what they are.
pub fn keys_json(dir: &Path) -> Value {
    let used = clients::last_used(dir);
    let list = clients::load(dir).unwrap_or_default();
    Value::Array(
        list.iter()
            .map(|c| {
                json!({
                    "name": c.name,
                    "created": c.created,
                    "last_used": used.get(&c.name),
                    "webhook": c.webhook,
                })
            })
            .collect(),
    )
}

/// `ferrule channels …`.
#[derive(Subcommand, Debug)]
pub enum ChannelsCmd {
    /// The HTTP API's keys: one per program that calls it
    Keys {
        #[command(subcommand)]
        op: KeysCmd,
    },
}

#[derive(Subcommand, Debug)]
pub enum KeysCmd {
    /// Every key's name, when it was made and last used, and its webhook
    List {
        /// One JSON array, for scripts
        #[arg(long)]
        json: bool,
    },
    /// A new key, printed once (only its hash is kept)
    Add {
        /// 1–32 of a-z, 0-9, - and _; also its chat's name
        name: String,
        /// Where messages no request waits for (task results) are POSTed,
        /// signed with a secret printed once
        #[arg(long)]
        webhook: Option<String>,
    },
    /// Sets a key's webhook (a new signing secret, printed once), or
    /// clears it with --off
    Webhook {
        name: String,
        url: Option<String>,
        #[arg(long, conflicts_with = "url")]
        off: bool,
    },
    /// Takes a key away: its next request gets 401
    Revoke { name: String },
}

pub fn run(op: ChannelsCmd) -> Result<()> {
    let ChannelsCmd::Keys { op } = op;
    let dir = dir()?;
    let fail = |e: String| anyhow!(e);
    match op {
        KeysCmd::List { json } => {
            let list = clients::load(&dir).map_err(fail)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&keys_json(&dir))?);
                return Ok(());
            }
            if list.is_empty() {
                println!("No keys yet: `ferrule channels keys add <name>`.");
                return Ok(());
            }
            let used = clients::last_used(&dir);
            for c in list {
                let when = |t: i64| {
                    chrono::DateTime::from_timestamp(t, 0)
                        .map(|d| {
                            d.with_timezone(&chrono::Local)
                                .format("%Y-%m-%d %H:%M")
                                .to_string()
                        })
                        .unwrap_or_default()
                };
                println!(
                    "{:<20} made {}  last used {}{}",
                    c.name,
                    when(c.created),
                    used.get(&c.name).map_or("never".into(), |t| when(*t)),
                    c.webhook
                        .map(|w| format!("  webhook {w}"))
                        .unwrap_or_default()
                );
            }
        }
        KeysCmd::Add { name, webhook } => {
            let made = clients::add(&dir, &name, webhook.as_deref()).map_err(fail)?;
            println!("The key for `{name}` (shown once, keep it in the program's secrets):");
            println!("{}", made.key);
            if let Some(s) = made.webhook_secret {
                println!(
                    "Its webhook's signing secret (X-Ferrule-Signature: sha256=HMAC of the body):"
                );
                println!("{s}");
            }
            println!("It works at once; the gateway needs no restart.");
        }
        KeysCmd::Webhook { name, url, off } => {
            if url.is_none() && !off {
                bail!("give the webhook's URL, or --off to clear it");
            }
            match clients::set_webhook(&dir, &name, url.as_deref()).map_err(fail)? {
                Some(s) => {
                    println!(
                        "`{name}` posts to {}; its signing secret (shown once):",
                        url.unwrap_or_default()
                    );
                    println!("{s}");
                }
                None => println!("`{name}` has no webhook now."),
            }
        }
        KeysCmd::Revoke { name } => {
            if !clients::revoke(&dir, &name).map_err(fail)? {
                bail!("there's no key `{name}` (`ferrule channels keys list`)");
            }
            println!("`{name}` is revoked: its next request gets 401.");
        }
    }
    Ok(())
}

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
    fn the_card_checks_its_settings() {
        assert!(check(&toml::Table::new()).is_ok());
        assert!(check(&table(&[("public", "tunnel".into())])).is_ok());
        let e = check(&table(&[("public", "yes".into())])).unwrap_err();
        assert!(e.contains("`tunnel`"), "{e}");
        assert!(check(&table(&[("port", 0.into())])).is_err());
        assert!(check(&table(&[("port", 70000.into())])).is_err());
        assert!(check(&table(&[("requests_per_minute", 0.into())])).is_err());
    }

    #[test]
    fn a_tunnel_needs_cloudflared_here() {
        let dir = tempfile::tempdir().unwrap();
        let e = tunnel_bin(&Cloudflared::Fetch(dir.path().join("cloudflared")))
            .unwrap_err()
            .to_string();
        assert!(e.contains("isn't here yet"), "{e}");
        assert!(tunnel_bin(&Cloudflared::Off).is_err());
        let at = dir.path().join("cf");
        assert_eq!(tunnel_bin(&Cloudflared::At(at.clone())).unwrap(), at);
    }

    #[tokio::test]
    async fn listening_tells_the_gateway_from_anything_else() {
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = free.local_addr().unwrap().port();
        drop(free);
        assert_eq!(listening(port).await, Listening::Free);
        let other = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = other.local_addr().unwrap().port();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut s, _) = other.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf).await;
            let _ = s
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                .await;
        });
        assert!(matches!(listening(port).await, Listening::Other(_)));
    }
}
