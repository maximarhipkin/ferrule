//! `ferrule login` / `ferrule logout`, and `/login` / `/logout` in the
//! owner's chat (docs/m35-subscriptions.md §3). A chat signs in to the
//! ChatGPT plan by device code only: no password, no token and no redirect
//! is ever typed into a chat. The Claude plan never signs in from a chat.

use super::{chatgpt, now};
use crate::config::{self, Plan};
use crate::trust;
use anyhow::{bail, Context, Result};
use ferrule_gateway::InboundMessage;
use ferrule_plans::chatgpt::auth::{self, Callback, Pkce, Tokens};
use ferrule_plans::ChatGpt;
use ferrule_trust::Hub;
use std::sync::Arc;
use std::time::Duration;

/// The model a new `[providers.chatgpt]` starts on.
pub const CHATGPT_MODEL: &str = "gpt-5.5";
/// How long the browser flow waits for the redirect or a paste.
const BROWSER_LIMIT: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Which {
    /// The ChatGPT plan (Plus, Pro, Business…), signed in here
    Chatgpt,
    /// The Claude plan, through the unmodified `claude` binary
    Claude,
}

/// How `ferrule login chatgpt` signs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// A device code, and the browser flow when the account has it off.
    Device,
    /// The browser comes back to a loopback listener (or its address is
    /// pasted).
    Browser,
    /// No listener: the owner pastes the address the browser ended on.
    Paste,
}

/// `[plans.chatgpt] issuer`, "" (OpenAI's) when there's no config.
fn issuer() -> String {
    config::Config::load()
        .ok()
        .and_then(|(cfg, _)| cfg.plans.chatgpt.issuer)
        .unwrap_or_default()
}

/// `paste_token`: `ferrule login claude --token`.
pub async fn login(which: Which, flow: Flow, paste_token: bool) -> Result<()> {
    if paste_token && which != Which::Claude {
        bail!("--token is for `ferrule login claude`");
    }
    match which {
        Which::Chatgpt => {
            println!("{}.", sign_in_chatgpt(&issuer(), flow).await?);
            if let Some(line) = add_provider(Plan::Chatgpt, CHATGPT_MODEL)? {
                println!("{line}");
            }
            println!(
                "Models on the plan: {}.",
                super::chatgpt_models(&issuer()).await.join(", ")
            );
            Ok(())
        }
        Which::Claude => super::claude::login(paste_token).await,
    }
}

pub async fn logout(which: Which) -> Result<()> {
    match which {
        Which::Chatgpt => {
            let out = chatgpt(&issuer())?.log_out().await?;
            println!("{}", logged_out_words(out));
            Ok(())
        }
        Which::Claude => super::claude::logout().await,
    }
}

/// Signs in to the ChatGPT plan at the terminal and stores the sign-in;
/// says who signed in ("Signed in as …").
pub async fn sign_in_chatgpt(issuer: &str, flow: Flow) -> Result<String> {
    let plan = chatgpt(issuer)?;
    let tokens = chatgpt_tokens(&plan, flow).await?;
    let meta = plan.sign_in(tokens).await?;
    let who = super::SignIn::In {
        email: meta.email,
        plan: meta.plan,
    };
    Ok(capitalize(&who.word()))
}

