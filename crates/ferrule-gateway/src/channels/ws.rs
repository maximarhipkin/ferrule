//! M31: a WebSocket client for the Discord Gateway and Slack Socket Mode.
//! `wss://` is TLS through rustls with the webpki roots and `ring`, as
//! reqwest does it, so no OpenSSL or aws-lc joins the build; `ws://` is for
//! the tests' mock servers. An `HTTPS_PROXY` is honoured as reqwest honours
//! it: a `CONNECT` tunnel (with the URL's basic credentials), skipped for
//! `NO_PROXY` hosts and always for loopback.

use futures_util::{SinkExt, StreamExt};
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::rustls;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
pub use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
pub use tokio_tungstenite::tungstenite::protocol::CloseFrame;
pub use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

/// A TCP connection, plain or TLS.
pub enum Stream {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_flush(cx),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

pub type Socket = WebSocketStream<Stream>;

/// What a URL names: its scheme's TLS, host and port.
#[derive(Debug, PartialEq, Eq)]
struct Target {
    tls: bool,
    host: String,
    port: u16,
}

fn target(url: &str) -> Result<Target, String> {
    let (tls, rest) = if let Some(r) = url.strip_prefix("wss://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("ws://") {
        (false, r)
    } else {
        return Err("a WebSocket URL must start with ws:// or wss://".into());
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let (host, port) = split_host_port(authority, if tls { 443 } else { 80 })?;
    Ok(Target { tls, host, port })
}

fn split_host_port(authority: &str, default: u16) -> Result<(String, u16), String> {
    if let Some(v6) = authority.strip_prefix('[') {
        let (host, rest) = v6.split_once(']').ok_or("a bad IPv6 host in the URL")?;
        let port = match rest.strip_prefix(':') {
            Some(p) => p.parse().map_err(|_| "a bad port in the URL")?,
            None => default,
        };
        return Ok((host.to_string(), port));
    }
    match authority.rsplit_once(':') {
        Some((h, p)) => Ok((
            h.to_string(),
            p.parse().map_err(|_| "a bad port in the URL")?,
        )),
        None if authority.is_empty() => Err("no host in the URL".into()),
        None => Ok((authority.to_string(), default)),
    }
}

/// The proxy to tunnel through for `host`, if any: `(host, port,
/// basic credentials)`.
fn proxy_for(host: &str, tls: bool) -> Option<(String, u16, Option<String>)> {
    if is_loopback(host) {
        return None;
    }
    let var = |names: &[&str]| {
        names
            .iter()
            .find_map(|n| std::env::var(n).ok().filter(|v| !v.trim().is_empty()))
    };
    let no_proxy = var(&["NO_PROXY", "no_proxy"]).unwrap_or_default();
    if bypassed(host, &no_proxy) {
        return None;
    }
    let url = if tls {
        var(&["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"])
    } else {
        var(&["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"])
    }?;
    let rest = url
        .strip_prefix("http://")
        .or_else(|| (!url.contains("://")).then_some(url.as_str()))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let (creds, hostport) = match authority.rsplit_once('@') {
        Some((c, h)) => (Some(percent_decode(c)), h),
        None => (None, authority),
    };
    let (phost, pport) = split_host_port(hostport, 80).ok()?;
    Some((phost, pport, creds))
}

fn is_loopback(host: &str) -> bool {
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Whether `NO_PROXY` covers `host`: `*`, the host itself, or a domain it's
/// under (`.example.com` and `example.com` both cover `a.example.com`).
fn bypassed(host: &str, no_proxy: &str) -> bool {
    no_proxy.split(',').map(str::trim).any(|entry| {
        let entry = entry.trim_start_matches('.');
        entry == "*"
            || (!entry.is_empty()
                && (host.eq_ignore_ascii_case(entry)
                    || host
                        .to_ascii_lowercase()
                        .ends_with(&format!(".{}", entry.to_ascii_lowercase()))))
    })
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// `CONNECT host:port` through the proxy; the stream is the tunnel.
async fn tunnel(
    proxy: (String, u16, Option<String>),
    host: &str,
    port: u16,
) -> io::Result<TcpStream> {
    let (phost, pport, creds) = proxy;
    let mut s = TcpStream::connect((phost.as_str(), pport)).await?;
    let mut req = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n");
    if let Some(c) = creds {
        req.push_str(&format!(
            "Proxy-Authorization: Basic {}\r\n",
            base64(c.as_bytes())
        ));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).await?;
    // The proxy's answer, up to the blank line; nothing follows it until
    // we speak.
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if s.read(&mut byte).await? == 0 || head.len() > 16 * 1024 {
            return Err(io::Error::other("the proxy closed the CONNECT"));
        }
        head.push(byte[0]);
    }
    let status = String::from_utf8_lossy(&head);
    let line = status.lines().next().unwrap_or("");
    if line.split_whitespace().nth(1) != Some("200") {
        return Err(io::Error::other(format!(
            "the proxy refused the tunnel: {line}"
        )));
    }
    Ok(s)
}

fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: std::sync::OnceLock<Arc<rustls::ClientConfig>> = std::sync::OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let roots =
                rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            Arc::new(
                rustls::ClientConfig::builder_with_provider(provider)
                    .with_safe_default_protocol_versions()
                    .expect("ring supports the default TLS versions")
                    .with_root_certificates(roots)
                    .with_no_client_auth(),
            )
        })
        .clone()
}

