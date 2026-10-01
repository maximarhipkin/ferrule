//! The chat commands, listed once (M48): `/help` and Telegram's `/` menu
//! both read this list, so a command can't be in one and not the other.

use serde_json::{json, Value};

/// Where a command is offered in Telegram's menu.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Shown {
    /// In every chat, groups included.
    Everywhere,
    /// Only in a private chat (the owner's commands).
    Private,
}

#[derive(Clone, Copy, Debug)]
pub struct Command {
    /// Without the slash; also Telegram's own name for it.
    pub name: &'static str,
    /// What follows the name in `/help`, with its leading space.
    pub args: &'static str,
    /// The line under Telegram's `/` button.
    pub menu: &'static str,
    /// The line in `/help`.
    pub help: &'static str,
    pub shown: Shown,
}

/// The three the gateway answers itself.
pub const BUILT_IN: &[Command] = &[
    Command {
        name: "new",
        args: "",
        menu: "Start a fresh conversation (the old one is saved)",
        help: "start a fresh conversation; the old one is saved and memory stays (also /reset)",
        shown: Shown::Everywhere,
    },
    Command {
        name: "status",
        args: "",
        menu: "What I'm doing right now",
        help: "what I'm doing right now",
        shown: Shown::Everywhere,
    },
    Command {
        name: "help",
        args: "",
        menu: "The commands",
        help: "this list",
        shown: Shown::Everywhere,
    },
];

/// `/help`'s answer. `/status` is only there when something answers it.
pub fn help_text(cmds: &[Command], has_status: bool) -> String {
    let mut text = String::from("Commands:");
    for c in cmds.iter().filter(|c| has_status || c.name != "status") {
        text.push_str(&format!("\n/{}{} — {}", c.name, c.args, c.help));
    }
    text.push_str("\n\nAnything else is a message for me.");
    text
}

fn commands(cmds: &[Command], all: bool) -> Vec<Value> {
    cmds.iter()
        .filter(|c| all || c.shown == Shown::Everywhere)
        .map(|c| json!({"command": c.name, "description": c.menu}))
        .collect()
}

/// The `setMyCommands` bodies, one per scope: groups and the default get
/// the short list, private chats and the owner's own chat the full one
/// (a chat scope outranks the others, so the owner's chat gets it even if
/// private-chat scopes were ever cleared).
pub fn telegram_scopes(cmds: &[Command], owner: Option<i64>) -> Vec<Value> {
    let mut out = vec![
        json!({"commands": commands(cmds, false), "scope": {"type": "default"}}),
        json!({"commands": commands(cmds, false), "scope": {"type": "all_group_chats"}}),
        json!({"commands": commands(cmds, true), "scope": {"type": "all_private_chats"}}),
    ];
    if let Some(chat) = owner.filter(|c| *c > 0) {
        out.push(json!({
            "commands": commands(cmds, true),
            "scope": {"type": "chat", "chat_id": chat},
        }));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list() -> Vec<Command> {
        let mut v = BUILT_IN.to_vec();
        v.push(Command {
            name: "stop",
            args: "",
            menu: "Stop every run now",
            help: "stop every run now; nothing new starts until /resume",
            shown: Shown::Everywhere,
        });
        v.push(Command {
            name: "plan",
            args: " <task>",
            menu: "Explore first",
            help: "explore read-only",
            shown: Shown::Private,
        });
        v
    }

    #[test]
    fn help_text_keeps_todays_format() {
        let text = help_text(&list(), true);
        assert!(text.starts_with("Commands:\n/new — start a fresh conversation"));
        assert!(text.contains("\n/status — what I'm doing right now\n/help — this list"));
        assert!(text.contains("\n/plan <task> — explore read-only"));
        assert!(text.ends_with("\n\nAnything else is a message for me."));
        assert!(!help_text(&list(), false).contains("/status"));
    }

    #[test]
    fn scopes_put_the_full_list_in_private_chats() {
        let scopes = telegram_scopes(&list(), Some(42));
        let kinds: Vec<&str> = scopes
            .iter()
            .map(|s| s["scope"]["type"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            ["default", "all_group_chats", "all_private_chats", "chat"]
        );
        assert_eq!(scopes[3]["scope"]["chat_id"], 42);
        let names = |i: usize| -> Vec<String> {
            scopes[i]["commands"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| c["command"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(names(0), ["new", "status", "help", "stop"]);
        assert_eq!(names(1), names(0));
        assert_eq!(names(2), ["new", "status", "help", "stop", "plan"]);
        assert_eq!(names(3), names(2));
        // No owner (or a group id): no chat scope.
        assert_eq!(telegram_scopes(&list(), None).len(), 3);
        assert_eq!(telegram_scopes(&list(), Some(-100)).len(), 3);
    }
}
