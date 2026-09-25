//! M20 connections in the binary: the one `Connections` service per
//! process, how it reaches the owner (Telegram buttons, else the terminal),
//! the servers it adds next to the config's, `/connect` and friends in the
//! gateway, the `/status` section, and `ferrule connections …`. The design
//! is `docs/m20-connections.md`.

use crate::{config, secrets, trust};
use anyhow::{anyhow, bail, Context, Result};
use clap::Subcommand;
use ferrule_connections::{relay, Action, Actor, Button, Chat, Connections, Events, State};
use ferrule_gateway::{Channel, InboundMessage, OutboundMessage, Router};
use ferrule_mcp::McpServerConfig;
use serde_json::Value;
use std::sync::{Arc, Mutex, OnceLock, Weak};

static SHARED: OnceLock<Option<Arc<Connections>>> = OnceLock::new();
/// The config's own servers, as last computed: connections are added to
/// them, so the config follower and the connections follower hand the
/// manager the same whole list.
static BASE: Mutex<Vec<McpServerConfig>> = Mutex::new(Vec::new());
static GATEWAY: Mutex<Option<Door>> = Mutex::new(None);

/// Where the gateway can reach the owner and the agent.
struct Door {
    telegram: Option<Arc<dyn Channel>>,
    owner: Option<i64>,
    router: Weak<Router>,
}

/// The process's service, made on first use. `None` when it can't be
/// (no data dir, a bad custom catalog entry): ferrule runs without it.
pub fn shared(cfg: &config::Config) -> Option<Arc<Connections>> {
    SHARED
        .get_or_init(|| match make(cfg) {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!("connections are off: {e:#}");
                None
            }
        })
        .clone()
}

fn make(cfg: &config::Config) -> Result<Arc<Connections>> {
    let data = config::data_dir()?;
    let state_dir = data.join("mcp").join("connections");
    std::fs::create_dir_all(&state_dir)?;
    let host = ferrule_mcp::ServerHost {
        sandbox: crate::shared_sandbox(cfg)?,
        workspace: std::env::current_dir().unwrap_or_else(|_| data.clone()),
        state_dir,
    };
    let conns = Connections::new(
        &secrets::private_dir()?,
        cfg.connections.clone(),
        Arc::new(crate::config_follow::secret_value),
        Arc::new(GatewayEvents),
        Some(host),
    )?;
    set_gate(cfg, &conns);
    Ok(conns)
}

/// M19's gate: a connected service's tools that change something wait for
/// the owner's yes, unless `[connections] gate_writes = false`.
fn set_gate(cfg: &config::Config, conns: &Connections) {
    let Ok(hub) = trust::hub(cfg) else { return };
    let names = if conns.config().gate_writes {
        conns.servers().into_iter().map(|s| s.name).collect()
    } else {
        Vec::new()
    };
    hub.set_connected(names);
}

/// The gateway's way to the owner and back into a chat.
pub fn attach(cfg: &config::Config, telegram: Option<Arc<dyn Channel>>, router: &Arc<Router>) {
    *GATEWAY.lock().unwrap() = Some(Door {
        telegram,
        owner: trust::owner_chat(cfg),
        router: Arc::downgrade(router),
    });
}

/// `base` (the config's servers) plus one per working connection. A
/// config server of the same name wins.
pub fn with_connections(cfg: &config::Config, base: Vec<McpServerConfig>) -> Vec<McpServerConfig> {
    *BASE.lock().unwrap() = base.clone();
    match shared(cfg) {
        Some(conns) => merge(base, conns.servers()),
        None => base,
    }
}

fn merge(mut base: Vec<McpServerConfig>, extra: Vec<McpServerConfig>) -> Vec<McpServerConfig> {
    for server in extra {
        if base.iter().any(|s| s.name == server.name) {
            tracing::warn!(
                "the config's `{}` server shadows the connection of that name",
                server.name
            );
            continue;
        }
        base.push(server);
    }
    base
}

