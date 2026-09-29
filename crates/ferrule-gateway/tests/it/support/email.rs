//! M39 §5: a mock mail provider on 127.0.0.1 — an IMAP server (LOGIN,
//! CAPABILITY with or without IDLE, SELECT/EXAMINE, UID SEARCH/FETCH/STORE,
//! NOOP, IDLE pushing `* n EXISTS`, LOGOUT) and an SMTP server (EHLO,
//! AUTH PLAIN, MAIL, RCPT, DATA, QUIT) that records what was sent.

use super::{bind, runtime};
use base64::Engine;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

pub const ADDRESS: &str = "bot@mock.test";
pub const PASSWORD: &str = "abcd efgh ijkl mnop";

/// Google takes an app password with or without its spaces.
fn right(pass: &str) -> bool {
    pass.replace(' ', "") == PASSWORD.replace(' ', "")
}
pub const MAX: &str = "max@example.com";

pub struct Stored {
    pub uid: u32,
    pub raw: Vec<u8>,
    pub seen: bool,
}

pub struct Sent {
    pub from: String,
    pub to: Vec<String>,
    pub data: String,
}

pub struct State {
    pub idle: bool,
    pub uidvalidity: u32,
    pub next_uid: u32,
    pub mailbox: Vec<Stored>,
    pub logins: usize,
    pub refused: usize,
    pub idles: usize,
    pub noops: usize,
    pub body_fetches: Vec<u32>,
    pub sent: Vec<Sent>,
    pub smtp_logins: usize,
    /// Bumped to drop every open IMAP connection.
    pub kill: u32,
}

pub struct Provider {
    pub imap_port: u16,
    pub smtp_port: u16,
    pub state: Arc<Mutex<State>>,
}

impl Provider {
    pub fn start(idle: bool) -> Self {
        let state = Arc::new(Mutex::new(State {
            idle,
            uidvalidity: 7,
            next_uid: 1,
            mailbox: vec![],
            logins: 0,
            refused: 0,
            idles: 0,
            noops: 0,
            body_fetches: vec![],
            sent: vec![],
            smtp_logins: 0,
            kill: 0,
        }));
        let (imap, imap_port) = bind();
        let (smtp, smtp_port) = bind();
        let s = state.clone();
        runtime().spawn(async move {
            let l = TcpListener::from_std(imap).unwrap();
            while let Ok((sock, _)) = l.accept().await {
                let s = s.clone();
                tokio::spawn(async move {
                    let _ = imap_conn(sock, s).await;
                });
            }
        });
        let s = state.clone();
        runtime().spawn(async move {
            let l = TcpListener::from_std(smtp).unwrap();
            while let Ok((sock, _)) = l.accept().await {
                let s = s.clone();
                tokio::spawn(async move {
                    let _ = smtp_conn(sock, s).await;
                });
            }
        });
        Self {
            imap_port,
            smtp_port,
            state,
        }
    }

    pub fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    /// A new mail in INBOX: its UID.
    pub fn deliver(&self, raw: impl Into<Vec<u8>>) -> u32 {
        let mut s = self.state();
        let uid = s.next_uid;
        s.next_uid += 1;
        s.mailbox.push(Stored {
            uid,
            raw: raw.into(),
            seen: false,
        });
        uid
    }

    pub fn seen(&self, uid: u32) -> bool {
        self.state().mailbox.iter().any(|m| m.uid == uid && m.seen)
    }

    pub fn sent(&self) -> usize {
        self.state().sent.len()
    }

    pub fn sent_data(&self, i: usize) -> String {
        self.state().sent[i].data.clone()
    }
}

/// A mail as a client would send it, CRLF lines.
pub fn mail(from: &str, subject: &str, body: &str, extra: &[(&str, &str)]) -> Vec<u8> {
    let mut m = format!(
        "From: {from}\r\nTo: {ADDRESS}\r\nSubject: {subject}\r\nDate: Mon, 28 Sep 2026 10:00:00 +0000\r\n"
    );
    if !extra
        .iter()
        .any(|(n, _)| n.eq_ignore_ascii_case("message-id"))
    {
        m.push_str(&format!(
            "Message-ID: <{}@example.com>\r\n",
            subject.len() * 7919 + body.len()
        ));
    }
    for (n, v) in extra {
        m.push_str(&format!("{n}: {v}\r\n"));
    }
    m.push_str("Content-Type: text/plain; charset=utf-8\r\n\r\n");
    m.push_str(&body.replace('\n', "\r\n"));
    m.push_str("\r\n");
    m.into_bytes()
}

