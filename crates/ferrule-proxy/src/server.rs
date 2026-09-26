//! The proxy itself: an HTTP/1 proxy on loopback. Tunnels to hosts no
//! secret is bound to are passed through untouched; tunnels to bound hosts
//! are terminated with a local certificate so each request can have its
//! placeholders swapped for real values, and each response scrubbed back.
//!
//! Plain HTTP (absolute-form requests) is forwarded too, so ferrule's own
//! `http://` fetches leave the same way. Secrets only go over it to loopback
//! servers: a bound remote host asked for over `http://` is refused, since
//! anyone on the path would see the real value.

use crate::ca::Ca;
use crate::egress::{Denial, EgressPolicy, Source, Verdict};
use crate::hosts::HostPattern;
use crate::subst::{Scrubbed, Swaps};
use crate::upstream::{self, Upstream};
use anyhow::{Context as _, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use bytes::Bytes;
use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use http::{Method, Request, Response, StatusCode, Uri, Version};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt as _, Full};
use hyper::body::Incoming;
use hyper::client::conn::http1::SendRequest;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use rustls::ClientConfig;
use rustls_pki_types::ServerName;
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio_rustls::{TlsAcceptor, TlsConnector};
use tracing::debug;

pub(crate) type ProxyBody = BoxBody<Bytes, hyper::Error>;

/// Set on every response the egress policy produced, so a client can tell
/// a refusal from the site's own 403.
pub const EGRESS_HEADER: &str = "x-ferrule-egress";
const REPORT_EVERY: Duration = Duration::from_secs(10);

pub(crate) struct Secret {
    pub hosts: Vec<HostPattern>,
    pub placeholder: String,
    pub real: String,
    /// Also swapped in the URL, not just credential headers.
    pub in_url: bool,
}

pub(crate) type DenyHook = Arc<dyn Fn(&Denial) + Send + Sync>;

pub(crate) struct Shared {
    /// Decoded `Proxy-Authorization` credentials a request must carry:
    /// `ferrule:<token>` for commands, `ferrule-tool:<token>` for ferrule's
    /// own clients.
    pub expected_auth: Vec<u8>,
    pub tool_auth: Vec<u8>,
    /// `None`: no policy at all (the pre-M33 proxy, secrets only).
    pub egress: Option<EgressPolicy>,
    pub deny_hook: std::sync::RwLock<Option<DenyHook>>,
    pub denials: AtomicU64,
    /// When each (source, host, reason) was last reported, so a retry loop
    /// doesn't flood the ledger. Enforcement isn't rate-limited.
    pub reported: std::sync::Mutex<HashMap<(Source, String, &'static str), Instant>>,
    pub ca: Ca,
    /// Grows through `Broker::bind`; a tunnel takes its swaps when it opens.
    pub secrets: std::sync::RwLock<Vec<Secret>>,
    pub upstream: Option<Upstream>,
    pub http_upstream: Option<Upstream>,
    pub tls_client: Arc<ClientConfig>,
}

impl Shared {
    /// Counts a denial and hands it to the hook, at most once per 10 s for
    /// the same source, host and reason.
    fn report(&self, denial: &Denial) {
        self.denials.fetch_add(1, Ordering::Relaxed);
        tracing::info!(
            "egress denied: {} {} {}:{} ({})",
            denial.source,
            denial.method,
            denial.host,
            denial.port,
            denial.reason.as_str()
        );
        let key = (denial.source, denial.host.clone(), denial.reason.as_str());
        {
            let mut seen = self.reported.lock().unwrap();
            let now = Instant::now();
            if seen
                .get(&key)
                .is_some_and(|t| now.duration_since(*t) < REPORT_EVERY)
            {
                return;
            }
            if seen.len() > 1024 {
                seen.retain(|_, t| now.duration_since(*t) < REPORT_EVERY);
            }
            seen.insert(key, now);
        }
        let hook = self.deny_hook.read().unwrap().clone();
        if let Some(hook) = hook {
            hook(denial);
        }
    }

    /// Which client this is, from its proxy credentials.
    fn source(&self, headers: &HeaderMap) -> Option<Source> {
        let got = credentials(headers)?;
        if constant_time_eq(&got, &self.expected_auth) {
            Some(Source::Command)
        } else if constant_time_eq(&got, &self.tool_auth) {
            Some(Source::Tool)
        } else {
            None
        }
    }

