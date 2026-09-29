//! M39 §3: WhatsApp on the CLI side: the adapter's settings from
//! `[gateway.whatsapp]`, the dashboard card, and the relay's mailbox check
//! the doctor and setup share.

use super::card::{Field, Kind, Settings, Spec, Step};
use super::settings::{WhatsApp, WhatsAppInbound};
use crate::config::Config;
use crate::config_follow::secret_value;
use anyhow::{anyhow, Result};
use ferrule_gateway::channels::files::Inbox;
use ferrule_gateway::channels::whatsapp::{self as wa, Inbound, Template, WhatsAppConfig};
use std::future::Future;
use std::path::Path;
use std::pin::Pin;

/// A secret the config names, from the environment or `secrets.env`.
fn secret(env: &str, key: &str) -> Result<String> {
    secret_value(env).ok_or_else(|| {
        anyhow!("`{env}` isn't set (needed by [gateway.whatsapp] {key}) — run `ferrule setup` → WhatsApp, or the dashboard's WhatsApp card")
    })
}

/// The relay and its key the mailbox is on: `relay_url` if set, else the
/// instance's deployed relay.
pub fn relay(cfg: &Config, w: &WhatsApp) -> Option<(String, String)> {
    let url = w
        .relay_url
        .clone()
        .or_else(|| cfg.connections.relay_url.clone())?;
    Some((
        url,
        secret_value(ferrule_connections::relay::RELAY_KEY_ENV)?,
    ))
}

/// The adapter's settings. `workspace`: where files people send are saved;
/// `None` (`ferrule tasks run-now`): nothing is taken in, only sent.
pub fn config(cfg: &Config, w: &WhatsApp, workspace: Option<&Path>) -> Result<WhatsAppConfig> {
    let inbound = match (workspace, w.inbound) {
        (None, _) => Inbound::None,
        (Some(_), WhatsAppInbound::Listen) => Inbound::Listen {
            port: w.listen_port,
        },
        (Some(_), WhatsAppInbound::Relay) => {
            let (url, key) = relay(cfg, w).ok_or_else(|| {
                anyhow!("WhatsApp's webhooks come through the relay, and none is deployed (or {} is missing) — run `ferrule connections relay deploy`, or set [gateway.whatsapp] inbound = \"listen\" behind your own tunnel", ferrule_connections::relay::RELAY_KEY_ENV)
            })?;
            Inbound::Relay { url, key }
        }
    };
    Ok(WhatsAppConfig {
        phone_number_id: w.phone_number_id.clone(),
        token: secret(&w.token_env, "token_env")?,
        app_secret: secret(&w.app_secret_env, "app_secret_env")?,
        verify_token: secret(&w.verify_token_env, "verify_token_env")?,
        api_url: w.api_url.clone(),
        api_version: w.api_version.clone(),
        inbound,
        template: w.template.clone().map(|name| Template {
            name,
            language: w.template_language.clone(),
        }),
        state_dir: crate::config::data_dir()
            .ok()
            .map(|d| d.join("gateway").join("whatsapp")),
        inbox: workspace.map(|ws| Inbox::new(ws, w.max_file_mb)),
    })
}

/// Whether the relay at `url` has the WhatsApp mailbox (a relay deployed
/// before M39 doesn't).
pub async fn relay_has_mailbox(url: &str) -> Result<bool, String> {
    let url = format!("{}/health", url.trim_end_matches('/'));
    let client = ferrule_gateway::channels::ws::http_client(
        &url,
        std::time::Duration::from_secs(5),
        std::time::Duration::from_secs(10),
    );
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("the relay didn't answer: {}", e.without_url()))?;
    if !resp.status().is_success() {
        return Err(format!("the relay answered {}", resp.status()));
    }
    let v: serde_json::Value = resp
        .json()
        .await
        .map_err(|_| "the relay's /health isn't a ferrule relay's".to_string())?;
    Ok(v["wa"] == true)
}

/// Where Meta's webhook should point, in words, for setup and the card.
pub fn callback(base: &str) -> String {
    format!("callback URL {base}")
}

fn probe(s: Settings) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>> {
    Box::pin(async move {
        let w: WhatsApp = s.read()?;
        let token = s.secret("token_env").ok_or("paste the access token.")?;
        let p = wa::probe(&w.api_url, &w.api_version, &w.phone_number_id, token).await?;
        let who = if p.name.is_empty() {
            p.number.clone()
        } else {
            format!("{} ({})", p.number, p.name)
        };
        match w.inbound {
            WhatsAppInbound::Listen => Ok(format!(
                "{who} · point your tunnel at 127.0.0.1:{}, and Meta's callback URL at the tunnel's https address",
                w.listen_port
            )),
            WhatsAppInbound::Relay => {
                let url = w.relay_url.clone().or_else(|| s.relay.as_ref().map(|r| r.0.clone()));
                let (Some(url), Some(key)) = (url, s.relay.as_ref().map(|r| r.1.clone())) else {
                    return Err(format!(
                        "{who} works, but no relay is deployed for the webhooks: `ferrule connections relay deploy` (or choose listen, behind your own tunnel)"
                    ));
                };
                let verify = s.secret("verify_token_env").ok_or("type a verify token.")?;
                let app_secret = s.secret("app_secret_env").ok_or("paste the app secret.")?;
                let base = wa::configure_mailbox(&url, &key, verify, app_secret).await?;
                Ok(format!(
                    "{who} · in Meta's Webhooks, {} and your verify token, then subscribe to messages",
                    callback(&base)
                ))
            }
        }
    })
}

