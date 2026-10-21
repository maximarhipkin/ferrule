//! Asking the owner for a key (M54 §6.3). `secret_request` makes the
//! page's own checks, sends the owner a one-time login link that opens the
//! Keys panel with the name and hosts filled in, and remembers which
//! session asked. When the page saves that name, the session is woken, so
//! the bot carries on by itself. The key never passes through here: the
//! owner pastes it on the page, never in a chat.

use super::Admin;
use crate::dashboard::{door, tool_keys, Ctx};
use ferrule_extensions::manager::SYNC_EVERY;
use ferrule_gateway::health::clip;
use ferrule_gateway::{Redactor, Router};
use ferrule_trust::ChatRef;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock, Weak};
use std::time::Duration;

/// The most of the bot's reason the owner is shown, in characters.
const WHY: usize = 300;

/// The gateway's lanes, to wake the session that asked.
static ROUTER: Mutex<Option<Weak<Router>>> = Mutex::new(None);

/// The session that asked for each name; the last to ask wins.
// ponytail: in memory, so a restart forgets who asked; one entry per name
// asked for and not saved.
static ASKED: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

pub fn bind(router: Weak<Router>) {
    *ROUTER.lock().unwrap_or_else(|e| e.into_inner()) = Some(router);
}

/// `session` asked the owner for the key `name`.
pub fn asked(name: &str, session: &str) {
    ASKED
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(name.to_string(), session.to_string());
}

/// The page saved `name`: the session that asked for it, if one did, is
/// woken with the name and its hosts (never the value). The wake is sent
/// after a fixed `SYNC_EVERY` + 1 s, about when `config_follow` has bound
/// the new key into the running proxy.
// ponytail: a fixed delay with no word from config_follow that the bind
// happened, so a slow reload wakes the bot early; the upgrade is to wake
// from config_follow once it has bound the key.
pub fn saved(name: &str, hosts: &[String]) {
    let Some(session) = ASKED
        .get()
        .and_then(|m| m.lock().unwrap_or_else(|e| e.into_inner()).remove(name))
    else {
        return;
    };
    let Some(router) = ROUTER.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
        return;
    };
    let Ok(rt) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let text = format!(
        "[the owner saved the key {name}; it is sent only to {}]",
        hosts.join(", ")
    );
    rt.spawn(async move {
        tokio::time::sleep(SYNC_EVERY + Duration::from_secs(1)).await;
        if let Some(router) = router.upgrade() {
            router.wake(&session, text);
        }
    });
}

/// `secret_request`: a refusal has the page's own words. The owner gets
/// one message, the ask and then the link; the answer has neither the link
/// nor a key, so neither reaches the model or the transcript.
pub(super) async fn request(
    admin: &Admin,
    ctx: &Ctx,
    here: &ChatRef,
    session: &str,
    name: &str,
    hosts: &[String],
    why: &str,
) -> Result<String, String> {
    let Some(dash) = admin.page() else {
        return Err("The dashboard isn't on in this process.".into());
    };
    let name = name.trim();
    let hosts = tool_keys::check_new(ctx, name, &hosts.join(",")).map_err(|(_, e)| e)?;
    asked(name, session);
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("key", name)
        .append_pair("hosts", &hosts.join(","))
        .finish();
    let ask = format!(
        "The bot asks for a key: {name}, sent only to {}. Why, in the bot's own words: “{}”\n\
         Paste the key on the page this link opens (you can change the sites there), \
         never in a chat.",
        hosts.join(", "),
        reason(why, &ctx.redactor),
    );
    door::send_link_with(dash, &admin.hub, here, &format!("?{query}"), &ask)
        .await
        .map_err(|e| format!("No link was sent: {e}"))?;
    admin.hub.audit().record(
        chrono::Utc::now(),
        "self_service.secret_request",
        None,
        None,
        json!({
            "name": name,
            "hosts": hosts,
            "by": format!("{} chat {}", here.channel, here.chat),
        }),
    );
    // The send itself is the channel's, after this answer.
    Ok(format!(
        "Sending the owner a link to paste {name}; you're woken when it's saved."
    ))
}

/// The bot's reason as the owner reads it, right above the real link. It
/// keeps only letters, digits, `_`, spaces and `, ' " ( ) - ! ? ;`; every
/// other character (`.`, `:`, `/`, `@`, `#`, `%`, `&`, `=`, `。`, and every
/// whitespace, control and format character) becomes a space, so nothing
/// left can form a link, a domain, an IP, a mention or a command. Runs of
/// spaces become one. It is redacted before (where a secret is still whole)
/// and after, then clipped (clipping first could cut a secret where the
/// redactor no longer sees it).
fn reason(why: &str, redactor: &Redactor) -> String {
    let kept: String = redactor
        .redact(why)
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || "_ ,'\"()-!?;".contains(c) {
                c
            } else {
                ' '
            }
        })
        .collect();
    let line = kept.split_whitespace().collect::<Vec<_>>().join(" ");
    clip(&redactor.redact(&line), WHY)
}
