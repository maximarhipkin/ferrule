//! M22: one page for the whole app (docs/m22-dashboard.md). The gateway
//! serves it on 127.0.0.1; `/dashboard` in Telegram opens a cloudflared
//! quick tunnel for the phone and sends a one-time login link. Nothing here
//! calls a model: the page must work when the model doesn't.

pub mod auth;
pub mod cli;
pub mod door;
pub mod http;

use crate::config::{Config, DashboardConfig};
use anyhow::{bail, Context, Result};
use auth::{Links, Sessions};
use ferrule_connections::tunnel::{self, Tunnel};
use ferrule_gateway::Redactor;
use http::{Request, Response};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;

const INDEX: &str = include_str!("assets/index.html");
const APP_JS: &str = include_str!("assets/app.js");
const APP_CSS: &str = include_str!("assets/app.css");

/// What the page reads and changes; each piece is there when the process
/// serving it has one (the gateway has them all, `ferrule dashboard` on
/// its own fewer).
pub struct Ctx {
    pub redactor: Arc<Redactor>,
    /// `[connections] cloudflared`, resolved.
    pub cloudflared: Option<PathBuf>,
}

impl Ctx {
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            redactor: Arc::new(crate::health::redactor(cfg)),
            cloudflared: cloudflared(cfg),
        }
    }
}

/// `[connections] cloudflared`: a path, "off", or looked up on PATH.
pub fn cloudflared(cfg: &Config) -> Option<PathBuf> {
    match cfg.connections.cloudflared.as_deref() {
        Some("off") => None,
        Some(path) => Some(PathBuf::from(path)),
        None => ferrule_mcp::browser::find_command("cloudflared"),
    }
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
    pub ctx: Ctx,
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
        let sessions = Sessions::new(
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
            ctx,
        })
    }

    pub fn settings(&self) -> &DashboardConfig {
        &self.settings
    }

    /// The port it listens on; 0 before [`Dashboard::bind`].
    pub fn port(&self) -> u16 {
        self.port.load(Ordering::Relaxed)
    }

    /// Listens on `127.0.0.1:<port>` (never any other address) and serves
    /// until dropped; also closes the tunnel once it's idle.
    pub async fn bind(self: &Arc<Self>, port: u16) -> Result<u16> {
        let listener = TcpListener::bind(("127.0.0.1", port))
            .await
            .with_context(|| format!("listening on 127.0.0.1:{port}"))?;
        let port = listener.local_addr()?.port();
        self.port.store(port, Ordering::Relaxed);
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
        if dead || (idle && self.links.pending() == 0 && self.sessions.live() == 0) {
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
        Ok(format!("http://127.0.0.1:{}/login#{token}", self.port()))
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
        if self.settings.remote != "tunnel" {
            bail!(
                "[dashboard] remote is \"{}\", not \"tunnel\"",
                self.settings.remote
            );
        }
        let Some(bin) = self.ctx.cloudflared.clone() else {
            bail!("cloudflared isn't installed (or [connections] cloudflared is \"off\")");
        };
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
        self.sessions.clear();
        *self.tunnel.lock().await = None;
        *self.tunnel_host.lock().unwrap() = None;
        self.links.revoke()
    }

    /// Loopback with our port, the open tunnel's host, and the hosts links
    /// and sessions were made for (a tunnel `ferrule dashboard link
    /// --remote` opened from another process).
    fn host_allowed(&self, host: &str) -> bool {
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
            || self.sessions.hosts().iter().any(|h| h == host)
    }

    fn is_loopback(&self, host: &str) -> bool {
        host.starts_with("127.0.0.1:")
            || host.starts_with("localhost:")
            || host.starts_with("[::1]:")
    }

    /// The origin a same-origin request from `host` carries.
    fn origin_for(&self, host: &str) -> String {
        if self.is_loopback(host) {
            format!("http://{host}")
        } else {
            format!("https://{host}")
        }
    }

    pub async fn handle(&self, req: Request) -> Response {
        let host = req.header("host").unwrap_or("").to_ascii_lowercase();
        if !self.host_allowed(&host) {
            return Response::text(421, "unknown host");
        }
        let get = req.method == "GET" || req.method == "HEAD";
        match (req.method.as_str(), req.path.as_str()) {
            (_, "/" | "/login" | "/index.html") if get => {
                return Response::new(200, "text/html; charset=utf-8", INDEX)
            }
            (_, "/app.js") if get => {
                return Response::new(200, "text/javascript; charset=utf-8", APP_JS)
            }
            (_, "/app.css") if get => {
                return Response::new(200, "text/css; charset=utf-8", APP_CSS)
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
        let secure = !self.is_loopback(&host);
        match (get, req.path.as_str()) {
            (true, "/api/session") => Response::json(200, &json!({ "csrf": granted.csrf })),
            (false, "/api/logout") => {
                if let Some(c) = cookie {
                    self.sessions.close(c);
                }
                Response::json(200, &json!({ "ok": true }))
                    .with_header("Set-Cookie", auth::clear_cookie(secure))
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

    fn login(&self, host: &str, body: &Value) -> Response {
        let token = body.get("token").and_then(Value::as_str).unwrap_or("");
        let used = match self.links.consume(token) {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!("dashboard links: {e:#}");
                return refuse(503, "the link store can't be read; try again");
            }
        };
        match used {
            Some(u) if u.host.as_deref().is_none_or(|h| h == host) => {
                let (cookie, csrf) = self.sessions.open(host);
                self.touch();
                let secure = !self.is_loopback(host);
                Response::json(200, &json!({ "csrf": csrf })).with_header(
                    "Set-Cookie",
                    auth::set_cookie(&cookie, secure, minutes(self.settings.session_hours * 60)),
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
        let _ = (get, req, body);
        None
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn dash() -> (tempfile::TempDir, Arc<Dashboard>) {
        let dir = tempfile::tempdir().unwrap();
        let d = Dashboard::new(
            DashboardConfig::default(),
            Links::at(dir.path().join("links.json")),
            Ctx {
                redactor: Arc::new(Redactor::new(["sk-SEEDED-SECRET".to_string()])),
                cloudflared: None,
            },
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
}
