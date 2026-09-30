//! M22: one page for the whole app (docs/m22-dashboard.md). The gateway
//! serves it on 127.0.0.1; `/dashboard` in Telegram opens a cloudflared
//! quick tunnel for the phone and sends a one-time login link. Nothing here
//! calls a model: the page must work when the model doesn't.

pub mod api;
pub mod auth;
pub mod channels;
pub mod chat;
pub mod cli;
pub mod config_page;
pub mod console;
pub mod door;
mod healthz;
pub mod http;
pub mod models_page;
pub mod notices;
pub mod panel;
mod public;
pub mod runs;
mod telegram;
#[cfg(test)]
pub mod testing;

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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;

const INDEX: &str = include_str!("assets/index.html");
const APP_JS: &str = include_str!("assets/app.js");
const APP_CSS: &str = include_str!("assets/app.css");
/// Sets the theme before the first paint (the CSP allows no inline script).
const THEME_JS: &str = include_str!("assets/theme.js");
/// IBM Plex (OFL 1.1), served from the binary so the page never depends on
/// a font CDN: Sans and Mono cut to Latin-1, Sans Hebrew whole, each loaded
/// only when the page has a character in its `unicode-range`.
const FONTS: &[(&str, &[u8])] = &[
    (
        "plex-sans-400.woff2",
        include_bytes!("assets/fonts/plex-sans-400.woff2"),
    ),
    (
        "plex-sans-600.woff2",
        include_bytes!("assets/fonts/plex-sans-600.woff2"),
    ),
    (
        "plex-sans-hebrew-400.woff2",
        include_bytes!("assets/fonts/plex-sans-hebrew-400.woff2"),
    ),
    (
        "plex-sans-hebrew-600.woff2",
        include_bytes!("assets/fonts/plex-sans-hebrew-600.woff2"),
    ),
    (
        "plex-mono-400.woff2",
        include_bytes!("assets/fonts/plex-mono-400.woff2"),
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
    #[allow(dead_code)] // M44 part 5
    started: Instant,
    pub ctx: Ctx,
}

const HEALTH_ONLY: &str = "this bot's page has no public address: set FERRULE_PUBLIC_URL (or [dashboard] public_url) to the URL it is opened at";

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
        ("href", "/fonts/plex-sans-400.woff2"),
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
                    let resp = match http::read(&mut conn).await {
                        Ok(req) => me.handle(req).await,
                        Err(0) => return,
                        Err(status) => Response::text(status, "bad request"),
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
    pub async fn remote_link(&self) -> Result<String> {
        let host = self.open_tunnel().await?;
        let token = self
            .links
            .mint(Some(&host), minutes(self.settings.link_minutes))?;
        self.touch();
        Ok(format!("https://{host}/login#{token}"))
    }

    /// Opens the quick tunnel (or keeps the open one): its host.
    pub async fn open_tunnel(&self) -> Result<String> {
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
                return host_of(&t.url);
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
        Ok(host)
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
                door::link_text(self, &link, true)
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
        let get = req.method == "GET" || req.method == "HEAD";
        match (req.method.as_str(), req.path.as_str()) {
            (_, "/" | "/login" | "/index.html") if get => {
                return Response::new(200, "text/html; charset=utf-8", self.index.clone())
            }
            (_, "/app.js") if get => {
                return Response::new(200, "text/javascript; charset=utf-8", APP_JS)
            }
            (_, "/app.css") if get => {
                return Response::new(200, "text/css; charset=utf-8", APP_CSS)
            }
            (_, "/theme.js") if get => {
                return Response::new(200, "text/javascript; charset=utf-8", THEME_JS)
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
        let cookie = req.cookie(auth::COOKIE);
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
        match (get, req.path.as_str()) {
            (true, "/api/session") => {
                Response::json(200, &json!({ "csrf": granted.csrf, "user": granted.user }))
            }
            (false, "/api/logout") => {
                if let Some(c) = cookie {
                    self.sessions.close(c);
                }
                Response::json(200, &json!({ "ok": true })).with_header(
                    "Set-Cookie",
                    auth::clear_cookie(secure, &self.cookie_path(&host)),
                )
            }
            _ => match self.api(get, &req, &body).await {
                Some((status, mut value)) => {
                    redact_value(&mut value, &self.ctx.redactor);
                    Response::json(status, &value)
                }
                None => refuse(404, "no such endpoint"),
            },
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
        Response::json(200, &json!({ "csrf": csrf })).with_header(
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
        let used = match self.links.consume(token) {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!("dashboard links: {e:#}");
                return refuse(503, "the link store can't be read; try again");
            }
        };
        match used {
            Some(u) if u.host.as_deref().is_none_or(|h| h == host) => {
                let (cookie, csrf) = match self.sessions.open(host, self.links.revoked_ms(), None) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!("dashboard sessions: {e:#}");
                        return refuse(503, "the session store can't be written; try again");
                    }
                };
                self.touch();
                Response::json(200, &json!({ "csrf": csrf })).with_header(
                    "Set-Cookie",
                    auth::set_cookie(
                        &cookie,
                        self.secure_for(host),
                        minutes(self.settings.session_hours * 60),
                        &self.cookie_path(host),
                    ),
                )
            }
            _ => refuse(
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
    async fn fonts_are_cached_a_year_and_the_rest_not_at_all() {
        let (_d, d) = dash();
        let cache = |r: &Response| {
            r.headers
                .iter()
                .find(|(k, _)| k == "Cache-Control")
                .map(|(_, v)| v.clone())
        };
        let r = d
            .handle(req("GET", "/fonts/plex-sans-400.woff2", &[], ""))
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
        for path in ["/", "/app.js", "/app.css", "/theme.js"] {
            let r = d.handle(req("GET", path, &[], "")).await;
            assert_eq!(r.status, 200, "{path}");
            assert_eq!(cache(&r), None, "{path}");
        }
        let r = d.handle(req("GET", "/api/session", &[], "")).await;
        assert_eq!(cache(&r), None);
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
        assert_eq!(r.status, 401, "used on loopback, it's spent and refused");
        let token = d
            .links
            .mint(Some("a-b.trycloudflare.com"), Duration::from_secs(60))
            .unwrap();
        let body = json!({ "token": token }).to_string();
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
            "href=\"/b/b_x/fonts/plex-sans-400.woff2\"",
            "href=\"/b/b_x/app.css\"",
            "src=\"/b/b_x/app.js\"",
        ] {
            assert!(html.contains(want), "{want} in {html}");
        }
        assert!(index_for("").contains("src=\"/app.js\""));
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
