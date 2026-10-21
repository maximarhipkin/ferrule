//! M22: one page for the whole app (docs/m22-dashboard.md). The gateway
//! serves it on 127.0.0.1; `/dashboard` in Telegram opens a cloudflared
//! quick tunnel for the phone and sends a one-time login link. Nothing here
//! calls a model: the page must work when the model doesn't.

pub mod api;
pub mod auth;
pub mod backup_page;
pub mod channels;
pub mod chat;
pub mod cli;
pub mod config_page;
pub mod console;
pub mod door;
mod healthz;
pub mod http;
pub mod memory;
pub mod models_page;
pub mod notices;
#[cfg(test)]
mod page_tests;
pub mod panel;
mod public;
pub mod runs;
mod safe;
mod sealed;
pub mod setup;
mod telegram;
#[cfg(test)]
pub mod testing;
pub(crate) mod tool_keys;

use crate::config::{Config, DashboardConfig};
use anyhow::{bail, Context, Result};
use auth::{Links, Sessions};
use ferrule_connections::cloudflared::Cloudflared;
use ferrule_connections::tunnel::{self, Tunnel};
use ferrule_gateway::Redactor;
use http::{Request, Response};
pub use public::Public;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;

const INDEX: &str = include_str!("assets/index.html");
const APP_JS: &str = include_str!("assets/app.js");
const APP_CSS: &str = include_str!("assets/app.css");
/// Sets the theme before the first paint (the CSP allows no inline script).
const THEME_JS: &str = include_str!("assets/theme.js");
/// The page's fixed words in Hebrew: fetched only when Hebrew is chosen.
const LANG_HE: &str = include_str!("assets/lang-he.js");
/// Inter, Heebo and Geist Mono (OFL 1.1), served from the binary so the page
/// never depends on a font CDN. Each is one variable file (weights 400–600)
/// cut to the characters in its `unicode-range`: Inter and Geist Mono to
/// Latin, Heebo to Hebrew, which only a page showing Hebrew fetches. They are
/// cached for a year as immutable, so a changed font must get a new file name.
const FONTS: &[(&str, &[u8])] = &[
    (
        "inter-latin.woff2",
        include_bytes!("assets/fonts/inter-latin.woff2"),
    ),
    (
        "heebo-hebrew.woff2",
        include_bytes!("assets/fonts/heebo-hebrew.woff2"),
    ),
    (
        "geist-mono-latin.woff2",
        include_bytes!("assets/fonts/geist-mono-latin.woff2"),
    ),
];
const FONT_LICENSE: &str = include_str!("assets/fonts/OFL.txt");
/// A year: a font's name changes with its content.
const FOREVER: &str = "public, max-age=31536000, immutable";

/// What the page reads and changes; each piece is there when the process
/// serving it has one (the gateway has them all, `ferrule dashboard` on
/// its own fewer).
pub struct Ctx {
    pub redactor: Arc<Redactor>,
    /// `[connections] cloudflared`, resolved: fetched on the first tunnel
    /// when it's nowhere on the machine.
    pub cloudflared: Cloudflared,
    /// The running gateway's lanes and health; `None` in `ferrule
    /// dashboard` on its own.
    pub live: Option<api::Live>,
    pub models: Option<Arc<crate::models::Models>>,
    pub hub: Option<Arc<ferrule_trust::Hub>>,
    pub connections: Option<Arc<ferrule_connections::Connections>>,
    /// The owner's primary chat, on any channel (M31).
    pub owner_chat: Option<ferrule_trust::ChatRef>,
    pub tasks: Option<crate::tasks_admin::TasksAdmin>,
    /// `<data>`: the ledger, the agents and the catalog's cache.
    pub data: Option<PathBuf>,
    /// The config file, re-read for the extensions and the catalog's
    /// sources.
    pub config_path: Option<PathBuf>,
    /// Where project skills are found.
    pub workspace: Option<PathBuf>,
    /// The candidate eval running from the page (M24), one at a time.
    pub evals: Arc<crate::model_eval::Jobs>,
    /// `ferrule` commands started from the page (M37: doctor, the console).
    pub runs: Arc<runs::Runs>,
    /// Cloudflare's API, for deploying the relay (a mock in tests).
    pub cf_api: String,
    /// Plan sign-ins started from the page (M37).
    pub plans: Arc<models_page::PlanFlows>,
    /// The page's own chat channel (M37 §4.3); `None` outside the gateway.
    pub chat: Option<Arc<chat::DashboardChannel>>,
    /// The backup started from the page, if one is running (M47).
    pub backups: backup_page::Job,
}

impl Ctx {
    pub fn from_config(cfg: &Config) -> Self {
        let hub = crate::trust::hub(cfg).ok();
        let data = crate::config::data_dir().ok();
        Self {
            redactor: Arc::new(crate::health::redactor(cfg)),
            cloudflared: cloudflared(cfg),
            live: None,
            models: crate::models::shared().ok(),
            connections: crate::connections::shared(cfg),
            owner_chat: crate::trust::owners(cfg).into_iter().next(),
            tasks: data.as_ref().map(|d| {
                crate::tasks_admin::TasksAdmin::new(d.join("tasks.db"), hub.clone())
                    .with_config(crate::config::config_path().ok().flatten())
            }),
            hub,
            data,
            config_path: crate::config::config_path().ok().flatten(),
            workspace: std::env::current_dir().ok(),
            evals: Arc::default(),
            cf_api: crate::connections_setup::CF_API.to_string(),
            plans: Arc::default(),
            runs: Arc::default(),
            chat: None,
            backups: Arc::default(),
        }
    }

    /// Only the redactor: every section says it isn't available here.
    #[cfg(test)]
    pub fn bare(redactor: Arc<Redactor>) -> Self {
        Self {
            redactor,
            cloudflared: Cloudflared::Missing,
            live: None,
            models: None,
            hub: None,
            connections: None,
            owner_chat: None,
            tasks: None,
            data: None,
            config_path: None,
            workspace: None,
            evals: Arc::default(),
            cf_api: crate::connections_setup::CF_API.to_string(),
            plans: Arc::default(),
            runs: Arc::default(),
            chat: None,
            backups: Arc::default(),
        }
    }
}

/// `[connections] cloudflared`: "off", a path, one found on the machine,
/// or Cloudflare's build fetched into `<data>/bin` when first needed.
pub fn cloudflared(cfg: &Config) -> Cloudflared {
    let bin = crate::config::data_dir_path().map(|d| d.join("bin"));
    Cloudflared::resolve(cfg.connections.cloudflared.as_deref(), bin.as_deref())
}

pub struct Dashboard {
    settings: DashboardConfig,
    links: Links,
    sessions: Sessions,
    port: AtomicU16,
    tunnel: tokio::sync::Mutex<Option<Tunnel>>,
    /// The open tunnel's host, readable without the async lock.
    tunnel_host: Mutex<Option<String>>,
    /// The last logged-in request or login.
    last_used: Mutex<Instant>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// `[dashboard] public_url`, parsed: the proxy's host, origin and path prefix.
    public: Option<Public>,
    /// A non-loopback bind without `public_url`: only /healthz and /busyz answer.
    health_only: AtomicBool,
    /// index.html with the base path in it, made once.
    index: String,
    /// The panel's key: managed mode with a panel secret and a bot id.
    panel: Option<panel::Key>,
    nonces: panel::Nonces,
    /// For /healthz's `uptime_secs`.
    started: Instant,
    /// One-time keys the page seals typed keys to (M52.1).
    seals: sealed::Seals,
    /// M52.6: each session's chat confirmation, by the cookie's hash.
    step_ups: Arc<Mutex<std::collections::HashMap<String, StepUp>>>,
    pub ctx: Ctx,
}

const HEALTH_ONLY: &str = "this bot's page has no public address: set FERRULE_PUBLIC_URL (or [dashboard] public_url) to the URL it is opened at";

