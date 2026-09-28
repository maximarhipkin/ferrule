//! M37: setting connections up, the same steps from the dashboard's
//! Connections page and from `ferrule connections setup`: the relay (the
//! fixed callback address) deployed to the owner's Cloudflare account or
//! an existing one checked and used, Google's OAuth client saved, and a
//! tile's way in walked through at the terminal. Every secret goes to the
//! secrets file (write-only) and is applied live; nothing is echoed.
//! Design: `docs/m37-control-room.md` §3.5.

use crate::secrets;
use anyhow::{bail, Context, Result};
use ferrule_connections::catalog::{AuthKind, Field, Service};
use ferrule_connections::relay::{self, Account, Relay};
use ferrule_connections::Connections;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const CF_API: &str = "https://api.cloudflare.com/client/v4";
pub const CF_TOKEN: &str = "CLOUDFLARE_API_TOKEN";
pub const CF_ACCOUNT: &str = "CLOUDFLARE_ACCOUNT_ID";
pub const GOOGLE_CLIENT: &str = "FERRULE_GOOGLE_CLIENT";

/// A relay deploy's outcome: done, or the token reaches several accounts
/// and the owner picks one.
#[derive(Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Deployed {
    Done {
        url: String,
        callback: String,
        steps: Vec<(String, bool)>,
    },
    ChooseAccount {
        accounts: Vec<Account>,
    },
}

/// Where setup writes: the config file (`relay_url`) and the secrets
/// file. The dashboard's tests point both into a temp dir.
pub struct Place {
    pub config: PathBuf,
    pub secrets: PathBuf,
}

impl Place {
    /// This install's config and secrets file.
    pub fn here(config: &Path) -> Result<Self> {
        Ok(Self {
            config: config.to_path_buf(),
            secrets: secrets::path()?,
        })
    }

    /// The environment first, then the secrets file.
    pub(crate) fn get(&self, name: &str) -> Option<String> {
        if let Some(v) = std::env::var(name).ok().filter(|v| !v.is_empty()) {
            return Some(v);
        }
        secrets::read(&self.secrets)
            .ok()?
            .into_iter()
            .find(|(n, v)| n == name && !v.is_empty())
            .map(|(_, v)| v)
    }

    fn put(&self, name: &str, value: &str) -> Result<()> {
        secrets::set(&self.secrets, name, value)
    }
}

/// Checks `relay` a few times: a new Worker takes seconds to be served.
async fn settled_check(relay: &Relay, tries: u32) -> Vec<(String, bool)> {
    let mut steps = Vec::new();
    for n in 0..tries.max(1) {
        steps = relay::check(relay).await;
        if steps.iter().all(|(_, ok)| *ok) {
            break;
        }
        if n + 1 < tries {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
    }
    steps
}

fn first_failure(steps: &[(String, bool)]) -> Option<&str> {
    steps.iter().find(|(_, ok)| !ok).map(|(s, _)| s.as_str())
}

/// Deploys (or updates) the relay Worker to the owner's Cloudflare
/// account, checks it end to end, and uses it from now on: the relay key
/// and the Cloudflare token are stored, `[connections] relay_url` is set
/// in the config, and the running process switches without a restart.
/// `token`/`account` come from the page or prompt, else the secrets file.
pub async fn deploy_relay(
    conns: &Connections,
    place: &Place,
    api: &str,
    token: Option<&str>,
    account: Option<&str>,
    name: &str,
    tries: u32,
) -> Result<Deployed> {
    let token = match token.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => t.to_string(),
        None => place.get(CF_TOKEN).with_context(|| {
            format!(
                "paste a Cloudflare API token: make one at {} from the \"Edit Cloudflare \
                 Workers\" template",
                relay::CF_TOKEN_URL
            )
        })?,
    };
    if token.chars().any(|c| c.is_whitespace() || c.is_control()) {
        bail!("that doesn't look like a Cloudflare API token (it has spaces in it)");
    }
    let account = match account.map(str::trim).filter(|a| !a.is_empty()) {
        Some(a) => a.to_string(),
        None => {
            let accounts = relay::accounts(api, &token).await?;
            match accounts.as_slice() {
                [] => bail!(
                    "the token doesn't reach any Cloudflare account: make it for your account \
                     (Account Resources → Include → your account)"
                ),
                [one] => one.id.clone(),
                _ => match place
                    .get(CF_ACCOUNT)
                    .filter(|a| accounts.iter().any(|x| &x.id == a))
                {
                    Some(a) => a,
                    None => return Ok(Deployed::ChooseAccount { accounts }),
                },
            }
        }
    };
    // One relay key per install, kept across deploys.
    let relay_key = match place.get(relay::RELAY_KEY_ENV) {
        Some(k) => k,
        None => {
            let k = ferrule_connections::seal::b64(&ferrule_connections::seal::random::<32>());
            place.put(relay::RELAY_KEY_ENV, &k)?;
            k
        }
    };
    let url = relay::deploy(&relay::Deploy {
        api,
        token: &token,
        account: &account,
        name,
        relay_key: &relay_key,
    })
    .await?;
    place.put(CF_TOKEN, &token)?;
    place.put(CF_ACCOUNT, &account)?;
    let r = Relay::new(&url, &relay_key);
    let steps = settled_check(&r, tries).await;
    if let Some(step) = first_failure(&steps) {
        bail!(
            "the relay was deployed to {url} but doesn't work yet ({step} failed). Cloudflare can \
             take a minute to serve a new Worker: check it again shortly"
        );
    }
    set_relay_url(&place.config, &url)?;
    conns.set_relay_url(Some(url.clone()));
    Ok(Deployed::Done {
        callback: r.callback_url(),
        url,
        steps,
    })
}