/// Re-apply the servers whenever a connection comes, goes or breaks: the
/// tools appear in (or leave) running sessions without a restart (M17).
pub fn follow(manager: &Arc<ferrule_extensions::ExtensionManager>, cfg: &config::Config) {
    let Some(conns) = shared(cfg) else { return };
    let cfg = cfg.clone();
    let manager = Arc::downgrade(manager);
    let mut changed = conns.subscribe();
    tokio::spawn(async move {
        // Our own changes bump `changed`; another process's show up as a
        // new stamp on the store file, looked at every 2 s.
        let mut stamp = conns.store_stamp();
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
        loop {
            tokio::select! {
                got = changed.changed() => if got.is_err() { return },
                _ = tick.tick() => {
                    let now = conns.store_stamp();
                    if now == stamp {
                        continue;
                    }
                }
            }
            stamp = conns.store_stamp();
            let Some(manager) = manager.upgrade() else {
                return;
            };
            set_gate(&cfg, &conns);
            let base = BASE.lock().unwrap().clone();
            for change in manager.set_configured(merge(base, conns.servers())).await {
                tracing::info!("connections: mcp server {change}");
            }
        }
    });
}

/// The agent's `connection_request` and `connection_list`, for a session
/// id like `telegram__<chat>`.
pub fn tools(cfg: &config::Config, session: &str) -> Vec<Arc<dyn ferrule_core::Tool>> {
    let Some(conns) = shared(cfg) else {
        return Vec::new();
    };
    ferrule_connections::tools::tools(&conns, chat_of(session))
}

fn chat_of(session: &str) -> Chat {
    match session.split_once("__") {
        Some((channel, id)) if !channel.is_empty() && !id.is_empty() => Chat {
            channel: channel.into(),
            id: id.into(),
        },
        _ => Chat {
            channel: "terminal".into(),
            id: session.into(),
        },
    }
}

fn gateway_button(b: Button) -> ferrule_gateway::Button {
    ferrule_gateway::Button {
        text: b.text,
        action: match b.action {
            Action::Url(u) => ferrule_gateway::ButtonAction::Url(u),
            Action::Command(c) => ferrule_gateway::ButtonAction::Command(c),
        },
    }
}

/// A button as a line at the terminal: a chat command becomes the CLI's.
fn terminal_line(b: &Button) -> String {
    match &b.action {
        Action::Url(u) => format!("  {}: {u}", b.text),
        Action::Command(c) => {
            let words: Vec<&str> = c.split_whitespace().collect();
            let cli = match words.as_slice() {
                ["/connect", what] => format!("ferrule connections add {what}"),
                ["/connect", what, "write"] => format!("ferrule connections add {what} --write"),
                ["/disconnect", what] => format!("ferrule connections remove {what}"),
                _ => c.clone(),
            };
            format!("  {}: {cli}", b.text)
        }
    }
}

struct GatewayEvents;

#[async_trait::async_trait]
impl Events for GatewayEvents {
    async fn tell_owner(&self, text: &str, buttons: Vec<Button>) {
        let route = {
            let door = GATEWAY.lock().unwrap();
            door.as_ref()
                .and_then(|d| Some((d.telegram.clone()?, d.owner?)))
        };
        if let Some((telegram, owner)) = route {
            let msg = OutboundMessage {
                channel: telegram.name().to_string(),
                chat_id: owner.to_string(),
                text: text.to_string(),
                reply_to: None,
                attachments: vec![],
            };
            let out: Vec<_> = buttons.iter().cloned().map(gateway_button).collect();
            if ferrule_gateway::send_with_buttons(telegram.as_ref(), msg, &out)
                .await
                .is_ok()
            {
                return;
            }
        }
        eprintln!("ferrule: {text}");
        for b in &buttons {
            eprintln!("{}", terminal_line(b));
        }
    }

    fn audit(&self, event: &str, detail: Value) {
        if let Some(hub) = trust::existing_hub() {
            hub.audit()
                .record(chrono::Utc::now(), event, None, None, detail);
        }
    }

    async fn connected(&self, name: &str, chat: Option<&Chat>) {
        let Some(chat) = chat else { return };
        let router = GATEWAY
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|d| d.router.upgrade());
        let Some(router) = router else { return };
        let session = format!("{}__{}", chat.channel, chat.id);
        if router.can_wake(&session) {
            router.wake(
                &session,
                format!(
                    "[ferrule] `{name}` is connected now, as you asked; its tools are available. \
                     Carry on with what you needed it for."
                ),
            );
        }
    }
}

