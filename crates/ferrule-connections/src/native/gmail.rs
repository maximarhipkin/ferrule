//! Gmail with an app password: search and read over IMAP, send over SMTP.
//! Sending is a write: listed only on a write connection, and gated like
//! any write tool. A session is opened per call; nothing stays logged in.

use super::imap::{Error as ImapError, Imap};
use super::mime::{self, Outgoing};
use super::smtp::Smtp;
use super::{arg_str, cap, limit, opt_str, tool, Endpoints};
use async_trait::async_trait;
use ferrule_mcp::local::LocalServer;
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub struct Gmail {
    id: String,
    email: String,
    password: String,
    write: bool,
    ep: Endpoints,
}

const REFUSED: &str = "Google refused the email and app password. Check the address, and make a \
     new app password at https://myaccount.google.com/apppasswords (it needs 2-Step \
     Verification on the account). Your normal Google password won't work here.";

fn imap_err(e: ImapError, login: bool) -> String {
    match e {
        ImapError::No(t) if login || t.contains("AUTHENTICATIONFAILED") => REFUSED.into(),
        ImapError::No(t) if t.contains("WEBALERT") || t.contains("ALERT") => {
            "Google wants you to sign in once in a browser before this connection can use \
             Gmail: open https://mail.google.com, then try again."
                .into()
        }
        ImapError::No(_) => "Gmail refused the request".into(),
        ImapError::Broken(t) => t,
    }
}

impl Gmail {
    pub fn new(
        id: &str,
        fields: &BTreeMap<String, String>,
        write: bool,
        ep: Endpoints,
    ) -> Result<Self, String> {
        let get = |k: &str| {
            fields
                .get(k)
                .filter(|v| !v.is_empty())
                .cloned()
                .ok_or_else(|| format!("the {} is missing", k.replace('_', " ")))
        };
        Ok(Self {
            id: format!("native:gmail:{id}"),
            email: get("email")?,
            password: get("app_password")?,
            write,
            ep,
        })
    }

    async fn session(&self) -> Result<Imap, String> {
        let stream = super::net::connect(&self.ep.imap).await?;
        let mut imap = Imap::start(stream).await.map_err(|e| imap_err(e, false))?;
        imap.login(&self.email, &self.password)
            .await
            .map_err(|e| imap_err(e, true))?;
        let all = imap.all_mail().await.map_err(|e| imap_err(e, false))?;
        imap.examine(&all).await.map_err(|e| imap_err(e, false))?;
        Ok(imap)
    }

    /// Logs in and out; the address it's for.
    pub async fn check(&self) -> Result<String, String> {
        self.session().await?.logout().await;
        Ok(self.email.clone())
    }

    async fn search(&self, args: &Value) -> Result<String, String> {
        let query = opt_str(args, "query").unwrap_or("in:inbox");
        let n = limit(args, 10, 25) as usize;
        let mut imap = self.session().await?;
        let uids = imap.search(query).await.map_err(|e| imap_err(e, false))?;
        let total = uids.len();
        let newest: Vec<u32> = uids.iter().rev().take(n).copied().collect();
        let heads = imap
            .fetch(&newest, "BODY.PEEK[HEADER.FIELDS (FROM TO SUBJECT DATE)]")
            .await
            .map_err(|e| imap_err(e, false))?;
        imap.logout().await;
        if heads.is_empty() {
            return Ok(format!("no messages match `{query}`"));
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
        out.push_str("(read one with gmail_read and its id)\n");
        Ok(out)
    }

    async fn fetch_one(&self, uid: u32) -> Result<mime::Message, String> {
        let mut imap = self.session().await?;
        let got = imap
            .fetch(&[uid], "BODY.PEEK[]")
            .await
            .map_err(|e| imap_err(e, false))?;
        imap.logout().await;
        got.into_iter()
            .next()
            .map(|(_, raw)| mime::parse(&raw))
            .ok_or_else(|| format!("there's no message with id {uid}"))
    }

    fn uid(args: &Value, name: &str) -> Result<u32, String> {
        args[name]
            .as_u64()
            .map(|n| n as u32)
            .or_else(|| args[name].as_str().and_then(|s| s.trim().parse().ok()))
            .ok_or_else(|| format!("`{name}` is a message id from gmail_search"))
    }

    async fn read(&self, args: &Value) -> Result<String, String> {
        let m = self.fetch_one(Self::uid(args, "id")?).await?;
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
            let original = self.fetch_one(Self::uid(args, "reply_to_id")?).await?;
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
        });
        let rcpts: Vec<String> = to.iter().chain(&cc).map(|a| mime::bare(a)).collect();
        let stream = super::net::connect(&self.ep.smtp).await?;
        let mut smtp = Smtp::start(stream).await.map_err(|e| e.text)?;
        smtp.login(&self.email, &self.password).await.map_err(|e| {
            if e.auth {
                REFUSED.to_string()
            } else {
                e.text
            }
        })?;
        smtp.send_mail(&self.email, &rcpts, &message)
            .await
            .map_err(|e| e.text)?;
        smtp.quit().await;
        Ok(format!("sent to {}", rcpts.join(", ")))
    }
}

#[async_trait]
impl LocalServer for Gmail {
    fn id(&self) -> String {
        self.id.clone()
    }

    fn tools(&self) -> Vec<Value> {
        let mut t = vec![
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
        ];
        if self.write {
            t.push(tool(
                "gmail_send",
                "Send an email from this Gmail account. With `reply_to_id`, it replies in that thread (to the sender, with Re: subject, unless given).",
                json!({
                    "to": { "type": "string", "description": "Addresses, comma-separated" },
                    "cc": { "type": "string" },
                    "subject": { "type": "string" },
                    "body": { "type": "string", "description": "Plain text" },
                    "reply_to_id": { "type": "integer" },
                }),
                &["body"],
                false,
            ));
        }
        t
    }

    async fn call(&self, name: &str, args: Value) -> Result<String, String> {
        match name {
            "gmail_search" => self.search(&args).await,
            "gmail_read" => self.read(&args).await,
            "gmail_send" if self.write => self.send(&args).await,
            "gmail_send" => Err("this connection is read-only".into()),
            other => Err(format!("no tool `{other}`")),
        }
    }
}