/// An https relay address, tidied.
fn relay_address(url: &str) -> Result<String> {
    let url = url.trim().trim_end_matches('/');
    let url = url.strip_suffix("/cb").unwrap_or(url);
    let ok = url::Url::parse(url).is_ok_and(|u| {
        u.scheme() == "https"
            || (u.scheme() == "http"
                && matches!(u.host_str(), Some("127.0.0.1") | Some("localhost")))
    });
    if !ok {
        bail!(
            "the relay's address starts with https:// (like https://ferrule-relay.you.workers.dev)"
        );
    }
    Ok(url.to_string())
}

/// Uses a relay that's already deployed: checked end to end with `key`
/// first; only a working one is saved.
pub async fn use_relay(
    conns: &Connections,
    place: &Place,
    url: &str,
    key: &str,
) -> Result<(String, Vec<(String, bool)>)> {
    let url = relay_address(url)?;
    let key = key.trim();
    if key.is_empty() || key.chars().any(|c| c.is_whitespace() || c.is_control()) {
        bail!("the relay key is the RELAY_KEY the Worker was deployed with (no spaces)");
    }
    let r = Relay::new(&url, key);
    let steps = relay::check(&r).await;
    if let Some(step) = first_failure(&steps) {
        bail!(
            "nothing was saved: {step} failed at {url}. Check the address, and that the key is \
             the one this relay was deployed with"
        );
    }
    place.put(relay::RELAY_KEY_ENV, key)?;
    set_relay_url(&place.config, &url)?;
    conns.set_relay_url(Some(url.clone()));
    Ok((r.callback_url(), steps))
}

/// The relay's steps, checked now.
pub async fn check_relay(conns: &Connections, place: &Place) -> Result<Vec<(String, bool)>> {
    let url = conns
        .relay_url()
        .context("no relay is set up: deploy one, or use one you have")?;
    let key = place.get(relay::RELAY_KEY_ENV).with_context(|| {
        format!(
            "the relay's key ({}) isn't saved here",
            relay::RELAY_KEY_ENV
        )
    })?;
    Ok(relay::check(&Relay::new(&url, &key)).await)
}

/// Saves the owner's Google OAuth client (write-only). Says which
/// redirect URI Google must have.
pub fn save_google_client(
    conns: &Connections,
    place: &Place,
    id: &str,
    client_secret: &str,
) -> Result<String> {
    let (id, client_secret) = (id.trim(), client_secret.trim());
    if !id.ends_with(".apps.googleusercontent.com") || id.contains(char::is_whitespace) {
        bail!("the client id ends in .apps.googleusercontent.com (Google Cloud → Clients → your Web client)");
    }
    if client_secret.is_empty() || client_secret.contains(char::is_whitespace) {
        bail!("the client secret is the one shown next to the client id (no spaces)");
    }
    place.put(&format!("{GOOGLE_CLIENT}_ID"), id)?;
    place.put(&format!("{GOOGLE_CLIENT}_SECRET"), client_secret)?;
    Ok(match conns.relay_url() {
        Some(url) => format!(
            "Saved. Google's client must list {}/cb under Authorized redirect URIs.",
            url.trim_end_matches('/')
        ),
        None => "Saved. Set up the fixed callback address next, then add its /cb address to the \
                 client's Authorized redirect URIs."
            .into(),
    })
}

/// `[connections] relay_url = <url>`, comments and the rest kept.
pub fn set_relay_url(path: &Path, url: &str) -> Result<()> {
    let mut t = crate::setup::Target::load(path.to_path_buf())?;
    let root = t.root();
    if !root.contains_key("connections") {
        root.insert(
            "connections",
            toml_edit::Item::Table(toml_edit::Table::new()),
        );
    }
    let table = root
        .get_mut("connections")
        .and_then(|i| i.as_table_like_mut())
        .context("[connections] in the config isn't a table")?;
    table.insert("relay_url", toml_edit::value(url));
    t.save()
}

