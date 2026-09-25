//! The connections service: one per gateway (and one per `ferrule
//! connections` command). The agent can only ask; the owner starts a flow
//! with a button or a command; the flow ends with a sealed token in the
//! store and the service's tools live, or with a fixed-phrase error.
//!
//! Who's who is decided by the caller ([`Actor`]): the gateway knows the
//! owner's chat, the agent's tools know their session.

use crate::catalog::{AuthKind, Catalog, ClientKind, Service};
use crate::config::ConnectionsConfig;
use crate::credential::{header_for, Broken, StoreCredential};
use crate::keyform::KeyForm;
use crate::oauth::{self, Client, Endpoints, Pkce};
use crate::relay::{Poll, Relay, Slot, RELAY_KEY_ENV};
use crate::seal::{b64, random};
use crate::store::{now, OauthMeta, Record, Secret, State, Store};
use crate::tunnel;
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use ferrule_mcp::{Auth, McpClient, McpServerConfig, ServerHost};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{oneshot, watch};

/// A chat on some channel (`telegram`, `local`, …).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Chat {
    pub channel: String,
    pub id: String,
}

/// Who is asking.
#[derive(Debug, Clone, PartialEq)]
pub enum Actor {
    /// The owner, in their chat.
    Owner(Chat),
    /// The agent, from a session in this chat.
    Agent(Chat),
    /// Someone at ferrule's own terminal (`ferrule connections add`).
    Terminal,
    /// Anyone else.
    Other(Chat),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Opens in the browser.
    Url(String),
    /// Sent back as if the owner typed it (a Telegram callback).
    Command(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Button {
    pub text: String,
    pub action: Action,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Reply {
    pub text: String,
    pub buttons: Vec<Button>,
}

impl Reply {
    fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            buttons: Vec::new(),
        }
    }
}

/// What the service tells the world. The gateway's version sends to the
/// owner's Telegram chat, writes M19's audit log and wakes the agent.
#[async_trait]
pub trait Events: Send + Sync {
    async fn tell_owner(&self, text: &str, buttons: Vec<Button>);
    /// Never given a secret.
    fn audit(&self, event: &str, detail: Value);
    /// `name` is connected and its tools are coming; `chat` is where the
    /// agent asked for it, if it did.
    async fn connected(&self, name: &str, chat: Option<&Chat>);
}

/// Events that go nowhere (the terminal's commands print instead).
pub struct Quiet;

#[async_trait]
impl Events for Quiet {
    async fn tell_owner(&self, _: &str, _: Vec<Button>) {}
    fn audit(&self, _: &str, _: Value) {}
    async fn connected(&self, _: &str, _: Option<&Chat>) {}
}

pub type Secrets = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// An agent's request waiting for the owner's tap.
struct Ask {
    write: bool,
    chat: Chat,
}

enum Kind {
    Oauth {
        endpoints: Endpoints,
        client: Client,
        verifier: String,
        redirect: String,
    },
    Key {
        /// Taken when the envelope arrives: one use.
        form: Option<KeyForm>,
        slot: String,
    },
}

struct Flow {
    service: Service,
    write: bool,
    requested_by: &'static str,
    ask_chat: Option<Chat>,
    via: &'static str,
    kind: Kind,
    done: Option<oneshot::Sender<Result<String, String>>>,
    driver: Option<tokio::task::AbortHandle>,
}

#[derive(Default)]
struct Inner {
    /// Keyed by `state` (the relay slot's id for relay flows).
    flows: HashMap<String, Flow>,
    asks: HashMap<String, Ask>,
    declined: HashMap<String, Instant>,
}

/// A flow that started: what to show, and its outcome (the terminal waits
/// on it; the gateway doesn't).
pub struct Started {
    pub reply: Reply,
    pub done: oneshot::Receiver<Result<String, String>>,
}

/// One connection, for `/connections`, `/status` and M22. No secrets.
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub name: String,
    pub title: String,
    pub state: State,
    pub write: bool,
    pub via: String,
    pub requested_by: String,
    pub tools: Option<usize>,
    pub connected_at: u64,
    pub expires_at: Option<u64>,
    pub refreshed_at: Option<u64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub connections: Vec<Status>,
    /// Services with a flow waiting on the owner's browser.
    pub pending: Vec<String>,
    /// Services the agent asked for, waiting on the owner's tap.
    pub asked: Vec<String>,
    pub relay: Option<String>,
}

const DECLINE_QUIET: Duration = Duration::from_secs(600);

