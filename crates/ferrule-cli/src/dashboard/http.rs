//! Just enough HTTP/1.1 for one page on 127.0.0.1: one request per
//! connection (`Connection: close`), a size limit on the head and the body,
//! and a deadline on reading them. The browser is the only client.

use std::collections::BTreeMap;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const MAX_HEAD: usize = 16 * 1024;
const MAX_BODY: usize = 64 * 1024;
const READ_DEADLINE: Duration = Duration::from_secs(10);

/// What one request may weigh and how long it may take to arrive.
#[derive(Debug, Clone, Copy)]
pub struct Limit {
    pub body: usize,
    pub deadline: Duration,
}

impl Default for Limit {
    fn default() -> Self {
        Self {
            body: MAX_BODY,
            deadline: READ_DEADLINE,
        }
    }
}

/// The request line and headers, read before the body: enough for the
/// caller to decide how much body it will accept (M47: a photo).
#[derive(Debug)]
pub struct Head {
    pub method: String,
    /// Without the query.
    pub path: String,
    /// Lower-case names.
    pub headers: BTreeMap<String, String>,
}

/// At most this many big bodies are read at once, so a few slow uploads
/// can't hold the memory of many.
static BIG: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    /// Without the query.
    pub path: String,
    pub query: BTreeMap<String, String>,
    /// Lower-case names.
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    /// A cookie's value.
    pub fn cookie(&self, name: &str) -> Option<&str> {
        cookie_in(self.header("cookie")?, name)
    }
}

impl Head {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    pub fn cookie(&self, name: &str) -> Option<&str> {
        cookie_in(self.header("cookie")?, name)
    }
}

fn cookie_in<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    header.split(';').find_map(|c| {
        let (k, v) = c.trim().split_once('=')?;
        (k == name).then_some(v)
    })
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// A file sent from disk instead of `body` (a backup, M47).
    pub file: Option<std::path::PathBuf>,
}

impl Response {
    pub fn new(status: u16, content_type: &'static str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            content_type,
            headers: Vec::new(),
            body: body.into(),
            file: None,
        }
    }

    /// `path`'s bytes, streamed. Never cached: it is the owner's data.
    pub fn file(path: std::path::PathBuf, content_type: &'static str) -> Self {
        let mut r = Self::new(200, content_type, Vec::new());
        r.file = Some(path);
        r.with_header("Cache-Control", "no-store".into())
    }

    pub fn json(status: u16, value: &serde_json::Value) -> Self {
        Self::new(status, "application/json", value.to_string())
    }

    pub fn text(status: u16, text: &str) -> Self {
        Self::new(status, "text/plain; charset=utf-8", text.to_string())
    }

    pub fn with_header(mut self, name: &str, value: String) -> Self {
        self.headers.push((name.into(), value));
        self
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        413 => "Payload Too Large",
        421 => "Misdirected Request",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

/// Reads one request; `Err` holds the status to answer with (or 0: the
/// connection went away, answer nothing).
/// `limit` sees the head and says how big a body it may have; only a
/// caller that has checked the session (and asks for more than the usual
/// 64 KB) gets a bigger one.
pub async fn read(stream: &mut TcpStream, limit: impl Fn(&Head) -> Limit) -> Result<Request, u16> {
    // The head has the same, short deadline whatever the body may be.
    let mut buf = Vec::with_capacity(2048);
    let head = match tokio::time::timeout(READ_DEADLINE, read_head(stream, &mut buf)).await {
        Ok(Ok(head)) => head,
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err(408),
    };
    let allowed = limit(&head.head);
    let len = head.len;
    if len > allowed.body {
        return Err(413);
    }
    // A big body is read a few at a time, and not for long.
    let _slot = if allowed.body > MAX_BODY && len > MAX_BODY {
        Some(BIG.try_acquire().map_err(|_| 503u16)?)
    } else {
        None
    };
    tokio::time::timeout(allowed.deadline, read_body(stream, buf, head, len))
        .await
        .unwrap_or(Err(408))
}

struct Parsed {
    head: Head,
    query: BTreeMap<String, String>,
    len: usize,
    /// Where the body starts in the bytes read so far.
    body_at: usize,
}

async fn read_head(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Result<Parsed, u16> {
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = find(buf, b"\r\n\r\n") {
            break i;
        }
        if buf.len() > MAX_HEAD {
            return Err(413);
        }
        let n = stream.read(&mut chunk).await.map_err(|_| 0u16)?;
        if n == 0 {
            return Err(0);
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    if head_end > MAX_HEAD {
        return Err(413);
    }
    let head = std::str::from_utf8(&buf[..head_end]).map_err(|_| 400u16)?;
    let mut lines = head.split("\r\n");
    let mut first = lines.next().ok_or(400u16)?.split(' ');
    let method = first.next().ok_or(400u16)?.to_string();
    let target = first.next().ok_or(400u16)?.to_string();
    let mut headers = BTreeMap::new();
    for line in lines {
        let (k, v) = line.split_once(':').ok_or(400u16)?;
        headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
    }
    let len: usize = match headers.get("content-length") {
        Some(v) => v.parse().map_err(|_| 400u16)?,
        None => 0,
    };
    if headers.contains_key("transfer-encoding") {
        return Err(400);
    }
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), parse_query(q)),
        None => (target, BTreeMap::new()),
    };
    Ok(Parsed {
        head: Head {
            method,
            path,
            headers,
        },
        query,
        len,
        body_at: head_end + 4,
    })
}

