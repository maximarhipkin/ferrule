//! Reaching the real server: directly, or through the proxy ferrule itself was
//! started behind (`HTTPS_PROXY` for tunnels, `HTTP_PROXY` for plain HTTP),
//! so a corporate or container proxy keeps working once sandboxed commands
//! are pointed at ferrule instead.

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use percent_encoding::percent_decode_str;
use std::fmt;
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RESPONSE_HEAD: usize = 16 * 1024;

#[derive(Clone)]
pub struct Upstream {
    host: String,
    port: u16,
    /// `Basic …` for `Proxy-Authorization`, from the URL's userinfo.
    auth: Option<String>,
    no_proxy: NoProxy,
}

// Hand-written so the proxy credentials never reach a log line.
impl fmt::Debug for Upstream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Upstream")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("auth", &self.auth.as_ref().map(|_| "<redacted>"))
            .field("no_proxy", &self.no_proxy)
            .finish()
    }
}

impl fmt::Display for Upstream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "http://{}:{}", self.host, self.port)
    }
}

impl Upstream {
    /// From `HTTPS_PROXY` / `ALL_PROXY` (either case) and `NO_PROXY`. `None`
    /// when no proxy is set.
    pub fn from_env() -> Result<Option<Self>> {
        Self::from_vars(&["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"])
    }

    /// The same for plain HTTP: `HTTP_PROXY` / `ALL_PROXY` (either case).
    pub fn from_env_http() -> Result<Option<Self>> {
        Self::from_vars(&["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"])
    }

    fn from_vars(names: &[&str]) -> Result<Option<Self>> {
        let var = |names: &[&str]| {
            names
                .iter()
                .find_map(|n| std::env::var(n).ok().filter(|v| !v.trim().is_empty()))
        };
        let Some(url) = var(names) else {
            return Ok(None);
        };
        let no_proxy = var(&["NO_PROXY", "no_proxy"]).unwrap_or_default();
        Self::parse(&url, &no_proxy).map(Some)
    }

    pub fn parse(url: &str, no_proxy: &str) -> Result<Self> {
        let url = url.trim();
        let rest = match url.split_once("://") {
            Some((scheme, rest)) if scheme.eq_ignore_ascii_case("http") => rest,
            Some((scheme, _)) => {
                bail!("only http:// upstream proxies are supported, not {scheme}://")
            }
            None => url,
        };
        let authority = rest.split('/').next().unwrap_or_default();
        let (userinfo, hostport) = match authority.rsplit_once('@') {
            Some((u, h)) => (Some(u), h),
            None => (None, authority),
        };
        let port = |p: &str| {
            p.parse::<u16>()
                .with_context(|| format!("bad port in upstream proxy `{hostport}`"))
        };
        let (host, port) = if let Some(v6) = hostport.strip_prefix('[') {
            let (h, after) = v6
                .split_once(']')
                .context("unclosed `[` in the upstream proxy host")?;
            (h, after.strip_prefix(':').map_or(Ok(80), port)?)
        } else {
            match hostport.rsplit_once(':') {
                Some((h, p)) => (h, port(p)?),
                None => (hostport, 80),
            }
        };
        if host.is_empty() {
            bail!("upstream proxy URL has no host");
        }
        let auth = userinfo.map(|u| {
            let (user, pass) = u.split_once(':').unwrap_or((u, ""));
            let dec = |s: &str| percent_decode_str(s).decode_utf8_lossy().into_owned();
            format!(
                "Basic {}",
                STANDARD.encode(format!("{}:{}", dec(user), dec(pass)))
            )
        });
        Ok(Self {
            host: host.to_string(),
            port,
            auth,
            no_proxy: NoProxy::parse(no_proxy),
        })
    }

    /// Whether `NO_PROXY` sends `host` around the upstream proxy.
    pub fn bypasses(&self, host: &str) -> bool {
        self.no_proxy.matches(host)
    }
}

/// Open a TCP stream to `host:port`, through `upstream` unless it's bypassed.
pub(crate) async fn connect(
    upstream: Option<&Upstream>,
    host: &str,
    port: u16,
) -> Result<TcpStream> {
    tokio::time::timeout(CONNECT_TIMEOUT, async {
        match upstream.filter(|u| !u.bypasses(host)) {
            None => TcpStream::connect((host, port))
                .await
                .with_context(|| format!("connecting to {host}:{port}")),
            Some(up) => tunnel(up, host, port).await,
        }
    })
    .await
    .with_context(|| format!("connecting to {host}:{port}: timed out"))?
}

/// A connection for one plain-HTTP request to `host:port`: to `upstream`
/// unless it's bypassed, which is returned so the request can go in absolute
/// form with its credentials, or else straight to the server.
pub(crate) async fn connect_http<'u>(
    upstream: Option<&'u Upstream>,
    host: &str,
    port: u16,
) -> Result<(TcpStream, Option<&'u Upstream>)> {
    let via = upstream.filter(|u| !u.bypasses(host));
    let (addr, what) = match via {
        Some(up) => ((up.host.as_str(), up.port), format!("upstream proxy {up}")),
        None => ((host, port), format!("{host}:{port}")),
    };
    let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
        .await
        .with_context(|| format!("connecting to {what}: timed out"))?
        .with_context(|| format!("connecting to {what}"))?;
    Ok((tcp, via))
}

impl Upstream {
    /// `Basic …` for `Proxy-Authorization`, when the URL had credentials.
    pub(crate) fn auth(&self) -> Option<&str> {
        self.auth.as_deref()
    }
}

