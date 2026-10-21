//! `/dashboard` and `/dashboard off` in a chat, answered by the gateway
//! itself like `/status`: no model, no lane, so it works mid-turn, with the
//! kill switch on or every model down (docs/m22-dashboard.md §1).

use super::Dashboard;
use ferrule_gateway::InboundMessage;
use ferrule_trust::{ChatRef, Hub};
use std::sync::Arc;

pub struct DashboardDoor {
    pub dash: Arc<Dashboard>,
    pub hub: Arc<Hub>,
}

impl DashboardDoor {
    #[cfg(test)]
    pub const HANDLES: &'static [&'static str] = &["dashboard"];

    fn owner_chat(&self, msg: &InboundMessage) -> Option<bool> {
        crate::trust::owner_in(&self.hub, msg)
    }
}

/// The message that carries a login link.
pub fn link_text(dash: &Dashboard, link: &str, remote: bool) -> String {
    let minutes = dash.settings().link_minutes.max(1);
    let mut text = format!(
            "Dashboard: {link}\nOne login, valid for {minutes} min. /dashboard off revokes every link and session."
        );
    if !remote && dash.public().is_none() {
        text.push_str(&format!(
                "\nThat address works on the machine itself. From elsewhere: ssh -L {p}:127.0.0.1:{p} <server>, then open it.",
                p = dash.port()
            ));
    }
    text
}

/// Whether a link goes through the quick tunnel (else it's the loopback or
/// the public address).
fn remote(dash: &Dashboard) -> bool {
    !crate::managed::on() && dash.settings().remote == "tunnel" && dash.ctx.cloudflared.possible()
}

/// `link` with `suffix` just before the `#` that carries the token.
fn with_suffix(link: &str, suffix: &str) -> String {
    match link.split_once('#') {
        Some((page, token)) => format!("{page}{suffix}#{token}"),
        None => format!("{link}{suffix}"),
    }
}

/// The message with a one-time login link, `suffix` (what the page opens)
/// before its `#`. When links go through the tunnel it is opened first; if
/// it doesn't open, the message says why and carries the loopback link
/// instead. `Err`: why there's no link at all.
async fn link_message(dash: &Dashboard, suffix: &str) -> Result<String, String> {
    let local = || {
        dash.local_link()
            .map(|link| link_text(dash, &with_suffix(&link, suffix), false))
            .map_err(|e| format!("{e:#}"))
    };
    if !remote(dash) {
        return local();
    }
    match dash.remote_link().await {
        Ok(link) => Ok(link_text(dash, &with_suffix(&link, suffix), true)),
        Err(e) => {
            tracing::warn!("dashboard tunnel: {e:#}");
            let why = format!("The tunnel didn't open: {e:#}.");
            match local() {
                Ok(l) => Ok(format!("{why}\n{l}")),
                Err(_) => Err(why),
            }
        }
    }
}

/// Sends the owner a one-time login link (see [`link_message`]): to `to`
/// when that's the owner's private chat, else to their primary chat
/// (`Hub::tell_in`), never into a group. The link is only ever this
/// message from the gateway: never an answer a model or a session
/// transcript gets. `Err`: no link was sent.
pub async fn send_link(
    dash: &Dashboard,
    hub: &Hub,
    to: &ChatRef,
    suffix: &str,
) -> Result<(), String> {
    send_link_with(dash, hub, to, suffix, "").await
}

/// [`send_link`], with `lead` (when not empty) as the message's first
/// paragraph: it arrives with the link, never after it.
pub async fn send_link_with(
    dash: &Dashboard,
    hub: &Hub,
    to: &ChatRef,
    suffix: &str,
    lead: &str,
) -> Result<(), String> {
    let dest = if hub.is_owner(to) {
        Some(to.clone())
    } else {
        hub.primary()
    };
    let Some(dest) = dest else {
        return Err("there's no private chat with the owner to send it to".into());
    };
    // The HTTP API is for programs: what it sends unasked waits in an
    // outbox for whoever holds a key.
    if dest.channel == "http" {
        return Err(
            "the owner's chat is the HTTP API; a login link goes only to a person's chat".into(),
        );
    }
    let mut text = link_message(dash, suffix).await?;
    if !lead.is_empty() {
        text = format!("{lead}\n\n{text}");
    }
    hub.tell_in(&dest, text);
    Ok(())
}

#[async_trait::async_trait]
impl ferrule_gateway::Interceptor for DashboardDoor {
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
        if !cmd.eq_ignore_ascii_case("/dashboard") {
            return None;
        }
        // Anyone else gets nothing at all: not even that it exists.
        let Some(in_owner_chat) = self.owner_chat(msg) else {
            return Some(String::new());
        };
        if rest.eq_ignore_ascii_case("off") {
            return Some(match self.dash.off().await {
                Ok(()) => {
                    "Dashboard closed: every link and session is revoked and the tunnel is shut."
                        .into()
                }
                Err(e) => format!(
                    "The tunnel is closed, but the links or sessions file didn't write: {e:#}"
                ),
            });
        }
        if !rest.is_empty() {
            return Some("Usage: /dashboard (a one-time login link), /dashboard off".into());
        }
        let ready = !remote(&self.dash) || self.dash.tunnel_open().await;
        // In the owner's chat the link is the reply itself, as an answer
        // to their message (the HTTP API's stays in the response).
        if in_owner_chat && ready {
            return Some(match link_message(&self.dash, "").await {
                Ok(text) => text,
                Err(e) => format!("No link: {e}"),
            });
        }
        // Else it goes to the owner's own chat only, never into a group:
        // from a group, to their primary chat.
        let to = if in_owner_chat {
            self.hub.owner_on(&msg.channel)
        } else {
            self.hub.primary()
        };
        let Some(to) = to else {
            return Some(String::new());
        };
        if ready {
            return Some(match send_link(&self.dash, &self.hub, &to, "").await {
                Ok(()) => "I sent the dashboard link to your private chat.".into(),
                Err(e) => format!("No link: {e}"),
            });
        }
        let where_ = if in_owner_chat {
            ""
        } else {
            " I'll send it to your private chat."
        };
        // Opening a tunnel takes a few seconds: answer now, send the link
        // when it's up.
        let dash = self.dash.clone();
        let hub = self.hub.clone();
        tokio::spawn(async move {
            if let Err(e) = send_link(&dash, &hub, &to, "").await {
                hub.tell_in(&to, format!("No link: {e}"));
            }
        });
        Some(if self.dash.ctx.cloudflared.needs_fetch() {
            format!(
                "Fetching cloudflared (Cloudflare's tunnel program) the first time, then opening the dashboard's tunnel; the link follows in a minute.{where_}"
            )
        } else {
            format!("Opening the dashboard's tunnel, the link follows in a moment.{where_}")
        })
    }
}
