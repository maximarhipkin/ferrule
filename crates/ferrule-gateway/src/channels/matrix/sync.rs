//! What comes in: the `/sync` long-poll, invites, messages and reactions.

use super::{enc, localpart, MatrixChannel, MxError};
use crate::channels::access::{self, Dm};
use crate::channels::files;
use crate::error::GatewayError;
use crate::message::InboundMessage;
use reqwest::Method;
use serde_json::{json, Value};
use std::time::{Duration, SystemTime};
use tokio::sync::mpsc;

/// Timeline events per room per sync; lazy members; nothing else.
const FILTER: &str = r#"{"presence":{"not_types":["*"]},"account_data":{"not_types":["*"]},"room":{"timeline":{"limit":50},"state":{"lazy_load_members":true},"ephemeral":{"not_types":["*"]},"account_data":{"not_types":["*"]}}}"#;

impl MatrixChannel {
    pub(super) async fn run_sync(
        &self,
        tx: mpsc::Sender<InboundMessage>,
    ) -> Result<(), GatewayError> {
        self.syncing
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let mut backoff = self.timing.backoff_min;
        loop {
            if tx.is_closed() {
                return Ok(());
            }
            let me = match self.me().await {
                Ok(me) => me,
                Err(e) => {
                    tracing::warn!("matrix: {}", self.refused(&e));
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(self.timing.backoff_max);
                    continue;
                }
            };
            let since = {
                let mut state = self.state.lock().unwrap();
                if state.user_id != me {
                    // Another account's position means nothing here.
                    state.user_id = me.clone();
                    state.next_batch = None;
                }
                state.next_batch.clone()
            };
            // The first sync catches up: what was said before is not
            // answered now.
            let catch_up = since.is_none();
            let timeout = if catch_up {
                Duration::ZERO
            } else {
                self.timing.sync
            };
            match self.sync_once(since.as_deref(), timeout).await {
                Ok(v) => {
                    *self.last_poll.lock().unwrap() = Some(SystemTime::now());
                    *self.sync_problem.lock().unwrap() = None;
                    backoff = self.timing.backoff_min;
                    if !self.on_sync(&v, &me, catch_up, &tx).await {
                        return Ok(());
                    }
                    if let Some(next) = v["next_batch"].as_str() {
                        self.state.lock().unwrap().next_batch = Some(next.to_string());
                        self.save_state();
                    }
                }
                Err(e) if e.status == 429 => {
                    let wait = e
                        .retry_after
                        .unwrap_or(backoff)
                        .min(self.timing.backoff_max);
                    tokio::time::sleep(wait).await;
                }
                Err(e) => {
                    tracing::warn!("matrix: {}", e.message);
                    *self.sync_problem.lock().unwrap() = Some(e.message);
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(self.timing.backoff_max);
                }
            }
        }
    }

    async fn sync_once(&self, since: Option<&str>, timeout: Duration) -> Result<Value, MxError> {
        let url = self.url("/_matrix/client/v3/sync");
        let mut query = vec![
            ("filter", FILTER.to_string()),
            ("timeout", timeout.as_millis().to_string()),
        ];
        if let Some(s) = since {
            query.push(("since", s.to_string()));
        }
        let resp = self
            .call_with(
                |_| {
                    self.client
                        .get(&url)
                        .query(&query)
                        .timeout(timeout + Duration::from_secs(30))
                },
                "sync",
            )
            .await?;
        Self::json_of(resp, "sync").await
    }

    /// One sync's rooms; `false` when the gateway stopped listening.
    async fn on_sync(
        &self,
        v: &Value,
        me: &str,
        catch_up: bool,
        tx: &mpsc::Sender<InboundMessage>,
    ) -> bool {
        if let Some(invites) = v["rooms"]["invite"].as_object() {
            for (room, r) in invites {
                self.on_invite(room, r, me).await;
            }
        }
        if let Some(left) = v["rooms"]["leave"].as_object() {
            for room in left.keys() {
                self.forget_room(room);
            }
        }
        let Some(joined) = v["rooms"]["join"].as_object() else {
            return true;
        };
        for (room, r) in joined {
            for ev in r["state"]["events"].as_array().into_iter().flatten() {
                self.on_state(room, ev);
            }
            for ev in r["timeline"]["events"].as_array().into_iter().flatten() {
                if ev["state_key"].is_string() {
                    self.on_state(room, ev);
                    continue;
                }
                if catch_up {
                    continue;
                }
                let sender = ev["sender"].as_str().unwrap_or("");
                if sender == me {
                    continue;
                }
                let msg = match ev["type"].as_str().unwrap_or("") {
                    "m.room.encrypted" => {
                        self.mark_encrypted(room);
                        self.notice_encrypted(room).await;
                        None
                    }
                    "m.room.message" => self.on_message(room, ev, me).await,
                    "m.reaction" => self.on_reaction(room, ev),
                    _ => None,
                };
                if let Some(m) = msg {
                    if tx.send(m).await.is_err() {
                        return false;
                    }
                }
            }
        }
        true
    }