/// M52.6: the Safe's writes. Each needs a login from the last
/// [`STEP_UP_MS`], or the owner's Allow in their chat within it.
const STEP_UP_PATHS: &[&str] = &[
    "/api/secrets/rotate",
    "/api/secrets/delete",
    "/api/secrets/hosts",
    "/api/secrets/remove",
];
const STEP_UP_MS: u64 = 10 * 60 * 1000;
/// How long the owner has to answer the chat's card.
const STEP_UP_ASK: Duration = Duration::from_secs(5 * 60);
const STEP_UP: &str = "Changing a key needs a login from the last 10 minutes. Confirm it in your chat, or open a new link with /dashboard.";
/// The approvals' `op` for a step-up question: the dashboard can't answer
/// one of these itself (`chat::answer`).
pub(crate) const STEP_UP_OP: &str = "dashboard.step_up";

/// One session's step-up (M52.6), in memory: a restart asks again.
#[derive(Debug, Clone, Default)]
struct StepUp {
    pending: bool,
    approved_ms: Option<u64>,
    refused: Option<String>,
}

/// The page's HTML for a page served under `base` ("" or "/b/<id>"): the
/// base path in a meta tag for the script, and in every asset's address.
fn index_for(base: &str) -> String {
    let mut html = INDEX.replacen(
        "<meta charset=\"utf-8\">",
        &format!("<meta charset=\"utf-8\">\n<meta name=\"ferrule-base\" content=\"{base}/\">"),
        1,
    );
    for (attr, path) in [
        ("src", "/theme.js"),
        ("href", "/fonts/inter-latin.woff2"),
        ("href", "/app.css"),
        ("src", "/app.js"),
    ] {
        html = html.replacen(
            &format!("{attr}=\"{path}\""),
            &format!("{attr}=\"{base}{path}\""),
            1,
        );
    }
    html
}

/// A login link: at the public address when there is one, else on
/// loopback.
pub fn link_url(settings: &DashboardConfig, port: u16, token: &str) -> String {
    match settings
        .public_url
        .as_deref()
        .and_then(|u| Public::parse(u).ok())
    {
        Some(p) => format!("{}{}/login#{token}", p.origin, p.base),
        None => format!("http://127.0.0.1:{port}/login#{token}"),
    }
}

impl Drop for Dashboard {
    fn drop(&mut self) {
        for t in self.tasks.lock().unwrap().drain(..) {
            t.abort();
        }
    }
}

fn minutes(n: u64) -> Duration {
    Duration::from_secs(n.max(1) * 60)
}

impl Dashboard {
    pub fn new(settings: DashboardConfig, links: Links, ctx: Ctx) -> Arc<Self> {
        Self::new_with(settings, links, ctx, panel::Key::from_managed())
    }

    /// [`Dashboard::new`] with the panel's key given (the tests').
    pub fn new_with(
        settings: DashboardConfig,
        links: Links,
        ctx: Ctx,
        panel: Option<panel::Key>,
    ) -> Arc<Self> {
        let public = settings
            .public_url
            .as_deref()
            .and_then(|u| Public::parse(u).ok());
        let index = index_for(public.as_ref().map_or("", |p| p.base.as_str()));
        let nonces = panel::Nonces::at(links.file_beside("nonces.json"));
        let sessions = Sessions::beside(
            &links,
            minutes(settings.idle_minutes),
            minutes(settings.session_hours.max(1) * 60),
        );
        Arc::new(Self {
            settings,
            links,
            sessions,
            port: AtomicU16::new(0),
            tunnel: tokio::sync::Mutex::new(None),
            tunnel_host: Mutex::new(None),
            last_used: Mutex::new(Instant::now()),
            tasks: Mutex::new(Vec::new()),
            public,
            health_only: AtomicBool::new(false),
            index,
            panel,
            nonces,
            started: Instant::now(),
            seals: sealed::Seals::default(),
            step_ups: Arc::default(),
            ctx,
        })
    }

    pub fn settings(&self) -> &DashboardConfig {
        &self.settings
    }

    /// The address the page is opened at, when `[dashboard] public_url` says.
    pub fn public(&self) -> Option<&Public> {
        self.public.as_ref()
    }

    /// The port it listens on; 0 before [`Dashboard::bind`].
    pub fn port(&self) -> u16 {
        self.port.load(Ordering::Relaxed)
    }

