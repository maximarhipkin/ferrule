//! M59: the two bridge channels on the CLI side: personal WhatsApp through
//! WAHA (`[gateway.whatsapp_personal]`) and iMessage through BlueBubbles
//! Server (`[gateway.imessage]`). The adapters' settings, and their
//! dashboard cards. The bridges are the owner's to run; ferrule only talks
//! to them.

use super::card::{Field, Kind, Settings, Spec, Step};
use super::settings::{IMessage, WhatsAppPersonal};
use crate::config_follow::secret_value;
use anyhow::{anyhow, Result};
use ferrule_gateway::channels::bluebubbles::{self as bb, BlueBubblesConfig};
use ferrule_gateway::channels::files::Inbox;
use ferrule_gateway::channels::waha::{self, WahaConfig};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

/// WAHA's API key, from the env var the config names.
pub fn waha_key(w: &WhatsAppPersonal, secret: impl Fn(&str) -> Option<String>) -> Result<String> {
    secret(&w.api_key_env).ok_or_else(|| {
        anyhow!(
            "`{}` isn't set (needed by [gateway.whatsapp_personal] api_key_env) — run `ferrule setup` → WhatsApp (personal), or the dashboard's card",
            w.api_key_env
        )
    })
}

fn waha_with(w: &WhatsAppPersonal, key: String, inbox: Option<Inbox>) -> WahaConfig {
    WahaConfig {
        url: w.url.trim().trim_end_matches('/').to_string(),
        session: w.session.trim().to_string(),
        api_key: Some(key),
        inbox,
    }
}

/// The WAHA adapter's settings. `workspace`: where files people send are
/// saved; `None` (`ferrule tasks run-now`): nothing is taken in.
pub fn waha_config(w: &WhatsAppPersonal, workspace: Option<&Path>) -> Result<WahaConfig> {
    Ok(waha_with(
        w,
        waha_key(w, secret_value)?,
        workspace.map(|ws| Inbox::new(ws, w.max_file_mb)),
    ))
}

/// The BlueBubbles server password, from the env var the config names.
pub fn bb_password(m: &IMessage, secret: impl Fn(&str) -> Option<String>) -> Result<String> {
    secret(&m.password_env).ok_or_else(|| {
        anyhow!(
            "`{}` isn't set (needed by [gateway.imessage] password_env) — run `ferrule setup` → iMessage, or the dashboard's card",
            m.password_env
        )
    })
}

/// Where the iMessage poll keeps its position.
pub fn bb_cursor() -> Option<PathBuf> {
    crate::config::data_dir()
        .ok()
        .map(|d| d.join("gateway").join("imessage").join("cursor"))
}

fn bb_with(m: &IMessage, password: String, inbox: Option<Inbox>) -> BlueBubblesConfig {
    BlueBubblesConfig {
        url: m.url.trim().trim_end_matches('/').to_string(),
        password,
        private_api: m.private_api,
        poll: Duration::from_secs(m.poll_secs.max(1)),
        inbox,
        cursor: None,
    }
}

/// The BlueBubbles adapter's settings. `workspace`: the gateway, which
/// saves what people send and keeps the poll's position; `None`: only
/// sends.
pub fn bb_config(m: &IMessage, workspace: Option<&Path>) -> Result<BlueBubblesConfig> {
    let mut c = bb_with(
        m,
        bb_password(m, secret_value)?,
        workspace.map(|ws| Inbox::new(ws, m.max_file_mb)),
    );
    if workspace.is_some() {
        c.cursor = bb_cursor();
    }
    Ok(c)
}

fn waha_probe(s: Settings) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>> {
    Box::pin(async move {
        let w: WhatsAppPersonal = s.read()?;
        let key = waha_key(&w, |env| s.secrets.get(env).cloned()).map_err(|e| e.to_string())?;
        let p = waha::probe(waha_with(&w, key, None)).await?;
        if !p.working() {
            return Err(p.explain());
        }
        let mut said = p.summary();
        if w.allowed_users.is_empty() && w.allowed_groups.is_empty() {
            said.push_str(
                " · no one is allowed yet: list your number (your own allows Message yourself)",
            );
        }
        Ok(said)
    })
}

fn waha_check(t: &toml::Table) -> Result<(), String> {
    let w: WhatsAppPersonal = toml::Value::Table(t.clone())
        .try_into()
        .map_err(|e: toml::de::Error| e.message().to_string())?;
    if !(w.url.starts_with("http://") || w.url.starts_with("https://")) {
        return Err(format!(
            "WAHA's address is a URL like http://127.0.0.1:3000, not `{}`",
            w.url
        ));
    }
    for u in &w.allowed_users {
        let n = waha::normalize(u);
        if n.is_empty() || (n.contains('@') && !n.ends_with("@lid")) {
            return Err(format!(
                "`{u}` isn't a number like 972501234567 (digits with the country code)"
            ));
        }
    }
    for g in &w.allowed_groups {
        if !waha::is_group(g.trim()) {
            return Err(format!("`{g}` isn't a group id (they end in @g.us)"));
        }
    }
    Ok(())
}

