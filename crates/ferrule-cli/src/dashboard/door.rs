//! `/dashboard` and `/dashboard off` in Telegram, answered by the gateway
//! itself like `/status`: no model, no lane, so it works mid-turn, with the
//! kill switch on or every model down (docs/m22-dashboard.md §1).

use super::Dashboard;
use ferrule_gateway::InboundMessage;
use ferrule_trust::Hub;
use std::sync::Arc;

pub struct DashboardDoor {
    pub dash: Arc<Dashboard>,
    pub hub: Arc<Hub>,
}

impl DashboardDoor {
    fn owner_chat(&self, msg: &InboundMessage) -> Option<bool> {
        let owner = self.hub.owner()?;
        if msg.chat_id.parse::<i64>().ok() == Some(owner) {
            return Some(true);
        }
        let sender = msg.sender_id.as_deref().and_then(|s| s.parse::<i64>().ok());
        (sender == Some(owner)).then_some(false)
    }
}

/// The message that carries a login link.
pub fn link_text(dash: &Dashboard, link: &str, remote: bool) -> String {
    let minutes = dash.settings().link_minutes.max(1);
    let mut text = format!(
            "Dashboard: {link}\nOne login, valid for {minutes} min. /dashboard off revokes every link and session."
        );
    if !remote {
        text.push_str(&format!(
                "\nThat address works on the machine itself. From elsewhere: ssh -L {p}:127.0.0.1:{p} <server>, then open it.",
                p = dash.port()
            ));
    }
    text
}

#[async_trait::async_trait]
impl ferrule_gateway::Interceptor for DashboardDoor {
    async fn intercept(&self, msg: &InboundMessage) -> Option<String> {
        if msg.channel != "telegram" {
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
                    "Sessions and the tunnel are closed, but the links file didn't write: {e:#}"
                ),
            });
        }
        if !rest.is_empty() {
            return Some("Usage: /dashboard (a one-time login link), /dashboard off".into());
        }
        // The link goes to the owner's own chat only, never into a group.
        let where_ = if in_owner_chat {
            String::new()
        } else {
            " I'll send it to your private chat.".into()
        };
        let remote = self.dash.settings().remote == "tunnel" && self.dash.ctx.cloudflared.is_some();
        if !remote {
            let text = match self.dash.local_link() {
                Ok(link) => link_text(&self.dash, &link, false),
                Err(e) => return Some(format!("No link: {e:#}")),
            };
            if in_owner_chat {
                return Some(text);
            }
            self.hub.tell_owner(text);
            return Some("I sent the dashboard link to your private chat.".into());
        }
        if in_owner_chat && self.dash.tunnel_open().await {
            return Some(match self.dash.remote_link().await {
                Ok(link) => link_text(&self.dash, &link, true),
                Err(e) => format!("No link: {e:#}"),
            });
        }
        // Opening a tunnel takes a few seconds: answer now, send the link
        // when it's up.
        let dash = self.dash.clone();
        let hub = self.hub.clone();
        tokio::spawn(async move {
            let text = match dash.remote_link().await {
                Ok(link) => link_text(&dash, &link, true),
                Err(e) => {
                    tracing::warn!("dashboard tunnel: {e:#}");
                    let mut t = format!("The tunnel didn't open: {e:#}.");
                    if let Ok(l) = dash.local_link() {
                        t.push('\n');
                        t.push_str(&link_text(&dash, &l, false));
                    }
                    t
                }
            };
            hub.tell_owner(text);
        });
        Some(format!(
            "Opening the dashboard's tunnel, the link follows in a moment.{where_}"
        ))
    }
}