    /// Runs the policy on one request. `Ok(None)`: no policy, or the
    /// upstream proxy connects; `Ok(Some(addrs))`: connect to exactly these.
    async fn vet(
        &self,
        source: Source,
        method: &str,
        host: &str,
        port: u16,
        upstream: Option<&Upstream>,
    ) -> Result<Option<Vec<SocketAddr>>, Result<Denial, String>> {
        let Some(policy) = &self.egress else {
            return Ok(None);
        };
        let via = upstream.is_some_and(|u| !u.bypasses(host));
        match policy.vet(source, method, host, port, via).await {
            Verdict::Addrs(a) => Ok(Some(a)),
            Verdict::Upstream => Ok(None),
            Verdict::Deny(d) => {
                self.report(&d);
                Err(Ok(d))
            }
            Verdict::Unresolved(e) => Err(Err(e)),
        }
    }

    /// Every secret bound to `host`, or `None` when the tunnel stays blind.
    fn swaps_for(&self, host: &str) -> Option<Arc<Swaps>> {
        let pairs: Vec<_> = self
            .secrets
            .read()
            .unwrap()
            .iter()
            .filter(|s| s.hosts.iter().any(|h| h.matches(host)))
            .map(|s| (s.placeholder.clone(), s.real.clone(), s.in_url))
            .collect();
        (!pairs.is_empty()).then(|| Arc::new(Swaps::new(pairs)))
    }
}

pub(crate) async fn serve(listener: TcpListener, shared: Arc<Shared>) {
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(e) => {
                // Out of descriptors, usually; don't spin.
                debug!("proxy accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let shared = shared.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req| handle(req, shared.clone()));
            let conn = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), svc)
                .with_upgrades();
            if let Err(e) = conn.await {
                debug!("proxy connection ended: {e}");
            }
        });
    }
}

async fn handle(
    mut req: Request<Incoming>,
    shared: Arc<Shared>,
) -> Result<Response<ProxyBody>, Infallible> {
    let Some(source) = shared.source(req.headers()) else {
        let mut resp = text(
            StatusCode::PROXY_AUTHENTICATION_REQUIRED,
            "ferrule proxy: credentials required\n",
        );
        resp.headers_mut().insert(
            header::PROXY_AUTHENTICATE,
            HeaderValue::from_static("Basic realm=\"ferrule\""),
        );
        return Ok(resp);
    };
    if req.method() != Method::CONNECT {
        return Ok(plain_http(req, shared, source).await);
    }
    let Some((host, port)) = connect_target(req.uri()) else {
        return Ok(text(
            StatusCode::BAD_REQUEST,
            "ferrule proxy: CONNECT needs host:port\n",
        ));
    };
    let addrs = match shared
        .vet(source, "CONNECT", &host, port, shared.upstream.as_ref())
        .await
    {
        Ok(addrs) => addrs,
        Err(Ok(denial)) => return Ok(deny_tunnel(req, host, denial, shared)),
        Err(Err(e)) => {
            return Ok(text(
                StatusCode::BAD_GATEWAY,
                &format!("ferrule proxy: {e}\n"),
            ))
        }
    };
    // Connect before answering, so a dead host is a 502 the client can
    // report rather than a tunnel that closes on the first byte.
    let tcp = match upstream::connect(shared.upstream.as_ref(), &host, port, addrs.as_deref()).await
    {
        Ok(tcp) => tcp,
        Err(e) => {
            return Ok(text(
                StatusCode::BAD_GATEWAY,
                &format!("ferrule proxy: {e:#}\n"),
            ))
        }
    };
    let swaps = shared.swaps_for(&host);
    let on_upgrade = hyper::upgrade::on(&mut req);
    tokio::spawn(async move {
        let client = match on_upgrade.await {
            Ok(u) => TokioIo::new(u),
            Err(e) => return debug!("CONNECT {host}:{port} upgrade failed: {e}"),
        };
        let result = match swaps {
            None => blind(client, tcp).await,
            Some(swaps) => mitm(client, tcp, host.clone(), port, addrs, swaps, shared).await,
        };
        if let Err(e) = result {
            debug!("tunnel to {host}:{port} ended: {e:#}");
        }
    });
    Ok(Response::new(empty()))
}

async fn blind(mut client: TokioIo<hyper::upgrade::Upgraded>, mut tcp: TcpStream) -> Result<()> {
    tokio::io::copy_bidirectional(&mut client, &mut tcp).await?;
    Ok(())
}

