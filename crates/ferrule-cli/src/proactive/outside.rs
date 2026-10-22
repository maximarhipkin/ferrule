//! M64: what the pass reads from connected calendars and mailboxes
//! (docs/m64-proactive.md §3): the native connections' read tools, called
//! in-process, never an agent turn. Outside content: clipped, scrubbed of
//! every known secret, and fenced as data in the pass's prompt.

use crate::config::Config;
use chrono::{Duration, Utc};
use chrono_tz::Tz;
use serde_json::{json, Value};

/// How much of one connection's answer the pass reads.
const CHARS: usize = 2_000;
/// How long one connection gets to answer.
const WAIT: std::time::Duration = std::time::Duration::from_secs(20);

/// What one kind of source gave: the text (`None`: nothing to read), and
/// what happened, for the run log.
pub struct Read {
    pub text: Option<String>,
    pub status: String,
}

/// The next 24 hours of every connected calendar.
pub async fn calendar(cfg: &Config, tz: Tz) -> Read {
    let now = Utc::now();
    let today = now.with_timezone(&tz).date_naive();
    let tomorrow = today.succ_opt().unwrap_or(today);
    read(
        cfg,
        "calendar",
        &[
            (
                "events_list",
                json!({"from": today.to_string(), "to": tomorrow.to_string(), "limit": 20}),
            ),
            (
                "calendar_list",
                json!({"from": now.to_rfc3339(), "to": (now + Duration::hours(24)).to_rfc3339(), "limit": 20}),
            ),
        ],
    )
    .await
}

/// Unread mail from the last day: the search's list (sender, subject,
/// date), never a message's body.
pub async fn mail(cfg: &Config, tz: Tz) -> Read {
    let yesterday = (Utc::now() - Duration::hours(24))
        .with_timezone(&tz)
        .date_naive();
    read(
        cfg,
        "mailbox",
        &[
            (
                "gmail_search",
                json!({"query": "is:unread in:inbox newer_than:1d", "limit": 10}),
            ),
            (
                "mail_search",
                json!({"unseen": true, "since": yesterday.to_string(), "limit": 10}),
            ),
        ],
    )
    .await
}

async fn read(cfg: &Config, what: &str, wanted: &[(&str, Value)]) -> Read {
    let Some(conns) = crate::connections::shared(cfg) else {
        return Read {
            text: None,
            status: "connections are off on this machine".into(),
        };
    };
    let redactor = crate::health::redactor(cfg);
    let (mut parts, mut failed) = (Vec::new(), Vec::new());
    for server in conns.servers() {
        let Some(local) = &server.local else { continue };
        let tools: Vec<String> = local
            .0
            .tools()
            .iter()
            .filter_map(|t| t["name"].as_str().map(str::to_string))
            .collect();
        for (tool, args) in wanted {
            if !tools.iter().any(|t| t == tool) {
                continue;
            }
            match tokio::time::timeout(WAIT, local.0.call(tool, args.clone())).await {
                Ok(Ok(text)) => parts.push(format!(
                    "[{}]\n{}",
                    server.name,
                    ferrule_gateway::health::clip(&redactor.redact(&text), CHARS)
                )),
                Ok(Err(e)) => failed.push(format!(
                    "{}: {}",
                    server.name,
                    ferrule_gateway::health::clip(&redactor.redact(&e), 160)
                )),
                Err(_) => failed.push(format!("{}: no answer in 20 s", server.name)),
            }
        }
    }
    let status = match (parts.len(), failed.is_empty()) {
        (0, true) => format!("no {what} connected"),
        (n, true) => format!("{n} {what}(s) read"),
        (n, false) => format!("{n} {what}(s) read; failed: {}", failed.join("; ")),
    };
    Read {
        text: (!parts.is_empty()).then(|| parts.join("\n\n")),
        status,
    }
}
