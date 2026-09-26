//! `/caps`, `/mcp` and `/skills` in a chat (docs/m24-dashboard-2.md §3),
//! owner only: the same operations as the dashboard and the CLI.

use crate::models::Retire;
use crate::settings_admin::{cap_key, show, Settings};
use ferrule_gateway::InboundMessage;
use ferrule_trust::Hub;
use std::sync::Arc;

pub struct SettingsDoor {
    pub settings: Settings,
    pub hub: Arc<Hub>,
    /// Retires every chat's lane after a skill changes: the catalog is in
    /// the system prompt.
    pub retire: Retire,
}

const CAPS_USAGE: &str = "Usage:\n\
/caps: the caps\n\
/caps <key> <value>: set one (0 turns it off); raising one asks, then /caps <key> <value> confirm";
const MCP_USAGE: &str = "Usage:\n\
/mcp: the MCP servers\n\
/mcp off <name>, /mcp on <name>: turn a configured one off or back on";
const SKILLS_USAGE: &str = "Usage:\n\
/skills: the skills\n\
/skills off <name>, /skills on <name>: stop offering one, or offer it again";
const HOOKS: &str = "Workspace hooks are trusted on the dashboard (Extensions, with the diff and the SHA-256) or with `ferrule hooks trust` at a terminal — not from a chat.";

impl SettingsDoor {
    fn is_owner(&self, msg: &InboundMessage) -> bool {
        crate::trust::owner_in(&self.hub, msg).is_some()
    }

    fn caps(&self, words: &[&str], by: &str) -> anyhow::Result<String> {
        match words {
            [] => {
                let view = self.settings.view()?;
                let mut out = vec!["Caps (0 = off):".to_string()];
                out.extend(
                    view.caps
                        .iter()
                        .map(|c| format!("{}: {}", c.key, show(c.key, c.value))),
                );
                out.push(CAPS_USAGE.lines().skip(2).collect::<Vec<_>>().join("\n"));
                Ok(out.join("\n"))
            }
            [key, value] | [key, value, "confirm"] => {
                let key = cap_key(key)?;
                let v: f64 = value
                    .trim_start_matches('$')
                    .parse()
                    .map_err(|_| anyhow::anyhow!("{value} isn't a number"))?;
                let changes = [(key.to_string(), v)];
                if words.len() == 2 {
                    if let Some(q) = self.settings.caps_question(&changes)? {
                        return Ok(format!(
                            "{q}\nSend /caps {key} {value} confirm to go ahead."
                        ));
                    }
                }
                Ok(self.settings.set_caps(&changes, by)?.said)
            }
            _ => Ok(CAPS_USAGE.into()),
        }
    }

    fn mcp(&self, words: &[&str], by: &str) -> anyhow::Result<String> {
        match words {
            [] => {
                let view = self.settings.view()?;
                if view.mcp.is_empty() {
                    return Ok("No MCP servers.".into());
                }
                let mut out: Vec<String> = view
                    .mcp
                    .iter()
                    .map(|m| {
                        format!(
                            "{} ({}, {}){}",
                            m.name,
                            m.runs,
                            m.origin,
                            if m.disabled { " — off" } else { "" }
                        )
                    })
                    .collect();
                out.push("/mcp off <name>, /mcp on <name>".into());
                Ok(out.join("\n"))
            }
            [op @ ("on" | "off"), name] => {
                Ok(self.settings.mcp_set_disabled(name, *op == "off", by)?.said)
            }
            _ => Ok(MCP_USAGE.into()),
        }
    }

    fn skills(&self, words: &[&str], by: &str) -> anyhow::Result<String> {
        match words {
            [] => {
                let view = self.settings.view()?;
                if view.skills.is_empty() {
                    return Ok("No skills.".into());
                }
                let mut out: Vec<String> = view
                    .skills
                    .iter()
                    .map(|s| {
                        let mut line = format!(
                            "{} ({}){}",
                            s.name,
                            s.scope,
                            if s.disabled { " — off" } else { "" }
                        );
                        if !s.triggers.is_empty() {
                            line.push_str(&format!(" — triggers: {}", s.triggers.join(", ")));
                        }
                        line
                    })
                    .collect();
                out.push("/skills off <name>, /skills on <name>".into());
                Ok(out.join("\n"))
            }
            [op @ ("on" | "off"), name] => {
                let d = self.settings.skill_set_disabled(name, *op == "off", by)?;
                (self.retire)(None);
                Ok(d.said)
            }
            _ => Ok(SKILLS_USAGE.into()),
        }
    }
}