/// Answers a refused CONNECT inside the tunnel: TLS with the proxy's CA,
/// then a 403 that says why on every request. A 403 to the CONNECT itself
/// gets reduced to "tunnel failed" by most clients, so the model would
/// never read the reason.
fn deny_tunnel(
    mut req: Request<Incoming>,
    host: String,
    denial: Denial,
    shared: Arc<Shared>,
) -> Response<ProxyBody> {
    let on_upgrade = hyper::upgrade::on(&mut req);
    let msg = denial.message("https");
    tokio::spawn(async move {
        let client = match on_upgrade.await {
            Ok(u) => TokioIo::new(u),
            Err(e) => return debug!("CONNECT {host} upgrade failed: {e}"),
        };
        let Ok(cfg) = shared.ca.server_config(&host) else {
            return;
        };
        let Ok(tls) = TlsAcceptor::from(cfg).accept(client).await else {
            return debug!("denied tunnel to {host}: the client didn't finish TLS");
        };
        let svc = service_fn(move |_req: Request<Incoming>| {
            let msg = msg.clone();
            async move { Ok::<_, Infallible>(denied(&msg)) }
        });
        let _ = http1::Builder::new()
            .keep_alive(false)
            .serve_connection(TokioIo::new(tls), svc)
            .await;
    });
    Response::new(empty())
}

/// The 403 for a refused request.
fn denied(msg: &str) -> Response<ProxyBody> {
    let mut resp = text(StatusCode::FORBIDDEN, msg);
    resp.headers_mut()
        .insert(EGRESS_HEADER, HeaderValue::from_static("denied"));
    resp
}

async fn mitm(
    client: TokioIo<hyper::upgrade::Upgraded>,
    tcp: TcpStream,
    host: String,
    port: u16,
    addrs: Option<Vec<SocketAddr>>,
    swaps: Arc<Swaps>,
    shared: Arc<Shared>,
) -> Result<()> {
    let acceptor = TlsAcceptor::from(shared.ca.server_config(&host)?);
    let tls = acceptor
        .accept(client)
        .await
        .with_context(|| format!("TLS from the client for {host} (does it trust ferrule's CA?)"))?;
    let origin = Arc::new(Origin {
        host,
        port,
        addrs,
        shared,
        first: std::sync::Mutex::new(Some(tcp)),
        sender: Mutex::new(None),
    });
    let svc = service_fn(move |req| forward(req, origin.clone(), swaps.clone()));
    http1::Builder::new()
        .serve_connection(TokioIo::new(tls), svc)
        .await?;
    Ok(())
}

/// The real server behind one intercepted tunnel. The TLS connection to it
/// is made on the first request and remade if the server closes it.
struct Origin {
    host: String,
    port: u16,
    /// What the egress policy checked; redials go here, not to a fresh
    /// lookup that a rebinding DNS server could answer differently.
    addrs: Option<Vec<SocketAddr>>,
    shared: Arc<Shared>,
    /// The socket opened before the CONNECT was answered, used by the first dial.
    first: std::sync::Mutex<Option<TcpStream>>,
    sender: Mutex<Option<SendRequest<Incoming>>>,
}

impl Origin {
    async fn dial(&self) -> Result<SendRequest<Incoming>> {
        let first = self.first.lock().unwrap().take();
        let tcp = match first {
            Some(tcp) => tcp,
            None => {
                upstream::connect(
                    self.shared.upstream.as_ref(),
                    &self.host,
                    self.port,
                    self.addrs.as_deref(),
                )
                .await?
            }
        };
        let name = ServerName::try_from(self.host.clone())?;
        let tls = TlsConnector::from(self.shared.tls_client.clone())
            .connect(name, tcp)
            .await
            .with_context(|| format!("TLS to {}", self.host))?;
        let (sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls)).await?;
        let host = self.host.clone();
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                debug!("connection to {host} ended: {e}");
            }
        });
        Ok(sender)
    }

    /// Sends on the open connection, redialling once if the server had
    /// closed it before the request went out (safe: nothing was sent).
    async fn send(&self, mut req: Request<Incoming>) -> Result<Response<Incoming>> {
        let mut guard = self.sender.lock().await;
        let mut retried = false;
        loop {
            if guard.as_ref().is_none_or(|s| s.is_closed()) {
                *guard = Some(self.dial().await?);
            }
            let sender = guard.as_mut().expect("dialled above");
            if let Err(e) = sender.ready().await {
                *guard = None;
                if retried {
                    return Err(e.into());
                }
                retried = true;
                continue;
            }
            match sender.try_send_request(req).await {
                Ok(resp) => return Ok(resp),
                Err(mut e) => {
                    *guard = None;
                    match e.take_message() {
                        Some(back) if !retried => {
                            retried = true;
                            req = back;
                        }
                        _ => return Err(e.into_error().into()),
                    }
                }
            }
        }
    }
}