fn logged_out_words(out: ferrule_plans::chatgpt::LoggedOut) -> &'static str {
    match (out.was_signed_in, out.revoked) {
        (false, _) => "There was no ChatGPT sign-in here.",
        (true, true) => "Signed out of the ChatGPT plan; OpenAI revoked the sign-in.",
        (true, false) => {
            "Signed out of the ChatGPT plan here. OpenAI didn't confirm the revocation; \
             ChatGPT's settings (Security → sessions) can end it there."
        }
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

async fn chatgpt_tokens(plan: &ChatGpt, flow: Flow) -> Result<Tokens> {
    let (http, issuer) = (plan.http(), plan.issuer());
    if flow == Flow::Device {
        match auth::start_device(http, issuer).await? {
            Some(code) => {
                println!(
                    "Open {} and enter the code {}\n(it's good for 15 minutes; any device will do). Waiting…",
                    code.page, code.user_code
                );
                return auth::finish_device(http, issuer, &code).await;
            }
            None => println!(
                "Device sign-in is off for this account (ChatGPT: Settings → Security turns it on), \
                 so this signs in through a browser instead."
            ),
        }
    }
    let callback = match flow {
        Flow::Paste => None,
        _ => match Callback::bind(issuer).await {
            Ok(cb) => Some(cb),
            Err(e) => {
                println!("{e:#}");
                None
            }
        },
    };
    let redirect = match &callback {
        Some(cb) => cb.redirect.clone(),
        None => format!(
            "http://127.0.0.1:{}/auth/callback",
            issuer.ports.first().copied().unwrap_or(1455)
        ),
    };
    let pkce = Pkce::new();
    let state = auth::new_state();
    println!(
        "Open this address in a browser and sign in:\n\n{}\n",
        auth::authorize_url(issuer, &redirect, &pkce, &state)
    );
    let code = match callback {
        Some(cb) => {
            println!(
                "If the browser is on another machine, the page it ends on won't load: \
                 copy that page's address and paste it here."
            );
            tokio::select! {
                code = cb.wait(&state, BROWSER_LIMIT) => code?,
                code = pasted(&state) => code?,
            }
        }
        None => {
            println!("The page it ends on won't load: copy that page's address and paste it here.");
            pasted(&state).await?
        }
    };
    auth::exchange(http, issuer, &code, &pkce.verifier, &redirect).await
}

/// Lines from stdin until one is this sign-in's redirect; a wrong one is
/// said and another is read.
async fn pasted(state: &str) -> Result<String> {
    use tokio::io::AsyncBufReadExt;
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        match auth::code_from_redirect(line.trim(), state) {
            Ok(code) => return Ok(code),
            Err(e) => println!("{e:#}. Paste the whole address."),
        }
    }
    bail!("no address was pasted")
}

/// After a sign-in: `[providers.chatgpt]` in the config if no provider is
/// on the plan yet. The default model isn't changed (setup or `/model`
/// does that).
pub(crate) fn add_provider(plan: Plan, model: &str) -> Result<Option<String>> {
    let base = plan.as_str();
    let Some(path) = config::config_path()? else {
        return Ok(Some(format!(
            "There's no config yet: `ferrule setup` picks the plan as the model, \
             or add [providers.{base}] with plan = \"{base}\" and model = \"{model}\"."
        )));
    };
    add_provider_at(&path, plan, model)
}

/// [`add_provider`] in the config at `path` (the dashboard's).
pub(crate) fn add_provider_at(
    path: &std::path::Path,
    plan: Plan,
    model: &str,
) -> Result<Option<String>> {
    let base = plan.as_str();
    let mut t = crate::setup::Target::load(path.to_path_buf())?;
    if t.config()?.providers.values().any(|p| p.plan == Some(plan)) {
        return Ok(None);
    }
    let root = t.root();
    if !root.contains_key("providers") {
        let mut providers = toml_edit::Table::new();
        providers.set_implicit(true);
        root.insert("providers", toml_edit::Item::Table(providers));
    }
    let providers = root
        .get_mut("providers")
        .and_then(|i| i.as_table_like_mut())
        .context("[providers] in the config isn't a table")?;
    let name = if providers.contains_key(base) {
        format!("{base}-plan")
    } else {
        base.to_string()
    };
    let mut table = toml_edit::Table::new();
    table.insert("plan", toml_edit::value(base));
    table.insert("model", toml_edit::value(model));
    providers.insert(&name, toml_edit::Item::Table(table));
    t.save()?;
    Ok(Some(format!(
        "Added [providers.{name}] to {}; `/model default {name}` (or `ferrule setup`) makes it the default.",
        path.display()
    )))
}

/// `/login` and `/logout`, before any chat turn, so neither the command
/// nor the code ever reaches a transcript.
pub struct PlanDoor {
    hub: Arc<Hub>,
    plan: PlanSource,
}

/// Where the door finds the ChatGPT plan (a mock one in tests).
type PlanSource = Box<dyn Fn() -> Result<Arc<ChatGpt>> + Send + Sync>;

const CLAUDE_IN_CHAT: &str =
    "The Claude plan can't be signed in from a chat: Anthropic's sign-in has \
     to complete in its own flow. Run `ferrule login claude` on the server. \
     Never paste a Claude token into a chat.";

