//! What a compaction summary is asked to read (M62).
//!
//! The folded head is rendered as text the summary model can actually
//! use: the system prompt is left out (it stays in the history on its
//! own), each tool call is its name and its arguments, and each tool
//! result is named and cut to a gist. One message is capped, and the
//! agent then keeps the whole rendering inside the model's window,
//! summarizing it in pieces when it does not fit.

use crate::message::{ImageRef, Message, Role};
use crate::profile::COMPACTION_TEMPLATE;

/// One message's share of the summarizer's input. OpenHands caps each
/// event it feeds its condenser at 10_000 characters
/// (`max_event_length`).
pub const SUMMARY_EVENT_CHARS: usize = 10_000;
/// One tool call's arguments, after long string values are shortened.
const SUMMARY_ARG_CHARS: usize = 2_000;
/// A single string argument (a file's contents, a pasted command)
/// before the whole arguments object is capped.
const SUMMARY_ARG_VALUE_CHARS: usize = 500;
/// A tool result the summarizer sees. An error keeps a head and a tail,
/// because the message is often the last line.
const SUMMARY_RESULT_CHARS: usize = 4_000;
/// Below this, shrinking the budget further does not help: the
/// fallback extract is used instead.
pub const MIN_SUMMARY_BUDGET_CHARS: usize = 1_024;
/// The extract that replaces a failed model summary. Smaller than a
/// head that was large enough to need compacting.
const EXTRACTIVE_CHARS: usize = 12_000;

/// The folded `messages` as the summarizer should read them. System
/// messages are skipped. Empty when the head had nothing else.
pub fn render_for_summary(messages: &[Message]) -> String {
    let mut blocks = Vec::new();
    for (i, _) in messages.iter().enumerate() {
        if let Some(block) = render_message(messages, i) {
            blocks.push(block);
        }
    }
    blocks.join("\n\n")
}

/// Characters of transcript the summary request may carry, given the
/// agent's context window. The template, the reply and a margin are
/// held back: the byte/4 estimate runs low on some texts, and a
/// compaction model smaller than this window is handled by shrinking
/// after a "prompt is too long" rather than by guessing its window.
pub fn summary_budget_chars(context_window: usize, output_reserve: usize) -> usize {
    let template_tokens = COMPACTION_TEMPLATE.len() / 4 + 48;
    let margin = (context_window / 10).min(8_192);
    let reserve = output_reserve.min(context_window / 2);
    let mut tokens = context_window
        .saturating_sub(reserve)
        .saturating_sub(margin)
        .saturating_sub(template_tokens);
    // A small window (the tests' 1_000, an 8K local model) still has to
    // fold a real head in one piece when the head is short.
    let floor = (context_window / 5).max(256);
    let cap = context_window
        .saturating_sub(reserve)
        .saturating_sub(64)
        .max(128);
    tokens = tokens.max(floor).min(cap);
    tokens.saturating_mul(4)
}

