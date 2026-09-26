//! Socket Mode: `apps.connections.open` gives a one-use `wss://` address;
//! Slack says `hello`, then sends envelopes, each acknowledged at once by
//! its `envelope_id` (Slack redelivers what isn't acked within 3 s). Slack
//! asks for a fresh connection now and then (`disconnect`); a closed or
//! silent socket is reopened with backoff. The address carries a ticket,
//! so it is never logged.

use super::{SlackChannel, Token};
use crate::channels::ws::{self, Message};
use crate::error::GatewayError;
use crate::message::InboundMessage;
use futures_util::SinkExt;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;

/// How one connection ended.
enum End {
    /// Reconnect after the backoff.
    Again(String),
    /// Slack asked for a new connection: open one now, nothing's wrong.
    Refresh(&'static str),
    /// A failure no retry fixes (a rejected token).
    Fatal(String),
    /// The gateway stopped listening (shutdown).
    Done,
}

fn fatal_error(e: &str) -> bool {
    [
        "invalid_auth",
        "not_authed",
        "account_inactive",
        "token_revoked",
        "not_allowed_token_type",
    ]
    .iter()
    .any(|k| e.ends_with(k))
}

impl SlackChannel {
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
                    tracing::error!("slack: {why}");
                    self.set_problem(Some(why.clone()));
                    return Err(GatewayError::Channel(format!("slack {why}")));
                }
                End::Refresh(reason) => {
                    tracing::info!("slack: Slack asked for a new connection ({reason})");
                    continue;
                }
                End::Again(why) => why,
            };
            tracing::warn!("slack: {why}; reconnecting");
            self.set_problem(Some(format!("{why}; reconnecting")));
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(self.timing.backoff_max);
        }
    }

    /// The bot's own user id, from `auth.test`, once.
    async fn who_am_i(&self) -> Result<(), End> {
        if self.bot_id().is_some() {
            return Ok(());
        }
        match self.call("auth.test", json!({}), Token::Bot, true).await {
            Ok(me) => {
                *self.bot.lock().unwrap() = me["user_id"].as_str().map(str::to_string);
                tracing::info!(
                    bot = me["user"].as_str().unwrap_or(""),
                    team = me["team"].as_str().unwrap_or(""),
                    "slack: signed in"
                );
                Ok(())
            }
            Err(e) if fatal_error(&e.to_string()) => {
                Err(End::Fatal(super::explain(e.to_string(), Token::Bot)))
            }
            Err(e) => Err(End::Again(format!("couldn't reach Slack ({e})"))),
        }
    }

    /// One connection, from `apps.connections.open` to its end.
    async fn connection(&self, tx: &mpsc::Sender<InboundMessage>, backoff: &mut Duration) -> End {
        if let Err(end) = self.who_am_i().await {
            return end;
        }
        let url = match self
            .call("apps.connections.open", json!({}), Token::App, true)
            .await
        {
            Ok(v) => match v["url"].as_str() {
                Some(u) => u.to_string(),
                None => return End::Again("Slack gave no Socket Mode address".into()),
            },
            Err(e) if fatal_error(&e.to_string()) => {
                return End::Fatal(super::explain(e.to_string(), Token::App))
            }
            Err(e) => return End::Again(format!("couldn't open a Socket Mode connection ({e})")),
        };
        let mut socket = match ws::connect(&url, self.timing.connect).await {
            Ok(s) => s,
            Err(e) => return End::Again(format!("couldn't reach Slack's socket ({e})")),
        };
        let mut next_ping = Instant::now() + self.timing.ping;
        let mut heard = Instant::now();
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(next_ping) => {
                    next_ping = Instant::now() + self.timing.ping;
                    if let Err(e) = socket.send(Message::Ping(Vec::new().into())).await {
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
                        Some(Ok(Message::Close(_))) => return End::Again("Slack closed the socket".into()),
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
                    // Acknowledged before anything else, inside Slack's 3 s.
                    if let Some(id) = v["envelope_id"].as_str() {
                        if let Err(e) = ws::send_json(&mut socket, &json!({ "envelope_id": id })).await {
                            return End::Again(format!("couldn't acknowledge an event ({e})"));
                        }
                    }
                    let msg = match v["type"].as_str().unwrap_or("") {
                        "hello" => {
                            *backoff = self.timing.backoff_min;
                            self.set_problem(None);
                            tracing::info!("slack: connected");
                            None
                        }
                        "disconnect" => {
                            ws::close(&mut socket, 1000, "reconnecting").await;
                            return match v["reason"].as_str().unwrap_or("") {
                                "link_disabled" => End::Again(
                                    "Socket Mode is off for this app (link_disabled): switch it on at api.slack.com → your app → Socket Mode".into(),
                                ),
                                "warning" => End::Refresh("warning"),
                                _ => End::Refresh("refresh_requested"),
                            };
                        }
                        "events_api" => self.on_event(&v["payload"]).await,
                        "interactive" => self.on_action(&v["payload"]).await,
                        "slash_commands" => self.on_slash(&v["payload"]).await,
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
}
