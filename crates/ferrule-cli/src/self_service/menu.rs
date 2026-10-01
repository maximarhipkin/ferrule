//! The owner's chat commands as one list (M48): `/help` and Telegram's `/`
//! menu both read it, after the three commands the gateway answers itself.

use ferrule_gateway::menu::{Command, Shown, BUILT_IN};

const fn owner(
    name: &'static str,
    args: &'static str,
    menu: &'static str,
    help: &'static str,
) -> Command {
    Command {
        name,
        args,
        menu,
        help,
        shown: Shown::Private,
    }
}

pub const COMMANDS: &[Command] = &[
    Command {
        name: "stop",
        args: "",
        menu: "Stop every run now",
        help: "stop every run now; nothing new starts until /resume",
        shown: Shown::Everywhere,
    },
    owner(
        "resume",
        "",
        "Let runs start again after /stop",
        "let runs start again (owner)",
    ),
    owner(
        "plan",
        " <task>",
        "Explore first, then ask before acting",
        "explore read-only, then ask before running the plan",
    ),
    owner(
        "undo",
        "",
        "Undo my last commit",
        "revert the agent's last commit (owner)",
    ),
    owner(
        "model",
        "",
        "Show or switch the model",
        "show or switch the model",
    ),
    owner(
        "update",
        "",
        "Check for a new Ferrule and install it",
        "check for a new Ferrule and install it, after you approve (owner)",
    ),
    owner(
        "restart",
        "",
        "Restart Ferrule",
        "restart Ferrule; I tell you when it's back (owner)",
    ),
    owner(
        "doctor",
        "",
        "Check my setup and offer fixes",
        "check the setup and offer fixes (owner)",
    ),
    owner(
        "login",
        "",
        "Sign in to a ChatGPT plan",
        "sign in to a plan",
    ),
    owner("logout", "", "Sign out of a plan", "sign out of a plan"),
    owner("connect", "", "Connect a service", "connect a service"),
    owner(
        "connections",
        "",
        "Connected services",
        "connected services",
    ),
    owner(
        "skills",
        "",
        "Skills: list, turn on or off",
        "installed skills; turn one on or off",
    ),
    owner(
        "mcp",
        "",
        "MCP servers: list, turn on or off",
        "MCP servers; turn one on or off",
    ),
    owner("hooks", "", "Workspace hooks", "the workspace's hooks"),
    owner("caps", "", "Spending caps", "spending caps"),
    owner(
        "dashboard",
        "",
        "A link to the dashboard",
        "a link to the dashboard (owner's private chat)",
    ),
];

/// Everything `/help` and the menu list; without `dashboard` when the page
/// is off.
pub fn all(dashboard: bool) -> Vec<Command> {
    BUILT_IN
        .iter()
        .chain(COMMANDS)
        .filter(|c| dashboard || c.name != "dashboard")
        .copied()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrule_gateway::menu::{help_text, telegram_scopes};

    #[test]
    fn help_and_the_telegram_menu_are_one_list() {
        let cmds = all(true);
        let help = help_text(&cmds, true);
        let in_help: Vec<&str> = help
            .lines()
            .filter_map(|l| l.strip_prefix('/'))
            .filter_map(|l| l.split(' ').next())
            .collect();
        let scopes = telegram_scopes(&cmds, Some(42));
        let private = scopes
            .iter()
            .find(|s| s["scope"]["type"] == "all_private_chats")
            .unwrap();
        let in_menu: Vec<&str> = private["commands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["command"].as_str().unwrap())
            .collect();
        assert_eq!(in_help, in_menu);
        assert_eq!(in_menu.len(), 20);
        assert!(in_menu.contains(&"dashboard"), "/dashboard is in the menu");
        assert!(!all(false).iter().any(|c| c.name == "dashboard"));
    }

    #[test]
    fn every_menu_name_is_a_valid_telegram_command() {
        for c in all(true) {
            assert!(
                (1..=32).contains(&c.name.len())
                    && c.name
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{}",
                c.name
            );
            assert!((1..=256).contains(&c.menu.chars().count()), "{}", c.name);
            assert!(!c.help.is_empty(), "{}", c.name);
        }
    }

    #[test]
    fn every_command_in_the_list_has_a_door() {
        use crate::{
            connections::ConnectionsDoor, dashboard::door::DashboardDoor, models::ModelDoor,
            settings_door::SettingsDoor, subscription::login::PlanDoor, trust::OwnerDoor,
        };
        let doors: Vec<&str> = [
            OwnerDoor::HANDLES,
            DashboardDoor::HANDLES,
            ModelDoor::HANDLES,
            PlanDoor::HANDLES,
            ConnectionsDoor::HANDLES,
            SettingsDoor::HANDLES,
            super::super::SelfServiceDoor::HANDLES,
        ]
        .concat();
        for c in all(true) {
            if matches!(c.name, "new" | "status" | "help") {
                continue; // the gateway answers these itself
            }
            assert!(doors.contains(&c.name), "/{} has no door", c.name);
        }
    }
}