    /// Listens on `[dashboard] bind` (127.0.0.1 unless set) and serves
    /// until dropped; also closes the tunnel once it's idle. With port 0,
    /// the port it used last time comes first, so a saved `ssh -L` still
    /// reaches it after a restart.
    pub async fn bind(self: &Arc<Self>, port: u16) -> Result<u16> {
        let ip: std::net::IpAddr = self
            .settings
            .bind
            .parse()
            .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
        let last = (port == 0).then(|| self.sessions.last_port()).flatten();
        let listener = match last {
            Some(last) => match TcpListener::bind((ip, last)).await {
                Ok(l) => l,
                Err(_) => TcpListener::bind((ip, 0)).await?,
            },
            None => TcpListener::bind((ip, port))
                .await
                .with_context(|| format!("listening on {ip}:{port}"))?,
        };
        let port = listener.local_addr()?.port();
        self.port.store(port, Ordering::Relaxed);
        if !ip.is_loopback() && self.public.is_none() {
            self.health_only.store(true, Ordering::Relaxed);
            tracing::warn!("the dashboard listens on {ip}:{port} without [dashboard] public_url (FERRULE_PUBLIC_URL): only /healthz and /busyz answer");
        }
        self.sessions.remember_port(port);
        let weak = Arc::downgrade(self);
        let serve = tokio::spawn(async move {
            loop {
                let Ok((mut conn, _)) = listener.accept().await else {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                };
                let Some(me) = weak.upgrade() else { return };
                tokio::spawn(async move {
                    let resp = match http::read(&mut conn, |head| me.limit_for(head)).await {
                        Ok(req) => me.handle(req).await,
                        Err(0) => return,
                        // As JSON, so the page shows the sentence, not "HTTP 413".
                        Err(status) => refuse(
                            status,
                            match status {
                                413 => "That's too big to send.",
                                503 => "Another upload is in progress; try again in a moment.",
                                408 => "That took too long to arrive; try again.",
                                _ => "The page sent a request this server can't read.",
                            },
                        ),
                    };
                    http::write(&mut conn, resp).await;
                });
            }
        });
        let weak = Arc::downgrade(self);
        let idle = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(20)).await;
                let Some(me) = weak.upgrade() else { return };
                me.close_if_idle().await;
            }
        });
        self.tasks.lock().unwrap().extend([serve, idle]);
        Ok(port)
    }

    fn touch(&self) {
        *self.last_used.lock().unwrap() = Instant::now();
    }

    /// The tunnel closes once nobody has used the page for `idle_minutes`
    /// and no unused link is waiting, or when cloudflared has died.
    pub async fn close_if_idle(&self) {
        let mut slot = self.tunnel.lock().await;
        let Some(t) = slot.as_mut() else { return };
        let idle = self.last_used.lock().unwrap().elapsed() >= minutes(self.settings.idle_minutes);
        let dead = !t.alive();
        let live = self.sessions.live(self.links.revoked_ms());
        if dead || (idle && self.links.pending() == 0 && live == 0) {
            if dead {
                tracing::warn!("the dashboard's quick tunnel went away");
            } else {
                tracing::info!("dashboard idle: closing its tunnel");
            }
            *slot = None;
            *self.tunnel_host.lock().unwrap() = None;
        }
    }

    /// Whether a tunnel is open.
    pub async fn tunnel_open(&self) -> bool {
        let mut slot = self.tunnel.lock().await;
        let alive = slot.as_mut().is_some_and(Tunnel::alive);
        if !alive && slot.is_some() {
            *slot = None;
            *self.tunnel_host.lock().unwrap() = None;
        }
        alive
    }

    /// A login link for this machine's browser.
    pub fn local_link(&self) -> Result<String> {
        let token = self.links.mint(None, minutes(self.settings.link_minutes))?;
        Ok(link_url(&self.settings, self.port(), &token))
    }

    /// A login link through the tunnel, opening it first if needed.
    /// cloudflared prints a quick tunnel's address before Cloudflare
    /// routes it, so a link sent at once fails on the first tap: a new
    /// tunnel gets up to [`TUNNEL_READY`] to answer before the link is
    /// minted, and the link says whether it did.
    pub async fn remote_link(&self) -> Result<RemoteLink> {
        let (host, opened) = self.open_tunnel().await?;
        let ready = !opened || tunnel_answers(&host, TUNNEL_READY).await;
        if !ready {
            tracing::warn!(
                "dashboard: the new tunnel didn't answer within {} s; sending its link anyway",
                TUNNEL_READY.as_secs()
            );
        }
        let token = self
            .links
            .mint(Some(&host), minutes(self.settings.link_minutes))?;
        self.touch();
        Ok(RemoteLink {
            url: format!("https://{host}/login#{token}"),
            ready,
        })
    }

    /// Opens the quick tunnel (or keeps the open one): its host, and
    /// whether this call opened it.
    pub async fn open_tunnel(&self) -> Result<(String, bool)> {
        crate::managed::forbid("a tunnel", "the panel's proxy is the way in")?;
        if self.settings.remote != "tunnel" {
            bail!(
                "[dashboard] remote is \"{}\", not \"tunnel\"",
                self.settings.remote
            );
        }
        let bin = self.ctx.cloudflared.path().await?;
        let mut slot = self.tunnel.lock().await;
        if let Some(t) = slot.as_mut() {
            if t.alive() {
                return Ok((host_of(&t.url)?, false));
            }
        }
        let port = self.port();
        if port == 0 {
            bail!("the dashboard isn't listening");
        }
        let t = tunnel::open(&bin, port).await?;
        let host = host_of(&t.url)?;
        *slot = Some(t);
        *self.tunnel_host.lock().unwrap() = Some(host.clone());
        self.touch();
        Ok((host, true))
    }

    /// `/dashboard off`: every link and session stops working and the
    /// tunnel closes.
    pub async fn off(&self) -> Result<()> {
        let cleared = self.sessions.clear();
        *self.tunnel.lock().await = None;
        *self.tunnel_host.lock().unwrap() = None;
        self.links.revoke()?;
        cleared
    }

    /// At start (docs/m24-dashboard-2.md §1): a tunnel session that was
    /// live when the last process stopped can't be used again, since the
    /// next tunnel has a new name. Those go, and when one did, and a
    /// tunnel can be opened, and none was sent in the last 10 minutes: a
    /// new tunnel and the owner's message with its one-time link.
    pub async fn relink_after_restart(&self) -> Option<String> {
        let keep: Vec<String> = self
            .public
            .iter()
            .map(|p| auth::host_key(&p.host))
            .collect();
        let gone = match self.sessions.retire_tunnel_sessions(
            self.links.revoked_ms(),
            Duration::from_secs(600),
            &keep,
        ) {
            Ok(g) => g?,
            Err(e) => {
                tracing::warn!("dashboard sessions: {e:#}");
                return None;
            }
        };
        if crate::managed::on()
            || self.settings.remote != "tunnel"
            || !self.ctx.cloudflared.possible()
        {
            tracing::info!(
                "{gone} dashboard tunnel session(s) ended with the restart; no tunnel to reopen"
            );
            return None;
        }
        match self.remote_link().await {
            Ok(link) => Some(format!(
                "The gateway restarted, so the dashboard has a new address.\n{}",
                door::remote_text(self, &link)
            )),
            Err(e) => {
                tracing::warn!("dashboard: reopening the tunnel after the restart: {e:#}");
                None
            }
        }
    }

    /// Loopback with our port, the open tunnel's host, and the hosts links
    /// and sessions were made for (a tunnel `ferrule dashboard link
    /// --remote` opened from another process).
    fn host_allowed(&self, host: &str) -> bool {
        if self.public.as_ref().is_some_and(|p| p.host == host) {
            return true;
        }
        let port = self.port();
        if [
            format!("127.0.0.1:{port}"),
            format!("localhost:{port}"),
            format!("[::1]:{port}"),
        ]
        .iter()
        .any(|h| h == host)
        {
            return true;
        }
        if self.tunnel_host.lock().unwrap().as_deref() == Some(host) {
            return true;
        }
        self.links.hosts().iter().any(|h| h == host)
            || self
                .sessions
                .hosts(self.links.revoked_ms())
                .iter()
                .any(|h| h == host)
    }

    fn is_loopback(&self, host: &str) -> bool {
        host.starts_with("127.0.0.1:")
            || host.starts_with("localhost:")
            || host.starts_with("[::1]:")
    }

    /// The origin a same-origin request from `host` carries.
    fn origin_for(&self, host: &str) -> String {
        if let Some(p) = self.public.as_ref().filter(|p| p.host == host) {
            return p.origin.clone();
        }
        if self.is_loopback(host) {
            format!("http://{host}")
        } else {
            format!("https://{host}")
        }
    }

    /// Whether a cookie for `host` is `Secure`: as the public address says,
    /// else unless it is loopback.
    fn secure_for(&self, host: &str) -> bool {
        match self.public.as_ref().filter(|p| p.host == host) {
            Some(p) => p.secure,
            None => !self.is_loopback(host),
        }
    }

    /// The cookie's `Path`: the public address's prefix, else `/`.
    fn cookie_path(&self, host: &str) -> String {
        match self.public.as_ref().filter(|p| p.host == host) {
            Some(p) => format!("{}/", p.base),
            None => "/".into(),
        }
    }

    /// The session cookie's name on `host` (M52.6: `__Host-` where the
    /// origin allows it).
    fn cookie_name(&self, host: &str) -> &'static str {
        auth::cookie_name(self.secure_for(host), &self.cookie_path(host))
    }

    /// Whether `cookie`'s session may make a Safe write now: opened in the
    /// last 10 minutes, or the owner allowed it in their chat in the last
    /// 10 (M52.6).
    fn stepped_up(&self, cookie: &str) -> bool {
        let now = auth::now_ms();
        let fresh = |t: u64| now.saturating_sub(t) < STEP_UP_MS;
        self.sessions.opened_ms(cookie).is_some_and(fresh)
            || self
                .step_ups
                .lock()
                .unwrap()
                .get(&ferrule_connections::seal::sha256_b64(cookie.as_bytes()))
                .and_then(|s| s.approved_ms)
                .is_some_and(fresh)
    }

    /// `GET /api/stepup`: where this session's step-up stands.
    fn step_up_state(&self, cookie: &str) -> Response {
        let state = self
            .step_ups
            .lock()
            .unwrap()
            .get(&ferrule_connections::seal::sha256_b64(cookie.as_bytes()))
            .cloned()
            .unwrap_or_default();
        Response::json(
            200,
            &json!({
                "fresh": self.stepped_up(cookie),
                "pending": state.pending,
                "refused": state.refused,
            }),
        )
    }

    /// `POST /api/stepup {path, name?}`: asks the owner, in a chat that
    /// isn't this page, whether this browser may change keys for 10
    /// minutes. The words are the server's: the page only says which
    /// write and which key's name.
    fn step_up_ask(&self, cookie: &str, host: &str, body: &Value) -> Response {
        if self.stepped_up(cookie) {
            return Response::json(200, &json!({ "fresh": true }));
        }
        let no_chat = "There's no chat to confirm it in: open a new login link with /dashboard (or `ferrule dashboard link` on the machine).";
        let Some(hub) = self.ctx.hub.clone() else {
            return refuse(409, no_chat);
        };
        let Some(chat) = hub.owners().into_iter().find(|o| o.channel != "dashboard") else {
            return refuse(409, no_chat);
        };
        let doing = match body.get("path").and_then(Value::as_str).unwrap_or("") {
            "secrets/rotate" => "replace a key",
            "secrets/delete" => "delete a key",
            "secrets/hosts" => "change where a tool key may be sent",
            "secrets/remove" => "remove a tool key",
            _ => "change keys",
        };
        let name: String = body
            .get("name")
            .and_then(Value::as_str)
            .filter(|n| {
                (1..=80).contains(&n.len())
                    && n.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_:.-".contains(&b))
            })
            .map(|n| format!(" ({n})"))
            .unwrap_or_default();
        let key = ferrule_connections::seal::sha256_b64(cookie.as_bytes());
        {
            let mut all = self.step_ups.lock().unwrap();
            let s = all.entry(key.clone()).or_default();
            if s.pending {
                return Response::json(200, &json!({ "asked": true }));
            }
            *s = StepUp {
                pending: true,
                ..StepUp::default()
            };
        }
        let at = auth::host_key(host);
        let text = format!(
            "The dashboard, signed in from {at}, wants to {doing}{name}. Is that you? Allow lets that browser change keys for 10 minutes."
        );
        let map = self.step_ups.clone();
        tokio::spawn(async move {
            let q = ferrule_trust::hub::Question {
                subject: "dashboard step-up",
                what: "a key change on the dashboard",
                text: &text,
                timeout: STEP_UP_ASK,
                op: Some(STEP_UP_OP),
            };
            let answer = hub.ask_in("dashboard", Some(chat), q).await;
            let mut all = map.lock().unwrap();
            let s = all.entry(key).or_default();
            s.pending = false;
            match answer {
                Ok(_) => s.approved_ms = Some(auth::now_ms()),
                Err(why) => s.refused = Some(why),
            }
        });
        Response::json(200, &json!({ "asked": true }))
    }

    /// The path without the public address's prefix; a proxy that strips
    /// it itself sends paths that don't have it, and those stay as they are.
    fn strip_base(&self, path: &str) -> String {
        let Some(base) = self
            .public
            .as_ref()
            .map(|p| p.base.as_str())
            .filter(|b| !b.is_empty())
        else {
            return path.to_string();
        };
        if path == base {
            return "/".into();
        }
        match path.strip_prefix(base) {
            Some(rest) if rest.starts_with('/') => rest.to_string(),
            _ => path.to_string(),
        }
    }

    /// How much body this request may bring, decided from its head alone.
    /// Only a photo for the chat gets more than the usual 64 KB, and only
    /// from someone already logged in and passing the same checks `handle`
    /// makes (same origin, JSON, CSRF); anyone else gets the usual limit and
    /// the usual refusal, and none of their body is read.
    fn limit_for(&self, head: &http::Head) -> http::Limit {
        let usual = http::Limit::default();
        if head.method != "POST"
            || self.strip_base(&head.path) != "/api/chat/photo"
            || self.health_only.load(Ordering::Relaxed)
        {
            return usual;
        }
        let host = head.header("host").unwrap_or("").to_ascii_lowercase();
        let same_origin = head.header("origin") == Some(self.origin_for(&host).as_str());
        let json = head
            .header("content-type")
            .is_some_and(|c| c.starts_with("application/json"));
        let granted = self.sessions.check(
            head.cookie(self.cookie_name(&host)),
            &host,
            self.links.revoked_ms(),
        );
        let csrf = head.header(auth::CSRF_HEADER).unwrap_or("");
        match granted {
            Some(g)
                if self.host_allowed(&host) && same_origin && json && auth::same(csrf, &g.csrf) =>
            {
                http::Limit {
                    body: chat::MAX_PHOTO_BODY,
                    deadline: Duration::from_secs(60),
                }
            }
            _ => usual,
        }
    }

    pub async fn handle(&self, mut req: Request) -> Response {
        req.path = self.strip_base(&req.path);
        match req.path.as_str() {
            "/healthz" => return healthz::healthz(self, &req),
            "/busyz" => return healthz::busyz(self, &req),
            _ => {}
        }
        if self.health_only.load(Ordering::Relaxed) {
            return Response::text(421, HEALTH_ONLY);
        }
        let host = req.header("host").unwrap_or("").to_ascii_lowercase();
        if !self.host_allowed(&host) {
            return Response::text(421, "unknown host");
        }
        // M52.6, opt-in: off this machine, only the owner's tailnet login
        // (Tailscale Serve sets the header and strips a client's own).
        if !self.settings.tailscale_users.is_empty() && !self.is_loopback(&host) {
            let who = req.header("tailscale-user-login").unwrap_or("");
            if !self
                .settings
                .tailscale_users
                .iter()
                .any(|u| !who.is_empty() && u.eq_ignore_ascii_case(who))
            {
                return Response::text(
                    403,
                    "this page answers only its owner's tailnet login ([dashboard] tailscale_users)",
                );
            }
        }
        let get = req.method == "GET" || req.method == "HEAD";
        match (req.method.as_str(), req.path.as_str()) {
            (_, "/" | "/login" | "/index.html") if get => {
                return squeezed(
                    &req,
                    "text/html; charset=utf-8",
                    self.index.as_bytes(),
                    None,
                )
            }
            (_, "/app.js") if get => {
                static GZ: OnceLock<Vec<u8>> = OnceLock::new();
                return squeezed(
                    &req,
                    "text/javascript; charset=utf-8",
                    APP_JS.as_bytes(),
                    Some(&GZ),
                );
            }
            (_, "/app.css") if get => {
                static GZ: OnceLock<Vec<u8>> = OnceLock::new();
                return squeezed(
                    &req,
                    "text/css; charset=utf-8",
                    APP_CSS.as_bytes(),
                    Some(&GZ),
                );
            }
            (_, "/theme.js") if get => {
                static GZ: OnceLock<Vec<u8>> = OnceLock::new();
                return squeezed(
                    &req,
                    "text/javascript; charset=utf-8",
                    THEME_JS.as_bytes(),
                    Some(&GZ),
                );
            }
            (_, "/lang-he.js") if get => {
                static GZ: OnceLock<Vec<u8>> = OnceLock::new();
                return squeezed(
                    &req,
                    "text/javascript; charset=utf-8",
                    LANG_HE.as_bytes(),
                    Some(&GZ),
                );
            }
            (_, "/fonts/OFL.txt") if get => {
                return Response::new(200, "text/plain; charset=utf-8", FONT_LICENSE)
                    .with_header("Cache-Control", FOREVER.into())
            }
            (_, path) if get && path.starts_with("/fonts/") => {
                if let Some((_, bytes)) = FONTS.iter().find(|(n, _)| path[7..] == **n) {
                    return Response::new(200, "font/woff2", *bytes)
                        .with_header("Cache-Control", FOREVER.into());
                }
            }
            _ => {}
        }
        if !req.path.starts_with("/api/") {
            return Response::text(404, "not found");
        }
        if !get && req.method != "POST" {
            return Response::text(405, "method not allowed");
        }
        // Every POST: same origin, JSON.
        if !get {
            if req.header("origin") != Some(self.origin_for(&host).as_str()) {
                return refuse(403, "wrong origin");
            }
            if !req
                .header("content-type")
                .is_some_and(|c| c.starts_with("application/json"))
            {
                return refuse(403, "not JSON");
            }
        }
        let body: Value = if get || req.body.is_empty() {
            json!({})
        } else {
            match serde_json::from_slice(&req.body) {
                Ok(v) => v,
                Err(_) => return refuse(400, "the body isn't JSON"),
            }
        };
        if req.path == "/api/login" && !get {
            return self.login(&host, &body);
        }
        let cookie = req.cookie(self.cookie_name(&host));
        let Some(granted) = self.sessions.check(cookie, &host, self.links.revoked_ms()) else {
            return refuse(401, "not logged in: ask for a new link with /dashboard");
        };
        if !get {
            let sent = req.header(auth::CSRF_HEADER).unwrap_or("");
            if !auth::same(sent, &granted.csrf) {
                return refuse(403, "missing or wrong CSRF token");
            }
        }
        self.touch();
        let secure = self.secure_for(&host);
        // M52.6: a Safe write from a login older than 10 minutes needs
        // the owner's Allow in their chat first (the page asks for it).
        if !get
            && STEP_UP_PATHS.contains(&req.path.as_str())
            && cookie.is_some_and(|c| !self.stepped_up(c))
        {
            return Response::json(403, &json!({ "error": STEP_UP, "step_up": true }));
        }
        match (get, req.path.as_str()) {
            (true, "/api/stepup") => match cookie {
                Some(c) => self.step_up_state(c),
                None => refuse(401, "not logged in"),
            },
            (false, "/api/stepup") => match cookie {
                Some(c) => self.step_up_ask(c, &host, &body),
                None => refuse(401, "not logged in"),
            },
            (true, "/api/session") => Response::json(
                200,
                &json!({
                    "csrf": granted.csrf,
                    "user": granted.user,
                    "session_hours": self.settings.session_hours,
                    "idle_minutes": self.settings.idle_minutes,
                }),
            ),
            (true, "/api/backups/download") => backup_page::download(&self.ctx, &req),
            (false, "/api/seal") => match self.seals.issue(&granted.csrf) {
                Ok(key) => Response::json(200, &key),
                Err(e) => {
                    tracing::warn!("dashboard: no one-time key: {e:#}");
                    refuse(503, "the page's one-time key couldn't be made; try again")
                }
            },
            (false, "/api/logout") => {
                if let Some(c) = cookie {
                    self.sessions.close(c);
                }
                Response::json(200, &json!({ "ok": true })).with_header(
                    "Set-Cookie",
                    auth::clear_cookie(secure, &self.cookie_path(&host)),
                )
            }
            // A sealed key opens here, in memory, or nothing runs (M52.1).
            _ if !get => match self.seals.open_all(&granted.csrf, body) {
                Ok(body) => self.answer(false, &req, &body).await,
                Err(why) => refuse(400, &why),
            },
            _ => self.answer(true, &req, &body).await,
        }
    }

    /// An endpoint's answer, redacted.
    async fn answer(&self, get: bool, req: &Request, body: &Value) -> Response {
        match self.api(get, req, body).await {
            Some((status, mut value)) => {
                redact_value(&mut value, &self.ctx.redactor);
                Response::json(status, &value)
            }
            None => refuse(404, "no such endpoint"),
        }
    }

    /// A panel token: signed, for this bot, unexpired and never used before.
    fn panel_login(&self, host: &str, token: &str) -> Response {
        let Some(key) = &self.panel else {
            tracing::warn!("panel sign-in refused: no panel secret or bot id on this bot");
            return refuse(401, panel::REFUSED);
        };
        let now = auth::now_secs();
        let claims = match panel::verify(token, key, now)
            .and_then(|c| self.nonces.spend(&c.nonce, c.exp, now).map(|()| c))
        {
            Ok(c) => c,
            Err(why) => {
                tracing::warn!("panel sign-in refused: {why}");
                return refuse(401, panel::REFUSED);
            }
        };
        let (cookie, csrf) =
            match self
                .sessions
                .open(host, self.links.revoked_ms(), Some(&claims.user))
            {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("dashboard sessions: {e:#}");
                    return refuse(503, "the session store can't be written; try again");
                }
            };
        tracing::info!(user = %claims.user, "panel sign-in");
        self.touch();
        Response::json(
            200,
            &json!({
                "csrf": csrf,
                "session_hours": self.settings.session_hours,
                "idle_minutes": self.settings.idle_minutes,
            }),
        )
        .with_header(
            "Set-Cookie",
            auth::set_cookie(
                &cookie,
                self.secure_for(host),
                minutes(self.settings.session_hours * 60),
                &self.cookie_path(host),
            ),
        )
    }

    fn login(&self, host: &str, body: &Value) -> Response {
        let token = body.get("token").and_then(Value::as_str).unwrap_or("");
        if token.starts_with(panel::PREFIX) {
            return self.panel_login(host, token);
        }
        let used = match self.links.consume(token, host) {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!("dashboard links: {e:#}");
                return refuse(503, "the link store can't be read; try again");
            }
        };
        match used {
            auth::Consumed::WrongHost { made_for } => {
                // Never the token. The host header is the visitor's own text.
                tracing::warn!(
                    "dashboard: a login link made for {made_for} was opened on {host:?}; refused, and the link still works on its own host"
                );
                refuse(
                    401,
                    "this link was made for another address: open it exactly as it was sent",
                )
            }
            auth::Consumed::Used => {
                let (cookie, csrf) = match self.sessions.open(host, self.links.revoked_ms(), None) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!("dashboard sessions: {e:#}");
                        return refuse(503, "the session store can't be written; try again");
                    }
                };
                self.touch();
                Response::json(
                    200,
                    &json!({
                        "csrf": csrf,
                        "session_hours": self.settings.session_hours,
                        "idle_minutes": self.settings.idle_minutes,
                    }),
                )
                .with_header(
                    "Set-Cookie",
                    auth::set_cookie(
                        &cookie,
                        self.secure_for(host),
                        minutes(self.settings.session_hours * 60),
                        &self.cookie_path(host),
                    ),
                )
            }
            auth::Consumed::Invalid => refuse(
                401,
                "this link was used, expired or isn't valid: ask for a new one with /dashboard",
            ),
        }
    }

    /// The sections' endpoints: `(status, body)`, or `None` for an unknown
    /// path.
    async fn api(&self, get: bool, req: &Request, body: &Value) -> Option<(u16, Value)> {
        api::route(&self.ctx, get, req, body).await
    }
}

