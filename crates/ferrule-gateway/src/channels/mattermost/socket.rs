//! The WebSocket (`/api/v4/websocket`): the token goes in an
//! `authentication_challenge`, then the server sends events: `posted` (the
//! post itself a JSON string in `data.post`) and `reaction_added`. A `ping`
//! action goes out every 30 s; a socket silent for 90 s, closed or failed
//! is reopened with backoff (1 → 60 s). A refused token stops the channel.

use super::{is_id, MattermostChannel};
use crate::channels::access::{self, Dm};
use crate::channels::files;
use crate::channels::ws::{self, Message};
use crate::error::GatewayError;
use crate::message::InboundMessage;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;

/// How one connection ended.
enum End {
    /// Reconnect after the backoff.
    Again(String),
    /// A failure no retry fixes (a refused token).
    Fatal(String),
    /// The gateway stopped listening (shutdown).
    Done,
}

impl MattermostChannel {
    pub(super) async fn run_socket(
        &self,
        tx: mpsc::Sender<InboundMessage>,
    ) -> Result<(), GatewayError> {
        let mut backoff = self.timing.backoff_min;
        loop {
            let why = match self.connection(&tx, &mut backoff).await {
                End::Done => return Ok(()),
                End::Fatal(why) => {
                    let why = format!("stopped: {why}");
                    tracing::error!("mattermost: {why}");
                    self.set_socket_problem(Some(why.clone()));
                    return Err(GatewayError::Channel(format!("mattermost {why}")));
                }
                End::Again(why) => why,
            };
            tracing::warn!("mattermost: {why}; reconnecting");
            self.set_socket_problem(Some(format!("{why}; reconnecting")));
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(self.timing.backoff_max);
        }
    }

    fn socket_url(&self) -> String {
        let base = if let Some(rest) = self.base.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = self.base.strip_prefix("http://") {
            format!("ws://{rest}")
        } else {
            self.base.clone()
        };
        format!("{base}/api/v4/websocket")
    }