async fn forward(
    req: Request<Incoming>,
    origin: Arc<Origin>,
    swaps: Arc<Swaps>,
) -> Result<Response<ProxyBody>, Infallible> {
    let (mut parts, body) = req.into_parts();
    // A Host naming another site would let a shared front end (a CDN, say)
    // route the real secret to someone else's app.
    let named = parts.uri.host().map(str::to_string).or_else(|| {
        parts
            .headers
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .map(host_of)
    });
    if let Some(named) = named {
        if !named
            .trim_end_matches('.')
            .eq_ignore_ascii_case(&origin.host)
        {
            return Ok(text(
                StatusCode::MISDIRECTED_REQUEST,
                &format!(
                    "ferrule proxy: request for {named} on a tunnel to {}\n",
                    origin.host
                ),
            ));
        }
    }
    if parts.headers.contains_key(header::UPGRADE) {
        return Ok(text(
            StatusCode::NOT_IMPLEMENTED,
            "ferrule proxy: protocol upgrades (websockets) aren't supported on hosts with secrets\n",
        ));
    }

    strip_hop_by_hop(&mut parts.headers);
    // Identity bodies only, so the response can be scrubbed.
    parts.headers.remove(header::ACCEPT_ENCODING);
    parts.headers.remove(header::EXPECT);
    if !parts.headers.contains_key(header::HOST) {
        let host = if origin.port == 443 {
            origin.host.clone()
        } else {
            format!("{}:{}", origin.host, origin.port)
        };
        if let Ok(v) = HeaderValue::from_str(&host) {
            parts.headers.insert(header::HOST, v);
        }
    }
    for (name, value) in parts.headers.iter_mut() {
        if let Some(real) = swaps.inject_header(name, value) {
            *value = real;
        }
    }
    let pq = parts.uri.path_and_query().map_or("/", |pq| pq.as_str());
    let pq = swaps.inject_uri(pq).unwrap_or_else(|| pq.to_string());
    parts.uri = match pq.parse::<Uri>() {
        Ok(uri) => uri,
        Err(_) => {
            return Ok(text(
                StatusCode::BAD_REQUEST,
                "ferrule proxy: bad request target\n",
            ))
        }
    };
    parts.version = Version::HTTP_11;

    let resp = match origin.send(Request::from_parts(parts, body)).await {
        Ok(resp) => resp,
        Err(e) => {
            return Ok(text(
                StatusCode::BAD_GATEWAY,
                &format!("ferrule proxy: {}: {e:#}\n", origin.host),
            ))
        }
    };
    Ok(scrub_response(resp, swaps))
}