impl PlanDoor {
    /// `issuer`: `[plans.chatgpt] issuer`, "" for OpenAI's.
    pub fn new(hub: Arc<Hub>, issuer: String) -> Self {
        Self {
            hub,
            plan: Box::new(move || chatgpt(&issuer)),
        }
    }

    async fn answer(&self, msg: &InboundMessage, cmd: &str, rest: &str) -> String {
        let word = rest.split_whitespace().next().unwrap_or("").to_lowercase();
        let login = cmd == "/login";
        match word.as_str() {
            "claude" | "claude-code" | "anthropic" if login => return CLAUDE_IN_CHAT.into(),
            "claude" | "claude-code" | "anthropic" => {
                return "Run `ferrule logout claude` on the server.".into()
            }
            "chatgpt" | "openai" | "codex" => {}
            _ => {
                return format!(
                    "Usage: {cmd} chatgpt. (The Claude plan signs in on the server: `ferrule login claude`.)"
                )
            }
        }
        // The owner's own chat only: a device code shown in a group could
        // be typed in by anyone there first.
        if trust::owner_in(&self.hub, msg) != Some(true) {
            return "Only the owner can sign in or out, in their own chat with me.".into();
        }
        let plan = match (self.plan)() {
            Ok(p) => p,
            Err(e) => return format!("The ChatGPT plan isn't usable here: {e:#}"),
        };
        if !login {
            return match plan.log_out().await {
                Ok(out) => logged_out_words(out).into(),
                Err(e) => format!("Signing out failed: {e:#}"),
            };
        }
        let code = match auth::start_device(plan.http(), plan.issuer()).await {
            Ok(Some(code)) => code,
            Ok(None) => {
                return "Device sign-in is off for this ChatGPT account. Turn it on in ChatGPT \
                        (Settings → Security) and send /login chatgpt again, or run \
                        `ferrule login chatgpt --browser` on the server."
                    .into()
            }
            Err(e) => return format!("Couldn't start the sign-in: {e:#}"),
        };
        let reply = format!(
            "Open {} and enter the code {} (good for 15 minutes). \
             I'll say here when it's done. Never send me a password or a token.",
            code.page, code.user_code
        );
        let hub = self.hub.clone();
        tokio::spawn(async move {
            let said = match auth::finish_device(plan.http(), plan.issuer(), &code).await {
                Ok(tokens) => match plan.sign_in(tokens).await {
                    Ok(meta) => {
                        let who = super::SignIn::In {
                            email: meta.email,
                            plan: meta.plan,
                        };
                        format!(
                            "{} on the ChatGPT plan. Its models are `chatgpt/…`; /status shows the usage limits.",
                            capitalize(&who.word())
                        )
                    }
                    Err(e) => format!("The sign-in came back but couldn't be stored: {e:#}"),
                },
                Err(e) => format!("The ChatGPT sign-in didn't complete: {e:#}"),
            };
            hub.tell_owner(said);
        });
        reply
    }
}

#[async_trait::async_trait]
impl ferrule_gateway::Interceptor for PlanDoor {
    async fn intercept(&self, msg: &InboundMessage) -> Option<String> {
        if !trust::is_chat_channel(&msg.channel) {
            return None;
        }
        let t = msg.text.trim();
        let (cmd, rest) = match t.split_once(char::is_whitespace) {
            Some((c, r)) => (c, r.trim()),
            None => (t, ""),
        };
        let cmd = cmd.split('@').next().unwrap_or(cmd).to_lowercase();
        if cmd != "/login" && cmd != "/logout" {
            return None;
        }
        Some(self.answer(msg, &cmd, rest).await)
    }
}