    /// One connection, from the challenge to its end.
    async fn connection(&self, tx: &mpsc::Sender<InboundMessage>, backoff: &mut Duration) -> End {
        let me = match self.me().await {
            Ok(me) => me,
            Err(e) if e.status == 401 => return End::Fatal(super::refused(&e)),
            Err(e) => return End::Again(format!("couldn't reach Mattermost ({})", e.message)),
        };
        self.resolve_users().await;
        let mut socket = match ws::connect(&self.socket_url(), self.timing.connect).await {
            Ok(s) => s,
            Err(e) => return End::Again(format!("couldn't open the WebSocket ({e})")),
        };
        let mut seq: u64 = 1;
        let challenge = json!({
            "seq": seq,
            "action": "authentication_challenge",
            "data": { "token": self.cfg.token },
        });
        if let Err(e) = ws::send_json(&mut socket, &challenge).await {
            return End::Again(format!("couldn't authenticate the socket ({e})"));
        }
        let mut next_ping = Instant::now() + self.timing.ping;
        let mut heard = Instant::now();
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(next_ping) => {
                    next_ping = Instant::now() + self.timing.ping;
                    seq += 1;
                    if let Err(e) = ws::send_json(&mut socket, &json!({ "seq": seq, "action": "ping" })).await {
                        return End::Again(format!("couldn't ping the socket ({e})"));
                    }
                }
                _ = tokio::time::sleep_until(heard + self.timing.dead) => {
                    ws::close(&mut socket, 1000, "silent").await;
                    return End::Again(format!(
                        "the socket was silent for {}s",
                        self.timing.dead.as_secs()
                    ));
                }
                frame = ws::next(&mut socket) => {
                    let text = match frame {
                        Some(Ok(Message::Text(t))) => t,
                        Some(Ok(Message::Close(_))) => return End::Again("Mattermost closed the socket".into()),
                        Some(Ok(_)) => {
                            heard = Instant::now();
                            self.frame();
                            continue;
                        }
                        Some(Err(e)) => return End::Again(format!("the socket dropped ({e})")),
                        None => return End::Again("the socket ended".into()),
                    };
                    heard = Instant::now();
                    self.frame();
                    let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
                    if v["seq_reply"] == 1 {
                        if v["status"] == "OK" {
                            *backoff = self.timing.backoff_min;
                            self.set_socket_problem(None);
                            tracing::info!(bot = %me.username, "mattermost: connected");
                        } else {
                            ws::close(&mut socket, 1000, "refused").await;
                            let why = v["error"]["message"].as_str().or(v["error"]["id"].as_str()).unwrap_or("no reason given");
                            return End::Again(format!("the socket refused the token ({why})"));
                        }
                        continue;
                    }
                    let msg = match v["event"].as_str().unwrap_or("") {
                        "posted" => self.on_posted(&v, &me.id, &me.username).await,
                        "reaction_added" => self.on_reaction(&v, &me.id),
                        _ => None,
                    };
                    if let Some(msg) = msg {
                        if tx.send(msg).await.is_err() {
                            ws::close(&mut socket, 1000, "shutting down").await;
                            return End::Done;
                        }
                    }
                }
            }
        }
    }

    /// A `posted` event, if it's for the agent.
    pub(super) async fn on_posted(
        &self,
        v: &Value,
        me: &str,
        username: &str,
    ) -> Option<InboundMessage> {
        let d = &v["data"];
        let post: Value = serde_json::from_str(d["post"].as_str()?).ok()?;
        let user = post["user_id"].as_str()?.to_string();
        if user == me
            || post["type"].as_str().is_some_and(|t| !t.is_empty())
            || post["props"]["from_bot"] == "true"
            || post["props"]["from_webhook"] == "true"
        {
            return None;
        }
        let id = post["id"].as_str()?.to_string();
        let channel = post["channel_id"].as_str()?.to_string();
        let root = post["root_id"]
            .as_str()
            .filter(|r| !r.is_empty())
            .map(str::to_string);
        let name = d["sender_name"]
            .as_str()
            .map(|n| n.trim_start_matches('@').to_string())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| user.clone());
        let mut text = post["message"].as_str().unwrap_or("").to_string();
        let (chat_id, reply_root) = if d["channel_type"] == "D" {
            self.remember_dm(&user, &channel);
            match self.access.dm(&user, &name, &text) {
                Dm::Admit => {}
                Dm::Tell(t) | Dm::Paired(t) => {
                    self.tell(&channel, None, t).await;
                    return None;
                }
                Dm::Drop => return None,
            }
            (user.clone(), None)
        } else {
            if !self.access.channel_allowed(&channel) {
                self.access.ignore(&channel, &name, "channel message");
                return None;
            }
            let form = format!("@{username}");
            let mentions: Vec<String> = d["mentions"]
                .as_str()
                .and_then(|m| serde_json::from_str(m).ok())
                .unwrap_or_default();
            let mentioned = mentions.iter().any(|m| m == me)
                || contains_ci(&text, &form)
                || root.as_deref().is_some_and(|r| self.is_ours(r));
            if !mentioned {
                return None;
            }
            text = access::strip_leading(&text, &[form]);
            let root = root.clone().unwrap_or_else(|| id.clone());
            (format!("{channel}/{root}"), Some(root))
        };
        let mut saved = vec![];
        let mut refused = vec![];
        let infos: Vec<Value> = post["metadata"]["files"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let file_ids: Vec<String> = post["file_ids"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|f| f.as_str().map(str::to_string))
            .collect();
        for fid in &file_ids {
            let info = match infos.iter().find(|i| i["id"] == fid.as_str()) {
                Some(i) => i.clone(),
                None => self
                    .call(
                        reqwest::Method::GET,
                        &format!("/files/{}/info", super::enc(fid)),
                        None,
                        "file info",
                    )
                    .await
                    .unwrap_or(Value::Null),
            };
            let fname = info["name"]
                .as_str()
                .filter(|n| !n.is_empty())
                .unwrap_or("file")
                .to_string();
            let mime = info["mime_type"].as_str().filter(|m| !m.is_empty());
            match self
                .fetch_file(fid, &fname, info["size"].as_u64(), mime)
                .await
            {
                Ok(s) => saved.push(s),
                Err(_) if self.cfg.inbox.is_none() => {
                    let kind = access::file_kind(mime);
                    if text.trim().is_empty() {
                        self.tell(
                            &channel,
                            reply_root.as_deref(),
                            access::cannot_read_text(kind),
                        )
                        .await;
                        return None;
                    }
                    text = access::unread_note(&text, kind);
                }
                Err(r) => {
                    self.tell(&channel, reply_root.as_deref(), files::refused_reply(&r))
                        .await;
                    refused.push(r);
                }
            }
        }
        if !file_ids.is_empty() {
            text = files::with_notes(&text, &saved, &refused);
        }
        if text.trim().is_empty() {
            return None;
        }
        Some(InboundMessage {
            channel: "mattermost".into(),
            chat_id,
            sender: name,
            sender_id: Some(user),
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
            reply_to: None,
            ts: post["create_at"].as_i64().unwrap_or(0) / 1000,
        })
    }

    /// A reaction on an approval, from an allowed user: its answer.
    pub(super) fn on_reaction(&self, v: &Value, me: &str) -> Option<InboundMessage> {
        let r: Value = serde_json::from_str(v["data"]["reaction"].as_str()?).ok()?;
        let user = r["user_id"].as_str()?;
        if user == me || !is_id(user) || !self.access.user_allowed(user) {
            return None;
        }
        let post = r["post_id"].as_str()?;
        let emoji = r["emoji_name"].as_str()?;
        let (chat, cmd) = self.approval_answer(post, emoji)?;
        Some(InboundMessage {
            channel: "mattermost".into(),
            chat_id: chat,
            sender: user.to_string(),
            sender_id: Some(user.to_string()),
            message_id: format!("{post}:{emoji}:{user}"),
            text: cmd,
            attachments: vec![],
            reply_to: Some(post.to_string()),
            ts: r["create_at"].as_i64().unwrap_or(0) / 1000,
        })
    }
}

/// Whether `text` holds `form` as a whole mention (case-blind): `@ferrule`
/// but not `@ferrule-dev`.
fn contains_ci(text: &str, form: &str) -> bool {
    let lower = text.to_lowercase();
    let form = form.to_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find(&form) {
        let end = from + i + form.len();
        let mut rest = lower[end..].chars();
        let next = match rest.next() {
            // `@ferrule.` ends a sentence; `@ferrule.dev` is someone else.
            Some('.') => rest.next().filter(|c| c.is_alphanumeric()),
            c => c,
        };
        if !next.is_some_and(|c| c.is_alphanumeric() || "_-".contains(c)) {
            return true;
        }
        from = end;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::contains_ci;

    #[test]
    fn a_mention_is_a_whole_name() {
        assert!(contains_ci("hey @Ferrule, look", "@ferrule"));
        assert!(contains_ci("@ferrule", "@ferrule"));
        assert!(!contains_ci("@ferrule-dev look", "@ferrule"));
        assert!(contains_ci("@ferrule-dev and @ferrule", "@ferrule"));
        assert!(contains_ci("thanks @ferrule.", "@ferrule"));
        assert!(!contains_ci("@ferrule.dev", "@ferrule"));
    }
}