pub struct Connections {
    store: Arc<Store>,
    cfg: ConnectionsConfig,
    catalog: Catalog,
    secrets: Secrets,
    http: reqwest::Client,
    events: Arc<dyn Events>,
    host: Option<ServerHost>,
    inner: Mutex<Inner>,
    changed: Arc<watch::Sender<u64>>,
    creds: Mutex<HashMap<String, Auth>>,
    /// How long a started flow waits for the owner.
    pub flow_ttl: Duration,
    /// How often a relay slot is polled.
    pub poll_every: Duration,
}

impl Connections {
    /// `private` is ferrule's private dir; `secrets` looks a name up in the
    /// environment and the secrets file; `host` is where MCP servers run,
    /// to count a new connection's tools (none: not counted).
    pub fn new(
        private: &std::path::Path,
        cfg: ConnectionsConfig,
        secrets: Secrets,
        events: Arc<dyn Events>,
        host: Option<ServerHost>,
    ) -> Result<Arc<Self>> {
        let catalog = Catalog::with_custom(&cfg.custom)?;
        Ok(Arc::new(Self {
            store: Arc::new(Store::new(private)),
            cfg,
            catalog,
            secrets,
            http: oauth::http_client(),
            events,
            host,
            inner: Mutex::new(Inner::default()),
            changed: Arc::new(watch::channel(0).0),
            creds: Mutex::new(HashMap::new()),
            flow_ttl: Duration::from_secs(900),
            poll_every: Duration::from_secs(2),
        }))
    }

    /// Tests shorten the waits.
    pub fn with_timing(mut self: Arc<Self>, ttl: Duration, poll: Duration) -> Arc<Self> {
        let me = Arc::get_mut(&mut self).expect("set timing before sharing");
        me.flow_ttl = ttl;
        me.poll_every = poll;
        self
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    pub fn config(&self) -> &ConnectionsConfig {
        &self.cfg
    }

    /// Bumped whenever the set of connected servers changes.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    fn bump(&self) {
        self.changed.send_modify(|n| *n += 1);
    }

    fn relay(&self) -> Option<Relay> {
        let url = self.cfg.relay_url.as_deref()?;
        let key = (self.secrets)(RELAY_KEY_ENV)?;
        Some(Relay::new(url, &key))
    }

    fn cloudflared(&self) -> Option<PathBuf> {
        match self.cfg.cloudflared.as_deref() {
            Some("off") => None,
            Some(path) => Some(PathBuf::from(path)),
            None => ferrule_mcp::browser::find_command("cloudflared"),
        }
    }

    // ---- the agent -----------------------------------------------------

    /// The agent's `connection_request`: asks the owner, never connects.
    pub async fn request(
        &self,
        chat: &Chat,
        what: &str,
        write: bool,
        reason: &str,
    ) -> Result<String> {
        let service = self.catalog.resolve(what)?;
        let name = service.name.clone();
        if let Some(r) = self.store.load()?.iter().find(|r| r.name == name) {
            if r.state == State::Connected && (r.write || !write) {
                return Ok(format!(
                    "{} is already connected; its tools are `mcp__{name}__*`.",
                    service.title()
                ));
            }
        }
        {
            let mut inner = self.inner.lock().unwrap();
            inner.declined.retain(|_, at| at.elapsed() < DECLINE_QUIET);
            if inner.declined.contains_key(&name) {
                return Ok(format!(
                    "The owner declined {} a few minutes ago; don't ask again now.",
                    service.title()
                ));
            }
            if inner.asks.contains_key(&name)
                || inner.flows.values().any(|f| f.service.name == name)
            {
                return Ok(format!(
                    "The owner has already been asked to connect {}; waiting for them.",
                    service.title()
                ));
            }
            inner.asks.insert(
                name.clone(),
                Ask {
                    write,
                    chat: chat.clone(),
                },
            );
        }
        let reason: String = reason
            .chars()
            .filter(|c| !c.is_control())
            .take(300)
            .collect();
        let command = format!("/connect {name}{}", if write { " write" } else { "" });
        let mut text = format!(
            "The agent asks to connect {}{}.\n{}",
            service.title(),
            if write { " with write access" } else { "" },
            service.read_only_story(write),
        );
        let scopes = service.scopes(write);
        if !scopes.is_empty() {
            text.push_str(&format!("\nScopes: {}", scopes.join(", ")));
        }
        if !reason.trim().is_empty() {
            text.push_str(&format!("\nIts reason: \"{}\"", reason.trim()));
        }
        if let Some(note) = &service.note {
            text.push_str(&format!("\n{note}"));
        }
        self.events
            .tell_owner(
                &text,
                vec![
                    Button {
                        text: format!("Connect {}", service.title()),
                        action: Action::Command(command),
                    },
                    Button {
                        text: "Decline".into(),
                        action: Action::Command(format!("/decline {name}")),
                    },
                ],
            )
            .await;
        self.events.audit(
            "connection_requested",
            json!({"service": name, "write": write, "chat": chat}),
        );
        Ok(format!(
            "Asked the owner to connect {}. Nothing is connected until they approve; \
             when they do, its tools (`mcp__{name}__*`) appear and you'll be told.",
            service.title()
        ))
    }

