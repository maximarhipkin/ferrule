//! M63.2: a mailbox over IMAP and SMTP with an app password, any provider.
//! Gmail's connection (M37) is this with Gmail's own search syntax and its
//! All Mail folder (`gmail.rs`); every other provider, from a preset in the
//! catalog (iCloud, Fastmail, Yahoo) or typed in, searches with RFC 3501's
//! criteria in the mailbox it names.
//!
//! The password stays in this process and goes only to the two hosts the
//! connection was saved with, over TLS (from the first byte, or STARTTLS on
//! 143 and 587): never in the clear. Reading is untrusted content (the
//! trust hub marks a connected service's read, M54.5); sending is a write,
//! listed only on a write connection and gated like any write tool. A
//! session is opened per call; nothing stays logged in.

use super::imap::{Error as ImapError, Imap, Part};
use super::mime::{self, Outgoing};
use super::net::Addr;
use super::smtp::Smtp;
use super::{arg_str, cap, limit, opt_str, tool, Endpoints};
use async_trait::async_trait;
use ferrule_mcp::local::LocalServer;
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Which mail service's habits to follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    /// Gmail: `X-GM-RAW` search over All Mail; tools `gmail_*`.
    Gmail,
    /// Any IMAP server: RFC 3501 criteria in a named mailbox; tools `mail_*`.
    Imap,
}

/// One end: where, and whether TLS starts after a plain greeting.
#[derive(Debug, Clone)]
pub struct Server {
    pub addr: Addr,
    pub starttls: bool,
}

pub struct Mail {
    id: String,
    flavor: Flavor,
    email: String,
    imap_user: String,
    smtp_user: String,
    password: String,
    imap: Server,
    smtp: Server,
    write: bool,
    /// What a refused login is told: where to make a new app password.
    refused: String,
}

const GMAIL_REFUSED: &str =
    "Google refused the email and app password. Check the address, and make \
     a new app password at https://myaccount.google.com/apppasswords (it needs 2-Step \
     Verification on the account). Your normal Google password won't work here.";

/// The fields a connection of `flavor` was saved with, as a server: `email`
/// and `app_password`, plus for any IMAP server its `imap_host`,
/// `imap_port`, `imap_tls`, `smtp_*`, and `username` or `imap_user` (a
/// template: `{email}`, `{local}`). Gmail's servers come from `ep`, as do
/// a test's: a `127.0.0.1` host is spoken to in the clear only when
/// `ep.loopback`.
pub fn new(
    id: &str,
    flavor: Flavor,
    fields: &BTreeMap<String, String>,
    write: bool,
    ep: &Endpoints,
    key_url: Option<&str>,
) -> Result<Mail, String> {
    let get = |k: &str| {
        fields
            .get(k)
            .filter(|v| !v.is_empty())
            .cloned()
            .ok_or_else(|| format!("the {} is missing", k.replace('_', " ")))
    };
    let email = get("email")?;
    let password = get("app_password")?;
    let (imap, smtp, imap_user, smtp_user, refused, prefix) = match flavor {
        Flavor::Gmail => (
            Server {
                addr: ep.imap.clone(),
                starttls: false,
            },
            Server {
                addr: ep.smtp.clone(),
                starttls: false,
            },
            email.clone(),
            email.clone(),
            GMAIL_REFUSED.to_string(),
            "gmail",
        ),
        Flavor::Imap => {
            let server = |side: &str| -> Result<Server, String> {
                let host = get(&format!("{side}_host"))?;
                let port: u16 = get(&format!("{side}_port"))?
                    .parse()
                    .map_err(|_| format!("the {side} port isn't a number"))?;
                let starttls = fields
                    .get(&format!("{side}_tls"))
                    .is_some_and(|t| t == "starttls");
                let plain = ep.loopback && host == "127.0.0.1";
                Ok(Server {
                    starttls: starttls && !plain,
                    addr: Addr {
                        tls: !starttls && !plain,
                        ..Addr::plain(&host, port)
                    },
                })
            };
            let local = email.split('@').next().unwrap_or(&email).to_string();
            let user = |template: Option<&String>| match fields.get("username") {
                Some(u) if !u.is_empty() => u.clone(),
                _ => template
                    .map(|t| t.replace("{email}", &email).replace("{local}", &local))
                    .unwrap_or_else(|| email.clone()),
            };
            let refused = match key_url {
                Some(url) => format!(
                    "The mail server refused the address and app password. Check them, and \
                     make a new app password at {url}. An account's normal password usually \
                     won't work here."
                ),
                None => "The mail server refused the user name and password. Check them; most \
                         providers want an app password here, not the account's normal one."
                    .to_string(),
            };
            (
                server("imap")?,
                server("smtp")?,
                user(fields.get("imap_user")),
                user(None),
                refused,
                "mail",
            )
        }
    };
    Ok(Mail {
        id: format!("native:{prefix}:{id}"),
        flavor,
        email,
        imap_user,
        smtp_user,
        password,
        imap,
        smtp,
        write,
        refused,
    })
}

