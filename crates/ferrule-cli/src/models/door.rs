//! `/model` in a chat (docs/m21-models.md §4), owner only, and the
//! models section of `/status`.

use super::*;
use ferrule_gateway::InboundMessage;

/// Retires one lane (`Some(session)`) or every chat's (`None`).
pub type Retire = Arc<dyn Fn(Option<&str>) + Send + Sync>;

pub struct ModelDoor {
    pub models: Arc<Models>,
    pub hub: Arc<Hub>,
    /// The gateway's `--provider`: every chat is on it until a restart.
    pub fixed: Option<String>,
    /// Retire one lane (`Some(session)`) or, with `None`, every chat's
    /// lane, so the next message builds an agent with the new model's
    /// harness profile. (Each call already picks its model when it's made.)
    pub retire: Retire,
}

const USAGE: &str = "Usage:\n\
/model: the models, the default and this chat's\n\
/model default <ref>: the default for every chat\n\
/model use <ref>: this chat's model; /model use default clears it\n\
/model fallback <ref> …: where turns go when a model is down; /model fallback off\n\
/model test <ref>: one real call\n\
/model strong: this chat's next turn on the strong tier (routing)\n\
/model tier:strong: pin this chat to a tier; /model tiers shows them\n\
A ref is provider/model, a provider, an alias, a model id or a tier (tier:cheap, tier:strong).";

impl ModelDoor {
    /// The owner chat, or the owner writing in a group.
    fn is_owner(&self, msg: &InboundMessage) -> bool {
        crate::trust::owner_in(&self.hub, msg).is_some()
    }

    async fn answer(&self, msg: &InboundMessage, rest: &str) -> String {
        let (channel, chat) = (msg.channel.as_str(), msg.chat_id.as_str());
        let by = format!("{channel} chat {chat}");
        let session = ferrule_gateway::session::session_id(channel, chat);
        let (sub, arg) = match rest.split_once(char::is_whitespace) {
            Some((s, a)) => (s, a.trim()),
            None => (rest, ""),
        };
        let fixed_note = self.fixed.as_ref().map(|p| {
            format!(
                "\nThis gateway was started with --provider {p}, so chats stay on it until it restarts without that."
            )
        });
        let result = match (sub.to_lowercase().as_str(), arg) {
            ("", _) | ("list", _) => {
                let mut text = render(&self.models.view(), Some((channel, chat)));
                text.extend(fixed_note);
                return text;
            }
            ("tiers", "") | ("route", "") => {
                let text = super::routing_admin::render(&self.models.view().routing);
                return if text.is_empty() {
                    "Routing isn't set up: `ferrule model route` suggests a cheap/strong pair."
                        .into()
                } else {
                    text.trim_start().to_string()
                };
            }
            ("strong", "") => {
                return self
                    .models
                    .force_strong(channel, chat)
                    .unwrap_or_else(|e| format!("Nothing changed: {e}"))
            }
            (tier, "") if super::routing::is_tier_ref(tier) => {
                self.models.pin(channel, chat, sub, &by).map(|d| {
                    (self.retire)(Some(&session));
                    d.said
                })
            }
            ("default", w) if !w.is_empty() => self.models.set_default(w, &by).map(|d| {
                (self.retire)(None);
                d.said + &fixed_note.unwrap_or_default()
            }),
            ("use", "default") => self.models.unpin(channel, chat, &by).map(|d| {
                (self.retire)(Some(&session));
                d.said
            }),
            ("use", w) if !w.is_empty() => self.models.pin(channel, chat, w, &by).map(|d| {
                (self.retire)(Some(&session));
                d.said + &fixed_note.unwrap_or_default()
            }),
            ("fallback", "off") => self.models.set_fallback(&[], &by).map(|d| d.said),
            ("fallback", w) if !w.is_empty() => {
                let words: Vec<String> = w.split_whitespace().map(str::to_string).collect();
                self.models.set_fallback(&words, &by).map(|d| d.said)
            }
            ("test", w) if !w.is_empty() => {
                let out = self.models.test(w).await;
                return format!("{}: {}", out.reference, out.said);
            }
            _ => return USAGE.into(),
        };
        result.unwrap_or_else(|e| format!("Nothing changed: {e:#}"))
    }
}

#[async_trait::async_trait]
impl ferrule_gateway::Interceptor for ModelDoor {
    async fn intercept(&self, msg: &InboundMessage) -> Option<String> {
        if !crate::trust::is_chat_channel(&msg.channel) {
            return None;
        }
        let t = msg.text.trim();
        let (cmd, rest) = match t.split_once(char::is_whitespace) {
            Some((c, r)) => (c, r.trim()),
            None => (t, ""),
        };
        let cmd = cmd.split('@').next().unwrap_or(cmd);
        if !cmd.eq_ignore_ascii_case("/model") {
            return None;
        }
        if !self.is_owner(msg) {
            return Some("Only the owner can change models.".into());
        }
        Some(self.answer(msg, rest).await)
    }
}