    /// A state event: encryption turned on, members changed.
    fn on_state(&self, room: &str, ev: &Value) {
        match ev["type"].as_str().unwrap_or("") {
            "m.room.encryption" => self.mark_encrypted(room),
            "m.room.member" => {
                if let Some(r) = self.rooms.lock().unwrap().get_mut(room) {
                    r.members = None;
                }
            }
            _ => {}
        }
    }

    fn forget_room(&self, room: &str) {
        self.rooms.lock().unwrap().remove(room);
        self.left.lock().unwrap().insert(room.to_string());
        let changed = {
            let mut state = self.state.lock().unwrap();
            let before = state.dms.len();
            state.dms.retain(|_, r| r != room);
            before != state.dms.len()
        };
        if changed {
            self.save_state();
        }
    }

    /// An invite: joined when an allowed user sent it (or while pairing).
    async fn on_invite(&self, room: &str, r: &Value, me: &str) {
        let events: Vec<&Value> = r["invite_state"]["events"]
            .as_array()
            .map(|a| a.iter().collect())
            .unwrap_or_default();
        let inviter = events
            .iter()
            .find(|e| {
                e["type"] == "m.room.member"
                    && e["state_key"] == me
                    && e["content"]["membership"] == "invite"
            })
            .and_then(|e| e["sender"].as_str())
            .unwrap_or("")
            .to_string();
        let direct = events.iter().any(|e| {
            e["type"] == "m.room.member"
                && e["state_key"] == me
                && e["content"]["is_direct"] == true
        });
        if !(self.pairing || self.access.user_allowed(&inviter)) {
            self.access.ignore(room, &inviter, "invite");
            return;
        }
        let path = format!("/_matrix/client/v3/join/{}", enc(room));
        if let Err(e) = self
            .call(Method::POST, &path, Some(&json!({})), "join")
            .await
        {
            tracing::warn!("matrix: couldn't join {room}: {}", e.message);
            return;
        }
        tracing::info!("matrix: joined {room}, invited by {inviter}");
        if direct && !inviter.is_empty() {
            self.remember_dm(&inviter, room);
        }
        if events.iter().any(|e| e["type"] == "m.room.encryption") || self.is_encrypted(room).await
        {
            self.mark_encrypted(room);
            self.notice_encrypted(room).await;
        }
    }

    /// A text or file message, if it's for the agent.
    async fn on_message(&self, room: &str, ev: &Value, me: &str) -> Option<InboundMessage> {
        let c = &ev["content"];
        let msgtype = c["msgtype"].as_str().unwrap_or("");
        if msgtype == "m.notice" || c["m.relates_to"]["rel_type"] == "m.replace" {
            return None;
        }
        let sender = ev["sender"].as_str()?.to_string();
        let id = ev["event_id"].as_str()?.to_string();
        if self.is_encrypted(room).await {
            self.notice_encrypted(room).await;
            return None;
        }
        let reply_to = c["m.relates_to"]["m.in_reply_to"]["event_id"]
            .as_str()
            .map(str::to_string);
        let media = matches!(msgtype, "m.image" | "m.file" | "m.audio" | "m.video");
        let body = c["body"].as_str().unwrap_or("");
        // A file's body is its name, unless a caption was given.
        let mut text = if media {
            match c["filename"].as_str() {
                Some(f) if f != body => body.to_string(),
                _ => String::new(),
            }
        } else if reply_to.is_some() {
            strip_reply_fallback(body)
        } else {
            body.to_string()
        };
        let dm = self.is_dm(room).await;
        let chat_id = if dm {
            match self.access.dm(&sender, &sender, &text) {
                Dm::Admit => {}
                Dm::Tell(t) | Dm::Paired(t) => {
                    self.tell(room, t).await;
                    return None;
                }
                Dm::Drop => return None,
            }
            self.remember_dm(&sender, room);
            sender.clone()
        } else {
            if !self.access.channel_allowed(room) {
                self.access.ignore(room, &sender, "room");
                return None;
            }
            let forms = self.mention_forms(me);
            let mentioned = c["m.mentions"]["user_ids"]
                .as_array()
                .is_some_and(|a| a.iter().any(|u| u == me))
                || body.contains(me)
                || forms.iter().any(|f| starts_with_ci(text.trim_start(), f))
                || reply_to.as_deref().is_some_and(|r| self.is_ours(r));
            if !mentioned {
                return None;
            }
            text = access::strip_leading(&text, &forms);
            room.to_string()
        };
        let mut saved = vec![];
        let mut refused = vec![];
        if media {
            let name = c["filename"]
                .as_str()
                .or(c["body"].as_str())
                .filter(|n| !n.is_empty())
                .unwrap_or("file")
                .to_string();
            let mime = c["info"]["mimetype"].as_str();
            let r = match c["url"].as_str() {
                Some(mxc) => {
                    self.fetch_media(mxc, &name, c["info"]["size"].as_u64(), mime, &id)
                        .await
                }
                None => Err(files::Refused {
                    name: name.clone(),
                    why: "it is end-to-end encrypted, and ferrule can't decrypt it".into(),
                }),
            };
            match r {
                Ok(s) => saved.push(s),
                Err(r) if self.cfg.inbox.is_none() => {
                    let kind = access::file_kind(mime);
                    if text.trim().is_empty() {
                        self.tell(room, access::cannot_read_text(kind)).await;
                        return None;
                    }
                    text = access::unread_note(&text, kind);
                    let _ = r;
                }
                Err(r) => {
                    self.tell(room, files::refused_reply(&r)).await;
                    refused.push(r);
                }
            }
            text = files::with_notes(&text, &saved, &refused);
        } else if !matches!(msgtype, "m.text" | "m.emote") {
            tracing::info!(room = %room, "matrix: got a {msgtype} message ferrule can't read");
            return None;
        }
        if text.trim().is_empty() {
            return None;
        }
        Some(InboundMessage {
            channel: "matrix".into(),
            chat_id,
            sender: sender.clone(),
            sender_id: Some(sender),
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
            ts: ev["origin_server_ts"].as_i64().unwrap_or(0) / 1000,
        })
    }