impl Mail {
    fn prefix(&self) -> &'static str {
        match self.flavor {
            Flavor::Gmail => "gmail",
            Flavor::Imap => "mail",
        }
    }

    fn imap_err(&self, e: ImapError, login: bool) -> String {
        match e {
            ImapError::No(t) if login || t.contains("AUTHENTICATIONFAILED") => self.refused.clone(),
            ImapError::No(t)
                if self.flavor == Flavor::Gmail
                    && (t.contains("WEBALERT") || t.contains("ALERT")) =>
            {
                "Google wants you to sign in once in a browser before this connection can use \
                 Gmail: open https://mail.google.com, then try again."
                    .into()
            }
            ImapError::No(_) if self.flavor == Flavor::Gmail => "Gmail refused the request".into(),
            ImapError::No(t) => format!("the mail server refused the request ({})", clip(&t)),
            ImapError::Broken(t) => t,
        }
    }

    /// Logged in, with `mailbox` open read-only (Gmail: All Mail).
    async fn session(&self, mailbox: Option<&str>) -> Result<Imap, String> {
        let mut imap = self.login().await?;
        let name = match self.flavor {
            Flavor::Gmail => imap.all_mail().await.map_err(|e| self.imap_err(e, false))?,
            Flavor::Imap => mailbox.unwrap_or("INBOX").to_string(),
        };
        imap.examine(&name).await.map_err(|e| match e {
            ImapError::No(_) if self.flavor == Flavor::Imap => {
                format!("there's no mailbox `{name}` (mail_folders lists them)")
            }
            e => self.imap_err(e, false),
        })?;
        Ok(imap)
    }

    async fn login(&self) -> Result<Imap, String> {
        let stream = super::net::connect(&self.imap.addr).await?;
        let mut imap = Imap::start(stream)
            .await
            .map_err(|e| self.imap_err(e, false))?;
        if self.imap.starttls {
            imap = imap
                .starttls(&self.imap.addr.host)
                .await
                .map_err(|e| match e {
                    ImapError::No(_) => format!(
                        "{} doesn't offer STARTTLS on port {}: use its TLS port (usually 993)",
                        self.imap.addr.host, self.imap.addr.port
                    ),
                    e => self.imap_err(e, false),
                })?;
        }
        imap.login(&self.imap_user, &self.password)
            .await
            .map_err(|e| self.imap_err(e, true))?;
        Ok(imap)
    }

    /// Logs in and out; the address it's for.
    pub async fn check(&self) -> Result<String, String> {
        self.session(None).await?.logout().await;
        Ok(self.email.clone())
    }

    async fn search(&self, args: &Value) -> Result<String, String> {
        let n = limit(args, 10, 25) as usize;
        let mailbox = opt_str(args, "mailbox");
        let mut imap = self.session(mailbox).await?;
        let (uids, what) = match self.flavor {
            Flavor::Gmail => {
                let query = opt_str(args, "query").unwrap_or("in:inbox");
                let uids = imap
                    .search(query)
                    .await
                    .map_err(|e| self.imap_err(e, false))?;
                (uids, format!("`{query}`"))
            }
            Flavor::Imap => {
                let owned = criteria(args)?;
                let mut parts: Vec<Part> = Vec::new();
                for (key, value) in &owned {
                    parts.push(Part::Atom(key));
                    match value {
                        Some(Crit::Str(v)) => parts.push(Part::Str(v)),
                        Some(Crit::Atom(v)) => parts.push(Part::Atom(v)),
                        None => {}
                    }
                }
                let uids = imap
                    .search_criteria(&parts)
                    .await
                    .map_err(|e| self.imap_err(e, false))?;
                (uids, format!("in {}", mailbox.unwrap_or("INBOX")))
            }
        };
        let total = uids.len();
        let newest: Vec<u32> = uids.iter().rev().take(n).copied().collect();
        let heads = imap
            .fetch(&newest, "BODY.PEEK[HEADER.FIELDS (FROM TO SUBJECT DATE)]")
            .await
            .map_err(|e| self.imap_err(e, false))?;
        imap.logout().await;
        if heads.is_empty() {
            return Ok(format!("no messages match {what}"));
        }
        let mut heads = heads;
        heads.sort_by_key(|a| std::cmp::Reverse(a.0));
        let mut out = format!("{total} match; newest {}:\n", heads.len());
        for (uid, raw) in heads {
            let m = mime::parse(&raw);
            out.push_str(&format!(
                "id {uid} · {} · from {} · {}\n",
                m.date, m.from, m.subject
            ));
        }
        match self.flavor {
            Flavor::Gmail => out.push_str("(read one with gmail_read and its id)\n"),
            Flavor::Imap => out.push_str(&format!(
                "(read one with mail_read, its id and mailbox \"{}\")\n",
                mailbox.unwrap_or("INBOX")
            )),
        }
        Ok(out)
    }

    async fn fetch_one(&self, uid: u32, mailbox: Option<&str>) -> Result<mime::Message, String> {
        let mut imap = self.session(mailbox).await?;
        let got = imap
            .fetch(&[uid], "BODY.PEEK[]")
            .await
            .map_err(|e| self.imap_err(e, false))?;
        imap.logout().await;
        got.into_iter()
            .next()
            .map(|(_, raw)| mime::parse(&raw))
            .ok_or_else(|| format!("there's no message with id {uid}"))
    }

    fn uid(&self, args: &Value, name: &str) -> Result<u32, String> {
        args[name]
            .as_u64()
            .map(|n| n as u32)
            .or_else(|| args[name].as_str().and_then(|s| s.trim().parse().ok()))
            .ok_or_else(|| format!("`{name}` is a message id from {}_search", self.prefix()))
    }

    async fn read(&self, args: &Value) -> Result<String, String> {
        let m = self
            .fetch_one(self.uid(args, "id")?, opt_str(args, "mailbox"))
            .await?;
        let mut out = format!(
            "From: {}\nTo: {}\n{}Date: {}\nSubject: {}\n\n{}\n",
            m.from,
            m.to,
            if m.cc.is_empty() {
                String::new()
            } else {
                format!("Cc: {}\n", m.cc)
            },
            m.date,
            m.subject,
            m.text.trim()
        );
        if !m.attachments.is_empty() {
            out.push_str(&format!("\nAttachments: {}\n", m.attachments.join(", ")));
        }
        Ok(cap(out))
    }

    async fn folders(&self) -> Result<String, String> {
        let mut imap = self.login().await?;
        let names = imap
            .mailboxes()
            .await
            .map_err(|e| self.imap_err(e, false))?;
        imap.logout().await;
        Ok(format!("Mailboxes: {}", names.join(", ")))
    }

    async fn send(&self, args: &Value) -> Result<String, String> {
        let split = |v: &Value| -> Vec<String> {
            match v {
                Value::String(s) => s.split(',').map(|a| a.trim().to_string()).collect(),
                Value::Array(a) => a
                    .iter()
                    .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
                    .collect(),
                _ => Vec::new(),
            }
            .into_iter()
            .filter(|a| !a.is_empty())
            .collect()
        };
        let body = arg_str(args, "body")?;
        let (mut to, cc) = (split(&args["to"]), split(&args["cc"]));
        let mut subject = opt_str(args, "subject").unwrap_or("").to_string();
        let mut reply: Option<mime::Message> = None;
        if args.get("reply_to_id").is_some_and(|v| !v.is_null()) {
            let original = self
                .fetch_one(self.uid(args, "reply_to_id")?, opt_str(args, "mailbox"))
                .await?;
            if to.is_empty() {
                to = vec![original.from.clone()];
            }
            if subject.is_empty() {
                subject = if original.subject.to_ascii_lowercase().starts_with("re:") {
                    original.subject.clone()
                } else {
                    format!("Re: {}", original.subject)
                };
            }
            reply = Some(original);
        }
        if to.is_empty() {
            return Err("`to` is required".into());
        }
        if subject.is_empty() {
            return Err("`subject` is required".into());
        }
        let message = mime::build(&Outgoing {
            from: &self.email,
            to: &to,
            cc: &cc,
            subject: &subject,
            body,
            in_reply_to: reply.as_ref().map(|m| m.message_id.as_str()),
            references: reply.as_ref().map(|m| m.references.as_str()),
            message_id: None,
            date: mime::date_now(),
            headers: &[],
            files: &[],
        });
        let rcpts: Vec<String> = to.iter().chain(&cc).map(|a| mime::bare(a)).collect();
        let stream = super::net::connect(&self.smtp.addr).await?;
        let mut smtp = Smtp::start(stream).await.map_err(|e| e.text)?;
        if self.smtp.starttls {
            smtp = smtp
                .starttls(&self.smtp.addr.host)
                .await
                .map_err(|e| e.text)?;
        }
        smtp.login(&self.smtp_user, &self.password)
            .await
            .map_err(|e| if e.auth { self.refused.clone() } else { e.text })?;
        smtp.send_mail(&self.email, &rcpts, &message)
            .await
            .map_err(|e| e.text)?;
        smtp.quit().await;
        Ok(format!("sent to {}", rcpts.join(", ")))
    }
}