    // ---- commands ------------------------------------------------------

    /// A chat message: a command, or a pasted redirect. `None`: not ours,
    /// pass it on to the agent.
    pub async fn intercept(self: &Arc<Self>, actor: &Actor, text: &str) -> Option<Reply> {
        let trimmed = text.trim();
        let mut words = trimmed.split_whitespace();
        let first = words.next().unwrap_or("");
        let command = first.split('@').next().unwrap_or("");
        let args: Vec<&str> = words.collect();
        let ours = matches!(
            command,
            "/connect" | "/connections" | "/disconnect" | "/decline"
        );
        if ours {
            let chat = match actor {
                Actor::Owner(c) => Some(c.clone()),
                Actor::Terminal => None,
                Actor::Agent(_) | Actor::Other(_) => {
                    self.events.audit(
                        "connection_refused",
                        json!({"command": command, "why": "not the owner"}),
                    );
                    return Some(Reply::text("Only the owner can manage connections."));
                }
            };
            return Some(match command {
                "/connections" => self.list_reply(),
                "/connect" => match args.first() {
                    None => Reply::text(format!(
                        "/connect <service> [write]: one of {}, or an https:// MCP URL.",
                        self.catalog.names().join(", ")
                    )),
                    Some(what) => {
                        let write = args.get(1).is_some_and(|w| *w == "write");
                        match self.start(actor, what, write).await {
                            Ok(s) => s.reply,
                            Err(e) => Reply::text(format!("Couldn't start: {e}")),
                        }
                    }
                },
                "/disconnect" => match args.first() {
                    None => Reply::text("/disconnect <name>"),
                    Some(name) => match self.disconnect(name).await {
                        Ok(text) => Reply::text(text),
                        Err(e) => Reply::text(format!("Couldn't disconnect: {e}")),
                    },
                },
                _ => match args.first() {
                    None => Reply::text("/decline <service>"),
                    Some(name) => self.decline(name, chat),
                },
            });
        }
        let pasted = crate::paste::parse(trimmed)?;
        Some(self.pasted(actor, pasted).await)
    }

    fn decline(&self, name: &str, _chat: Option<Chat>) -> Reply {
        let name = name.to_ascii_lowercase();
        let mut inner = self.inner.lock().unwrap();
        let asked = inner.asks.remove(&name).is_some();
        inner.declined.insert(name.clone(), Instant::now());
        drop(inner);
        self.events
            .audit("connection_declined", json!({"service": name}));
        Reply::text(if asked {
            format!("Declined; the agent won't ask for {name} again for 10 minutes.")
        } else {
            format!("Nothing was waiting for {name}; it won't be asked for 10 minutes.")
        })
    }

    fn list_reply(&self) -> Reply {
        let snap = match self.snapshot() {
            Ok(s) => s,
            Err(e) => return Reply::text(format!("The connections store: {e}")),
        };
        if snap.connections.is_empty() && snap.pending.is_empty() && snap.asked.is_empty() {
            return Reply::text(format!(
                "No connections. /connect <service>: {}.",
                self.catalog.names().join(", ")
            ));
        }
        let mut lines = Vec::new();
        for c in &snap.connections {
            let state = match c.state {
                State::Connected => "connected".to_string(),
                State::NeedsReconnect => format!(
                    "needs reconnecting ({}): tools suspended",
                    c.last_error.as_deref().unwrap_or("the grant is gone")
                ),
            };
            let tools = c.tools.map(|n| format!(", {n} tools")).unwrap_or_default();
            lines.push(format!(
                "• {} — {state}, {}{tools}, via {}",
                c.name,
                if c.write { "read-write" } else { "read-only" },
                c.via
            ));
        }
        for p in &snap.pending {
            lines.push(format!("• {p} — waiting for you to finish signing in"));
        }
        for a in &snap.asked {
            lines.push(format!(
                "• {a} — the agent asked; /connect {a} or /decline {a}"
            ));
        }
        Reply::text(lines.join("\n"))
    }