/// Opens a WebSocket to `url` within `deadline`. The error is in words and
/// never holds the URL (Slack's has a ticket in it).
pub async fn connect(url: &str, deadline: Duration) -> Result<Socket, String> {
    tokio::time::timeout(deadline, connect_inner(url))
        .await
        .map_err(|_| format!("connecting timed out after {}s", deadline.as_secs()))?
}

async fn connect_inner(url: &str) -> Result<Socket, String> {
    let t = target(url)?;
    let tcp = match proxy_for(&t.host, t.tls) {
        Some(proxy) => tunnel(proxy, &t.host, t.port)
            .await
            .map_err(|e| format!("through the proxy: {e}"))?,
        None => TcpStream::connect((t.host.as_str(), t.port))
            .await
            .map_err(|e| format!("couldn't connect to {}: {e}", t.host))?,
    };
    let _ = tcp.set_nodelay(true);
    let stream = if t.tls {
        let name = rustls::pki_types::ServerName::try_from(t.host.clone())
            .map_err(|_| format!("`{}` isn't a valid TLS name", t.host))?;
        let tls = tokio_rustls::TlsConnector::from(tls_config())
            .connect(name, tcp)
            .await
            .map_err(|e| format!("TLS with {}: {e}", t.host))?;
        Stream::Tls(Box::new(tls))
    } else {
        Stream::Plain(tcp)
    };
    let request = url
        .into_client_request()
        .map_err(|e| format!("a bad WebSocket request: {}", scrub(&e.to_string(), url)))?;
    let (socket, _) = tokio_tungstenite::client_async(request, stream)
        .await
        .map_err(|e| {
            format!(
                "the WebSocket handshake failed: {}",
                scrub(&e.to_string(), url)
            )
        })?;
    Ok(socket)
}

fn scrub(text: &str, url: &str) -> String {
    text.replace(url, "<url>")
}

/// Sends a JSON value as a text frame.
pub async fn send_json(socket: &mut Socket, v: &serde_json::Value) -> Result<(), String> {
    socket
        .send(Message::text(v.to_string()))
        .await
        .map_err(|e| format!("sending on the socket failed: {e}"))
}

/// The next frame, or `None` when the socket ended.
pub async fn next(socket: &mut Socket) -> Option<Result<Message, String>> {
    socket
        .next()
        .await
        .map(|r| r.map_err(|e| format!("the socket failed: {e}")))
}

/// Closes with `code` and doesn't wait long for the other side.
pub async fn close(socket: &mut Socket, code: u16, reason: &str) {
    let frame = CloseFrame {
        code: CloseCode::from(code),
        reason: reason.to_string().into(),
    };
    let _ = tokio::time::timeout(Duration::from_secs(2), socket.close(Some(frame))).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_name_their_target() {
        assert_eq!(
            target("wss://gateway.discord.gg/?v=10&encoding=json").unwrap(),
            Target {
                tls: true,
                host: "gateway.discord.gg".into(),
                port: 443
            }
        );
        assert_eq!(
            target("ws://127.0.0.1:4567/link").unwrap(),
            Target {
                tls: false,
                host: "127.0.0.1".into(),
                port: 4567
            }
        );
        assert_eq!(target("ws://[::1]:9/").unwrap().host, "::1");
        assert!(target("https://x").is_err());
    }

    #[test]
    fn no_proxy_covers_hosts_and_their_subdomains() {
        assert!(bypassed("slack.com", "localhost, slack.com"));
        assert!(bypassed("wss-primary.slack.com", ".slack.com"));
        assert!(bypassed("anything", "*"));
        assert!(!bypassed("notslack.com", "slack.com"));
        assert!(!bypassed("discord.gg", ""));
        assert!(is_loopback("127.0.0.1") && is_loopback("::1") && is_loopback("localhost"));
    }

    #[test]
    fn basic_credentials_encode() {
        assert_eq!(base64(b"x:secret"), "eDpzZWNyZXQ=");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"abc"), "YWJj");
        assert_eq!(percent_decode("a%40b:c%3Ad"), "a@b:c:d");
    }
}