/// A search key's value: a string the server matches, or a date.
enum Crit {
    Str(String),
    Atom(String),
}

/// `mail_search`'s arguments as RFC 3501 criteria, in a fixed order.
fn criteria(args: &Value) -> Result<Vec<(&'static str, Option<Crit>)>, String> {
    let mut out = Vec::new();
    for (arg, key) in [
        ("from", "FROM"),
        ("to", "TO"),
        ("subject", "SUBJECT"),
        ("text", "TEXT"),
        // What a model used to Gmail's tool sends: a word search.
        ("query", "TEXT"),
    ] {
        if let Some(v) = opt_str(args, arg) {
            out.push((key, Some(Crit::Str(v.trim().to_string()))));
        }
    }
    for (arg, key) in [("since", "SINCE"), ("before", "BEFORE")] {
        if let Some(v) = opt_str(args, arg) {
            out.push((key, Some(Crit::Atom(imap_date(v)?))));
        }
    }
    if args["unseen"].as_bool() == Some(true) {
        out.push(("UNSEEN", None));
    }
    Ok(out)
}

/// `2026-02-01` as IMAP's `1-Feb-2026`.
fn imap_date(day: &str) -> Result<String, String> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let bad = || format!("`{day}` isn't a date like 2026-02-01");
    let parts: Vec<&str> = day.trim().split('-').collect();
    let [y, m, d] = parts.as_slice() else {
        return Err(bad());
    };
    let (y, m, d): (u32, usize, u32) = (
        y.parse().map_err(|_| bad())?,
        m.parse().map_err(|_| bad())?,
        d.parse().map_err(|_| bad())?,
    );
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || y < 1970 {
        return Err(bad());
    }
    Ok(format!("{d}-{}-{y}", MONTHS[m - 1]))
}

