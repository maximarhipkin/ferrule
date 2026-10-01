//! `/update`, `/restart` and `/doctor` in the owner's chat (M48): the same
//! ops as the tool, so a person who types the command gets the same card.

use super::tool::ask_and_run;
use super::{doctor, offered, run, update, Admin, Op};
use ferrule_gateway::InboundMessage;
use ferrule_trust::ChatRef;
use std::sync::Arc;

pub struct SelfServiceDoor {
    admin: Arc<Admin>,
}

impl SelfServiceDoor {
    pub const HANDLES: &'static [&'static str] = &["update", "restart", "doctor"];

    pub fn new(admin: Arc<Admin>) -> Self {
        Self { admin }
    }

    /// `/update`: check, then the card.
    async fn update(admin: Arc<Admin>, here: ChatRef, session: String) {
        if let Some(why) = admin
            .ctx()
            .and_then(|ctx| super::locks::refusal(&Op::Update, ctx))
        {
            return admin.hub.tell_in(&here, why);
        }
        let said = match Self::check(&admin).await {
            Err(e) => Some(e),
            Ok(None) => None,
            Ok(Some(text)) => Some(text),
        };
        if let Some(text) = said {
            return admin.hub.tell_in(&here, text);
        }
        Self::ask(&admin, &here, &session, Op::Update).await;
    }

    /// `Ok(None)`: there's something to install. `Ok(Some)`: nothing to do.
    async fn check(admin: &Admin) -> Result<Option<String>, String> {
        let ctx = admin
            .ctx()
            .ok_or("Ferrule is still starting; try again in a few seconds.")?;
        let (apply, _) = admin.apply(ctx)?;
        match update::check(&apply).await? {
            Some(_) => Ok(None),
            None => Ok(Some(update::check_text(&apply.current, None))),
        }
    }

    async fn ask(admin: &Admin, here: &ChatRef, session: &str, op: Op) {
        let text = match ask_and_run(admin, here, session, op).await {
            // An approved update or restart ends with the process's own
            // message once it's back.
            Ok(said) => said,
            Err(why) => why,
        };
        admin.hub.tell_in(here, text);
    }

    /// `/doctor`: the report, then at most `MOST_FIXES` fix cards.
    async fn doctor(admin: Arc<Admin>, here: ChatRef, session: String) {
        let Some(ctx) = admin.ctx() else {
            return admin.hub.tell_in(
                &here,
                "Ferrule is still starting; try again in a few seconds.".into(),
            );
        };
        let (text, fixes) = match run::doctor_with_fixes(ctx).await {
            Ok(x) => x,
            Err(e) => {
                return admin
                    .hub
                    .tell_in(&here, format!("Doctor couldn't run: {e}"))
            }
        };
        admin.hub.tell_in(&here, text);
        for op in fixes.into_iter().take(doctor::MOST_FIXES) {
            Self::ask(&admin, &here, &session, op).await;
        }
    }
}

#[async_trait::async_trait]
impl ferrule_gateway::Interceptor for SelfServiceDoor {
    async fn intercept(&self, msg: &InboundMessage) -> Option<String> {
        let first = msg.text.split_whitespace().next()?;
        let cmd = first.split('@').next().unwrap_or(first).to_lowercase();
        let cmd = cmd.strip_prefix('/')?;
        if !Self::HANDLES.contains(&cmd) {
            return None;
        }
        if !crate::trust::is_chat_channel(&msg.channel) && msg.channel != "dashboard" {
            return None;
        }
        let verb = match cmd {
            "update" => "update",
            "restart" => "restart",
            _ => "check",
        };
        if !crate::trust::by_owner(&self.admin.hub, msg) {
            return Some(format!("Only the owner can {verb} Ferrule."));
        }
        let session = ferrule_gateway::session::session_id(&msg.channel, &msg.chat_id);
        let Some(here) = offered(&self.admin.hub, &session) else {
            return Some(
                "Ask me in our private chat: that's where I show you what changes and you approve it."
                    .into(),
            );
        };
        let admin = self.admin.clone();
        let (reply, work): (
            &str,
            std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
        ) = match cmd {
            "update" => (
                "Checking for a new Ferrule…",
                Box::pin(Self::update(admin, here, session)),
            ),
            "restart" => {
                let here2 = here.clone();
                (
                    "",
                    Box::pin(async move { Self::ask(&admin, &here2, &session, Op::Restart).await }),
                )
            }
            _ => (
                "Running doctor…",
                Box::pin(Self::doctor(admin, here, session)),
            ),
        };
        tokio::spawn(work);
        Some(reply.to_string())
    }
}