/// `/connect`, `/connections`, `/disconnect`, `/decline` and pasted
/// redirects, before any chat turn. Only the owner's Telegram chat (or the
/// gateway's own console) manages connections; a button tap is no more
/// trusted than typing its command.
pub struct ConnectionsDoor {
    pub conns: Arc<Connections>,
    pub owner: Option<i64>,
}

impl ConnectionsDoor {
    fn actor(&self, msg: &InboundMessage) -> Actor {
        let chat = Chat {
            channel: msg.channel.clone(),
            id: msg.chat_id.clone(),
        };
        match msg.channel.as_str() {
            "telegram" if self.owner.is_some_and(|o| o.to_string() == msg.chat_id) => {
                Actor::Owner(chat)
            }
            // The gateway's stdin: someone at the server itself.
            "local" => Actor::Terminal,
            _ => Actor::Other(chat),
        }
    }
}

#[async_trait::async_trait]
impl ferrule_gateway::Interceptor for ConnectionsDoor {
    async fn intercept(&self, _msg: &InboundMessage) -> Option<String> {
        None
    }

    async fn intercept_with_buttons(
        &self,
        msg: &InboundMessage,
    ) -> Option<(String, Vec<ferrule_gateway::Button>)> {
        let reply = self.conns.intercept(&self.actor(msg), &msg.text).await?;
        Some((
            reply.text,
            reply.buttons.into_iter().map(gateway_button).collect(),
        ))
    }
}

/// `/status`'s connections: names, state and age, never a secret.
pub fn status_lines(conns: &Connections) -> Vec<String> {
    let Ok(snap) = conns.snapshot() else {
        return vec!["the connections store doesn't read".into()];
    };
    let mut lines: Vec<String> = snap
        .connections
        .iter()
        .map(|s| {
            let state = match s.state {
                State::Connected => "connected",
                State::NeedsReconnect => "needs reconnecting",
            };
            let tools = s.tools.map(|n| format!(", {n} tools")).unwrap_or_default();
            let access = if s.write { "read-write" } else { "read-only" };
            format!("{}: {state}, {access}{tools}, via {}", s.name, s.via)
        })
        .collect();
    if !snap.pending.is_empty() {
        lines.push(format!("waiting on a login: {}", snap.pending.join(", ")));
    }
    if !snap.asked.is_empty() {
        lines.push(format!("asked for: {}", snap.asked.join(", ")));
    }
    if lines.is_empty() {
        lines.push("none".into());
    }
    lines.push(match &snap.relay {
        Some(_) => "relay: configured".into(),
        None => "relay: none (quick tunnel or paste-back)".into(),
    });
    lines
}

/// `ferrule doctor`'s line.
pub fn doctor_line(cfg: &config::Config) -> String {
    let store = match secrets::private_dir() {
        Ok(p) => ferrule_connections::Store::new(&p),
        Err(e) => return format!("unavailable: {e:#}"),
    };
    let n = match store.load() {
        Ok(r) => r.len(),
        Err(e) => return format!("the store doesn't read: {e:#}"),
    };
    let relay = match &cfg.connections.relay_url {
        Some(url) if crate::config_follow::secret_value(relay::RELAY_KEY_ENV).is_some() => {
            format!("relay {url}")
        }
        Some(_) => format!("relay set but {} is missing", relay::RELAY_KEY_ENV),
        None => "no relay".into(),
    };
    format!("{n} connected, {relay}")
}

#[derive(Subcommand)]
pub enum ConnectionsCmd {
    /// What's connected
    List,
    /// Connect a service from the catalog (or an https:// MCP URL): prints
    /// the login link and waits; an API key is typed here, hidden
    Add {
        service: String,
        /// Ask for write access too (read-only by default)
        #[arg(long)]
        write: bool,
    },
    /// Disconnect: revoke where the service allows, delete the token
    Remove { name: String },
    /// The catalog of services
    Catalog,
    /// The relay the login codes and key forms come back through
    Relay {
        #[command(subcommand)]
        op: RelayCmd,
    },
}