/// How long ago `since` (seconds) was, in days, for doctor.
pub fn age_words(since: u64) -> String {
    let days = now().saturating_sub(since) / 86_400;
    match days {
        0 => "today".into(),
        1 => "yesterday".into(),
        d => format!("{d} days ago"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrule_gateway::Interceptor;
    use ferrule_plans::mock;
    use std::sync::Mutex;

    /// The owner's chat, as the hub's notifier sees it.
    #[derive(Default)]
    struct Told(Mutex<Vec<String>>);

    #[async_trait::async_trait]
    impl ferrule_trust::Notifier for Told {
        async fn send(&self, _chat: i64, text: &str) -> Result<(), String> {
            self.0.lock().unwrap().push(text.to_string());
            Ok(())
        }
    }

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

    struct World {
        dir: tempfile::TempDir,
        state: mock::Shared,
        door: PlanDoor,
        told: Arc<Told>,
    }

    async fn world() -> World {
        let dir = tempfile::tempdir().unwrap();
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
        let told = Arc::new(Told::default());
        hub.set_notifier(Some(told.clone()));
        let state = mock::Shared::default();
        let url = mock::serve(state.clone()).await;
        let plan = Arc::new(ChatGpt::new(
            &dir.path().join("private"),
            None,
            mock::issuer(&url),
        ));
        let door = PlanDoor {
            hub,
            plan: Box::new(move || Ok(plan.clone())),
        };
        World {
            dir,
            state,
            door,
            told,
        }
    }

    async fn until_told(told: &Told) -> String {
        for _ in 0..200 {
            if let Some(t) = told.0.lock().unwrap().first() {
                return t.clone();
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("the owner was never told");
    }

    #[tokio::test]
    async fn the_owner_signs_in_by_device_code_in_their_own_chat() {
        let w = world().await;
        assert!(w.door.intercept(&msg("42", None, "hello")).await.is_none());
        let said = w
            .door
            .intercept(&msg("42", None, "/login@ferrule_bot chatgpt"))
            .await
            .unwrap();
        assert!(
            said.contains("/codex/device") && said.contains("ABCD-1234"),
            "{said}"
        );
        assert!(
            said.contains("Never send me a password or a token"),
            "{said}"
        );
        let done = until_told(&w.told).await;
        assert!(done.contains("Signed in as owner@example.com"), "{done}");
        let private = w.dir.path().join("private");
        assert!(matches!(
            super::super::chatgpt_state_at(&private),
            super::super::SignIn::In { .. }
        ));
        // Nothing the chat saw is a token.
        let refresh = w.state.lock().unwrap().current_refresh.clone();
        assert!(!said.contains(&refresh) && !done.contains(&refresh));

        let out = w
            .door
            .intercept(&msg("42", None, "/logout chatgpt"))
            .await
            .unwrap();
        assert!(out.starts_with("Signed out of the ChatGPT plan"), "{out}");
        assert!(!matches!(
            super::super::chatgpt_state_at(&private),
            super::super::SignIn::In { .. }
        ));
    }

    #[tokio::test]
    async fn only_the_owner_in_their_own_chat_and_never_the_claude_plan() {
        let w = world().await;
        for (chat, sender) in [("7", Some("7")), ("-100", Some("42")), ("-100", None)] {
            let no = w
                .door
                .intercept(&msg(chat, sender, "/login chatgpt"))
                .await
                .unwrap();
            assert!(no.starts_with("Only the owner"), "{chat}: {no}");
        }
        assert!(
            w.state.lock().unwrap().requests.is_empty(),
            "no code was asked for"
        );
        // The Claude plan is refused to anyone, the owner included.
        for word in ["claude", "claude-code", "anthropic"] {
            let no = w
                .door
                .intercept(&msg("42", None, &format!("/login {word}")))
                .await
                .unwrap();
            assert_eq!(no, CLAUDE_IN_CHAT);
        }
        let usage = w.door.intercept(&msg("42", None, "/login")).await.unwrap();
        assert!(usage.starts_with("Usage: /login chatgpt"), "{usage}");
        // Not a chat channel: not the door's.
        let mut cli = msg("42", None, "/login chatgpt");
        cli.channel = "cli".into();
        assert!(w.door.intercept(&cli).await.is_none());
    }

    #[tokio::test]
    async fn device_sign_in_off_points_to_the_setting_and_the_browser() {
        let w = world().await;
        w.state.lock().unwrap().no_device = true;
        let said = w
            .door
            .intercept(&msg("42", None, "/login chatgpt"))
            .await
            .unwrap();
        assert!(said.contains("Device sign-in is off"), "{said}");
        assert!(said.contains("ferrule login chatgpt --browser"), "{said}");
    }

    #[test]
    fn ages_read_as_days() {
        assert_eq!(age_words(now()), "today");
        assert_eq!(age_words(now() - 86_400 - 5), "yesterday");
        assert_eq!(age_words(now() - 10 * 86_400), "10 days ago");
    }
}