/// One absolute-form `http://` request, forwarded on a fresh connection.
async fn plain_http(
    req: Request<Incoming>,
    shared: Arc<Shared>,
    source: Source,
) -> Response<ProxyBody> {
    let (mut parts, body) = req.into_parts();
    let authority = match (parts.uri.scheme_str(), parts.uri.authority()) {
        (Some(scheme), Some(a)) if scheme.eq_ignore_ascii_case("http") => a.clone(),
        _ => {
            return text(
                StatusCode::BAD_REQUEST,
                "ferrule proxy: expected CONNECT or an absolute http:// URL\n",
            )
        }
    };
    let host = authority
        .host()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    let port = authority.port_u16().unwrap_or(80);
    let method = parts.method.to_string();
    let addrs = match shared
        .vet(source, &method, &host, port, shared.http_upstream.as_ref())
        .await
    {
        Ok(addrs) => addrs,
        Err(Ok(denial)) => return denied(&denial.message("http")),
        Err(Err(e)) => return text(StatusCode::BAD_GATEWAY, &format!("ferrule proxy: {e}\n")),
    };
    let swaps = shared.swaps_for(&host);
    if swaps.is_some() && !is_loopback(&host) {
        return text(
            StatusCode::FORBIDDEN,
            &format!(
                "ferrule proxy: {host} has secrets bound to it, and secrets only go over HTTPS; \
                 use https://{authority}\n"
            ),
        );
    }
    if parts.headers.contains_key(header::UPGRADE) {
        return text(
            StatusCode::NOT_IMPLEMENTED,
            "ferrule proxy: protocol upgrades (websockets) aren't supported over plain HTTP\n",
        );
    }

    strip_hop_by_hop(&mut parts.headers);
    parts.headers.remove(header::EXPECT);
    // The URL decides where the request goes; a Host naming another site
    // would let a shared front end route it (and any secret) elsewhere.
    if let Ok(v) = HeaderValue::from_str(authority.as_str()) {
        parts.headers.insert(header::HOST, v);
    }
    let pq = parts.uri.path_and_query().map_or("/", |pq| pq.as_str());
    let mut pq = pq.to_string();
    if let Some(swaps) = &swaps {
        // Identity bodies only, so the response can be scrubbed.
        parts.headers.remove(header::ACCEPT_ENCODING);
        for (name, value) in parts.headers.iter_mut() {
            if let Some(real) = swaps.inject_header(name, value) {
                *value = real;
            }
        }
        if let Some(real) = swaps.inject_uri(&pq) {
            pq = real;
        }
    }

    let (tcp, via) =
        match upstream::connect_http(shared.http_upstream.as_ref(), &host, port, addrs.as_deref())
            .await
        {
            Ok(c) => c,
            Err(e) => return text(StatusCode::BAD_GATEWAY, &format!("ferrule proxy: {e:#}\n")),
        };
    let target = match via {
        Some(up) => {
            if let Some(v) = up.auth().and_then(|a| HeaderValue::from_str(a).ok()) {
                parts.headers.insert(header::PROXY_AUTHORIZATION, v);
            }
            format!("http://{authority}{pq}")
        }
        None => pq,
    };
    parts.uri = match target.parse::<Uri>() {
        Ok(uri) => uri,
        Err(_) => {
            return text(
                StatusCode::BAD_REQUEST,
                "ferrule proxy: bad request target\n",
            )
        }
    };
    parts.version = Version::HTTP_11;

    let sent = async {
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tcp)).await?;
        let name = host.clone();
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                debug!("connection to {name} ended: {e}");
            }
        });
        anyhow::Ok(
            sender
                .send_request(Request::from_parts(parts, body))
                .await?,
        )
    };
    match (sent.await, swaps) {
        (Ok(resp), Some(swaps)) => scrub_response(resp, swaps),
        (Ok(resp), None) => {
            let (mut parts, body) = resp.into_parts();
            strip_hop_by_hop(&mut parts.headers);
            Response::from_parts(parts, body.boxed())
        }
        (Err(e), _) => text(
            StatusCode::BAD_GATEWAY,
            &format!("ferrule proxy: {host}: {e:#}\n"),
        ),
    }
}

/// Whether plain HTTP to `host` stays on this machine.
fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn scrub_response(resp: Response<Incoming>, swaps: Arc<Swaps>) -> Response<ProxyBody> {
    let (mut parts, body) = resp.into_parts();
    strip_hop_by_hop(&mut parts.headers);
    for value in parts.headers.values_mut() {
        if let Some(fake) = swaps
            .scrub(value.as_bytes())
            .and_then(|b| HeaderValue::from_bytes(&b).ok())
        {
            *value = fake;
        }
    }
    let encoded = parts
        .headers
        .get(header::CONTENT_ENCODING)
        .is_some_and(|v| !v.as_bytes().eq_ignore_ascii_case(b"identity"));
    let body = if encoded {
        // Compressed despite the stripped Accept-Encoding: can't scrub it.
        body.boxed()
    } else {
        if !swaps.same_lengths() {
            parts.headers.remove(header::CONTENT_LENGTH);
        }
        Scrubbed::new(body, swaps).boxed()
    };
    Response::from_parts(parts, body)
}

fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let named: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|t| HeaderName::from_bytes(t.trim().as_bytes()).ok())
        .collect();
    for name in named {
        headers.remove(name);
    }
    for name in [
        header::CONNECTION,
        HeaderName::from_static("proxy-connection"),
        HeaderName::from_static("keep-alive"),
        header::PROXY_AUTHENTICATE,
        header::PROXY_AUTHORIZATION,
        header::TE,
        header::TRAILER,
        header::TRANSFER_ENCODING,
        header::UPGRADE,
    ] {
        headers.remove(name);
    }
}