/// How long a new quick tunnel gets to answer before its link is sent
/// anyway (M55.5).
pub const TUNNEL_READY: Duration = Duration::from_secs(20);

/// A login link through the tunnel.
#[derive(Debug, Clone)]
pub struct RemoteLink {
    pub url: String,
    /// Whether the tunnel answered before the link was made. When it
    /// didn't, the message says the page may take a few more seconds.
    pub ready: bool,
}

/// Whether `https://<host>/healthz` answers as a ferrule dashboard within
/// `within`, asked again with backoff. Until Cloudflare routes a new quick
/// tunnel, its address gives an error page or no answer, never this JSON.
pub async fn tunnel_answers(host: &str, within: Duration) -> bool {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap_or_default();
    let url = format!("https://{host}/healthz");
    let ours = |v: &Value| v["status"].is_string() && v["version"].is_string();
    tokio::time::timeout(within, async {
        let mut pause = Duration::from_millis(250);
        loop {
            if let Ok(r) = http.get(&url).send().await {
                if r.json::<Value>().await.is_ok_and(|v| ours(&v)) {
                    return;
                }
            }
            tokio::time::sleep(pause).await;
            pause = (pause * 2).min(Duration::from_secs(2));
        }
    })
    .await
    .is_ok()
}

fn refuse(status: u16, why: &str) -> Response {
    Response::json(status, &json!({ "error": why }))
}

