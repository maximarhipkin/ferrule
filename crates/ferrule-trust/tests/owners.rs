//! M31: the owner in every channel. A Discord primary gets the approval
//! question (with buttons) and answers it from its own chat; `/stop` from a
//! Slack owner chat is recorded as that chat; Telegram-only config keeps
//! the Telegram owner as it was.

use async_trait::async_trait;
use ferrule_trust::{order_owners, ChatRef, FakeClock, Hub, Intercept, Notifier, TrustConfig};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

/// Records every message by chat; answers an approval with `reply` from
/// the chat it was asked in, by tapping the first choice.
/// A message: the chat, the text, the choices under it.
type Sent = (ChatRef, String, Vec<(String, String)>);

#[derive(Default)]
struct Chats {
    hub: OnceLock<Weak<Hub>>,
    sent: Mutex<Vec<Sent>>,
    tap: Option<usize>,
}

#[async_trait]
impl Notifier for Chats {
    async fn send(&self, chat: i64, text: &str) -> Result<(), String> {
        self.send_to(&ChatRef::from(chat), text).await
    }

    async fn send_to(&self, chat: &ChatRef, text: &str) -> Result<(), String> {
        self.send_choices(chat, text, &[]).await
    }

    async fn send_choices(
        &self,
        chat: &ChatRef,
        text: &str,
        choices: &[(String, String)],
    ) -> Result<(), String> {
        self.sent
            .lock()
            .unwrap()
            .push((chat.clone(), text.to_string(), choices.to_vec()));
        if let (Some(i), Some(hub)) = (self.tap, self.hub.get().and_then(Weak::upgrade)) {
            if let Some((_, reply)) = choices.get(i).cloned() {
                let chat = chat.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    hub.intercept(chat, &reply);
                });
            }
        }
        Ok(())
    }
}

fn hub(cfg: TrustConfig, chats: Arc<Chats>) -> (tempfile::TempDir, Arc<Hub>) {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let clock = Arc::new(FakeClock::at("2026-09-25T10:00:00Z"));
    let hub = Arc::new(
        Hub::new(cfg, &data, &data.join("ledger.jsonl"), clock, vec![])
            .unwrap()
            .with_poll(Duration::from_millis(20)),
    );
    chats.hub.set(Arc::downgrade(&hub)).ok();
    hub.set_notifier(Some(chats));
    (dir, hub)
}

#[tokio::test]
async fn a_discord_primary_gets_the_approval_with_buttons_and_its_tap_allows_it() {
    let chats = Arc::new(Chats {
        tap: Some(0),
        ..Default::default()
    });
    let (_dir, hub) = hub(
        TrustConfig {
            owner_chat: Some(42),
            discord_owner: Some("1001".into()),
            owner_channel: Some("discord".into()),
            ..Default::default()
        },
        chats.clone(),
    );
    assert_eq!(hub.primary(), Some(ChatRef::new("discord", "1001")));
    assert_eq!(hub.owner(), Some(42), "the Telegram owner is still there");
    let asked = hub
        .ask_owner(
            "s1",
            "rm -rf target",
            "Run `rm -rf target`?",
            Duration::from_secs(5),
        )
        .await;
    assert_eq!(asked, Ok(()));
    let sent = chats.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    let (chat, text, choices) = &sent[0];
    assert_eq!(chat, &ChatRef::new("discord", "1001"));
    assert!(text.contains("Reply `yes` to allow it"), "{text}");
    assert_eq!(choices[0].0, "Allow");
    assert!(choices[0].1.starts_with("yes "), "{:?}", choices);
    assert_eq!(choices[1].0, "Refuse");
    assert!(choices[1].1.starts_with("no "), "{:?}", choices);
    let answered: Vec<_> = hub
        .audit()
        .read(None)
        .unwrap()
        .into_iter()
        .filter(|e| e.event == "approval_asked" || e.event == "approval_answered")
        .map(|e| e.detail)
        .collect();
    assert_eq!(answered[0]["route"], "discord");
    assert_eq!(answered[0]["chat"], "1001");
    assert_eq!(answered[1]["route"], "discord");
    assert_eq!(answered[1]["answer"], "yes");
}

#[tokio::test]
async fn an_answer_from_another_owner_chat_does_not_count() {
    let chats = Arc::new(Chats::default());
    let (_dir, hub) = hub(
        TrustConfig {
            owner_chat: Some(42),
            discord_owner: Some("1001".into()),
            owner_channel: Some("discord".into()),
            ..Default::default()
        },
        chats.clone(),
    );
    let h = hub.clone();
    let ask = tokio::spawn(async move {
        h.ask_owner("s1", "x", "Run x?", Duration::from_millis(300))
            .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    // Telegram's owner says yes; the question went to Discord.
    assert_eq!(hub.intercept(42, "yes"), Intercept::Pass);
    assert!(ask.await.unwrap().is_err());
}

#[tokio::test]
async fn stop_from_a_slack_owner_chat_is_recorded_as_that_chat() {
    let chats = Arc::new(Chats::default());
    let (_dir, hub) = hub(
        TrustConfig {
            slack_owner: Some("U0OWNER".into()),
            ..Default::default()
        },
        chats,
    );
    let slack = ChatRef::new("slack", "U0OWNER");
    assert!(hub.is_owner(&slack));
    assert!(matches!(
        hub.intercept(slack.clone(), "/stop too expensive"),
        Intercept::Reply(_)
    ));
    let info = hub.stopped().unwrap();
    assert_eq!(info.by, "slack chat U0OWNER");
    assert_eq!(info.reason.as_deref(), Some("too expensive"));
    // Only an owner chat resumes.
    let stranger = ChatRef::new("slack", "U0OTHER");
    assert_eq!(
        hub.intercept(stranger, "/resume"),
        Intercept::Reply("Only the owner chat can resume ferrule.".into())
    );
    assert!(
        matches!(hub.intercept(slack, "/resume"), Intercept::Reply(t) if t.starts_with("Resumed"))
    );
    assert!(hub.stopped().is_none());
}

#[tokio::test]
async fn owners_are_one_per_channel_and_telegram_stays_primary_by_default() {
    let chats = Arc::new(Chats::default());
    let (_dir, hub) = hub(TrustConfig::default(), chats.clone());
    assert_eq!(hub.primary(), None);
    hub.set_owners(order_owners(
        vec![
            ChatRef::new("slack", "U1"),
            ChatRef::new("discord", "7"),
            ChatRef::from(42),
            ChatRef::new("slack", "U2"),
        ],
        None,
    ));
    assert_eq!(
        hub.owners(),
        vec![
            ChatRef::from(42),
            ChatRef::new("discord", "7"),
            ChatRef::new("slack", "U1"),
        ]
    );
    hub.set_owner(None);
    assert_eq!(hub.primary(), Some(ChatRef::new("discord", "7")));
    hub.set_owner(Some(9));
    assert_eq!(hub.primary(), Some(ChatRef::from(9)));
    assert_eq!(hub.owner_on("slack"), Some(ChatRef::new("slack", "U1")));
    hub.tell_owner("hello".into());
    tokio::time::sleep(Duration::from_millis(50)).await;
    let sent = chats.sent.lock().unwrap().clone();
    assert_eq!(sent[0].0, ChatRef::from(9));
}