async fn read_body(
    stream: &mut TcpStream,
    buf: Vec<u8>,
    parsed: Parsed,
    len: usize,
) -> Result<Request, u16> {
    let mut chunk = [0u8; 16 * 1024];
    let mut body = Vec::with_capacity(len.max(buf.len() - parsed.body_at));
    body.extend_from_slice(&buf[parsed.body_at..]);
    while body.len() < len {
        let n = stream.read(&mut chunk).await.map_err(|_| 0u16)?;
        if n == 0 {
            return Err(0);
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);
    let Head {
        method,
        path,
        headers,
    } = parsed.head;
    Ok(Request {
        method,
        path,
        query: parsed.query,
        headers,
        body,
    })
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

pub fn parse_query(q: &str) -> BTreeMap<String, String> {
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (unescape(k), unescape(v))
        })
        .collect()
}

/// `%XX` and `+` in a query component.
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3])
                    .ok()
                    .filter(|h| h.bytes().all(|c| c.is_ascii_hexdigit()))
                    .unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Writes `resp` with the headers every answer gets: no caching (unless
/// it carries its own `Cache-Control`), no framing, no referrer, a CSP that allows only this origin's own files.
pub async fn write(stream: &mut TcpStream, resp: Response) {
    // Nothing is cached unless the response says so (the fonts do).
    let cached = resp
        .headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("cache-control"));
    // A file goes out as it is read; if it can't be opened the answer is
    // a plain 404 instead of a head with nothing behind it.
    let (mut file, length) = match &resp.file {
        Some(path) => match tokio::fs::File::open(path).await {
            Ok(f) => match f.metadata().await {
                Ok(m) => {
                    let n = m.len();
                    (Some(f), n)
                }
                Err(_) => (None, 0),
            },
            Err(_) => {
                let gone =
                    Response::json(404, &serde_json::json!({ "error": "That file is gone." }));
                return Box::pin(write(stream, gone)).await;
            }
        },
        None => (None, resp.body.len() as u64),
    };
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\
         {}X-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\n\
         X-Frame-Options: DENY\r\n\
         Content-Security-Policy: default-src 'self'; img-src 'self' data:; object-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'\r\n",
        resp.status,
        reason(resp.status),
        resp.content_type,
        length,
        if cached { "" } else { "Cache-Control: no-store\r\n" },
    );
    for (k, v) in &resp.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes()).await;
    match file.as_mut() {
        Some(f) => {
            let _ = tokio::io::copy(f, stream).await;
        }
        None => {
            let _ = stream.write_all(&resp.body).await;
        }
    }
    let _ = stream.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_are_unescaped() {
        let q = parse_query("q=deep%20seek&x=a+b&bad=%zz&tail=%4");
        assert_eq!(q["q"], "deep seek");
        assert_eq!(q["x"], "a b");
        assert_eq!(q["bad"], "%zz");
        assert_eq!(q["tail"], "%4");
        assert_eq!(parse_query("q=%D7%A9%D7%9C%D7%95%D7%9D")["q"], "שלום");
    }
}