/// The OAuth services that come back through the relay's `/cb`, for the
/// card: which callback each uses.
pub fn callbacks(conns: &Connections) -> Vec<(String, String)> {
    conns
        .catalog()
        .services()
        .iter()
        .filter(|s| s.auth == AuthKind::Oauth)
        .map(|s| {
            let how = if s.fixed_callback {
                "the relay's /cb (required)"
            } else {
                "the relay's /cb, else a quick tunnel or paste-back"
            };
            (s.title().to_string(), how.to_string())
        })
        .collect()
}

// ---- the terminal ---------------------------------------------------------

/// The options on `tile` (or the one service named), simplest first.
pub fn options<'a>(conns: &'a Connections, what: &str) -> Vec<&'a Service> {
    let all = conns.catalog().services();
    let on_tile: Vec<&Service> = all.iter().filter(|s| s.tile() == what).collect();
    if !on_tile.is_empty() {
        return on_tile;
    }
    all.iter().filter(|s| s.name == what).collect()
}

/// `ferrule connections setup [service]`.
pub async fn run(
    conns: &Arc<Connections>,
    place: &Place,
    what: Option<&str>,
    write: bool,
    api: &str,
) -> Result<()> {
    let Some(what) = what else {
        return print_checklist(conns).await;
    };
    match what {
        "relay" => return relay_wizard(conns, place, api).await,
        "google-client" | "google_client" => return google_client_wizard(conns, place),
        _ => {}
    }
    let opts = options(conns, what);
    let service = match opts.as_slice() {
        [] => bail!(
            "`{what}` isn't a service or tile; `ferrule connections catalog` lists them, and \
             `ferrule connections setup relay` sets up the fixed callback address"
        ),
        [one] => (*one).clone(),
        many => {
            let labels: Vec<String> = many
                .iter()
                .map(|s| {
                    format!(
                        "{} — {}",
                        s.option.as_deref().unwrap_or(s.title()),
                        s.covers.as_deref().unwrap_or("")
                    )
                })
                .collect();
            let picked = inquire::Select::new("Which way in?", labels.clone())
                .prompt()
                .context("choosing an option")?;
            let i = labels.iter().position(|l| *l == picked).unwrap_or(0);
            many[i].clone()
        }
    };
    println!("{}", service.title());
    for (n, step) in service.guide.iter().enumerate() {
        println!("  {}. {step}", n + 1);
    }
    if service.auth == AuthKind::Oauth {
        let relay_live = conns.live_relay().await.is_some();
        if !conns.has_client(&service) && service.client_env.as_deref() == Some(GOOGLE_CLIENT) {
            if relay_live {
                google_client_wizard(conns, place)?;
            } else {
                println!("Google's own-app sign-in needs the fixed callback address first.");
                relay_wizard(conns, place, api).await?;
                google_client_wizard(conns, place)?;
            }
        } else if let Some(blocked) = conns.blocked(&service, relay_live) {
            println!("{}", blocked.text);
            if service.fixed_callback && !relay_live {
                let go = inquire::Confirm::new("Set up the fixed callback address now?")
                    .with_default(true)
                    .prompt()
                    .unwrap_or(false);
                if !go {
                    return Ok(());
                }
                relay_wizard(conns, place, api).await?;
            } else {
                return Ok(());
            }
        }
        return crate::connections::add(conns, &service.name, write).await;
    }
    connect_with_fields(conns, &service, write).await
}

/// Asks for `service`'s fields (secret ones hidden), then connects with
/// them: checked against the service before anything is saved.
pub async fn connect_with_fields(
    conns: &Connections,
    service: &Service,
    write: bool,
) -> Result<()> {
    let fields = prompt_fields(&service.key_fields())?;
    match conns
        .connect_key(&service.name, fields, write, "terminal")
        .await
    {
        Ok(msg) => {
            println!("{msg}");
            Ok(())
        }
        Err(why) => bail!("{why}"),
    }
}

fn prompt_fields(fields: &[Field]) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for f in fields {
        let label = format!(
            "{}{}:",
            f.label,
            if f.optional { " (optional)" } else { "" }
        );
        let value = if f.kind == "json" {
            let path = inquire::Text::new(&format!("{} — path to the file:", f.label))
                .prompt()
                .context("reading the path")?;
            let path = path.trim();
            std::fs::read_to_string(shellexpand_home(path))
                .with_context(|| format!("reading {path}"))?
        } else if f.secret {
            inquire::Password::new(&label)
                .without_confirmation()
                .prompt()
                .context("reading the value")?
        } else {
            let mut t = inquire::Text::new(&label);
            if let Some(h) = &f.hint {
                t = t.with_help_message(h);
            }
            if let Some(p) = &f.placeholder {
                t = t.with_placeholder(p);
            }
            t.prompt().context("reading the value")?
        };
        if !value.trim().is_empty() {
            out.insert(f.name.clone(), value);
        }
    }
    Ok(out)
}