/// A server's refusal, short: it's the server's text, not ours.
fn clip(text: &str) -> String {
    text.chars().take(120).collect()
}

#[async_trait]
impl LocalServer for Mail {
    fn id(&self) -> String {
        self.id.clone()
    }

    fn tools(&self) -> Vec<Value> {
        let mut t = match self.flavor {
            Flavor::Gmail => vec![
                tool(
                    "gmail_search",
                    "Search Gmail with Gmail's own syntax (`from:dana newer_than:7d`, `is:unread in:inbox`, `subject:invoice`). Lists the newest matches with their ids.",
                    json!({ "query": { "type": "string" }, "limit": { "type": "integer", "maximum": 25 } }),
                    &[],
                    true,
                ),
                tool(
                    "gmail_read",
                    "Read one Gmail message by the id gmail_search gave.",
                    json!({ "id": { "type": "integer" } }),
                    &["id"],
                    true,
                ),
            ],
            Flavor::Imap => vec![
                tool(
                    "mail_search",
                    "Search this mailbox. Every field given must match; none lists the newest mail. Lists the newest matches with their ids (ids belong to the mailbox searched).",
                    json!({
                        "mailbox": { "type": "string", "description": "Default INBOX; mail_folders lists the others" },
                        "from": { "type": "string" },
                        "to": { "type": "string" },
                        "subject": { "type": "string" },
                        "text": { "type": "string", "description": "Anywhere in the headers or body" },
                        "since": { "type": "string", "description": "YYYY-MM-DD" },
                        "before": { "type": "string", "description": "YYYY-MM-DD" },
                        "unseen": { "type": "boolean" },
                        "limit": { "type": "integer", "maximum": 25 },
                    }),
                    &[],
                    true,
                ),
                tool(
                    "mail_read",
                    "Read one message by the id mail_search gave, in the same mailbox.",
                    json!({ "id": { "type": "integer" }, "mailbox": { "type": "string", "description": "Default INBOX" } }),
                    &["id"],
                    true,
                ),
                tool(
                    "mail_folders",
                    "List this account's mailboxes (folders).",
                    json!({}),
                    &[],
                    true,
                ),
            ],
        };
        if self.write {
            let name = format!("{}_send", self.prefix());
            let mut props = json!({
                "to": { "type": "string", "description": "Addresses, comma-separated" },
                "cc": { "type": "string" },
                "subject": { "type": "string" },
                "body": { "type": "string", "description": "Plain text" },
                "reply_to_id": { "type": "integer" },
            });
            if self.flavor == Flavor::Imap {
                props["mailbox"] = json!({ "type": "string", "description": "Where reply_to_id is; default INBOX" });
            }
            t.push(tool(
                &name,
                "Send an email from this account. With `reply_to_id`, it replies in that thread (to the sender, with Re: subject, unless given).",
                props,
                &["body"],
                false,
            ));
        }
        t
    }

    async fn call(&self, name: &str, args: Value) -> Result<String, String> {
        let op = name
            .strip_prefix(self.prefix())
            .and_then(|r| r.strip_prefix('_'));
        match (op, self.flavor) {
            (Some("search"), _) => self.search(&args).await,
            (Some("read"), _) => self.read(&args).await,
            (Some("folders"), Flavor::Imap) => self.folders().await,
            (Some("send"), _) if self.write => self.send(&args).await,
            (Some("send"), _) => Err("this connection is read-only".into()),
            _ => Err(format!("no tool `{name}`")),
        }
    }
}