    /// Every connection and flow, for `/status` and M22.
    pub fn snapshot(&self) -> Result<Snapshot> {
        let connections = self
            .store
            .load()?
            .into_iter()
            .map(|r| Status {
                title: r.service.title().to_string(),
                name: r.name,
                state: r.state,
                write: r.write,
                via: r.via,
                requested_by: r.requested_by,
                tools: r.tools,
                connected_at: r.connected_at,
                expires_at: r.expires_at,
                refreshed_at: r.refreshed_at,
                last_error: r.last_error,
            })
            .collect();
        let inner = self.inner.lock().unwrap();
        let mut pending: Vec<String> = inner
            .flows
            .values()
            .map(|f| f.service.name.clone())
            .collect();
        pending.sort();
        let mut asked: Vec<String> = inner.asks.keys().cloned().collect();
        asked.sort();
        Ok(Snapshot {
            connections,
            pending,
            asked,
            relay: self.cfg.relay_url.clone(),
        })
    }

    // ---- flows ---------------------------------------------------------

    /// Start connecting `what`. Only the owner (or the terminal) can.
    pub async fn start(
        self: &Arc<Self>,
        actor: &Actor,
        what: &str,
        write: bool,
    ) -> Result<Started> {
        let owner_chat = match actor {
            Actor::Owner(c) => Some(c.clone()),
            Actor::Terminal => None,
            _ => bail!("only the owner can connect a service"),
        };
        let service = self.catalog.resolve(what)?;
        let name = service.name.clone();
        let (ask_chat, requested_by, write) = {
            let mut inner = self.inner.lock().unwrap();
            // A new flow replaces an older one for the same service.
            let old: Vec<String> = inner
                .flows
                .iter()
                .filter(|(_, f)| f.service.name == name)
                .map(|(s, _)| s.clone())
                .collect();
            for state in old {
                if let Some(mut f) = inner.flows.remove(&state) {
                    if let Some(d) = f.driver.take() {
                        d.abort();
                    }
                }
            }
            inner.declined.remove(&name);
            match inner.asks.remove(&name) {
                Some(ask) => (Some(ask.chat), "agent", write || ask.write),
                None if owner_chat.is_some() => (None, "owner", write),
                None => (None, "terminal", write),
            }
        };
        let relay = match self.relay() {
            Some(r) if r.healthy().await => Some(r),
            Some(_) => {
                tracing::warn!("the relay isn't answering; trying the fallbacks");
                None
            }
            None => None,
        };
        let (done_tx, done_rx) = oneshot::channel();
        let title = service.title().to_string();

        if service.auth == AuthKind::ApiKey {
            let Some(relay) = relay else {
                bail!(
                    "{title} takes an API key, and without a relay the only safe way in is \
                     the terminal: `ferrule connections add {name}`"
                );
            };
            let slot = Slot::new();
            let form = KeyForm::new()?;
            let link = relay.key_form_url(&slot, &form.public, &title);
            let state = slot.id.clone();
            self.insert(
                state.clone(),
                Flow {
                    service: service.clone(),
                    write,
                    requested_by,
                    ask_chat,
                    via: "relay",
                    kind: Kind::Key {
                        form: Some(form),
                        slot: state.clone(),
                    },
                    done: Some(done_tx),
                    driver: None,
                },
            );
            self.drive_relay(relay, slot);
            self.events.audit(
                "connection_started",
                json!({"service": name, "via": "relay", "kind": "api_key"}),
            );
            let mut text = format!(
                "Paste your {title} key into this one-time form. It's encrypted in your \
                 browser; the chat and the agent never see it. The link works for {} minutes.",
                self.flow_ttl.as_secs() / 60
            );
            if let Some(url) = &service.key_url {
                text.push_str(&format!("\nMake a key at {url}"));
            }
            return Ok(Started {
                reply: Reply {
                    text,
                    buttons: vec![Button {
                        text: format!("{title} key form"),
                        action: Action::Url(link),
                    }],
                },
                done: done_rx,
            });
        }

        // OAuth: pick the callback path.
        let endpoints = oauth::discover(&self.http, &service).await?;
        let mut tunnel_parts = None;
        let (via, redirect, state) = if let Some(relay) = &relay {
            let slot = Slot::new();
            let state = slot.id.clone();
            tunnel_parts = Some(Path::Relay(relay.clone(), slot));
            ("relay", relay.callback_url(), state)
        } else {
            let tunnel = match (service.client, self.cloudflared()) {
                (ClientKind::Dcr, Some(bin)) => match tunnel::listen(0).await {
                    Ok(listener) => match tunnel::open(&bin, listener.port).await {
                        Ok(t) => Some((listener, t)),
                        Err(e) => {
                            tracing::warn!("quick tunnel: {e:#}");
                            None
                        }
                    },
                    Err(e) => {
                        tracing::warn!("loopback listener: {e:#}");
                        None
                    }
                },
                _ => None,
            };
            let state = b64(&random::<32>());
            match tunnel {
                Some((listener, t)) => {
                    let redirect = format!("{}/callback", t.url);
                    tunnel_parts = Some(Path::Tunnel(listener, t));
                    ("tunnel", redirect, state)
                }
                None => ("paste", self.cfg.loopback_redirect.clone(), state),
            }
        };
        let client =
            oauth::client(&self.http, &service, &endpoints, &redirect, &*self.secrets).await?;
        let pkce = Pkce::new();
        let scopes = service.scopes(write).to_vec();
        let url = oauth::authorize_url(&oauth::AuthorizeRequest {
            service: &service,
            endpoints: &endpoints,
            client: &client,
            redirect: &redirect,
            scopes: &scopes,
            state: &state,
            pkce: &pkce,
            write,
        })?;
        self.insert(
            state.clone(),
            Flow {
                service: service.clone(),
                write,
                requested_by,
                ask_chat,
                via,
                kind: Kind::Oauth {
                    endpoints,
                    client,
                    verifier: pkce.verifier,
                    redirect,
                },
                done: Some(done_tx),
                driver: None,
            },
        );
        match tunnel_parts {
            Some(Path::Relay(relay, slot)) => self.drive_relay(relay, slot),
            Some(Path::Tunnel(listener, t)) => self.drive_tunnel(state.clone(), listener, t),
            None => self.drive_expiry(state.clone()),
        }
        self.events.audit(
            "connection_started",
            json!({"service": name, "via": via, "write": write, "requested_by": requested_by}),
        );
        let mut text = format!(
            "Sign in to {title} and approve. {}",
            service.read_only_story(write)
        );
        if !scopes.is_empty() {
            text.push_str(&format!("\nScopes: {}", scopes.join(", ")));
        }
        if via == "paste" {
            text.push_str(
                "\nAfter you approve, the browser lands on a page that doesn't load. \
                 Copy that page's address and send it here.",
            );
        }
        text.push_str(&format!(
            "\nThe link works for {} minutes.",
            self.flow_ttl.as_secs() / 60
        ));
        Ok(Started {
            reply: Reply {
                text,
                buttons: vec![Button {
                    text: format!("Connect {title}"),
                    action: Action::Url(url),
                }],
            },
            done: done_rx,
        })
    }