fn shellexpand_home(path: &str) -> std::path::PathBuf {
    match (path.strip_prefix("~/"), dirs::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => std::path::PathBuf::from(path),
    }
}

async fn relay_wizard(conns: &Arc<Connections>, place: &Place, api: &str) -> Result<()> {
    if conns.live_relay().await.is_some() {
        println!(
            "The relay is set up and answering at {}.",
            conns.relay_url().unwrap_or_default()
        );
        return Ok(());
    }
    let have = inquire::Select::new(
        "Fixed callback address:",
        vec![
            "Deploy a relay to my Cloudflare account (free)",
            "Use a relay I already have",
        ],
    )
    .prompt()
    .context("choosing")?;
    if have.starts_with("Use") {
        let url = inquire::Text::new("Relay address:").prompt()?;
        let key = inquire::Password::new("Relay key:")
            .without_confirmation()
            .prompt()?;
        let (callback, _) = use_relay(conns, place, &url, &key).await?;
        println!("Working. The callback address is {callback}");
        return Ok(());
    }
    let token = match place.get(CF_TOKEN) {
        Some(_) => None,
        None => {
            println!(
                "Make a Cloudflare API token at {} with the \"Edit Cloudflare Workers\" template.",
                relay::CF_TOKEN_URL
            );
            Some(
                inquire::Password::new("Cloudflare API token:")
                    .without_confirmation()
                    .prompt()?,
            )
        }
    };
    let mut account: Option<String> = None;
    loop {
        match deploy_relay(
            conns,
            place,
            api,
            token.as_deref(),
            account.as_deref(),
            "ferrule-relay",
            5,
        )
        .await?
        {
            Deployed::Done { url, callback, .. } => {
                println!("The relay is at {url} and works. The callback address is {callback}");
                return Ok(());
            }
            Deployed::ChooseAccount { accounts } => {
                let labels: Vec<String> = accounts
                    .iter()
                    .map(|a| format!("{} ({})", a.name, a.id))
                    .collect();
                let picked =
                    inquire::Select::new("Which Cloudflare account?", labels.clone()).prompt()?;
                let i = labels.iter().position(|l| *l == picked).unwrap_or(0);
                account = Some(accounts[i].id.clone());
            }
        }
    }
}

fn google_client_wizard(conns: &Connections, place: &Place) -> Result<()> {
    if let Some(url) = conns.relay_url() {
        println!(
            "In your Web client, the Authorized redirect URI is {}/cb",
            url.trim_end_matches('/')
        );
    }
    let id = inquire::Text::new("Google client id:").prompt()?;
    let s = inquire::Password::new("Google client secret:")
        .without_confirmation()
        .prompt()?;
    println!("{}", save_google_client(conns, place, &id, &s)?);
    Ok(())
}

async fn print_checklist(conns: &Connections) -> Result<()> {
    let list = conns.checklist().await;
    for c in &list.checks {
        println!("{:<28} {:<16} {}", c.title, c.state, c.text);
        if let Some(a) = &c.action {
            let cli = match (a.action, a.service) {
                ("relay_setup" | "relay_use" | "relay_check", _) => {
                    "ferrule connections setup relay".to_string()
                }
                ("google_client", _) => "ferrule connections setup google-client".into(),
                ("connect", Some(s)) => format!("ferrule connections setup {s}"),
                ("cancel", _) => "(cancel it on the dashboard, or wait: it expires)".into(),
                _ => String::new(),
            };
            println!("{:<28} → {}: {cli}", "", a.label);
        }
    }
    if let Some(cb) = &list.callback {
        println!("\ncallback address: {cb}");
    }
    let snap = conns.snapshot()?;
    for f in &snap.pending_flows {
        println!(
            "pending: {} ({}s old, expires in {}s)",
            f.title, f.age_secs, f.expires_in
        );
    }
    let mut tiles: Vec<&str> = conns
        .catalog()
        .services()
        .iter()
        .map(|s| s.tile())
        .collect();
    tiles.dedup();
    println!(
        "\nset one up: ferrule connections setup <{}>",
        tiles.join("|")
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relay_address_is_https_and_tidied() {
        assert_eq!(
            relay_address(" https://r.you.workers.dev/cb/ ").unwrap(),
            "https://r.you.workers.dev"
        );
        assert!(relay_address("r.you.workers.dev").is_err());
        assert!(relay_address("http://r.you.workers.dev").is_err());
        assert!(relay_address("http://127.0.0.1:9").is_ok());
    }
}
