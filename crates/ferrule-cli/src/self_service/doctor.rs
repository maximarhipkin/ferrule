//! Doctor's fixes as cards: each fix the dashboard offers that is an op
//! here is asked on its own, one at a time.

use super::Op;
use serde_json::Value;

/// The fixes asked after `/doctor` in a chat, at most.
pub const MOST_FIXES: usize = 3;

/// The page's fix as an op, when it is one.
pub fn fix_op(action: &str, body: &Value) -> Option<Op> {
    Some(match action {
        "channels/restart" => Op::ChannelRestart {
            name: body["name"].as_str()?.to_string(),
        },
        "config/restore" => Op::ConfigRestore,
        "gateway/restart" => Op::Restart,
        "console/run" if body["line"].as_str() == Some("update --check") => Op::UpdateCheck,
        _ => return None,
    })
}

fn section(key: &str) -> String {
    let mut c = key.chars();
    let name = c.next().map_or(String::new(), |f| {
        f.to_uppercase().collect::<String>() + c.as_str()
    });
    format!("the dashboard's {name} page")
}

/// The warn and fail lines, with a pointer to the page where a fix is
/// only a page; and the distinct fixes that are change ops.
pub fn read(report: &Value) -> (Vec<String>, Vec<Op>) {
    let mut lines = Vec::new();
    let mut ops: Vec<Op> = Vec::new();
    for item in report["items"].as_array().into_iter().flatten() {
        let level = item["level"].as_str().unwrap_or("");
        if !matches!(level, "warn" | "fail") {
            continue;
        }
        let mut line = format!(
            "{level}: {} — {}",
            item["what"].as_str().unwrap_or(""),
            item["detail"].as_str().unwrap_or("")
        );
        for fix in item["fixes"].as_array().into_iter().flatten() {
            if let Some(key) = fix["section"].as_str() {
                line.push_str(&format!(" ({})", section(key)));
            } else if let Some(op) = fix["action"]
                .as_str()
                .and_then(|a| fix_op(a, &fix["body"]))
                .filter(Op::is_change)
            {
                if !ops.contains(&op) {
                    ops.push(op);
                }
            }
        }
        lines.push(line);
    }
    (lines, ops)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn doctor_fixes_map_to_ops() {
        assert_eq!(
            fix_op("channels/restart", &json!({"name": "telegram"})),
            Some(Op::ChannelRestart {
                name: "telegram".into()
            })
        );
        assert_eq!(
            fix_op("config/restore", &json!({})),
            Some(Op::ConfigRestore)
        );
        assert_eq!(fix_op("gateway/restart", &json!({})), Some(Op::Restart));
        assert_eq!(
            fix_op("console/run", &json!({"line": "update --check"})),
            Some(Op::UpdateCheck)
        );
        assert_eq!(fix_op("console/run", &json!({"line": "doctor"})), None);
        assert_eq!(fix_op("kill/off", &json!({})), None);
        assert_eq!(fix_op("channels/restart", &json!({})), None);

        let report = json!({"items": [
            {"level": "ok", "what": "x", "detail": "fine"},
            {"level": "fail", "what": "telegram", "detail": "down", "fixes": [
                {"label": "Restart telegram", "action": "channels/restart", "body": {"name": "telegram"}}]},
            {"level": "warn", "what": "models", "detail": "none works", "fixes": [
                {"label": "Open Models", "section": "models"}]},
            {"level": "warn", "what": "updates", "detail": "old", "fixes": [
                {"label": "Check", "action": "console/run", "body": {"line": "update --check"}}]},
        ]});
        let (lines, ops) = read(&report);
        assert_eq!(lines.len(), 3);
        assert!(
            lines[1].ends_with("(the dashboard's Models page)"),
            "{lines:?}"
        );
        assert_eq!(
            ops,
            vec![Op::ChannelRestart {
                name: "telegram".into()
            }]
        );
    }
}