/// A receiving server's verdict that `domain` sent it.
pub fn vouched(domain: &str) -> String {
    format!(
        "mx.mock.test; dkim=pass header.d={domain}; spf=pass smtp.mailfrom=someone@{domain}; dmarc=pass header.from={domain}"
    )
}

fn split_header(raw: &[u8]) -> &[u8] {
    raw.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map_or(raw, |i| &raw[..i + 4])
}

async fn imap_conn(sock: tokio::net::TcpStream, s: Arc<Mutex<State>>) -> std::io::Result<()> {
    let generation = s.lock().unwrap().kill;
    let (r, mut w) = sock.into_split();
    let mut r = BufReader::new(r);
    w.write_all(b"* OK mock IMAP ready\r\n").await?;
    let mut line = String::new();
    loop {
        line.clear();
        if r.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        if s.lock().unwrap().kill != generation {
            return Ok(());
        }
        let l = line.trim_end().to_string();
        let (tag, rest) = l.split_once(' ').unwrap_or((&l, ""));
        let upper = rest.to_ascii_uppercase();
        let mut out = String::new();
        let mut bytes: Vec<u8> = vec![];
        if upper == "CAPABILITY" {
            let idle = if s.lock().unwrap().idle { " IDLE" } else { "" };
            out = format!("* CAPABILITY IMAP4rev1 AUTH=PLAIN{idle}\r\n{tag} OK done\r\n");
        } else if upper.starts_with("LOGIN ") {
            let args: Vec<&str> = rest
                .split_once(' ')
                .map_or("", |x| x.1)
                .split('"')
                .collect();
            let (user, pass) = (args.get(1).copied(), args.get(3).copied());
            let mut st = s.lock().unwrap();
            if user == Some(ADDRESS) && pass.is_some_and(right) {
                st.logins += 1;
                out = format!("{tag} OK logged in\r\n");
            } else {
                st.refused += 1;
                out = format!("{tag} NO [AUTHENTICATIONFAILED] Invalid credentials (Failure)\r\n");
            }
        } else if upper.starts_with("SELECT ") || upper.starts_with("EXAMINE ") {
            let st = s.lock().unwrap();
            out = format!(
                "* {} EXISTS\r\n* OK [UIDVALIDITY {}] ok\r\n* OK [UIDNEXT {}] ok\r\n{tag} OK [READ-WRITE] selected\r\n",
                st.mailbox.len(),
                st.uidvalidity,
                st.next_uid
            );
        } else if upper.starts_with("UID SEARCH UID ") {
            let from: u32 = rest[15..]
                .split(':')
                .next()
                .and_then(|n| n.parse().ok())
                .unwrap_or(1);
            let st = s.lock().unwrap();
            let mut uids: Vec<u32> = st
                .mailbox
                .iter()
                .map(|m| m.uid)
                .filter(|&u| u >= from)
                .collect();
            // `n:*` always names the newest mail (RFC 3501 §6.4.8).
            if uids.is_empty() {
                uids.extend(st.mailbox.last().map(|m| m.uid));
            }
            let list: Vec<String> = uids.iter().map(u32::to_string).collect();
            out = format!("* SEARCH {}\r\n{tag} OK done\r\n", list.join(" "))
                .replace("SEARCH \r\n", "SEARCH\r\n");
        } else if upper.starts_with("UID FETCH ") {
            let mut words = rest.split_whitespace().skip(2);
            let uid: u32 = words.next().and_then(|n| n.parse().ok()).unwrap_or(0);
            let mut st = s.lock().unwrap();
            let pos = st.mailbox.iter().position(|m| m.uid == uid);
            if let Some(pos) = pos {
                let raw = st.mailbox[pos].raw.clone();
                let seq = pos + 1;
                if upper.contains("BODY.PEEK[HEADER]") {
                    let head = split_header(&raw);
                    bytes.extend(
                        format!(
                            "* {seq} FETCH (UID {uid} RFC822.SIZE {} BODY[HEADER] {{{}}}\r\n",
                            raw.len(),
                            head.len()
                        )
                        .as_bytes(),
                    );
                    bytes.extend(head);
                } else {
                    st.body_fetches.push(uid);
                    bytes.extend(
                        format!("* {seq} FETCH (UID {uid} BODY[] {{{}}}\r\n", raw.len()).as_bytes(),
                    );
                    bytes.extend(&raw);
                }
                bytes.extend(b")\r\n");
            }
            bytes.extend(format!("{tag} OK fetched\r\n").as_bytes());
        } else if upper.starts_with("UID STORE ") {
            let uid: u32 = rest
                .split_whitespace()
                .nth(2)
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            let mut st = s.lock().unwrap();
            if let Some(m) = st.mailbox.iter_mut().find(|m| m.uid == uid) {
                m.seen = true;
            }
            out = format!("{tag} OK stored\r\n");
        } else if upper == "NOOP" {
            s.lock().unwrap().noops += 1;
            out = format!("{tag} OK noop\r\n");
        } else if upper == "IDLE" {
            let at = {
                let mut st = s.lock().unwrap();
                st.idles += 1;
                st.mailbox.len()
            };
            w.write_all(b"+ idling\r\n").await?;
            let mut told = false;
            let mut done = String::new();
            loop {
                tokio::select! {
                    n = r.read_line(&mut done) => {
                        if n? == 0 {
                            return Ok(());
                        }
                        break;
                    }
                    _ = tokio::time::sleep(Duration::from_millis(20)) => {
                        let (count, kill) = {
                            let st = s.lock().unwrap();
                            (st.mailbox.len(), st.kill)
                        };
                        if kill != generation {
                            return Ok(());
                        }
                        if count > at && !told {
                            told = true;
                            w.write_all(format!("* {count} EXISTS\r\n").as_bytes()).await?;
                        }
                    }
                }
            }
            out = format!("{tag} OK IDLE terminated\r\n");
        } else if upper == "LOGOUT" {
            w.write_all(format!("* BYE\r\n{tag} OK bye\r\n").as_bytes())
                .await?;
            return Ok(());
        } else {
            out = format!("{tag} BAD unknown command\r\n");
        }
        if !out.is_empty() {
            bytes.extend(out.as_bytes());
        }
        w.write_all(&bytes).await?;
    }
}