    fn insert(&self, state: String, flow: Flow) {
        self.inner.lock().unwrap().flows.insert(state, flow);
    }

    /// Takes the flow out: each state is used once.
    fn take(&self, state: &str) -> Option<Flow> {
        self.inner.lock().unwrap().flows.remove(state)
    }

    fn set_driver(&self, state: &str, handle: tokio::task::AbortHandle) {
        if let Some(f) = self.inner.lock().unwrap().flows.get_mut(state) {
            f.driver = Some(handle);
        } else {
            handle.abort();
        }
    }

    fn drive_relay(self: &Arc<Self>, relay: Relay, slot: Slot) {
        let me = Arc::downgrade(self);
        let state = slot.id.clone();
        let (ttl, every) = (self.flow_ttl, self.poll_every);
        let task = tokio::spawn(async move {
            let deadline = Instant::now() + ttl;
            let mut failures = 0;
            while Instant::now() < deadline {
                let got = relay.poll(&slot).await;
                let Some(me) = me.upgrade() else { return };
                match got {
                    Ok(Poll::Value(v)) => {
                        if let Some(flow) = me.take(&slot.id) {
                            me.finish(flow, v).await;
                        }
                        return;
                    }
                    Ok(Poll::Empty) => failures = 0,
                    Ok(Poll::Used) => {
                        if let Some(flow) = me.take(&slot.id) {
                            me.failed(flow, "the relay slot was already read").await;
                        }
                        return;
                    }
                    Err(e) => {
                        failures += 1;
                        tracing::warn!("relay poll: {e:#}");
                        if failures >= 30 {
                            if let Some(flow) = me.take(&slot.id) {
                                me.failed(flow, "the relay stopped answering").await;
                            }
                            return;
                        }
                    }
                }
                drop(me);
                tokio::time::sleep(every).await;
            }
            if let Some(me) = me.upgrade() {
                me.expire(&slot.id).await;
            }
        });
        self.set_driver(&state, task.abort_handle());
    }

    fn drive_tunnel(
        self: &Arc<Self>,
        state: String,
        mut listener: tunnel::Listener,
        t: tunnel::Tunnel,
    ) {
        let me = Arc::downgrade(self);
        let ttl = self.flow_ttl;
        let st = state.clone();
        let task = tokio::spawn(async move {
            let _tunnel = t;
            let deadline = tokio::time::Instant::now() + ttl;
            loop {
                let got = tokio::time::timeout_at(deadline, listener.rx.recv()).await;
                let Some(me) = me.upgrade() else { return };
                match got {
                    Ok(Some(cb)) if cb.state == st => {
                        if let Some(flow) = me.take(&st) {
                            let v = json!({"kind": "oauth", "code": cb.code, "error": cb.error});
                            me.finish(flow, v).await;
                        }
                        return;
                    }
                    // Someone else's guess at the tunnel: ignored.
                    Ok(Some(_)) => continue,
                    _ => {
                        me.expire(&st).await;
                        return;
                    }
                }
            }
        });
        self.set_driver(&state, task.abort_handle());
    }