#[derive(Subcommand)]
pub enum RelayCmd {
    /// Deploy (or update) the relay Worker to your Cloudflare account and
    /// set `[connections] relay_url`. Reads CLOUDFLARE_API_TOKEN and
    /// CLOUDFLARE_ACCOUNT_ID from the environment or the secrets file
    Deploy {
        /// The Worker's name
        #[arg(long, default_value = "ferrule-relay")]
        name: String,
        /// Cloudflare's API (tests point it elsewhere)
        #[arg(
            long,
            hide = true,
            default_value = "https://api.cloudflare.com/client/v4"
        )]
        api: String,
    },
    /// Check the relay end to end: health, a slot, a write, one read
    Check,
}

pub async fn run(op: ConnectionsCmd) -> Result<()> {
    let (cfg, path) = config::Config::load()?;
    if let ConnectionsCmd::Relay { op } = op {
        return relay_cmd(op, &cfg, &path).await;
    }
    let conns = shared(&cfg).ok_or_else(|| anyhow!("connections are unavailable here"))?;
    match op {
        ConnectionsCmd::List => {
            for line in status_lines(&conns) {
                println!("{line}");
            }
        }
        ConnectionsCmd::Catalog => {
            for s in conns.catalog().services() {
                println!("{:<12} {} ({:?})", s.name, s.title(), s.auth);
            }
        }
        ConnectionsCmd::Remove { name } => println!("{}", conns.disconnect(&name).await?),
        ConnectionsCmd::Add { service, write } => add(&conns, &service, write).await?,
        ConnectionsCmd::Relay { .. } => unreachable!(),
    }
    Ok(())
}

async fn add(conns: &Arc<Connections>, what: &str, write: bool) -> Result<()> {
    let service = conns.catalog().resolve(what)?;
    if service.auth == ferrule_connections::AuthKind::ApiKey {
        let key = inquire::Password::new(&format!("{} API key:", service.title()))
            .without_confirmation()
            .prompt()
            .context("reading the key")?;
        println!("{}", conns.add_key(what, &key).await?);
        return Ok(());
    }
    let started = conns.start(&Actor::Terminal, what, write).await?;
    println!("{}", started.reply.text);
    for b in &started.reply.buttons {
        println!("{}", terminal_line(b));
    }
    // A paste-back flow takes the address the browser ended on, here.
    let paste = {
        let conns = conns.clone();
        tokio::spawn(async move {
            let lines =
                tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(tokio::io::stdin()));
            let mut lines = lines;
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(reply) = conns.intercept(&Actor::Terminal, &line).await {
                    println!("{}", reply.text);
                }
            }
        })
    };
    let outcome = started.done.await;
    paste.abort();
    match outcome {
        Ok(Ok(msg)) => {
            println!("{msg}");
            Ok(())
        }
        Ok(Err(why)) => bail!("{why}"),
        Err(_) => bail!("the flow ended without an answer"),
    }
}

async fn relay_cmd(op: RelayCmd, cfg: &config::Config, path: &std::path::Path) -> Result<()> {
    let secret = crate::config_follow::secret_value;
    match op {
        RelayCmd::Check => {
            let url =
                cfg.connections.relay_url.as_deref().context(
                    "no [connections] relay_url: run `ferrule connections relay deploy`",
                )?;
            let key = secret(relay::RELAY_KEY_ENV)
                .with_context(|| format!("{} isn't in the secrets file", relay::RELAY_KEY_ENV))?;
            let steps = relay::check(&relay::Relay::new(url, &key)).await;
            let ok = steps.iter().all(|(_, ok)| *ok);
            for (step, passed) in steps {
                println!("{} {step}", if passed { "ok  " } else { "FAIL" });
            }
            if !ok {
                bail!("the relay at {url} isn't working");
            }
            println!("the relay at {url} works");
        }
        RelayCmd::Deploy { name, api } => {
            let token = secret("CLOUDFLARE_API_TOKEN")
                .context("CLOUDFLARE_API_TOKEN isn't set (a token with Workers Scripts: Edit)")?;
            let account =
                secret("CLOUDFLARE_ACCOUNT_ID").context("CLOUDFLARE_ACCOUNT_ID isn't set")?;
            // One relay key per install, kept across deploys.
            let relay_key = match secret(relay::RELAY_KEY_ENV) {
                Some(k) => k,
                None => {
                    let k =
                        ferrule_connections::seal::b64(&ferrule_connections::seal::random::<32>());
                    secrets::set(&secrets::path()?, relay::RELAY_KEY_ENV, &k)?;
                    k
                }
            };
            let url = relay::deploy(&relay::Deploy {
                api: &api,
                token: &token,
                account: &account,
                name: &name,
                relay_key: &relay_key,
            })
            .await?;
            set_relay_url(path, &url)?;
            println!(
                "the relay is at {url}; {} has relay_url set",
                path.display()
            );
        }
    }
    Ok(())
}