pub fn host_of(url: &str) -> Result<String> {
    Ok(url
        .strip_prefix("https://")
        .context("a tunnel URL starts with https://")?
        .trim_end_matches('/')
        .to_ascii_lowercase())
}

/// Whether an `Accept-Encoding` value takes gzip (and doesn't say `q=0`).
fn takes_gzip(value: &str) -> bool {
    value.split(',').any(|part| {
        let mut it = part.split(';');
        let coding = it.next().unwrap_or("").trim();
        let refused = it.any(|p| {
            let p = p.trim();
            p.strip_prefix("q=")
                .and_then(|q| q.trim().parse::<f32>().ok())
                .is_some_and(|q| q <= 0.0)
        });
        (coding.eq_ignore_ascii_case("gzip") || coding == "*") && !refused
    })
}

/// One of the page's own files: gzipped when the browser takes it (the
/// page is about 220 KB as written and about 65 KB gzipped, M47), whole
/// when it doesn't. `cache` keeps the compressed bytes of a file that
/// never changes while the process runs, so it is squeezed once.
fn squeezed(
    req: &Request,
    content_type: &'static str,
    raw: &[u8],
    cache: Option<&'static OnceLock<Vec<u8>>>,
) -> Response {
    if !req.header("accept-encoding").is_some_and(takes_gzip) {
        return Response::new(200, content_type, raw);
    }
    let squeeze = || {
        use std::io::Write;
        let mut z = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        let _ = z.write_all(raw);
        z.finish().unwrap_or_default()
    };
    let body = match cache {
        Some(c) => c.get_or_init(squeeze).clone(),
        None => squeeze(),
    };
    if body.is_empty() {
        return Response::new(200, content_type, raw);
    }
    Response::new(200, content_type, body)
        .with_header("Content-Encoding", "gzip".into())
        .with_header("Vary", "Accept-Encoding".into())
}

