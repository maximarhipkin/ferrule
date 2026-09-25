//! The fallbacks when there's no relay: a loopback listener for the OAuth
//! redirect, reached through a `cloudflared` quick tunnel (DCR services
//! only: an owner-made client can't list a random tunnel host), or not
//! reached at all (paste-back: the owner copies the address the browser
//! couldn't load). The listener answers `/callback` only and never echoes
//! what it got.

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq)]
pub struct Callback {
    pub state: String,
    pub code: Option<String>,
    pub error: Option<String>,
}

pub struct Listener {
    pub port: u16,
    pub rx: mpsc::Receiver<Callback>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.task.abort();
    }
}

const PAGE: &str = "<!doctype html><meta charset=utf-8><title>ferrule</title>\
<body style=\"font-family:sans-serif;max-width:32em;margin:3em auto\">\
<h1>Done</h1><p>ferrule has it. You can close this tab.</p>";

/// Listen on `127.0.0.1:<port>` (0: any free port).
pub async fn listen(port: u16) -> Result<Listener> {
    let socket = TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("listening on 127.0.0.1:{port}"))?;
    let port = socket.local_addr()?.port();
    let (tx, rx) = mpsc::channel(4);
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut conn, _)) = socket.accept().await else {
                continue;
            };
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let mut len = 0;
                let read = tokio::time::timeout(Duration::from_secs(10), async {
                    while len < buf.len() {
                        let n = conn.read(&mut buf[len..]).await.ok()?;
                        if n == 0 {
                            break;
                        }
                        len += n;
                        if buf[..len].windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    Some(())
                })
                .await;
                if !matches!(read, Ok(Some(()))) {
                    return;
                }
                let head = String::from_utf8_lossy(&buf[..len]);
                let target = head
                    .lines()
                    .next()
                    .and_then(|l| l.strip_prefix("GET "))
                    .and_then(|l| l.split(' ').next())
                    .unwrap_or("");
                let got = parse_target(target);
                let (status, body) = match &got {
                    Some(_) => ("200 OK", PAGE),
                    None => ("404 Not Found", "not found"),
                };
                let reply = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
                     Cache-Control: no-store\r\nReferrer-Policy: no-referrer\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = conn.write_all(reply.as_bytes()).await;
                let _ = conn.shutdown().await;
                if let Some(cb) = got {
                    let _ = tx.send(cb).await;
                }
            });
        }
    });
    Ok(Listener { port, rx, task })
}

fn parse_target(target: &str) -> Option<Callback> {
    let url = url::Url::parse(&format!("http://127.0.0.1{target}")).ok()?;
    if url.path() != "/callback" {
        return None;
    }
    let mut cb = Callback {
        state: String::new(),
        code: None,
        error: None,
    };
    for (k, v) in url.query_pairs() {
        match &*k {
            "state" => cb.state = v.into_owned(),
            "code" => cb.code = Some(v.into_owned()),
            "error" => cb.error = Some(v.into_owned()),
            _ => {}
        }
    }
    (!cb.state.is_empty() && (cb.code.is_some() || cb.error.is_some())).then_some(cb)
}

/// A running quick tunnel; stopped when dropped.
pub struct Tunnel {
    pub url: String,
    _child: tokio::process::Child,
}

impl Tunnel {
    /// Whether cloudflared is still running.
    pub fn alive(&mut self) -> bool {
        matches!(self._child.try_wait(), Ok(None))
    }
}

/// `cloudflared tunnel --url http://127.0.0.1:<port>`, and the
/// `https://….trycloudflare.com` it prints, within 30 s.
pub async fn open(cloudflared: &Path, port: u16) -> Result<Tunnel> {
    let mut child = tokio::process::Command::new(cloudflared)
        .args(["tunnel", "--no-autoupdate", "--url"])
        .arg(format!("http://127.0.0.1:{port}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting {}", cloudflared.display()))?;
    let stderr = child.stderr.take().context("cloudflared's output")?;
    let mut lines = BufReader::new(stderr).lines();
    let found = tokio::time::timeout(Duration::from_secs(30), async {
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(url) = trycloudflare(&line) {
                return Some(url);
            }
        }
        None
    })
    .await;
    let Ok(Some(url)) = found else {
        bail!("cloudflared didn't open a quick tunnel within 30 s");
    };
    // Keep reading, or cloudflared blocks on a full pipe.
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
    Ok(Tunnel { url, _child: child })
}

fn trycloudflare(line: &str) -> Option<String> {
    let start = line.find("https://")?;
    let rest = &line[start..];
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '|' || c == '"')
        .unwrap_or(rest.len());
    let url = &rest[..end];
    let host = url.strip_prefix("https://")?;
    (host.ends_with(".trycloudflare.com")
        && host != "api.trycloudflare.com"
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.'))
    .then(|| url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_listener_takes_a_callback_and_nothing_else() {
        let mut l = listen(0).await.unwrap();
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let base = format!("http://127.0.0.1:{}", l.port);
        let r = http.get(format!("{base}/other")).send().await.unwrap();
        assert_eq!(r.status(), 404);
        let r = http
            .get(format!("{base}/callback?code=C0DE&state=S"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert!(!r.text().await.unwrap().contains("C0DE"), "never echoed");
        let cb = l.rx.recv().await.unwrap();
        assert_eq!(cb.state, "S");
        assert_eq!(cb.code.as_deref(), Some("C0DE"));
    }

    #[test]
    fn the_tunnel_url_is_picked_from_cloudflareds_banner() {
        let line = "2026-09-25T10:00:00Z INF |  https://tidy-owl-bright-sun.trycloudflare.com  |";
        assert_eq!(
            trycloudflare(line).as_deref(),
            Some("https://tidy-owl-bright-sun.trycloudflare.com")
        );
        assert_eq!(
            trycloudflare("see https://api.trycloudflare.com/tunnel"),
            None
        );
        assert_eq!(trycloudflare("https://evil.example.com"), None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_quick_tunnel_is_whatever_cloudflared_prints() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("cloudflared");
        std::fs::write(
            &fake,
            "#!/bin/sh\necho 'INF starting' >&2\necho 'INF |  https://a-b-c.trycloudflare.com  |' >&2\nsleep 30\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let t = open(&fake, 1234).await.unwrap();
        assert_eq!(t.url, "https://a-b-c.trycloudflare.com");
    }
}