async fn tunnel(up: &Upstream, host: &str, port: u16) -> Result<TcpStream> {
    let mut stream = TcpStream::connect((up.host.as_str(), up.port))
        .await
        .with_context(|| format!("connecting to upstream proxy {up}"))?;
    let target = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let mut req = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n");
    if let Some(auth) = &up.auth {
        req.push_str(&format!("Proxy-Authorization: {auth}\r\n"));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).await?;

    // Byte at a time so nothing past the head (the server's first TLS bytes)
    // is swallowed; the head is a few hundred bytes, once per connection.
    let mut head = Vec::with_capacity(256);
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= MAX_RESPONSE_HEAD {
            bail!("upstream proxy {up} sent an oversized CONNECT response");
        }
        let b = stream
            .read_u8()
            .await
            .with_context(|| format!("upstream proxy {up} closed during CONNECT to {target}"))?;
        head.push(b);
    }
    let status_line =
        String::from_utf8_lossy(head.split(|&b| b == b'\r').next().unwrap_or_default())
            .into_owned();
    let code = status_line.split_whitespace().nth(1).unwrap_or_default();
    if !code.starts_with('2') || code.len() != 3 {
        bail!("upstream proxy {up} refused CONNECT {target}: {status_line}");
    }
    Ok(stream)
}

#[derive(Debug, Clone, Default)]
struct NoProxy {
    all: bool,
    /// Lowercased, without leading `.`/`*.` or a port.
    entries: Vec<String>,
}

impl NoProxy {
    fn parse(raw: &str) -> Self {
        let mut np = Self::default();
        for entry in raw.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            if entry == "*" {
                np.all = true;
                continue;
            }
            let e = entry.trim_start_matches("*.").trim_start_matches('.');
            let e = match e.rsplit_once(':') {
                Some((h, p)) if p.bytes().all(|b| b.is_ascii_digit()) && !h.contains(':') => h,
                _ => e,
            };
            np.entries.push(e.to_ascii_lowercase());
        }
        np
    }

    fn matches(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        self.all
            || self.entries.iter().any(|e| {
                host == *e
                    || (host.len() > e.len()
                        && host.ends_with(e.as_str())
                        && host.as_bytes()[host.len() - e.len() - 1] == b'.')
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn proxy_urls_parse_with_decoded_credentials() {
        let up = Upstream::parse("http://us%40er:p%3Ass@gateway.example:10255/", "").unwrap();
        assert_eq!(up.host, "gateway.example");
        assert_eq!(up.port, 10255);
        let auth = up.auth.as_deref().unwrap().strip_prefix("Basic ").unwrap();
        assert_eq!(STANDARD.decode(auth).unwrap(), b"us@er:p:ss");
        assert!(!format!("{up:?}").contains("p:ss") && !format!("{up:?}").contains(auth));

        let bare = Upstream::parse("proxy.local", "").unwrap();
        assert_eq!(
            (bare.host.as_str(), bare.port, bare.auth.is_none()),
            ("proxy.local", 80, true)
        );
        assert!(Upstream::parse("socks5://proxy:1080", "").is_err());
        assert!(Upstream::parse("https://proxy:443", "").is_err());
    }

    #[test]
    fn no_proxy_matches_hosts_and_their_subdomains() {
        let np = NoProxy::parse("localhost, .internal.example ,*.corp.example, 10.0.0.1:8080");
        assert!(np.matches("localhost"));
        assert!(np.matches("a.internal.example"));
        assert!(np.matches("internal.example"));
        assert!(np.matches("x.corp.example"));
        assert!(np.matches("10.0.0.1"));
        assert!(!np.matches("notinternal.example"));
        assert!(!np.matches("api.github.com"));
        assert!(NoProxy::parse("*").matches("anything"));
    }

    /// A fake upstream that checks the CONNECT, answers `status`, then echoes.
    async fn fake_proxy(status: &'static str) -> (u16, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                head.push(s.read_u8().await.unwrap());
            }
            // The first tunnelled bytes ride in the same packet as the 200.
            s.write_all(format!("HTTP/1.1 {status}\r\nVia: fake\r\n\r\nhello").as_bytes())
                .await
                .unwrap();
            String::from_utf8(head).unwrap()
        });
        (port, task)
    }

    #[tokio::test]
    async fn connect_goes_through_the_upstream_with_credentials() {
        let (port, task) = fake_proxy("200 Connection established").await;
        let up = Upstream::parse(&format!("http://u:p@127.0.0.1:{port}"), "").unwrap();
        let mut s = connect(Some(&up), "api.example.com", 443).await.unwrap();
        let mut first = [0u8; 5];
        s.read_exact(&mut first).await.unwrap();
        assert_eq!(
            &first, b"hello",
            "bytes after the head belong to the tunnel"
        );
        let head = task.await.unwrap();
        assert!(head.starts_with("CONNECT api.example.com:443 HTTP/1.1\r\n"));
        assert!(head.contains(&format!(
            "Proxy-Authorization: Basic {}",
            STANDARD.encode("u:p")
        )));
    }

    #[tokio::test]
    async fn a_refused_connect_is_an_error_and_no_proxy_goes_direct() {
        let (port, _task) = fake_proxy("407 Proxy Authentication Required").await;
        let up = Upstream::parse(&format!("http://127.0.0.1:{port}"), "").unwrap();
        let err = connect(Some(&up), "api.example.com", 443)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("407"), "{err:#}");

        let direct = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let direct_port = direct.local_addr().unwrap().port();
        let up = Upstream::parse("http://127.0.0.1:1", "127.0.0.1").unwrap();
        connect(Some(&up), "127.0.0.1", direct_port).await.unwrap();
    }
}