async fn smtp_conn(sock: tokio::net::TcpStream, s: Arc<Mutex<State>>) -> std::io::Result<()> {
    let (r, mut w) = sock.into_split();
    let mut r = BufReader::new(r);
    w.write_all(b"220 mock ESMTP\r\n").await?;
    let mut from = String::new();
    let mut to = vec![];
    let mut line = String::new();
    loop {
        line.clear();
        if r.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let l = line.trim_end();
        let upper = l.to_ascii_uppercase();
        let reply: String = if upper.starts_with("EHLO") {
            "250-mock\r\n250 AUTH PLAIN\r\n".into()
        } else if let Some(token) = l.strip_prefix("AUTH PLAIN ") {
            let plain = base64::engine::general_purpose::STANDARD
                .decode(token)
                .unwrap_or_default();
            if plain
                .strip_prefix(format!("\0{ADDRESS}\0").as_bytes())
                .is_some_and(|p| right(&String::from_utf8_lossy(p)))
            {
                s.lock().unwrap().smtp_logins += 1;
                "235 ok\r\n".into()
            } else {
                "535 5.7.8 Username and Password not accepted\r\n".into()
            }
        } else if upper.starts_with("MAIL FROM:") {
            from = l[10..].trim_matches(['<', '>']).to_string();
            to.clear();
            "250 ok\r\n".into()
        } else if upper.starts_with("RCPT TO:") {
            to.push(l[8..].trim_matches(['<', '>']).to_string());
            "250 ok\r\n".into()
        } else if upper == "DATA" {
            w.write_all(b"354 go\r\n").await?;
            let mut data = String::new();
            loop {
                line.clear();
                if r.read_line(&mut line).await? == 0 {
                    return Ok(());
                }
                if line == ".\r\n" {
                    break;
                }
                let l = line.strip_prefix('.').unwrap_or(&line);
                data.push_str(l);
            }
            s.lock().unwrap().sent.push(Sent {
                from: from.clone(),
                to: to.clone(),
                data,
            });
            "250 queued\r\n".into()
        } else if upper == "QUIT" {
            w.write_all(b"221 bye\r\n").await?;
            return Ok(());
        } else {
            "502 unknown\r\n".into()
        };
        w.write_all(reply.as_bytes()).await?;
    }
}
