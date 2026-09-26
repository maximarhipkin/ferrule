//! The Discord Gateway: one WebSocket that says hello, is identified (or
//! resumed), is kept alive by heartbeats, and carries every event. When it
//! drops, the session is resumed where it can be (Discord replays what was
//! missed) and identified afresh where it can't; a close that no retry can
//! fix (a wrong token, a bad intent) ends `run` with the reason.

use super::{DiscordChannel, INTENTS_BASE, INTENT_MESSAGE_CONTENT};
use crate::channels::ws::{self, Message, Socket};
use crate::error::GatewayError;
use crate::message::InboundMessage;
use reqwest::Method;
use serde_json::{json, Value};
use std::time::{Duration, SystemTime};
use tokio::sync::mpsc;
use tokio::time::Instant;

/// How one connection ended.
enum End {
    /// Reconnect; resume if the session allows it. `wait`: back off first.
    Again { wait: bool, why: String },
    /// The session can't be resumed: identify on the next connection.
    Fresh { wait: bool, why: String },
    /// A close no retry fixes.
    Fatal(String),
    /// The gateway stopped listening (shutdown).
    Done,
}

/// A number in [0, 1) that differs from run to run, for the first
/// heartbeat's jitter and the invalid-session wait.
fn jitter() -> f64 {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    f64::from(nanos % 1_000_000) / 1_000_000.0
}

fn with_query(url: &str) -> String {
    format!("{}/?v=10&encoding=json", url.trim_end_matches('/'))
}

/// A close code in words, and whether a retry can fix it.
fn close_meaning(code: u16) -> (&'static str, bool) {
    match code {
        4004 => ("the bot token is wrong (4004): copy it again from the developer portal → Bot → Reset Token", true),
        4010 => ("Discord refused the shard (4010)", true),
        4011 => ("the bot is in too many servers for one connection (4011: sharding required)", true),
        4012 => ("Discord refused the gateway version (4012)", true),
        4013 => ("Discord refused the intents (4013)", true),
        4014 => ("the MESSAGE_CONTENT intent isn't enabled (4014)", false),
        4007 => ("the resume sequence was invalid (4007)", false),
        4009 => ("the session timed out (4009)", false),
        _ => ("the gateway closed the connection", false),
    }
}

impl DiscordChannel {
    pub(super) async fn run_gateway(
        &self,
        tx: mpsc::Sender<InboundMessage>,
    ) -> Result<(), GatewayError> {
        let mut backoff = self.timing.backoff_min;
        loop {
            let (wait, why) = match self.connection(&tx, &mut backoff).await {
                End::Done => return Ok(()),
                End::Fatal(why) => {
                    let why = format!("stopped: {why}");
                    tracing::error!("discord: {why}");
                    self.set_problem(Some(why.clone()));
                    return Err(GatewayError::Channel(format!("discord {why}")));
                }
                End::Again { wait, why } => (wait, why),
                End::Fresh { wait, why } => {
                    *self.session.lock().unwrap() = Default::default();
                    (wait, why)
                }
            };
            tracing::warn!("discord: {why}; reconnecting");
            self.set_problem(Some(format!("{why}; reconnecting")));
            if wait {
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(self.timing.backoff_max);
            }
        }
    }

    /// Where to connect: the resume URL for a session that can resume,
    /// else the address `GET /gateway/bot` gives.
    async fn url(&self) -> Result<(String, bool), End> {
        {
            let s = self.session.lock().unwrap();
            if let (Some(_), Some(_), Some(url)) = (&s.id, s.seq, &s.resume_url) {
                return Ok((with_query(url), true));
            }
        }
        match self.rest(Method::GET, "/gateway/bot", None, true).await {
            Ok(v) => match v["url"].as_str() {
                Some(url) => Ok((with_query(url), false)),
                None => Err(End::Again {
                    wait: true,
                    why: "Discord's /gateway/bot gave no address".into(),
                }),
            },
            Err(e) if e.to_string().contains("status 401") => Err(End::Fatal(
                "Discord rejected the bot token (401): copy it again from the developer portal → Bot → Reset Token".into(),
            )),
            Err(e) => Err(End::Again {
                wait: true,
                why: format!("couldn't ask Discord for the gateway address ({e})"),
            }),
        }
    }