    fn drive_expiry(self: &Arc<Self>, state: String) {
        let me = Arc::downgrade(self);
        let ttl = self.flow_ttl;
        let st = state.clone();
        let task = tokio::spawn(async move {
            tokio::time::sleep(ttl).await;
            if let Some(me) = me.upgrade() {
                me.expire(&st).await;
            }
        });
        self.set_driver(&state, task.abort_handle());
    }

    async fn expire(&self, state: &str) {
        let Some(flow) = self.take(state) else { return };
        let name = flow.service.name.clone();
        self.events
            .audit("connection_expired", json!({"service": name}));
        self.failed(flow, "the link expired").await;
    }

    async fn failed(&self, mut flow: Flow, why: &str) {
        let title = flow.service.title().to_string();
        let name = flow.service.name.clone();
        self.events.audit(
            "connection_failed",
            json!({"service": name, "via": flow.via, "why": why}),
        );
        if let Some(done) = flow.done.take() {
            let _ = done.send(Err(why.to_string()));
        }
        self.events
            .tell_owner(
                &format!("Connecting {title} didn't work: {why}."),
                vec![Button {
                    text: "Try again".into(),
                    action: Action::Command(format!(
                        "/connect {name}{}",
                        if flow.write { " write" } else { "" }
                    )),
                }],
            )
            .await;
    }

    /// A pasted redirect. Only the owner's paste, for a pending paste-back
    /// flow, is used; anything else is refused (and a non-owner's paste of
    /// a live state cancels that flow).
    async fn pasted(self: &Arc<Self>, actor: &Actor, p: crate::paste::Pasted) -> Reply {
        let owner = matches!(actor, Actor::Owner(_) | Actor::Terminal);
        let pending_via = self
            .inner
            .lock()
            .unwrap()
            .flows
            .get(&p.state)
            .map(|f| f.via);
        if !owner {
            if let Some(flow) = pending_via.and_then(|_| self.take(&p.state)) {
                self.events.audit(
                    "connection_refused",
                    json!({"service": flow.service.name, "why": "pasted by someone other than the owner"}),
                );
                self.failed(
                    flow,
                    "someone other than the owner sent its code, so it was cancelled",
                )
                .await;
            }
            return Reply::text("That looks like a sign-in link; only the owner's are used.");
        }
        let Some(flow) = pending_via
            .filter(|v| *v == "paste")
            .and_then(|_| self.take(&p.state))
        else {
            self.events.audit(
                "connection_refused",
                json!({"why": "a pasted link matching no pending flow"}),
            );
            return Reply::text(
                "That link doesn't match a pending connection: it was used already, it expired, \
                 or it was changed. Nothing was connected.",
            );
        };
        let title = flow.service.title().to_string();
        let v = json!({"kind": "oauth", "code": p.code, "error": p.error});
        let this = self.clone();
        tokio::spawn(async move { this.finish(flow, v).await });
        Reply::text(format!("Got it; finishing the {title} connection."))
    }

    /// The value a callback brought: a code (or an error), or a key form
    /// envelope.
    async fn finish(&self, mut flow: Flow, v: Value) {
        let done = flow.done.take();
        let outcome = self.complete(&mut flow, v).await;
        match outcome {
            Ok(text) => {
                if let Some(done) = done {
                    let _ = done.send(Ok(text.clone()));
                }
                self.events.tell_owner(&text, Vec::new()).await;
                self.events
                    .connected(&flow.service.name, flow.ask_chat.as_ref())
                    .await;
            }
            Err(e) => {
                flow.done = done;
                self.failed(flow, &format!("{e:#}")).await;
            }
        }
    }