    /// The ways a room message addresses the bot at its start.
    fn mention_forms(&self, me: &str) -> Vec<String> {
        let name = self.me.lock().unwrap().1.clone();
        let local = localpart(me).to_string();
        let mut forms = vec![me.to_string(), format!("@{local}"), local];
        if !name.is_empty() {
            forms.push(format!("@{name}"));
            forms.push(name);
        }
        // Longest first, so `@ferrule` goes before `ferrule`.
        forms.sort_by_key(|f| std::cmp::Reverse(f.len()));
        forms.dedup();
        forms
    }

    /// A reaction on an approval, from an allowed user: its answer.
    fn on_reaction(&self, room: &str, ev: &Value) -> Option<InboundMessage> {
        let rel = &ev["content"]["m.relates_to"];
        if rel["rel_type"] != "m.annotation" {
            return None;
        }
        let sender = ev["sender"].as_str()?;
        if !self.access.user_allowed(sender) {
            return None;
        }
        let target = rel["event_id"].as_str()?;
        let key = rel["key"].as_str()?;
        let cmd = self.approval_answer(target, key)?;
        // The approval went to a DM's user id or to the room.
        let dm = self
            .state
            .lock()
            .unwrap()
            .dms
            .iter()
            .find(|(_, r)| *r == room)
            .map(|(u, _)| u.clone());
        Some(InboundMessage {
            channel: "matrix".into(),
            chat_id: dm
                .filter(|u| u == sender)
                .unwrap_or_else(|| room.to_string()),
            sender: sender.to_string(),
            sender_id: Some(sender.to_string()),
            message_id: ev["event_id"].as_str().unwrap_or("").to_string(),
            text: cmd,
            attachments: vec![],
            reply_to: Some(target.to_string()),
            ts: ev["origin_server_ts"].as_i64().unwrap_or(0) / 1000,
        })
    }

    /// A short answer from the adapter itself, as a notice (pairing, a
    /// refused file).
    async fn tell(&self, room: &str, text: String) {
        let content = json!({ "msgtype": "m.notice", "body": text });
        if let Err(e) = self.send_event(room, "m.room.message", &content).await {
            tracing::warn!("matrix: couldn't answer in {room}: {}", e.message);
        }
    }
}

fn starts_with_ci(text: &str, form: &str) -> bool {
    let n = form.len();
    text.len() >= n && text.is_char_boundary(n) && text[..n].eq_ignore_ascii_case(form)
}

/// A reply's body without the quoted original clients put before it
/// (`> <@a:b> text` lines, then a blank line).
pub(super) fn strip_reply_fallback(body: &str) -> String {
    if !body.starts_with("> ") {
        return body.to_string();
    }
    match body.split_once("\n\n") {
        Some((quote, rest)) if quote.split('\n').all(|l| l.starts_with('>')) => rest.to_string(),
        _ => body.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::strip_reply_fallback;

    #[test]
    fn a_reply_loses_its_quote() {
        assert_eq!(
            strip_reply_fallback("> <@a:x.org> earlier\n> more\n\nmy answer"),
            "my answer"
        );
        assert_eq!(strip_reply_fallback("> a quote alone"), "> a quote alone");
        assert_eq!(strip_reply_fallback("plain"), "plain");
    }
}
