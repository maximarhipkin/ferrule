//! A tiny HTTP/1.1 server: one request per connection, answered by a
//! synchronous handler.

use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    /// Path and query.
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    /// The body as a form, or a JSON object's top-level strings.
    pub fn params(&self) -> HashMap<String, String> {
        if let Value::Object(m) = self.json() {
            return m
                .into_iter()
                .map(|(k, v)| (k, v.as_str().map_or_else(|| v.to_string(), str::to_string)))
                .collect();
        }
        String::from_utf8_lossy(&self.body)
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (decode(k), decode(v)))
            .collect()
    }
}

fn decode(s: &str) -> String {
    let s = s.replace('+', " ");
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Response {
    pub fn json(v: Value) -> Self {
        Self {
            status: 200,
            headers: vec![],
            body: v.to_string(),
        }
    }

    pub fn empty(status: u16) -> Self {
        Self {
            status,
            headers: vec![],
            body: String::new(),
        }
    }

    pub fn with_status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }

    pub fn with_header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }
}

pub type Handler = Arc<dyn Fn(Request) -> Response + Send + Sync>;

/// Serves `handler` on 127.0.0.1; the port.
pub fn serve(handler: Handler) -> u16 {
    let (listener, port) = super::bind();
    super::runtime().spawn(async move {
        let listener = TcpListener::from_std(listener).unwrap();
        while let Ok((mut stream, _)) = listener.accept().await {
            let handler = handler.clone();
            tokio::spawn(async move {
                let Some(req) = read(&mut stream).await else {
                    return;
                };
                let resp = handler(req);
                let reason = match resp.status {
                    200 => "OK",
                    204 => "No Content",
                    429 => "Too Many Requests",
                    _ => "Status",
                };
                let mut head = format!(
                    "HTTP/1.1 {} {reason}\r\ncontent-length: {}\r\nconnection: close\r\ncontent-type: application/json\r\n",
                    resp.status,
                    resp.body.len()
                );
                for (k, v) in &resp.headers {
                    head.push_str(&format!("{k}: {v}\r\n"));
                }
                head.push_str("\r\n");
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(resp.body.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    port
}

async fn read(stream: &mut tokio::net::TcpStream) -> Option<Request> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_string();
    let path = first.next()?.to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_string()))
        .collect();
    let len = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Some(Request {
        method,
        path,
        headers,
        body,
    })
}