    async fn complete(&self, flow: &mut Flow, v: Value) -> Result<String> {
        let service = flow.service.clone();
        let (secret, oauth_meta, granted, expires_at) = match &mut flow.kind {
            Kind::Key { form, slot } => {
                let form = form.take().context("the key form was used already")?;
                let key = open_envelope(form, slot, &v)?;
                (
                    Secret {
                        api_key: Some(key),
                        ..Default::default()
                    },
                    None,
                    None,
                    None,
                )
            }
            Kind::Oauth {
                endpoints,
                client,
                verifier,
                redirect,
            } => {
                if v["kind"] != "oauth" {
                    bail!("the relay brought something other than a sign-in code");
                }
                if let Some(err) = v["error"].as_str() {
                    let code: String = err
                        .chars()
                        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                        .take(40)
                        .collect();
                    bail!("the service said {code}");
                }
                let code = v["code"].as_str().context("no code came back")?;
                let resource = service.resource_param.then(|| service.url(flow.write));
                let tokens = oauth::exchange(
                    &self.http, endpoints, client, code, verifier, redirect, resource,
                )
                .await
                .map_err(|e| anyhow!("the code exchange was {e}"))?;
                let meta = OauthMeta {
                    token_endpoint: endpoints.token.clone(),
                    revocation_endpoint: endpoints.revocation.clone(),
                    client_id: client.id.clone(),
                    resource: resource.map(String::from),
                };
                (
                    Secret {
                        access_token: Some(tokens.access_token),
                        refresh_token: tokens.refresh_token,
                        client_secret: client.secret.clone(),
                        api_key: None,
                    },
                    Some(meta),
                    tokens.scope,
                    tokens.expires_in.map(|s| now() + s),
                )
            }
        };
        self.save_new(
            &service,
            flow.write,
            flow.via,
            flow.requested_by,
            secret,
            oauth_meta,
            granted,
            expires_at,
        )
        .await
    }

    /// Seal and store a new connection, count its tools, and tell whoever
    /// is following. Replaces an older connection of the same name.
    #[allow(clippy::too_many_arguments)]
    async fn save_new(
        &self,
        service: &Service,
        write: bool,
        via: &str,
        requested_by: &str,
        secret: Secret,
        oauth: Option<OauthMeta>,
        granted: Option<String>,
        expires_at: Option<u64>,
    ) -> Result<String> {
        let name = service.name.clone();
        let mut record = Record {
            name: name.clone(),
            service: service.clone(),
            write,
            scopes: service.scopes(write).to_vec(),
            granted,
            state: State::Connected,
            connected_at: now(),
            expires_at,
            refreshed_at: None,
            last_error: None,
            notice_sent: false,
            tools: None,
            via: via.to_string(),
            requested_by: requested_by.to_string(),
            oauth,
            sealed: self.store.seal(&name, &secret)?,
        };
        if header_for(&record, &secret).is_none() {
            bail!("nothing to authenticate with came back");
        }
        let stored = record.clone();
        self.store
            .update(move |all| {
                all.retain(|r| r.name != stored.name);
                all.push(stored);
                Ok(())
            })
            .await?;
        let tools = self.count_tools(&record).await;
        if let Some(n) = tools {
            record.tools = Some(n);
            let _ = self
                .store
                .update(|all| {
                    if let Some(r) = all.iter_mut().find(|r| r.name == name) {
                        r.tools = Some(n);
                    }
                    Ok(())
                })
                .await;
        }
        self.bump();
        self.events.audit(
            "connection_added",
            json!({"service": name, "via": via, "write": write,
                   "requested_by": requested_by, "tools": tools}),
        );
        let access = if write { "read-write" } else { "read-only" };
        Ok(match tools {
            Some(n) => format!(
                "{} is connected ({access}, {n} tools). Its tools are live now.",
                service.title()
            ),
            None => format!(
                "{} is connected ({access}). Its tools are being added.",
                service.title()
            ),
        })
    }

    async fn count_tools(&self, record: &Record) -> Option<usize> {
        let host = self.host.clone()?;
        let cfg = self.server_config(record);
        let client = McpClient::new(cfg, host).ok()?;
        let got = tokio::time::timeout(Duration::from_secs(30), client.list_tools()).await;
        client.shutdown().await;
        match got {
            Ok(Ok(tools)) => Some(tools.len()),
            _ => None,
        }
    }

    /// The terminal's way in for an API key: read at the terminal, never
    /// through a chat.
    pub async fn add_key(&self, what: &str, key: &str) -> Result<String> {
        let service = self.catalog.resolve(what)?;
        if service.auth != AuthKind::ApiKey {
            bail!("{} signs in with OAuth, not a key", service.title());
        }
        let key = key.trim();
        if key.is_empty() || key.chars().any(char::is_control) {
            bail!("that doesn't look like an API key");
        }
        self.save_new(
            &service,
            false,
            "terminal",
            "terminal",
            Secret {
                api_key: Some(key.to_string()),
                ..Default::default()
            },
            None,
            None,
            None,
        )
        .await
    }