pub const WAHA_SPEC: Spec = Spec {
    name: "whatsapp_personal",
    fields: &[
        Field {
            key: "url",
            label: "WAHA address",
            hint: "where WAHA answers: http://127.0.0.1:3000",
            kind: Kind::Text,
            optional: false,
        },
        Field {
            key: "session",
            label: "Session",
            hint: "the WAHA session paired with your number: default",
            kind: Kind::Text,
            optional: true,
        },
        Field {
            key: "api_key_env",
            label: "API key",
            hint: "the key WAHA was started with (WAHA_API_KEY)",
            kind: Kind::Secret {
                env: "WAHA_API_KEY",
            },
            optional: false,
        },
        Field {
            key: "allowed_users",
            label: "Allowed numbers",
            hint: "972501234567 — whose messages reach the agent; your own number allows Message yourself",
            kind: Kind::List,
            optional: true,
        },
        Field {
            key: "allowed_groups",
            label: "Allowed groups",
            hint: "group ids (…@g.us) where a mention of your number reaches it",
            kind: Kind::List,
            optional: true,
        },
    ],
    guide: &[
        Step {
            text: "Read the risk first: WAHA automates WhatsApp Web, which WhatsApp's terms don't allow; a number can be banned",
            url: Some("https://www.whatsapp.com/legal/terms-of-service"),
        },
        Step {
            text: "Run WAHA with an API key (docker run -p 127.0.0.1:3000:3000 -e WAHA_API_KEY=… devlikeapro/waha)",
            url: Some("https://waha.devlike.pro/docs/overview/quick-start/"),
        },
        Step {
            text: "In WAHA's dashboard, start the session and scan its QR code with WhatsApp → Linked devices",
            url: None,
        },
        Step {
            text: "Save and Test here, then message the number from an allowed one",
            url: None,
        },
    ],
    probe: waha_probe,
    check: waha_check,
};

fn bb_probe(s: Settings) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>> {
    Box::pin(async move {
        let m: IMessage = s.read()?;
        let pw = bb_password(&m, |env| s.secrets.get(env).cloned()).map_err(|e| e.to_string())?;
        let p = bb::probe(bb_with(&m, pw, None)).await?;
        let mut said = p.summary();
        if m.private_api && !p.private_api {
            said.push_str(" · private_api is on here but not on the server");
        }
        if m.allowed_users.is_empty() {
            said.push_str(" · no one is allowed yet: list your number or Apple ID");
        }
        Ok(said)
    })
}

fn bb_check(t: &toml::Table) -> Result<(), String> {
    let m: IMessage = toml::Value::Table(t.clone())
        .try_into()
        .map_err(|e: toml::de::Error| e.message().to_string())?;
    if !(m.url.starts_with("http://") || m.url.starts_with("https://")) {
        return Err(format!(
            "the server's address is a URL like http://192.168.1.20:1234, not `{}`",
            m.url
        ));
    }
    for u in &m.allowed_users {
        if bb::normalize(u) == "+" || u.trim().is_empty() {
            return Err(format!(
                "`{u}` isn't a phone number like +15551234567 or an Apple ID email"
            ));
        }
    }
    Ok(())
}

pub const BB_SPEC: Spec = Spec {
    name: "imessage",
    fields: &[
        Field {
            key: "url",
            label: "Server address",
            hint: "BlueBubbles Server on the Mac: http://192.168.1.20:1234",
            kind: Kind::Text,
            optional: false,
        },
        Field {
            key: "password_env",
            label: "Server password",
            hint: "the password set in BlueBubbles Server",
            kind: Kind::Secret {
                env: "BLUEBUBBLES_PASSWORD",
            },
            optional: false,
        },
        Field {
            key: "allowed_users",
            label: "Allowed people",
            hint: "+15551234567 or an Apple ID email — whose messages reach the agent",
            kind: Kind::List,
            optional: true,
        },
    ],
    guide: &[
        Step {
            text: "On a Mac signed in to Messages (ideally its own Apple ID), install BlueBubbles Server",
            url: Some("https://bluebubbles.app/install/"),
        },
        Step {
            text: "Give it Full Disk Access and Accessibility as it asks, set a server password, and keep the Mac awake",
            url: Some("https://docs.bluebubbles.app/server/"),
        },
        Step {
            text: "Use its local address if ferrule runs on the same network (http://<the Mac>:1234)",
            url: None,
        },
        Step {
            text: "Save and Test here, then send the Mac's Apple ID a message from an allowed number",
            url: None,
        },
    ],
    probe: bb_probe,
    check: bb_check,
};