/// The decoded `Proxy-Authorization: Basic` credentials.
fn credentials(headers: &HeaderMap) -> Option<Vec<u8>> {
    let v = headers.get(header::PROXY_AUTHORIZATION)?.as_bytes();
    if v.len() < 6 || !v[..6].eq_ignore_ascii_case(b"basic ") {
        return None;
    }
    STANDARD.decode(v[6..].trim_ascii()).ok()
}

#[cfg(test)]
fn authorized(headers: &HeaderMap, expected: &[u8]) -> bool {
    credentials(headers).is_some_and(|got| constant_time_eq(&got, expected))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `host:port` from a CONNECT target, brackets stripped from IPv6 literals.
fn connect_target(uri: &Uri) -> Option<(String, u16)> {
    let auth = uri.authority()?;
    let host = auth.host().trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() {
        return None;
    }
    Some((host.to_ascii_lowercase(), auth.port_u16().unwrap_or(443)))
}

/// The host part of a `Host` header value.
fn host_of(value: &str) -> String {
    match value.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default().to_string(),
        None => value.rsplit_once(':').map_or(value, |(h, _)| h).to_string(),
    }
}

fn text(status: StatusCode, msg: &str) -> Response<ProxyBody> {
    let mut resp = Response::new(
        Full::new(Bytes::from(msg.to_string()))
            .map_err(|never| match never {})
            .boxed(),
    );
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
}

fn empty() -> ProxyBody {
    http_body_util::Empty::new()
        .map_err(|never| match never {})
        .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_credentials_are_checked_exactly() {
        let expected = b"ferrule:secret-token";
        let mut h = HeaderMap::new();
        assert!(!authorized(&h, expected));
        let with = |v: String| {
            let mut h = HeaderMap::new();
            h.insert(
                header::PROXY_AUTHORIZATION,
                HeaderValue::from_str(&v).unwrap(),
            );
            h
        };
        assert!(authorized(
            &with(format!("Basic {}", STANDARD.encode(expected))),
            expected
        ));
        assert!(authorized(
            &with(format!("basic  {}", STANDARD.encode(expected))),
            expected
        ));
        assert!(!authorized(
            &with(format!("Basic {}", STANDARD.encode("ferrule:secret-tokem"))),
            expected
        ));
        assert!(!authorized(
            &with(format!("Bearer {}", STANDARD.encode(expected))),
            expected
        ));
        h.insert(
            header::PROXY_AUTHORIZATION,
            HeaderValue::from_static("Basic ???"),
        );
        assert!(!authorized(&h, expected));
    }

    #[test]
    fn hop_by_hop_headers_and_the_ones_connection_names_are_dropped() {
        let mut h = HeaderMap::new();
        h.insert(
            header::CONNECTION,
            HeaderValue::from_static("keep-alive, X-Private"),
        );
        h.insert("x-private", HeaderValue::from_static("1"));
        h.insert("keep-alive", HeaderValue::from_static("timeout=5"));
        h.insert(
            header::PROXY_AUTHORIZATION,
            HeaderValue::from_static("Basic x"),
        );
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer y"));
        strip_hop_by_hop(&mut h);
        assert_eq!(h.len(), 1);
        assert!(h.contains_key(header::AUTHORIZATION));
    }

    #[test]
    fn connect_targets_and_host_headers_parse() {
        let t = |s: &str| connect_target(&s.parse::<Uri>().unwrap());
        assert_eq!(
            t("API.GitHub.com:443"),
            Some(("api.github.com".into(), 443))
        );
        assert_eq!(t("[::1]:8443"), Some(("::1".into(), 8443)));
        assert_eq!(host_of("api.github.com:443"), "api.github.com");
        assert_eq!(host_of("api.github.com"), "api.github.com");
        assert_eq!(host_of("[::1]:443"), "::1");
    }

    #[test]
    fn only_loopback_hosts_count_as_local() {
        for h in ["localhost", "127.0.0.1", "127.3.4.5", "::1"] {
            assert!(is_loopback(h), "{h}");
        }
        for h in ["example.com", "10.0.0.1", "localhost.example.com", "::2"] {
            assert!(!is_loopback(h), "{h}");
        }
    }
}