/// `[connections] relay_url = <url>`, comments and the rest kept.
fn set_relay_url(path: &std::path::Path, url: &str) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_id_names_its_chat() {
        assert_eq!(
            chat_of("telegram__42"),
            Chat {
                channel: "telegram".into(),
                id: "42".into()
            }
        );
        assert_eq!(chat_of("chat-2026").channel, "terminal");
    }

    #[test]
    fn the_config_wins_a_name_clash() {
        let server = |name: &str, url: &str| -> McpServerConfig {
            serde_json::from_value(serde_json::json!({"name": name, "url": url})).unwrap()
        };
        let merged = merge(
            vec![server("notion", "https://mine.example/mcp")],
            vec![
                server("notion", "https://mcp.notion.com/mcp"),
                server("linear", "https://mcp.linear.app/mcp"),
            ],
        );
        let names: Vec<_> = merged
            .iter()
            .map(|s| (s.name.as_str(), s.url.as_deref().unwrap()))
            .collect();
        assert_eq!(
            names,
            [
                ("notion", "https://mine.example/mcp"),
                ("linear", "https://mcp.linear.app/mcp")
            ]
        );
    }

    #[test]
    fn a_chat_command_becomes_the_cli_one_at_the_terminal() {
        let b = |c: &str| Button {
            text: "Connect".into(),
            action: Action::Command(c.into()),
        };
        assert!(terminal_line(&b("/connect notion write"))
            .ends_with("ferrule connections add notion --write"));
        assert!(
            terminal_line(&b("/disconnect notion")).ends_with("ferrule connections remove notion")
        );
    }

    fn inbound(channel: &str, chat: &str, text: &str) -> InboundMessage {
        InboundMessage {
            channel: channel.into(),
            chat_id: chat.into(),
            sender: "someone".into(),
            message_id: String::new(),
            text: text.into(),
            attachments: vec![],
            reply_to: None,
            ts: 0,
        }
    }

    /// Only the owner's Telegram chat manages connections; a tap on a
    /// button in another chat is refused like the typed command; anything
    /// else goes on to the agent.
    #[tokio::test]
    async fn only_the_owners_chat_manages_connections() {
        use ferrule_gateway::Interceptor;
        let dir = tempfile::tempdir().unwrap();
        let conns = Connections::new(
            dir.path(),
            Default::default(),
            Arc::new(|_: &str| None),
            Arc::new(ferrule_connections::service::Quiet),
            None,
        )
        .unwrap();
        let door = ConnectionsDoor {
            conns,
            owner: Some(42),
        };
        let (text, _) = door
            .intercept_with_buttons(&inbound("telegram", "42", "/connections"))
            .await
            .unwrap();
        assert!(!text.contains("Only the owner"), "{text}");
        for cmd in ["/connect notion", "/connections", "/disconnect notion"] {
            let (text, buttons) = door
                .intercept_with_buttons(&inbound("telegram", "7", cmd))
                .await
                .unwrap();
            assert_eq!(text, "Only the owner can manage connections.");
            assert!(buttons.is_empty());
        }
        assert!(door
            .intercept_with_buttons(&inbound("telegram", "42", "hello"))
            .await
            .is_none());
    }

    /// Eval runs its tasks on its own harness: no MCP, no connections, no
    /// secrets. Its crate can't even name them.
    #[test]
    fn eval_never_touches_connections() {
        let manifest = include_str!("../../ferrule-eval/Cargo.toml");
        for dep in ["ferrule-connections", "ferrule-mcp", "ferrule-extensions"] {
            assert!(!manifest.contains(dep), "ferrule-eval depends on {dep}");
        }
        let cli_eval = include_str!("eval.rs");
        for name in ["connections", "mcp_servers", "self_extend"] {
            assert!(!cli_eval.contains(name), "eval.rs reaches {name}");
        }
    }
}
