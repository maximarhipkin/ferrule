//! What comes in: signal-cli's event stream (`GET /api/v1/events`, one
//! `data:` line of JSON per envelope), read for as long as it stays open and
//! checked with a `version` call every minute; each envelope becomes a
//! message, or nothing.

use super::{is_group, SignalChannel};
use crate::channels::access::{self, Dm};
use crate::channels::files::{self, Refused, Saved};
use crate::error::GatewayError;
use crate::message::InboundMessage;
use base64::Engine;
use serde_json::{json, Value};
use std::time::SystemTime;
use tokio::sync::mpsc;

/// Where a mention sits in the text.
const MENTION: char = '\u{FFFC}';

/// Why the stream ended.
enum End {
    /// The gateway is shutting down.
    Closed,
    Lost(String),
}

impl SignalChannel {
    pub(super) async fn run_events(
        &self,
        tx: mpsc::Sender<InboundMessage>,
    ) -> Result<(), GatewayError> {
        let mut backoff = self.timing.backoff_min;
        loop {
            if tx.is_closed() {
                return Ok(());
            }
            let started = std::time::Instant::now();
            match self.stream_once(&tx).await {
                End::Closed => return Ok(()),
                End::Lost(why) => {
                    tracing::warn!("signal: {why}");
                    *self.events_problem.lock().unwrap() = Some(why);
                }
            }
            // A stream that stayed up a while starts its waits afresh.
            if started.elapsed() >= self.timing.healthy {
                backoff = self.timing.backoff_min;
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(self.timing.backoff_max);
        }
    }

    fn polled(&self) {
        *self.last_poll.lock().unwrap() = Some(SystemTime::now());
    }

    /// Our own ACI uuid: mentions and quotes may carry only that.
    async fn learn_uuid(&self) {
        if self.me_uuid.lock().unwrap().is_some() {
            return;
        }
        let r = self
            .rpc("getUserStatus", json!({ "recipient": [self.cfg.account] }))
            .await;
        if let Ok(v) = r {
            if let Some(u) = v[0]["uuid"].as_str().filter(|u| !u.is_empty()) {
                *self.me_uuid.lock().unwrap() = Some(u.to_string());
            }
        }
    }

    async fn stream_once(&self, tx: &mpsc::Sender<InboundMessage>) -> End {
        let mut url = format!("{}/api/v1/events", self.base);
        if self.multi.load(std::sync::atomic::Ordering::Relaxed) {
            url = format!("{url}?account={}", urlencode(&self.cfg.account));
        }
        let resp = match self
            .stream_client
            .get(&url)
            .header("accept", "text/event-stream")
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return End::Lost(super::RpcError::transport(&self.base, e).message),
        };
        let status = resp.status();
        if !status.is_success() {
            // A daemon with several accounts wants ours named.
            if status.as_u16() == 400
                && !self.multi.swap(true, std::sync::atomic::Ordering::Relaxed)
            {
                return End::Lost("signal-cli serves several accounts; naming ours".into());
            }
            return End::Lost(format!("signal-cli's event stream at {url} said {status}"));
        }
        self.polled();
        *self.events_problem.lock().unwrap() = None;
        self.learn_uuid().await;
        let mut resp = resp;
        let mut buf: Vec<u8> = Vec::new();
        let mut data = String::new();
        let mut ping = tokio::time::interval(self.timing.ping);
        ping.tick().await;
        loop {
            let chunk = tokio::select! {
                c = resp.chunk() => c,
                _ = ping.tick() => {
                    match self.rpc("version", json!({})).await {
                        Ok(_) => self.polled(),
                        Err(e) => return End::Lost(e.message),
                    }
                    continue;
                }
            };
            let bytes = match chunk {
                Ok(Some(b)) => b,
                Ok(None) => return End::Lost("signal-cli closed its event stream".into()),
                Err(e) => {
                    return End::Lost(format!(
                        "signal-cli's event stream broke: {}",
                        e.without_url()
                    ))
                }
            };
            buf.extend_from_slice(&bytes);
            while let Some(nl) = buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = buf.drain(..=nl).collect();
                let line = String::from_utf8_lossy(&line);
                let line = line.trim_end_matches(['\n', '\r']);
                self.polled();
                if line.is_empty() {
                    if !data.is_empty() {
                        let event = std::mem::take(&mut data);
                        if !self.on_event(&event, tx).await {
                            return End::Closed;
                        }
                    }
                } else if let Some(d) = line.strip_prefix("data:") {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(d.strip_prefix(' ').unwrap_or(d));
                }
                // `:` comments keep the stream alive; `event:`/`id:` say
                // nothing we need.
            }
        }
    }

    /// One event's JSON: `false` once the gateway is gone.
    async fn on_event(&self, data: &str, tx: &mpsc::Sender<InboundMessage>) -> bool {
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            tracing::debug!("signal: an event that isn't JSON");
            return true;
        };
        // The HTTP daemon sends `{envelope, account}`; JSON-RPC's
        // notification form wraps the same in `params`.
        let body = if v.get("envelope").is_some() {
            &v
        } else {
            &v["params"]
        };
        if let Some(acc) = body["account"].as_str() {
            if acc != self.cfg.account {
                return true;
            }
        }
        match self.message(&body["envelope"]).await {
            Some(m) => tx.send(m).await.is_ok(),
            None => true,
        }
    }

    fn is_me(&self, number: Option<&str>, uuid: Option<&str>) -> bool {
        number == Some(self.cfg.account.as_str())
            || uuid.is_some_and(|u| self.me_uuid.lock().unwrap().as_deref() == Some(u))
    }

    /// An envelope as a message for the agent, when it is one and allowed.
    pub(super) async fn message(&self, env: &Value) -> Option<InboundMessage> {
        let number = env["sourceNumber"]
            .as_str()
            .or(env["source"].as_str().filter(|s| s.starts_with('+')))
            .filter(|s| !s.is_empty());
        let uuid = env["sourceUuid"].as_str().filter(|s| !s.is_empty());
        let name = env["sourceName"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or(number)
            .or(uuid)?
            .to_string();

        // "Note to Self" from the owner's phone, on a linked number.
        let sent = &env["syncMessage"]["sentMessage"];
        if sent.is_object() {
            let to = sent["destinationNumber"]
                .as_str()
                .or(sent["destination"].as_str());
            let to_uuid = sent["destinationUuid"].as_str();
            if let Some(u) = to_uuid.filter(|_| to == Some(self.cfg.account.as_str())) {
                self.me_uuid
                    .lock()
                    .unwrap()
                    .get_or_insert_with(|| u.to_string());
            }
            if sent["groupInfo"].is_object()
                || !self.is_me(to, to_uuid)
                || !self.access.user_allowed(&self.cfg.account)
            {
                return None;
            }
            let account = self.cfg.account.clone();
            return self.data(sent, &account, &account, &name, None).await;
        }

        let dm = &env["dataMessage"];
        if !dm.is_object() || dm["reaction"].is_object() || dm["remoteDelete"].is_object() {
            return None;
        }
        if self.is_me(number, uuid) {
            return None;
        }
        let id_of_sender = match (number, uuid) {
            (Some(n), Some(u)) if !self.access.user_allowed(n) && self.access.user_allowed(u) => u,
            (Some(n), _) => n,
            (None, Some(u)) => u,
            (None, None) => return None,
        }
        .to_string();

        if let Some(group) = dm["groupInfo"]["groupId"].as_str() {
            let mentioned = dm["mentions"].as_array().is_some_and(|ms| {
                ms.iter()
                    .any(|m| self.is_me(m["number"].as_str(), m["uuid"].as_str()))
            });
            let quoted = self.is_me(
                dm["quote"]["authorNumber"]
                    .as_str()
                    .or(dm["quote"]["author"].as_str()),
                dm["quote"]["authorUuid"].as_str(),
            );
            if !self.access.channel_allowed(group) {
                if mentioned || quoted {
                    self.access.ignore(group, &name, "group");
                }
                return None;
            }
            if !(mentioned || quoted) {
                return None;
            }
            return self
                .data(dm, group, &id_of_sender, &name, Some(group))
                .await;
        }

        let text = dm["message"].as_str().unwrap_or("");
        match self.access.dm(&id_of_sender, &name, text) {
            Dm::Admit => {}
            Dm::Tell(t) | Dm::Paired(t) => {
                self.tell(&id_of_sender, t).await;
                return None;
            }
            Dm::Drop => return None,
        }
        self.data(dm, &id_of_sender, &id_of_sender, &name, None)
            .await
    }

    /// A data message's text and files, for `chat`.
    async fn data(
        &self,
        dm: &Value,
        chat: &str,
        sender: &str,
        name: &str,
        group: Option<&str>,
    ) -> Option<InboundMessage> {
        let ts = dm["timestamp"].as_i64()?;
        let author = if group.is_none() && chat == self.cfg.account {
            self.cfg.account.clone()
        } else {
            sender.to_string()
        };
        let id = format!("{ts}:{author}");
        let mut text = self.mentions(dm);
        let mut saved: Vec<Saved> = vec![];
        let mut refused: Vec<Refused> = vec![];
        for att in dm["attachments"].as_array().into_iter().flatten() {
            let name = att["filename"]
                .as_str()
                .filter(|n| !n.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    format!(
                        "{}{}",
                        att["id"].as_str().unwrap_or("file"),
                        ext_for(att["contentType"].as_str())
                    )
                });
            let mime = att["contentType"].as_str();
            match self.fetch(att, &name, mime, &id, chat).await {
                Ok(s) => saved.push(s),
                Err(r) if self.cfg.inbox.is_none() => {
                    let kind = access::file_kind(mime);
                    if text.trim().is_empty() {
                        self.tell(chat, access::cannot_read_text(kind)).await;
                        return None;
                    }
                    text = access::unread_note(&text, kind);
                    let _ = r;
                }
                Err(r) => {
                    self.tell(chat, files::refused_reply(&r)).await;
                    refused.push(r);
                }
            }
        }
        let text = files::with_notes(&text, &saved, &refused);
        if text.trim().is_empty() {
            return None;
        }
        let q = &dm["quote"];
        let reply_to = q["id"].as_i64().and_then(|qts| {
            let who = q["authorNumber"]
                .as_str()
                .or(q["author"].as_str())
                .or(q["authorUuid"].as_str())?;
            Some(format!("{qts}:{who}"))
        });
        Some(InboundMessage {
            channel: "signal".into(),
            chat_id: chat.to_string(),
            sender: name.to_string(),
            sender_id: Some(sender.to_string()),
            message_id: id,
            text,
            attachments: saved
                .iter()
                .map(|s| crate::message::Attachment {
                    kind: s.mime.clone(),
                    url: s.path.to_string_lossy().into_owned(),
                    name: Some(s.rel.clone()),
                })
                .collect(),
            reply_to,
            ts: ts / 1000,
        })
    }

    /// The text with each mention's placeholder put back as `@name`, and
    /// ours taken out.
    fn mentions(&self, dm: &Value) -> String {
        let text = dm["message"].as_str().unwrap_or("");
        let mut ms: Vec<&Value> = dm["mentions"]
            .as_array()
            .map(|a| a.iter().collect())
            .unwrap_or_default();
        ms.sort_by_key(|m| m["start"].as_i64().unwrap_or(0));
        let mut ms = ms.into_iter();
        let mut out = String::with_capacity(text.len());
        for c in text.chars() {
            if c != MENTION {
                out.push(c);
                continue;
            }
            let Some(m) = ms.next() else {
                continue;
            };
            if self.is_me(m["number"].as_str(), m["uuid"].as_str()) {
                continue;
            }
            let who = m["name"]
                .as_str()
                .or(m["number"].as_str())
                .unwrap_or("someone");
            out.push('@');
            out.push_str(who);
        }
        access::strip_leading(&out, &[])
    }

    /// One attachment, from the daemon's store into the inbox.
    async fn fetch(
        &self,
        att: &Value,
        name: &str,
        mime: Option<&str>,
        id: &str,
        chat: &str,
    ) -> Result<Saved, Refused> {
        let Some(inbox) = &self.cfg.inbox else {
            return Err(Refused {
                name: name.to_string(),
                why: "files aren't saved on this gateway".into(),
            });
        };
        if let Some(n) = att["size"].as_u64().filter(|n| *n > inbox.max_bytes()) {
            return Err(inbox.too_big(name, n));
        }
        let Some(att_id) = att["id"].as_str() else {
            return Err(Refused {
                name: name.to_string(),
                why: "signal-cli didn't say where it is".into(),
            });
        };
        let mut params = json!({ "id": att_id });
        if is_group(chat) {
            params["groupId"] = json!(chat);
        } else {
            params["recipient"] = json!(chat);
        }
        let fail = |why: String| Refused {
            name: name.to_string(),
            why,
        };
        let r = self
            .rpc("getAttachment", params)
            .await
            .map_err(|e| fail(format!("it couldn't be fetched ({})", e.message)))?;
        let b64 = r
            .as_str()
            .or(r["data"].as_str())
            .ok_or_else(|| fail("signal-cli returned no data for it".into()))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .map_err(|_| fail("signal-cli returned it garbled".into()))?;
        if bytes.len() as u64 > inbox.max_bytes() {
            return Err(inbox.too_big(name, bytes.len() as u64));
        }
        inbox
            .save("signal", id, name, mime, &bytes)
            .map_err(|e| fail(format!("it couldn't be saved ({e})")))
    }
}

/// A file name's extension for a MIME type, when Signal sent no name
/// (photos and voice notes).
fn ext_for(mime: Option<&str>) -> &'static str {
    match mime.unwrap_or("") {
        "image/jpeg" => ".jpg",
        "image/png" => ".png",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        "audio/aac" => ".aac",
        "audio/mpeg" => ".mp3",
        "audio/ogg" => ".ogg",
        "video/mp4" => ".mp4",
        "application/pdf" => ".pdf",
        _ => "",
    }
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}