/// Every string in `value`, through the redactor.
pub fn redact_value(value: &mut Value, r: &Redactor) {
    match value {
        Value::String(s) => {
            let red = r.redact(s);
            if red != *s {
                *s = red;
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|v| redact_value(v, r)),
        Value::Object(o) => o.values_mut().for_each(|v| redact_value(v, r)),
        _ => {}
    }
}

/// Where a config the page replaced is kept: `<config>.prev`.
pub fn config_prev(file: &std::path::Path) -> PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push(".prev");
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn dash() -> (tempfile::TempDir, Arc<Dashboard>) {
        let dir = tempfile::tempdir().unwrap();
        let d = Dashboard::new(
            DashboardConfig::default(),
            Links::at(dir.path().join("links.json")),
            Ctx::bare(Arc::new(Redactor::new(["sk-SEEDED-SECRET".to_string()]))),
        );
        d.port.store(4321, Ordering::Relaxed);
        (dir, d)
    }

    fn req(method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Request {
        let mut h: BTreeMap<String, String> = headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        h.entry("host".into()).or_insert("127.0.0.1:4321".into());
        Request {
            method: method.into(),
            path: path.into(),
            query: BTreeMap::new(),
            headers: h,
            body: body.as_bytes().to_vec(),
        }
    }

    const JSON: (&str, &str) = ("content-type", "application/json");
    const ORIGIN: (&str, &str) = ("origin", "http://127.0.0.1:4321");

    async fn login(d: &Dashboard) -> (String, String) {
        let link = d.local_link().unwrap();
        let token = link.split_once('#').unwrap().1;
        let r = d
            .handle(req(
                "POST",
                "/api/login",
                &[JSON, ORIGIN],
                &json!({ "token": token }).to_string(),
            ))
            .await;
        assert_eq!(r.status, 200);
        let cookie = r
            .headers
            .iter()
            .find(|(k, _)| k == "Set-Cookie")
            .unwrap()
            .1
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let csrf: Value = serde_json::from_slice(&r.body).unwrap();
        (cookie, csrf["csrf"].as_str().unwrap().to_string())
    }

    #[tokio::test]
    async fn the_page_is_served_and_the_api_needs_a_session() {
        let (_d, d) = dash();
        let r = d.handle(req("GET", "/", &[], "")).await;
        assert_eq!(r.status, 200);
        assert!(String::from_utf8_lossy(&r.body).contains("app.js"));
        let r = d.handle(req("GET", "/api/session", &[], "")).await;
        assert_eq!(r.status, 401);
        let (cookie, csrf) = login(&d).await;
        let r = d
            .handle(req("GET", "/api/session", &[("cookie", &cookie)], ""))
            .await;
        assert_eq!(r.status, 200);
        assert!(String::from_utf8_lossy(&r.body).contains(&csrf));
    }

    #[tokio::test]
    async fn the_pages_own_files_are_gzipped_only_for_a_browser_that_takes_it() {
        use std::io::Read;
        let (_d, d) = dash();
        let has = |r: &Response, k: &str| {
            r.headers
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
        };
        for path in ["/", "/app.js", "/app.css", "/theme.js", "/lang-he.js"] {
            let plain = d.handle(req("GET", path, &[], "")).await;
            assert_eq!(has(&plain, "Content-Encoding"), None, "{path}");
            let zipped = d
                .handle(req(
                    "GET",
                    path,
                    &[("accept-encoding", "br, gzip;q=0.8")],
                    "",
                ))
                .await;
            assert_eq!(has(&zipped, "Content-Encoding").as_deref(), Some("gzip"));
            assert_eq!(
                has(&zipped, "Vary").as_deref(),
                Some("Accept-Encoding"),
                "{path}"
            );
            assert!(zipped.body.len() < plain.body.len(), "{path} shrinks");
            let mut back = Vec::new();
            flate2::read::GzDecoder::new(&zipped.body[..])
                .read_to_end(&mut back)
                .unwrap();
            assert_eq!(back, plain.body, "{path} round-trips");
            // `q=0` says no, and so does asking for nothing.
            for no in ["gzip;q=0", "identity", ""] {
                let r = d
                    .handle(req("GET", path, &[("accept-encoding", no)], ""))
                    .await;
                assert_eq!(has(&r, "Content-Encoding"), None, "{path} with {no:?}");
            }
        }
        // The API is never squeezed: a secret must not ride a compressed body.
        let r = d
            .handle(req(
                "GET",
                "/api/session",
                &[("accept-encoding", "gzip")],
                "",
            ))
            .await;
        assert_eq!(has(&r, "Content-Encoding"), None);
    }

    #[tokio::test]
    async fn fonts_are_cached_a_year_and_the_rest_not_at_all() {
        let (_d, d) = dash();
        let cache = |r: &Response| {
            r.headers
                .iter()
                .find(|(k, _)| k == "Cache-Control")
                .map(|(_, v)| v.clone())
        };
        let r = d
            .handle(req("GET", "/fonts/inter-latin.woff2", &[], ""))
            .await;
        assert_eq!((r.status, r.content_type), (200, "font/woff2"));
        assert_eq!(&r.body[..4], b"wOF2");
        assert!(cache(&r).unwrap().contains("max-age=31536000"));
        let r = d.handle(req("GET", "/fonts/OFL.txt", &[], "")).await;
        assert_eq!(r.status, 200);
        assert!(String::from_utf8_lossy(&r.body).contains("SIL Open Font License"));
        for name in FONTS.iter().map(|(n, _)| n) {
            let r = d
                .handle(req("GET", &format!("/fonts/{name}"), &[], ""))
                .await;
            assert_eq!(r.status, 200, "{name}");
        }
        let r = d.handle(req("GET", "/fonts/nope.woff2", &[], "")).await;
        assert_eq!(r.status, 404);
        let r = d.handle(req("GET", "/fonts/../app.js", &[], "")).await;
        assert_eq!(r.status, 404);
        // The page and its scripts change with the binary: never cached
        // (`http::write` adds no-store to anything without its own).
        for path in ["/", "/app.js", "/app.css", "/theme.js", "/lang-he.js"] {
            let r = d.handle(req("GET", path, &[], "")).await;
            assert_eq!(r.status, 200, "{path}");
            assert_eq!(cache(&r), None, "{path}");
        }
        let r = d.handle(req("GET", "/api/session", &[], "")).await;
        assert_eq!(cache(&r), None);
    }

    #[tokio::test]
    async fn the_new_fonts_and_their_licence_ship() {
        // M49: Inter, Heebo and Geist Mono replace IBM Plex. Each file is a
        // real woff2, cached for a year, and small; the licence names all three.
        let (_d, d) = dash();
        let caps = [
            ("inter-latin.woff2", 26_000),
            ("heebo-hebrew.woff2", 9_000),
            ("geist-mono-latin.woff2", 18_000),
        ];
        assert_eq!(FONTS.len(), caps.len());
        for (name, cap) in caps {
            let r = d
                .handle(req("GET", &format!("/fonts/{name}"), &[], ""))
                .await;
            assert_eq!((r.status, r.content_type), (200, "font/woff2"), "{name}");
            assert_eq!(&r.body[..4], b"wOF2", "{name}");
            assert_eq!(header(&r, "Cache-Control"), FOREVER, "{name}");
            assert!(
                r.body.len() < cap,
                "{name} is {} bytes, over {cap}",
                r.body.len()
            );
        }
        let r = d.handle(req("GET", "/fonts/OFL.txt", &[], "")).await;
        assert_eq!(
            (r.status, r.content_type),
            (200, "text/plain; charset=utf-8")
        );
        assert_eq!(header(&r, "Cache-Control"), FOREVER);
        let licence = String::from_utf8_lossy(&r.body);
        for who in [
            "The Inter Project Authors",
            "The Heebo Project Authors",
            "The Geist Project Authors",
            "SIL OPEN FONT LICENSE Version 1.1",
        ] {
            assert!(licence.contains(who), "OFL.txt lacks {who}");
        }
        for (what, src) in [
            ("app.css", APP_CSS),
            ("index.html", INDEX),
            ("OFL.txt", FONT_LICENSE),
        ] {
            assert!(
                !src.contains("Plex") && !src.contains("plex-"),
                "{what} still names Plex"
            );
        }
    }

    #[tokio::test]
    async fn a_foreign_host_is_refused_even_with_a_session() {
        let (_d, d) = dash();
        let (cookie, _) = login(&d).await;
        let r = d
            .handle(req(
                "GET",
                "/api/session",
                &[("cookie", &cookie), ("host", "rebind.example:4321")],
                "",
            ))
            .await;
        assert_eq!(r.status, 421);
        let r = d
            .handle(req("GET", "/", &[("host", "127.0.0.1:9999")], ""))
            .await;
        assert_eq!(r.status, 421);
    }

    #[tokio::test]
    async fn posts_need_the_csrf_token_the_origin_and_json() {
        let (_d, d) = dash();
        let (cookie, csrf) = login(&d).await;
        let c = ("cookie", cookie.as_str());
        let t = ("x-ferrule-csrf", csrf.as_str());
        let r = d
            .handle(req("POST", "/api/logout", &[c, JSON, ORIGIN], "{}"))
            .await;
        assert_eq!(r.status, 403, "no CSRF header");
        let r = d
            .handle(req(
                "POST",
                "/api/logout",
                &[c, JSON, ORIGIN, ("x-ferrule-csrf", "wrong")],
                "{}",
            ))
            .await;
        assert_eq!(r.status, 403, "a wrong CSRF token");
        let r = d
            .handle(req(
                "POST",
                "/api/logout",
                &[c, t, JSON, ("origin", "https://evil.example")],
                "{}",
            ))
            .await;
        assert_eq!(r.status, 403, "a foreign origin");
        let r = d
            .handle(req(
                "POST",
                "/api/logout",
                &[c, t, ORIGIN, ("content-type", "text/plain")],
                "{}",
            ))
            .await;
        assert_eq!(r.status, 403, "a form post");
        let r = d
            .handle(req("POST", "/api/logout", &[t, JSON, ORIGIN], "{}"))
            .await;
        assert_eq!(r.status, 401, "no session");
        let r = d
            .handle(req("POST", "/api/logout", &[c, t, JSON, ORIGIN], "{}"))
            .await;
        assert_eq!(r.status, 200);
        let r = d.handle(req("GET", "/api/session", &[c], "")).await;
        assert_eq!(r.status, 401, "logged out");
    }

    #[tokio::test]
    async fn a_link_logs_in_once_and_off_ends_every_session() {
        let (_d, d) = dash();
        let link = d.local_link().unwrap();
        let token = link.split_once('#').unwrap().1.to_string();
        let body = json!({ "token": token }).to_string();
        let r = d
            .handle(req("POST", "/api/login", &[JSON, ORIGIN], &body))
            .await;
        assert_eq!(r.status, 200);
        let r = d
            .handle(req("POST", "/api/login", &[JSON, ORIGIN], &body))
            .await;
        assert_eq!(r.status, 401, "a used link");
        let (cookie, _) = login(&d).await;
        d.off().await.unwrap();
        let r = d
            .handle(req("GET", "/api/session", &[("cookie", &cookie)], ""))
            .await;
        assert_eq!(r.status, 401);
    }

    #[tokio::test]
    async fn a_restart_ends_tunnel_sessions_and_keeps_local_ones() {
        let (_d, d) = dash();
        let (cookie, _) = login(&d).await;
        d.sessions
            .open("a-b.trycloudflare.com", d.links.revoked_ms(), None)
            .unwrap();
        assert_eq!(d.sessions.live(d.links.revoked_ms()), 2);
        // No cloudflared here: nothing to reopen, so no message.
        assert_eq!(d.relink_after_restart().await, None);
        assert_eq!(d.sessions.live(d.links.revoked_ms()), 1);
        let r = d
            .handle(req("GET", "/api/session", &[("cookie", &cookie)], ""))
            .await;
        assert_eq!(r.status, 200);
    }

    #[tokio::test]
    async fn a_link_made_for_the_tunnel_works_only_there() {
        let (_d, d) = dash();
        let token = d
            .links
            .mint(Some("a-b.trycloudflare.com"), Duration::from_secs(60))
            .unwrap();
        let body = json!({ "token": token }).to_string();
        let r = d
            .handle(req("POST", "/api/login", &[JSON, ORIGIN], &body))
            .await;
        assert_eq!(r.status, 401, "used on loopback, it's refused");
        // ...and not spent: the same link still works on its own host.
        let r = d
            .handle(req(
                "POST",
                "/api/login",
                &[
                    JSON,
                    ("host", "a-b.trycloudflare.com"),
                    ("origin", "https://a-b.trycloudflare.com"),
                ],
                &body,
            ))
            .await;
        assert_eq!(r.status, 200);
        let set = &r.headers.iter().find(|(k, _)| k == "Set-Cookie").unwrap().1;
        assert!(set.contains("Secure"), "{set}");
    }

    #[test]
    fn json_strings_are_redacted() {
        let r = Redactor::new(["sk-SEEDED-SECRET".to_string()]);
        let mut v = json!({"a": ["x sk-SEEDED-SECRET y", {"b": "sk-SEEDED-SECRET"}], "n": 1});
        redact_value(&mut v, &r);
        assert!(!v.to_string().contains("SEEDED"));
    }

    const PANEL_SECRET: &[u8] = b"test-panel-secret-0123456789abcdef0123";

    fn panel_dash(public: &str) -> (tempfile::TempDir, Arc<Dashboard>) {
        let dir = tempfile::tempdir().unwrap();
        let settings = DashboardConfig {
            public_url: Some(public.to_string()),
            ..DashboardConfig::default()
        };
        let d = Dashboard::new_with(
            settings,
            Links::at(dir.path().join("links.json")),
            Ctx::bare(Arc::new(Redactor::new(["sk-SEEDED-SECRET".to_string()]))),
            Some(panel::Key::new(PANEL_SECRET, "b_test")),
        );
        (dir, d)
    }

    fn panel_token(exp_from_now: i64, nonce: &str, secret: &[u8], bot: &str) -> String {
        let exp = auth::now_secs() as i64 + exp_from_now;
        panel::sign(
            &json!({ "bot": bot, "user": "u_1", "exp": exp, "nonce": nonce }),
            secret,
        )
    }

    async fn panel_post(d: &Dashboard, path: &str, token: &str, origin: &str) -> Response {
        d.handle(req(
            "POST",
            path,
            &[JSON, ("host", "bots.test"), ("origin", origin)],
            &json!({ "token": token }).to_string(),
        ))
        .await
    }

    fn header(r: &Response, name: &str) -> String {
        r.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    fn head(path: &str, headers: &[(&str, &str)]) -> http::Head {
        http::Head {
            method: "POST".into(),
            path: path.into(),
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[tokio::test]
    async fn only_a_signed_in_page_may_send_a_big_body_to_the_photo_route_even_under_a_prefix() {
        let (_dir, d) = panel_dash("http://bots.test/b/b_test/");
        let t = panel_token(60, "0123456789abcdef", PANEL_SECRET, "b_test");
        let r = panel_post(&d, "/b/b_test/api/login", &t, "http://bots.test").await;
        let cookie = header(&r, "Set-Cookie");
        let cookie = cookie.split(';').next().unwrap().to_string();
        let csrf = serde_json::from_slice::<Value>(&r.body).unwrap()["csrf"]
            .as_str()
            .unwrap()
            .to_string();
        let signed = |path: &str| {
            head(
                path,
                &[
                    ("host", "bots.test"),
                    ("origin", "http://bots.test"),
                    ("content-type", "application/json"),
                    ("cookie", cookie.as_str()),
                    (auth::CSRF_HEADER, csrf.as_str()),
                ],
            )
        };
        let big = |h: &http::Head| d.limit_for(h).body > 1024 * 1024;
        // Signed in, at the prefixed path or the bare one a proxy leaves.
        assert!(big(&signed("/b/b_test/api/chat/photo")));
        assert!(big(&signed("/api/chat/photo")));
        // Not the photo route, not a POST, no CSRF, another origin, no cookie.
        assert!(!big(&signed("/b/b_test/api/chat/send")));
        let mut get = signed("/b/b_test/api/chat/photo");
        get.method = "GET".into();
        assert!(!big(&get));
        let mut no_csrf = signed("/b/b_test/api/chat/photo");
        no_csrf.headers.remove(auth::CSRF_HEADER);
        assert!(!big(&no_csrf));
        let mut wrong_csrf = signed("/b/b_test/api/chat/photo");
        wrong_csrf
            .headers
            .insert(auth::CSRF_HEADER.into(), "nope".into());
        assert!(!big(&wrong_csrf));
        let mut cross = signed("/b/b_test/api/chat/photo");
        cross
            .headers
            .insert("origin".into(), "http://evil.test".into());
        assert!(!big(&cross));
        let mut anon = signed("/b/b_test/api/chat/photo");
        anon.headers.remove("cookie");
        assert!(!big(&anon));
        let mut host = signed("/b/b_test/api/chat/photo");
        host.headers.insert("host".into(), "evil.test".into());
        assert!(!big(&host));
        // And the usual limit is the small one.
        assert_eq!(d.limit_for(&anon).body, http::Limit::default().body);
    }

    #[tokio::test]
    async fn a_good_token_opens_a_session() {
        let (_dir, d) = panel_dash("http://bots.test/b/b_test/");
        let t = panel_token(60, "0123456789abcdef", PANEL_SECRET, "b_test");
        let r = panel_post(&d, "/b/b_test/api/login", &t, "http://bots.test").await;
        assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
        let cookie = header(&r, "Set-Cookie");
        assert!(cookie.contains("Path=/b/b_test/"), "{cookie}");
        assert!(!cookie.contains("Secure"), "{cookie}");
        let r = d
            .handle(req(
                "GET",
                "/b/b_test/api/session",
                &[
                    ("host", "bots.test"),
                    ("cookie", cookie.split(';').next().unwrap()),
                ],
                "",
            ))
            .await;
        assert_eq!(r.status, 200);
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["user"], "u_1");
    }

    #[tokio::test]
    async fn a_bad_panel_token_is_refused_the_same_way_every_time() {
        let (_dir, d) = panel_dash("http://bots.test/b/b_test/");
        let bad = [
            (
                "expired",
                panel_token(-5, "0123456789abcde1", PANEL_SECRET, "b_test"),
            ),
            (
                "signature",
                panel_token(60, "0123456789abcde2", b"another", "b_test"),
            ),
            (
                "other bot",
                panel_token(60, "0123456789abcde3", PANEL_SECRET, "b_other"),
            ),
            (
                "too far ahead",
                panel_token(600, "0123456789abcde4", PANEL_SECRET, "b_test"),
            ),
        ];
        for (what, t) in bad {
            let r = panel_post(&d, "/api/login", &t, "http://bots.test").await;
            assert_eq!(r.status, 401, "{what}");
            let v: Value = serde_json::from_slice(&r.body).unwrap();
            assert_eq!(v["error"], panel::REFUSED, "{what}");
        }
        // A replay: the second use of a good one.
        let t = panel_token(60, "0123456789abcde5", PANEL_SECRET, "b_test");
        assert_eq!(
            panel_post(&d, "/api/login", &t, "http://bots.test")
                .await
                .status,
            200
        );
        let r = panel_post(&d, "/api/login", &t, "http://bots.test").await;
        assert_eq!(r.status, 401);
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["error"], panel::REFUSED);
    }

    #[tokio::test]
    async fn without_a_panel_key_panel_tokens_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let settings = DashboardConfig {
            public_url: Some("http://bots.test/".into()),
            ..DashboardConfig::default()
        };
        let d = Dashboard::new_with(
            settings,
            Links::at(dir.path().join("links.json")),
            Ctx::bare(Arc::new(Redactor::new([]))),
            None,
        );
        let t = panel_token(60, "0123456789abcdef", PANEL_SECRET, "b_test");
        let r = panel_post(&d, "/api/login", &t, "http://bots.test").await;
        assert_eq!(r.status, 401);
    }

    #[tokio::test]
    async fn the_page_works_under_a_path_prefix() {
        let (_dir, d) = panel_dash("https://bots.test/b/b_test/");
        let get = |path: &str| req("GET", path, &[("host", "bots.test")], "");
        let r = d.handle(get("/b/b_test/")).await;
        assert_eq!(r.status, 200);
        assert!(String::from_utf8_lossy(&r.body).contains("content=\"/b/b_test/\""));
        assert_eq!(d.handle(get("/b/b_test/app.js")).await.status, 200);
        // A proxy that strips the prefix itself.
        assert_eq!(d.handle(get("/app.js")).await.status, 200);
        let t = panel_token(60, "0123456789abcdef", PANEL_SECRET, "b_test");
        let r = panel_post(&d, "/b/b_test/api/login", &t, "https://bots.test").await;
        assert_eq!(r.status, 200);
        let cookie = header(&r, "Set-Cookie");
        assert!(
            cookie.contains("Secure") && cookie.contains("Path=/b/b_test/"),
            "{cookie}"
        );
        let t = panel_token(60, "0123456789abcde9", PANEL_SECRET, "b_test");
        let r = panel_post(&d, "/b/b_test/api/login", &t, "http://bots.test").await;
        assert_eq!(r.status, 403);
        assert!(String::from_utf8_lossy(&r.body).contains("wrong origin"));
    }

    #[test]
    fn the_index_carries_the_base() {
        let html = index_for("/b/b_x");
        for want in [
            "content=\"/b/b_x/\"",
            "src=\"/b/b_x/theme.js\"",
            "href=\"/b/b_x/fonts/inter-latin.woff2\"",
            "href=\"/b/b_x/app.css\"",
            "src=\"/b/b_x/app.js\"",
        ] {
            assert!(html.contains(want), "{want} in {html}");
        }
        assert!(index_for("").contains("src=\"/app.js\""));
    }

    #[tokio::test]
    async fn health_answers_before_the_host_check_and_without_a_session() {
        let d = Dashboard::new_with(
            DashboardConfig::default(),
            Links::at(tempfile::tempdir().unwrap().path().join("links.json")),
            Ctx::bare(Arc::new(Redactor::new([]))),
            None,
        );
        let r = d
            .handle(req("GET", "/healthz", &[("host", "evil.test")], ""))
            .await;
        assert!(r.status != 421 && r.status != 401, "{}", r.status);
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
        // No gateway behind this page: it says so.
        assert_eq!(v["status"], "failing");
        assert_eq!(r.status, 503);
        let r = d
            .handle(req("POST", "/healthz", &[("host", "evil.test")], ""))
            .await;
        assert_eq!(r.status, 405);
        let r = d
            .handle(req("GET", "/busyz", &[("host", "evil.test")], ""))
            .await;
        assert_eq!(r.status, 200);
    }

    #[tokio::test]
    async fn a_non_loopback_bind_without_a_public_url_answers_health_only() {
        let dir = tempfile::tempdir().unwrap();
        let settings = DashboardConfig {
            bind: "0.0.0.0".into(),
            ..DashboardConfig::default()
        };
        let d = Dashboard::new_with(
            settings,
            Links::at(dir.path().join("links.json")),
            Ctx::bare(Arc::new(Redactor::new([]))),
            None,
        );
        if d.bind(0).await.is_err() {
            eprintln!("skipped: can't bind 0.0.0.0 here");
            return;
        }
        let port = d.port();
        let r = d
            .handle(req(
                "GET",
                "/",
                &[("host", &format!("127.0.0.1:{port}"))],
                "",
            ))
            .await;
        assert_eq!(r.status, 421);
        assert!(String::from_utf8_lossy(&r.body).contains("has no public address"));
        let r = d.handle(req("GET", "/healthz", &[], "")).await;
        assert_ne!(r.status, 421);
    }
}