    /// Revoke at the service where it can be, delete the token, and drop
    /// the tools.
    pub async fn disconnect(&self, name: &str) -> Result<String> {
        let name = name.to_ascii_lowercase();
        let records = self.store.load()?;
        let Some(record) = records.iter().find(|r| r.name == name).cloned() else {
            bail!("nothing called `{name}` is connected");
        };
        let mut revoked = None;
        if let (Some(meta), Ok(secret)) = (&record.oauth, self.store.open(&record)) {
            if let Some(endpoint) = &meta.revocation_endpoint {
                let client = Client {
                    id: meta.client_id.clone(),
                    secret: secret.client_secret.clone(),
                };
                let mut ok = false;
                if let Some(t) = &secret.refresh_token {
                    ok |= oauth::revoke(&self.http, endpoint, &client, t, "refresh_token").await;
                }
                if let Some(t) = &secret.access_token {
                    ok |= oauth::revoke(&self.http, endpoint, &client, t, "access_token").await;
                }
                revoked = Some(ok);
            }
        }
        self.store
            .update(|all| {
                all.retain(|r| r.name != name);
                Ok(())
            })
            .await?;
        self.creds
            .lock()
            .unwrap()
            .retain(|id, _| !id.starts_with(&format!("connection:{name}:")));
        self.bump();
        self.events.audit(
            "connection_removed",
            json!({"service": name, "revoked": revoked}),
        );
        let title = record.service.title();
        let tail = match revoked {
            Some(true) => format!("{title} revoked the grant."),
            Some(false) => format!(
                "{title} didn't confirm the revocation; remove ferrule's access in its settings too."
            ),
            None => record.service.revoke_note.clone().unwrap_or_else(|| {
                format!("{title} has no revocation endpoint; remove ferrule's access in its settings.")
            }),
        };
        Ok(format!(
            "Disconnected {name}: the token is deleted and its tools are gone. {tail}"
        ))
    }

    // ---- what the MCP layer sees ---------------------------------------

    fn server_config(&self, record: &Record) -> McpServerConfig {
        let service = &record.service;
        let mut headers = HashMap::new();
        if !record.write && service.read_only == crate::catalog::ReadOnly::Header {
            headers.extend(
                service
                    .read_only_headers
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone())),
            );
        }
        McpServerConfig {
            name: record.name.clone(),
            url: Some(service.url(record.write).to_string()),
            headers,
            auth: Some(self.auth(record)),
            ..Default::default()
        }
    }

    /// The same `Auth` for the same connection, so the follower doesn't
    /// restart a server that didn't change.
    fn auth(&self, record: &Record) -> Auth {
        let cred = StoreCredential {
            store: self.store.clone(),
            name: record.name.clone(),
            connected_at: record.connected_at,
            write: record.write,
            http: self.http.clone(),
            broken: self.broken(),
        };
        let id = ferrule_mcp::CredentialSource::id(&cred);
        self.creds
            .lock()
            .unwrap()
            .entry(id)
            .or_insert_with(|| Auth(Arc::new(cred)))
            .clone()
    }

    fn broken(&self) -> Broken {
        let events = self.events.clone();
        let changed = self.changed.clone();
        let store = self.store.clone();
        Arc::new(move |name: &str| {
            let name = name.to_string();
            let events = events.clone();
            let changed = changed.clone();
            let title = store
                .load()
                .ok()
                .and_then(|all| all.into_iter().find(|r| r.name == name))
                .map(|r| (r.service.title().to_string(), r.write))
                .unwrap_or_else(|| (name.clone(), false));
            changed.send_modify(|n| *n += 1);
            events.audit("connection_broken", json!({"service": name}));
            let text = format!(
                "{} needs reconnecting: the service no longer accepts ferrule's grant, so its \
                 tools are suspended.",
                title.0
            );
            let command = format!("/connect {name}{}", if title.1 { " write" } else { "" });
            let spawn = tokio::runtime::Handle::try_current();
            if let Ok(rt) = spawn {
                rt.spawn(async move {
                    events
                        .tell_owner(
                            &text,
                            vec![Button {
                                text: "Reconnect".into(),
                                action: Action::Command(command),
                            }],
                        )
                        .await;
                });
            }
        })
    }

    /// An MCP server per working connection. One that needs reconnecting
    /// is left out: its tools are suspended.
    pub fn servers(&self) -> Vec<McpServerConfig> {
        let Ok(records) = self.store.load() else {
            return Vec::new();
        };
        records
            .iter()
            .filter(|r| r.state == State::Connected)
            .map(|r| self.server_config(r))
            .collect()
    }
}

enum Path {
    Relay(Relay, Slot),
    Tunnel(tunnel::Listener, tunnel::Tunnel),
}

/// A key-form envelope from the relay: `{kind:"key", v, epk, iv, ct}`,
/// bound to the slot id it was written to.
fn open_envelope(form: KeyForm, slot: &str, v: &Value) -> Result<String> {
    if v["kind"] != "key" {
        bail!("the relay brought something other than a key");
    }
    form.open(slot, v)
}