#[async_trait::async_trait]
impl ferrule_gateway::Interceptor for SettingsDoor {
    async fn intercept(&self, msg: &InboundMessage) -> Option<String> {
        if !crate::trust::is_chat_channel(&msg.channel) {
            return None;
        }
        let mut words = msg.text.split_whitespace();
        let cmd = words.next()?;
        let cmd = cmd.split('@').next().unwrap_or(cmd).to_lowercase();
        if !matches!(cmd.as_str(), "/caps" | "/mcp" | "/skills" | "/hooks") {
            return None;
        }
        if !self.is_owner(msg) {
            return Some("Only the owner can change settings.".into());
        }
        let words: Vec<&str> = words.collect();
        let by = format!("{} chat {}", msg.channel, msg.chat_id);
        let result = match cmd.as_str() {
            "/caps" => self.caps(&words, &by),
            "/mcp" => self.mcp(&words, &by),
            "/skills" => self.skills(&words, &by),
            _ => Ok(HOOKS.into()),
        };
        Some(result.unwrap_or_else(|e| format!("Nothing changed: {e:#}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrule_gateway::Interceptor;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const CONFIG: &str = r#"
[trust]
max_usd_per_day = 5.0

[[mcp.servers]]
name = "files"
command = "mcp-files"
"#;

    fn msg(chat: &str, text: &str) -> InboundMessage {
        InboundMessage {
            channel: "telegram".into(),
            chat_id: chat.into(),
            sender: "someone".into(),
            sender_id: Some(chat.into()),
            message_id: "1".into(),
            text: text.into(),
            attachments: vec![],
            reply_to: None,
            ts: 0,
        }
    }

    fn door() -> (tempfile::TempDir, SettingsDoor, Arc<AtomicUsize>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ferrule.toml");
        std::fs::write(&path, CONFIG).unwrap();
        let cfg: crate::config::Config = toml::from_str(CONFIG).unwrap();
        let hub = Arc::new(
            Hub::new(
                cfg.trust.clone(),
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
        let settings = Settings::new(
            path,
            Some(dir.path().to_path_buf()),
            Some(hub.clone()),
            Some(dir.path().to_path_buf()),
        );
        let door = SettingsDoor {
            settings,
            hub,
            retire: Arc::new(move |_| {
                count.fetch_add(1, Ordering::SeqCst);
            }),
        };
        (dir, door, retired)
    }

    fn events(door: &SettingsDoor, name: &str) -> usize {
        door.hub
            .audit()
            .read(None)
            .unwrap()
            .iter()
            .filter(|e| e.event == name)
            .count()
    }

    #[tokio::test]
    async fn only_the_owner_and_raising_a_cap_needs_confirm() {
        let (_dir, door, _) = door();
        assert!(door.intercept(&msg("42", "hello")).await.is_none());
        let no = door
            .intercept(&msg("7", "/caps usd_per_day 50"))
            .await
            .unwrap();
        assert_eq!(no, "Only the owner can change settings.");

        let list = door.intercept(&msg("42", "/caps")).await.unwrap();
        assert!(list.contains("max_usd_per_day: $5"), "{list}");

        // Raising asks, and nothing changes until it's confirmed.
        let ask = door
            .intercept(&msg("42", "/caps@ferrule_bot usd_per_day 50"))
            .await
            .unwrap();
        assert!(ask.contains("confirm"), "{ask}");
        assert_eq!(door.hub.config().max_usd_per_day, 5.0);
        assert_eq!(events(&door, "settings.caps"), 0);

        let said = door
            .intercept(&msg("42", "/caps usd_per_day 50 confirm"))
            .await
            .unwrap();
        assert!(!said.starts_with("Nothing changed"), "{said}");
        assert_eq!(door.hub.config().max_usd_per_day, 50.0);
        assert_eq!(events(&door, "settings.caps"), 1);

        // Lowering doesn't ask.
        let said = door
            .intercept(&msg("42", "/caps usd_per_day 2"))
            .await
            .unwrap();
        assert!(!said.contains("confirm"), "{said}");
        assert_eq!(door.hub.config().max_usd_per_day, 2.0);

        let bad = door
            .intercept(&msg("42", "/caps usd_per_day lots"))
            .await
            .unwrap();
        assert!(bad.starts_with("Nothing changed"), "{bad}");
    }

    #[tokio::test]
    async fn mcp_and_skills_go_off_and_on_and_hooks_point_elsewhere() {
        let (dir, door, retired) = door();
        let skill = dir.path().join(".ferrule/skills/door-skill");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: door-skill\ndescription: A test skill.\n---\nBody.\n",
        )
        .unwrap();
        let off = door.intercept(&msg("42", "/mcp off files")).await.unwrap();
        assert!(!off.starts_with("Nothing changed"), "{off}");
        let list = door.intercept(&msg("42", "/mcp")).await.unwrap();
        assert!(list.contains("files") && list.contains("— off"), "{list}");
        door.intercept(&msg("42", "/mcp on files")).await.unwrap();
        assert!(!door
            .intercept(&msg("42", "/mcp"))
            .await
            .unwrap()
            .contains("— off"));
        assert_eq!(events(&door, "settings.mcp"), 2);

        let off = door
            .intercept(&msg("42", "/skills off door-skill"))
            .await
            .unwrap();
        assert!(!off.starts_with("Nothing changed"), "{off}");
        assert_eq!(retired.load(Ordering::SeqCst), 1);
        let text = std::fs::read_to_string(dir.path().join("ferrule.toml")).unwrap();
        assert!(text.contains("door-skill"), "{text}");
        assert_eq!(events(&door, "settings.skill"), 1);

        let hooks = door.intercept(&msg("42", "/hooks trust")).await.unwrap();
        assert!(hooks.contains("ferrule hooks trust"), "{hooks}");
    }
}