/// `text` split into pieces of at most `budget_chars`, on message
/// boundaries (`\n\n`) where a piece allows it. A single message longer
/// than the budget is cut mid-text.
pub fn chunk_transcript(text: &str, budget_chars: usize) -> Vec<String> {
    let budget = budget_chars.max(1);
    if text.is_empty() {
        return Vec::new();
    }
    if text.chars().count() <= budget {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut cur = String::new();
    let mut cur_n = 0usize;
    for piece in text.split("\n\n") {
        let n = piece.chars().count();
        let sep = usize::from(!cur.is_empty()) * 2;
        if !cur.is_empty() && cur_n + sep + n <= budget {
            cur.push_str("\n\n");
            cur.push_str(piece);
            cur_n += sep + n;
            continue;
        }
        if !cur.is_empty() {
            chunks.push(std::mem::take(&mut cur));
            cur_n = 0;
        }
        if n <= budget {
            cur = piece.to_string();
            cur_n = n;
        } else {
            let mut rest = piece;
            while !rest.is_empty() {
                let (head, tail) = split_chars(rest, budget);
                chunks.push(head.to_string());
                if tail.is_empty() {
                    break;
                }
                rest = tail;
            }
        }
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    chunks
}

/// The transcript piece one summary call reads. The first piece is the
/// chunk. A later piece carries the summary so far, shortened so the
/// chunk still fits in `budget_chars` beside it.
pub fn progressive_piece(prior: Option<&str>, chunk: &str, budget_chars: usize) -> String {
    let budget = budget_chars.max(1);
    let Some(prior) = prior.filter(|p| !p.is_empty()) else {
        return truncate_chars(chunk, budget);
    };
    let header = "Summary of the conversation so far:\n";
    let mid = "\n\nNext messages to fold in:\n";
    let overhead = header.chars().count() + mid.chars().count();
    let built = if overhead + 2 >= budget {
        format!("{header}{prior}{mid}{chunk}")
    } else {
        let room = budget - overhead;
        let prior_room = (room / 3).max(1);
        let chunk_room = room - prior_room;
        format!(
            "{header}{}\n\nNext messages to fold in:\n{}",
            truncate_chars(prior, prior_room),
            truncate_chars(chunk, chunk_room.max(1))
        )
    };
    // The headers alone can be longer than a tiny budget. The piece the
    // model is sent never exceeds the budget.
    truncate_chars(&built, budget)
}

/// A summary built with no model, from the already-rendered head. Used
/// when the summary call fails, so the history still shrinks and the run
/// continues. It says that it is not a model summary. The start and the
/// end are kept when the rendering itself is long: the request is usually
/// first, and what was just happening is last.
pub fn extractive_summary(rendered: &str, error: &str) -> String {
    let body = if rendered.chars().count() <= 8_000 {
        rendered.to_string()
    } else {
        format!(
            "{}\n…\n{}",
            take_chars(rendered, 5_000),
            last_chars(rendered, 3_000)
        )
    };
    let err = truncate_chars(&error.replace('\n', " "), 240);
    let out = format!(
        "[ferrule: the summary model failed ({err}); this is a shortened extract of the \
         folded messages, not a model summary]\n\n{body}\n"
    );
    truncate_chars(&out, EXTRACTIVE_CHARS)
}

/// The call this result answers: the nearest preceding one with its id.
/// A later call that reuses the id does not rename an earlier result.
fn tool_name(messages: &[Message], index: usize) -> &str {
    let Some(id) = messages[index].tool_call_id.as_deref() else {
        return "tool";
    };
    messages[..index]
        .iter()
        .rev()
        .find_map(|m| {
            m.tool_calls
                .iter()
                .find(|c| c.id == id)
                .map(|c| c.name.as_str())
        })
        .unwrap_or("tool")
}

fn render_message(messages: &[Message], index: usize) -> Option<String> {
    let m = &messages[index];
    let mut lines = Vec::new();
    match m.role {
        Role::System => return None,
        Role::User => {
            lines.push(format!("user: {}", cap_text(&prepared_body(m))));
        }
        Role::Assistant => {
            if let Some(body) = m.content.as_deref().filter(|s| !s.is_empty()) {
                lines.push(format!("assistant: {}", cap_text(body)));
            }
            for call in &m.tool_calls {
                lines.push(format!(
                    "assistant → {}({})",
                    call.name,
                    cap_args(&call.arguments)
                ));
            }
            if lines.is_empty() {
                lines.push("assistant:".into());
            }
        }
        Role::Tool => {
            let name = tool_name(messages, index);
            let body = prepared_body(m);
            let failed = result_failed(&body);
            let mark = if failed { " (error)" } else { "" };
            lines.push(format!(
                "tool {name}{mark}: {}",
                cap_result(&body, SUMMARY_RESULT_CHARS, failed)
            ));
        }
    }
    if !m.images.is_empty() {
        let shown: Vec<String> = m.images.iter().take(8).map(image_label).collect();
        let extra = m.images.len().saturating_sub(shown.len());
        let mut line = format!("[images: {}]", shown.join(", "));
        if extra > 0 {
            line.push_str(&format!(" (+{extra} more)"));
        }
        lines.push(line);
    }
    Some(truncate_chars(&lines.join("\n"), SUMMARY_EVENT_CHARS))
}

fn prepared_body(m: &Message) -> String {
    let body = m.content.clone().unwrap_or_default();
    if crate::agent::may_hold_skill(m) {
        crate::agent::elide_skill_blocks(&body)
    } else {
        body
    }
}

/// An error, a refusal or a call that never ran. A result dropped
/// because a later one was identical is not an error.
fn result_failed(text: &str) -> bool {
    !crate::agent::worked(text) && !text.starts_with("[superseded by identical")
}

fn cap_text(text: &str) -> String {
    truncate_chars(text, SUMMARY_EVENT_CHARS)
}

fn cap_args(value: &serde_json::Value) -> String {
    let mut v = value.clone();
    shrink_strings(&mut v, SUMMARY_ARG_VALUE_CHARS);
    truncate_chars(&v.to_string(), SUMMARY_ARG_CHARS)
}

fn shrink_strings(value: &mut serde_json::Value, max_chars: usize) {
    match value {
        serde_json::Value::String(s) => {
            if s.chars().count() > max_chars {
                *s = truncate_chars(s, max_chars);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                shrink_strings(item, max_chars);
            }
        }
        serde_json::Value::Object(map) => {
            for item in map.values_mut() {
                shrink_strings(item, max_chars);
            }
        }
        _ => {}
    }
}

fn cap_result(text: &str, max_chars: usize, error: bool) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    if !error || max_chars < 8 {
        return truncate_chars(text, max_chars);
    }
    // The useful line of a failure is often the last one.
    let head = max_chars * 2 / 3;
    let tail = max_chars - head;
    format!(
        "{}…{}",
        take_chars(text, head.saturating_sub(1)),
        last_chars(text, tail.saturating_sub(1))
    )
}

fn image_label(image: &ImageRef) -> String {
    if let Some(name) = &image.name {
        if !name.is_empty() {
            return name.clone();
        }
    }
    std::path::Path::new(&image.path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| image.path.clone())
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    if max_chars == 1 {
        return "…".into();
    }
    let mut out: String = text.chars().take(max_chars - 1).collect();
    out.push('…');
    out
}

fn take_chars(text: &str, n: usize) -> String {
    text.chars().take(n).collect()
}

fn last_chars(text: &str, n: usize) -> String {
    let count = text.chars().count();
    text.chars().skip(count.saturating_sub(n)).collect()
}

/// `(head, tail)` with `head` at most `n` characters, split on a char
/// boundary.
fn split_chars(text: &str, n: usize) -> (&str, &str) {
    if n == 0 {
        return ("", text);
    }
    for (seen, (i, _)) in text.char_indices().enumerate() {
        if seen == n {
            return (&text[..i], &text[i..]);
        }
    }
    (text, "")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ToolCall;

    fn call(name: &str, args: serde_json::Value) -> Message {
        Message::assistant(
            Some("removing it".into()),
            vec![ToolCall {
                id: "1".into(),
                name: name.into(),
                arguments: args,
            }],
            None,
        )
    }

    #[test]
    fn render_shows_the_tool_call_and_skips_the_system_prompt() {
        let messages = vec![
            Message::system("SYSTEM-PROMPT-MUST-NOT-BE-SUMMARIZED"),
            Message::user("delete the build directory"),
            call(
                "shell",
                serde_json::json!({"command": "rm -rf build_PROBE"}),
            ),
            Message::tool_result("1", "error: build_PROBE is missing"),
        ];
        let text = render_for_summary(&messages);
        assert!(text.contains("shell"));
        assert!(text.contains("rm -rf build_PROBE"));
        assert!(text.contains("build_PROBE is missing"));
        assert!(text.contains("tool shell (error):"));
        assert!(text.contains("delete the build directory"));
        assert!(!text.contains("SYSTEM-PROMPT-MUST-NOT-BE-SUMMARIZED"));
    }

    #[test]
    fn a_result_takes_the_name_of_the_call_that_precedes_it() {
        let call = |name: &str| {
            Message::assistant(
                None,
                vec![ToolCall {
                    id: "c4".into(),
                    name: name.into(),
                    arguments: serde_json::json!({}),
                }],
                None,
            )
        };
        let text = render_for_summary(&[
            call("edit"),
            Message::tool_result("c4", "wrote a.rs"),
            call("look"),
            Message::tool_result("c4", "nothing"),
        ]);
        assert!(text.contains("tool edit: wrote a.rs"), "{text}");
        assert!(text.contains("tool look: nothing"), "{text}");
    }

    #[test]
    fn a_long_tool_result_and_a_long_argument_are_capped() {
        let huge = format!("RESULT-GIST-START\n{}", "X".repeat(50_000));
        let args =
            serde_json::json!({"command": "rm -rf build_PROBE", "content": "Y".repeat(8_000)});
        let messages = vec![call("shell", args), Message::tool_result("1", huge)];
        let text = render_for_summary(&messages);
        assert!(text.contains("RESULT-GIST-START"));
        assert!(text.contains("rm -rf build_PROBE"));
        assert!(!text.contains(&"X".repeat(SUMMARY_RESULT_CHARS + 50)));
        assert!(!text.contains(&"Y".repeat(SUMMARY_ARG_VALUE_CHARS + 50)));
        assert!(text.chars().count() < SUMMARY_EVENT_CHARS * 3);
    }

    #[test]
    fn an_error_keeps_its_tail() {
        let messages = vec![
            call("shell", serde_json::json!({"command": "ls"})),
            Message::tool_result(
                "1",
                format!("error: start\n{}\nTAIL-MARKER", "x".repeat(20_000)),
            ),
        ];
        let text = render_for_summary(&messages);
        assert!(text.contains("tool shell (error):"));
        assert!(text.contains("error: start"));
        assert!(text.contains("TAIL-MARKER"), "{text}");
        assert!(!text.contains(&"x".repeat(SUMMARY_RESULT_CHARS + 50)));
    }

    #[test]
    fn chunks_follow_message_boundaries_and_a_budget() {
        let text = "user: one\n\nuser: two\n\nuser: three";
        assert_eq!(chunk_transcript(text, 10_000), vec![text.to_string()]);
        let chunks = chunk_transcript(text, 12);
        assert!(chunks.len() >= 2, "{chunks:?}");
        assert!(chunks.iter().all(|c| c.chars().count() <= 12), "{chunks:?}");
        let joined = progressive_piece(Some("earlier"), &chunks[1], 40);
        assert!(joined.contains("earlier") || joined.contains("…"));
        assert!(joined.chars().count() <= 40, "{}", joined.chars().count());
    }

    #[test]
    fn a_small_window_still_budgets_a_short_transcript() {
        let small = summary_budget_chars(1_000, 0);
        assert!(small >= 1_200, "{small}");
        let window = summary_budget_chars(8_000, 1_024);
        assert!(window / 4 + COMPACTION_TEMPLATE.len() / 4 < 8_000);
    }

    #[test]
    fn the_extract_keeps_the_request_and_the_tool_and_stays_short() {
        let messages = vec![
            Message::system("SYSTEM-PROMPT-MUST-NOT-BE-SUMMARIZED"),
            Message::user("delete the build directory"),
            call(
                "shell",
                serde_json::json!({"command": "rm -rf build_PROBE"}),
            ),
            Message::tool_result("1", format!("RESULT-GIST-START\n{}", "X".repeat(50_000))),
        ];
        let text = extractive_summary(&render_for_summary(&messages), "prompt is too long");
        assert!(text.contains("the summary model failed"));
        assert!(text.contains("delete the build directory"));
        assert!(text.contains("shell"));
        assert!(text.contains("rm -rf build_PROBE"));
        assert!(text.contains("RESULT-GIST-START"));
        assert!(!text.contains("SYSTEM-PROMPT-MUST-NOT-BE-SUMMARIZED"));
        assert!(!text.contains(&"X".repeat(SUMMARY_RESULT_CHARS + 50)));
        assert!(text.chars().count() <= EXTRACTIVE_CHARS);
    }
}