fn check(t: &toml::Table) -> Result<(), String> {
    toml::Value::Table(t.clone())
        .try_into::<WhatsApp>()
        .map(|_| ())
        .map_err(|e| e.message().to_string())
}

pub const SPEC: Spec = Spec {
    name: "whatsapp",
    fields: &[
        Field {
            key: "phone_number_id",
            label: "Phone number ID",
            hint: "digits, from API Setup → From (not the phone number)",
            kind: Kind::Text,
            optional: false,
        },
        Field {
            key: "token_env",
            label: "Access token",
            hint: "a system user's permanent token (EAA…) with whatsapp_business_messaging",
            kind: Kind::Secret {
                env: "WHATSAPP_TOKEN",
            },
            optional: false,
        },
        Field {
            key: "app_secret_env",
            label: "App secret",
            hint: "App settings → Basic → App secret: every webhook's signature is checked with it",
            kind: Kind::Secret {
                env: "WHATSAPP_APP_SECRET",
            },
            optional: false,
        },
        Field {
            key: "verify_token_env",
            label: "Verify token",
            hint: "any word you make up; type the same one into Meta's Webhooks form",
            kind: Kind::Secret {
                env: "WHATSAPP_VERIFY_TOKEN",
            },
            optional: false,
        },
        Field {
            key: "inbound",
            label: "Webhooks through",
            hint: "relay: your relay Worker's mailbox · listen: 127.0.0.1 behind your own tunnel",
            kind: Kind::Choice(&["relay", "listen"]),
            optional: true,
        },
        Field {
            key: "listen_port",
            label: "Listen port",
            hint: "with listen only: the 127.0.0.1 port your tunnel points at (8787)",
            kind: Kind::Number,
            optional: true,
        },
        Field {
            key: "template",
            label: "Template",
            hint: "optional: an approved utility template with one {{1}}, sent when the 24-hour window is closed",
            kind: Kind::Text,
            optional: true,
        },
        Field {
            key: "allowed_users",
            label: "Allowed numbers",
            hint: "digits with the country code, no + (972501234567)",
            kind: Kind::List,
            optional: true,
        },
    ],
    guide: &[
        Step {
            text: "Make a Business app and add the WhatsApp product",
            url: Some("https://developers.facebook.com/apps"),
        },
        Step {
            text: "WhatsApp → API Setup: copy the Phone number ID; add your number under To while testing",
            url: Some("https://developers.facebook.com/docs/whatsapp/cloud-api/get-started"),
        },
        Step {
            text: "Business settings → System users: a permanent token with whatsapp_business_messaging",
            url: Some("https://business.facebook.com/settings/system-users"),
        },
        Step {
            text: "App settings → Basic: copy the App secret",
            url: Some("https://developers.facebook.com/apps"),
        },
        Step {
            text: "Save and Test here, then WhatsApp → Configuration → Webhook: the callback URL Test gave and your verify token; subscribe to messages",
            url: Some("https://developers.facebook.com/docs/whatsapp/cloud-api/guides/set-up-webhooks"),
        },
    ],
    probe,
    check,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_card_reads_as_the_settings() {
        let mut t = toml::Table::new();
        t.insert("phone_number_id".into(), "123".into());
        assert!(check(&t).is_ok());
        t.insert("inbound".into(), "carrier-pigeon".into());
        assert!(check(&t).is_err());
    }

    #[tokio::test]
    async fn test_says_a_relay_is_needed_when_none_is_deployed() {
        let mock = wa_mock().await;
        let mut table = toml::Table::new();
        table.insert("phone_number_id".into(), "1110001".into());
        table.insert("api_url".into(), mock.clone().into());
        for (k, v) in [
            ("token_env", "WHATSAPP_TOKEN"),
            ("app_secret_env", "WHATSAPP_APP_SECRET"),
            ("verify_token_env", "WHATSAPP_VERIFY_TOKEN"),
        ] {
            table.insert(k.into(), v.into());
        }
        let secrets = [
            ("WHATSAPP_TOKEN", "EAAgood"),
            ("WHATSAPP_APP_SECRET", "s"),
            ("WHATSAPP_VERIFY_TOKEN", "v"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let s = Settings {
            table: table.clone(),
            secrets,
            relay: None,
        };
        let e = probe(s.clone()).await.unwrap_err();
        assert!(
            e.contains("+1 555 0100 (Shop) works") && e.contains("relay deploy"),
            "{e}"
        );
        table.insert("inbound".into(), "listen".into());
        let ok = probe(Settings { table, ..s }).await.unwrap();
        assert!(ok.contains("127.0.0.1:8787"), "{ok}");
    }

    /// A one-route Graph API: the number's info for the good token.
    async fn wa_mock() -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let mut buf = vec![0u8; 4096];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_lowercase();
                let body = if req.contains("bearer eaagood") {
                    r#"{"display_phone_number":"+1 555 0100","verified_name":"Shop"}"#
                } else {
                    r#"{"error":{"code":190,"message":"bad token"}}"#
                };
                let status = if body.contains("error") {
                    "401 Unauthorized"
                } else {
                    "200 OK"
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
            }
        });
        format!("http://127.0.0.1:{port}")
    }
}