    fn identify(&self) -> Value {
        let mut intents = INTENTS_BASE;
        if self
            .content_intent
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            intents |= INTENT_MESSAGE_CONTENT;
        }
        json!({ "op": 2, "d": {
            "token": self.token,
            "intents": intents,
            "properties": { "os": std::env::consts::OS, "browser": "ferrule", "device": "ferrule" },
        }})
    }

    fn resume(&self) -> Value {
        let s = self.session.lock().unwrap();
        json!({ "op": 6, "d": { "token": self.token, "session_id": s.id, "seq": s.seq } })
    }

    fn seq(&self) -> Value {
        json!(self.session.lock().unwrap().seq)
    }

    /// One connection, from HELLO to its end.
    async fn connection(&self, tx: &mpsc::Sender<InboundMessage>, backoff: &mut Duration) -> End {
        let (url, resuming) = match self.url().await {
            Ok(u) => u,
            Err(end) => return end,
        };
        let mut socket = match ws::connect(&url, self.timing.connect).await {
            Ok(s) => s,
            Err(e) => {
                return End::Again {
                    wait: true,
                    why: format!("couldn't reach Discord's gateway ({e})"),
                }
            }
        };
        let interval = match self.hello(&mut socket).await {
            Ok(i) => i,
            Err(why) => return End::Again { wait: true, why },
        };
        let hello = if resuming {
            self.resume()
        } else {
            self.identify()
        };
        if let Err(e) = ws::send_json(&mut socket, &hello).await {
            return End::Again {
                wait: true,
                why: format!("couldn't identify on the gateway ({e})"),
            };
        }
        let mut next_beat = Instant::now() + interval.mul_f64(jitter());
        let mut acked = true;
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(next_beat) => {
                    if !acked {
                        // A zombied connection: close it so it can be resumed.
                        ws::close(&mut socket, 4000, "no heartbeat ack").await;
                        return End::Again { wait: false, why: "Discord stopped answering heartbeats".into() };
                    }
                    acked = false;
                    next_beat = Instant::now() + interval;
                    if let Err(e) = ws::send_json(&mut socket, &json!({ "op": 1, "d": self.seq() })).await {
                        return End::Again { wait: true, why: format!("couldn't send a heartbeat ({e})") };
                    }
                }
                frame = ws::next(&mut socket) => {
                    let text = match frame {
                        Some(Ok(Message::Text(t))) => t,
                        Some(Ok(Message::Close(frame))) => {
                            let code = frame.map_or(1000, |f| u16::from(f.code));
                            return self.closed(code);
                        }
                        Some(Ok(_)) => continue,
                        Some(Err(e)) => return End::Again { wait: true, why: format!("the gateway dropped ({e})") },
                        None => return End::Again { wait: true, why: "the gateway connection ended".into() },
                    };
                    self.frame();
                    let Ok(payload) = serde_json::from_str::<Value>(&text) else { continue };
                    match payload["op"].as_u64() {
                        Some(0) => {
                            if let Some(s) = payload["s"].as_u64() {
                                self.session.lock().unwrap().seq = Some(s);
                            }
                            let t = payload["t"].as_str().unwrap_or("");
                            if matches!(t, "READY" | "RESUMED") {
                                *backoff = self.timing.backoff_min;
                            }
                            if let Some(msg) = self.dispatch(t, &payload["d"]).await {
                                if tx.send(msg).await.is_err() {
                                    ws::close(&mut socket, 1000, "shutting down").await;
                                    return End::Done;
                                }
                            }
                        }
                        Some(1) => {
                            if let Err(e) = ws::send_json(&mut socket, &json!({ "op": 1, "d": self.seq() })).await {
                                return End::Again { wait: true, why: format!("couldn't send a heartbeat ({e})") };
                            }
                        }
                        Some(7) => {
                            ws::close(&mut socket, 4000, "reconnect").await;
                            return End::Again { wait: false, why: "Discord asked for a reconnect".into() };
                        }
                        Some(9) => {
                            let (lo, hi) = self.timing.invalid_session;
                            tokio::time::sleep(lo + (hi.saturating_sub(lo)).mul_f64(jitter())).await;
                            ws::close(&mut socket, 4000, "invalid session").await;
                            let why = "Discord invalidated the session".to_string();
                            return if payload["d"].as_bool() == Some(true) {
                                End::Again { wait: false, why }
                            } else {
                                End::Fresh { wait: false, why }
                            };
                        }
                        Some(11) => acked = true,
                        _ => {}
                    }
                }
            }
        }
    }

    /// Waits for HELLO; its heartbeat interval.
    async fn hello(&self, socket: &mut Socket) -> Result<Duration, String> {
        let deadline = self.timing.hello;
        let wait = async {
            while let Some(frame) = ws::next(socket).await {
                match frame {
                    Ok(Message::Text(t)) => {
                        let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                        if v["op"] == 10 {
                            let ms = v["d"]["heartbeat_interval"].as_u64().unwrap_or(41_250);
                            return Ok(Duration::from_millis(ms.max(10)));
                        }
                    }
                    Ok(Message::Close(_)) => break,
                    Ok(_) => {}
                    Err(e) => return Err(format!("the gateway dropped before hello ({e})")),
                }
            }
            Err("the gateway closed before hello".to_string())
        };
        match tokio::time::timeout(deadline, wait).await {
            Ok(r) => {
                if r.is_ok() {
                    self.frame();
                }
                r
            }
            Err(_) => Err("the gateway sent no hello".into()),
        }
    }

    fn closed(&self, code: u16) -> End {
        let (words, fatal) = close_meaning(code);
        if fatal {
            return End::Fatal(words.to_string());
        }
        match code {
            4014 => {
                self.content_intent
                    .store(false, std::sync::atomic::Ordering::SeqCst);
                *self.intent_note.lock().unwrap() = Some(
                    "the MESSAGE_CONTENT intent is off, so ferrule reads only DMs and messages that mention it — switch it on in the developer portal → Bot → Privileged Gateway Intents".into(),
                );
                End::Fresh {
                    wait: false,
                    why: words.to_string(),
                }
            }
            4007 | 4009 => End::Fresh {
                wait: false,
                why: words.to_string(),
            },
            _ => End::Again {
                wait: true,
                why: format!("{words} ({code})"),
            },
        }
    }

    /// One dispatch event; a message for the agent, if it is one.
    async fn dispatch(&self, t: &str, d: &Value) -> Option<InboundMessage> {
        match t {
            "READY" => {
                let mut s = self.session.lock().unwrap();
                s.id = d["session_id"].as_str().map(str::to_string);
                s.resume_url = d["resume_gateway_url"].as_str().map(str::to_string);
                drop(s);
                *self.bot.lock().unwrap() = d["user"]["id"].as_str().map(str::to_string);
                *self.app.lock().unwrap() = d["application"]["id"].as_str().map(str::to_string);
                self.set_problem(None);
                tracing::info!(
                    bot = d["user"]["username"].as_str().unwrap_or(""),
                    "discord: connected"
                );
                None
            }
            "RESUMED" => {
                self.set_problem(None);
                tracing::info!("discord: resumed");
                None
            }
            "GUILD_CREATE" => {
                for c in ["channels", "threads"] {
                    for ch in d[c].as_array().into_iter().flatten() {
                        self.learn_channel(ch);
                    }
                }
                None
            }
            "CHANNEL_CREATE" | "CHANNEL_UPDATE" | "THREAD_CREATE" | "THREAD_UPDATE" => {
                if d["guild_id"].is_string() {
                    self.learn_channel(d);
                }
                None
            }
            "MESSAGE_CREATE" => self.on_message(d).await,
            "INTERACTION_CREATE" => self.on_interaction(d).await,
            _ => None,
        }
    }
}