/// The models section of `/status`: the default, fallback, outages, pins
/// and the model each session ran on last.
pub fn status_lines(models: &Models) -> Vec<String> {
    let view = models.view();
    let mut out = vec![format!(
        "default: {}",
        view.default.as_deref().unwrap_or("none")
    )];
    out.push(if view.fallback.is_empty() {
        "fallback: off".into()
    } else {
        format!("fallback: {}", view.fallback.join(" → "))
    });
    for m in view.models.iter().filter(|m| m.down_secs.is_some()) {
        out.push(format!(
            "down: {} ({}), retried in {} min",
            m.reference,
            m.down_reason.as_deref().unwrap_or("?"),
            m.down_secs.unwrap_or(0).div_ceil(60)
        ));
    }
    for p in &view.pins {
        out.push(format!(
            "pinned: {}:{} → {}",
            p.channel,
            p.chat,
            p.resolved
                .as_deref()
                .unwrap_or("not connected, on the default")
        ));
    }
    let now = Utc::now();
    for s in view.last_served.iter().take(5) {
        let ago = DateTime::parse_from_rfc3339(&s.at)
            .map(|t| (now - t.with_timezone(&Utc)).num_seconds().max(0))
            .unwrap_or(0);
        out.push(format!(
            "last ran: {} on {} ({})",
            s.session,
            s.reference,
            ago_text(ago)
        ));
    }
    let r = &view.routing;
    if r.on {
        let tiers: Vec<String> = r
            .tiers
            .iter()
            .map(|t| format!("{} ({})", t.name, t.reference))
            .collect();
        out.push(format!("routing: {}", tiers.join(" → ")));
        if let Some(cap) = r.strong_daily_usd {
            out.push(format!(
                "above the cheap tier today: ${:.2} of ${cap:.2}",
                r.strong_spent_today
            ));
        }
    }
    out.extend(view.problems.iter().map(|p| format!("problem: {p}")));
    out
}

fn ago_text(secs: i64) -> String {
    match secs {
        s if s < 60 => format!("{s} s ago"),
        s if s < 3600 => format!("{} min ago", s / 60),
        s => format!("{} h ago", s / 3600),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrule_gateway::Interceptor;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const CONFIG: &str = r#"
default_provider = "a"

[providers.a]
base_url = "http://127.0.0.1:1/v1"
api_key_env = "PATH"
model = "a-one"

[providers.b]
base_url = "http://127.0.0.1:2/v1"
api_key_env = "PATH"
model = "b-large"
"#;

    fn msg(chat: &str, sender: Option<&str>, text: &str) -> InboundMessage {
        InboundMessage {
            channel: "telegram".into(),
            chat_id: chat.into(),
            sender: "someone".into(),
            sender_id: sender.map(str::to_string),
            message_id: "1".into(),
            text: text.into(),
            attachments: vec![],
            reply_to: None,
            ts: 0,
        }
    }

    fn door() -> (tempfile::TempDir, ModelDoor, Arc<AtomicUsize>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ferrule.toml");
        std::fs::write(&path, CONFIG).unwrap();
        let cfg: Config = toml::from_str(CONFIG).unwrap();
        let models = Arc::new(Models::new(path, Some(dir.path().join("pins.json")), &cfg));
        let hub = Arc::new(
            Hub::new(
                Default::default(),
                dir.path(),
                &dir.path().join("ledger.jsonl"),
                Arc::new(ferrule_trust::SystemClock),
                vec![],
            )
            .unwrap(),
        );
        hub.set_owner(Some(42));
        let retired = Arc::new(AtomicUsize::new(0));
        let count = retired.clone();
        let door = ModelDoor {
            models,
            hub,
            fixed: None,
            retire: Arc::new(move |_| {
                count.fetch_add(1, Ordering::SeqCst);
            }),
        };
        (dir, door, retired)
    }

    #[tokio::test]
    async fn only_the_owner_changes_models_and_a_pin_is_per_chat() {
        let (_dir, door, retired) = door();
        // Not `/model`: passes.
        assert!(door.intercept(&msg("42", None, "hello")).await.is_none());
        // A stranger is told no, and nothing changes.
        let no = door
            .intercept(&msg("7", Some("7"), "/model default b"))
            .await
            .unwrap();
        assert_eq!(no, "Only the owner can change models.");
        assert_eq!(door.models.view().default.as_deref(), Some("a/a-one"));

        let list = door.intercept(&msg("42", None, "/model")).await.unwrap();
        assert!(list.contains("Default: a/a-one"), "{list}");
        assert!(list.contains("This chat: on the default"), "{list}");

        let said = door
            .intercept(&msg("42", None, "/model@ferrule_bot default b"))
            .await
            .unwrap();
        assert!(said.contains("b/b-large"), "{said}");
        assert_eq!(door.models.view().default.as_deref(), Some("b/b-large"));

        // The owner writing in a group pins that group.
        let said = door
            .intercept(&msg("-100", Some("42"), "/model use a"))
            .await
            .unwrap();
        assert!(!said.starts_with("Nothing changed"), "{said}");
        let here = door
            .intercept(&msg("-100", Some("42"), "/model"))
            .await
            .unwrap();
        assert!(here.contains("This chat: pinned to a/a-one"), "{here}");
        let there = door.intercept(&msg("42", None, "/model")).await.unwrap();
        assert!(there.contains("This chat: on the default"), "{there}");

        door.intercept(&msg("-100", Some("42"), "/model use default"))
            .await
            .unwrap();
        let here = door
            .intercept(&msg("-100", Some("42"), "/model"))
            .await
            .unwrap();
        assert!(here.contains("This chat: on the default"), "{here}");
        assert_eq!(retired.load(Ordering::SeqCst), 3);

        let bad = door
            .intercept(&msg("42", None, "/model use nowhere"))
            .await
            .unwrap();
        assert!(bad.starts_with("Nothing changed"), "{bad}");
        let usage = door
            .intercept(&msg("42", None, "/model what"))
            .await
            .unwrap();
        assert!(usage.starts_with("Usage:"), "{usage}");

        let lines = status_lines(&door.models);
        assert_eq!(lines[0], "default: b/b-large");
        assert_eq!(lines[1], "fallback: off");
    }
}
