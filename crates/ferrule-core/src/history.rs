//! `search_history`: the agent's way back into its own session after
//! compaction folded it away. Every message the model saw is still in the
//! session's append-only transcript; this tool searches it by words, and
//! fetches a shortened tool result back in full by its reference.
//!
//! It reads only its own session's file (there is no path or session
//! parameter), streams it line by line, and never returns the results of
//! earlier `search_history` calls, so repeated searches don't echo.

use crate::error::CoreError;
use crate::message::{Message, Role};
use crate::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use crate::transcript::Transcript;
use serde_json::json;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

pub const SEARCH_HISTORY: &str = "search_history";

/// A transcript line longer than this is skipped, not parsed.
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;
const SNIPPET_CHARS: usize = 400;
const DEFAULT_LIMIT: usize = 8;
const MAX_LIMIT: usize = 20;

/// The reference of a tool result: `r` + 16 hex digits of the FNV-1a-64
/// hash of its text. Content-addressed, so it survives renumbering and
/// providers that reuse tool-call ids.
pub fn result_ref(text: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("r{h:016x}")
}

/// One message of the session, as `search_history` sees it.
struct Entry {
    /// 1-based position among the session's message records.
    n: usize,
    message: Message,
    /// For a tool result: the name of the tool that produced it.
    tool: Option<String>,
}

pub struct SearchHistoryTool {
    path: PathBuf,
}

impl SearchHistoryTool {
    /// Searches the session `transcript` writes to.
    pub fn new(transcript: &Transcript) -> Self {
        Self {
            path: transcript.path().to_path_buf(),
        }
    }

    fn search(&self, query: &str, limit: usize) -> Result<String, CoreError> {
        let words: Vec<String> = {
            let mut w: Vec<String> = Vec::new();
            for t in query.split_whitespace().map(str::to_lowercase) {
                if !w.contains(&t) {
                    w.push(t);
                }
            }
            w
        };
        if words.is_empty() {
            return Ok("search_history needs a non-empty query".into());
        }
        let mut hits: Vec<(usize, Entry, usize)> = Vec::new(); // (matched, entry, byte pos)
        for_each_entry(&self.path, |e| {
            if e.tool.as_deref() == Some(SEARCH_HISTORY) {
                return;
            }
            let Some(text) = e.message.content.as_deref() else {
                return;
            };
            let lower = text.to_lowercase();
            let mut matched = 0;
            let mut first: Option<usize> = None;
            for w in &words {
                if let Some(p) = lower.find(w.as_str()) {
                    matched += 1;
                    first = Some(first.map_or(p, |f: usize| f.min(p)));
                }
            }
            if matched > 0 {
                let char_pos = lower[..first.unwrap_or(0)].chars().count();
                hits.push((matched, e, char_pos));
            }
        })?;
        if hits.is_empty() {
            return Ok(format!(
                "no message in this session's history matches {query:?}"
            ));
        }
        let total = hits.len();
        hits.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.n.cmp(&a.1.n)));
        hits.truncate(limit);
        let mut out = format!(
            "{} of {total} matching messages (most query words first, then newest):\n",
            hits.len()
        );
        for (_, e, pos) in &hits {
            let role = format!("{:?}", e.message.role).to_lowercase();
            let who = match &e.tool {
                Some(t) => format!("{role} ({t})"),
                None => role,
            };
            let text = e.message.content.as_deref().unwrap_or("");
            out.push_str(&format!(
                "\n[message {} · {who}] {}\n",
                e.n,
                snippet(text, *pos)
            ));
            if e.message.role == Role::Tool {
                out.push_str(&format!(
                    "  full text ({} chars): search_history {{\"ref\": \"{}\"}}\n",
                    text.chars().count(),
                    result_ref(text)
                ));
            }
        }
        Ok(out)
    }

    fn fetch(&self, reference: &str, offset: usize, page: usize) -> Result<String, CoreError> {
        let mut found: Option<Entry> = None;
        for_each_entry(&self.path, |e| {
            if e.message.role == Role::Tool
                && e.message.content.as_deref().map(result_ref).as_deref() == Some(reference)
            {
                found = Some(e);
            }
        })?;
        let Some(e) = found else {
            return Ok(format!(
                "no tool result with ref {reference} in this session's history; \
                 try search_history {{\"query\": …}} with words from its preview"
            ));
        };
        let text = e.message.content.unwrap_or_default();
        let total = text.chars().count();
        if offset >= total {
            return Ok(format!(
                "ref {reference} has {total} chars; offset {offset} is past the end"
            ));
        }
        let body: String = text.chars().skip(offset).take(page).collect();
        let end = offset + body.chars().count();
        let tool = e.tool.as_deref().unwrap_or("tool");
        let mut out = format!(
            "[{reference} · message {} · {tool} · {total} chars · showing {offset}–{end}]\n{body}",
            e.n
        );
        if end < total {
            out.push_str(&format!(
                "\n[more: search_history {{\"ref\": \"{reference}\", \"offset\": {end}}}]"
            ));
        }
        Ok(out)
    }
}

/// 400 characters around char position `pos`.
fn snippet(text: &str, pos: usize) -> String {
    let total = text.chars().count();
    let start = pos.saturating_sub(SNIPPET_CHARS / 4);
    let end = (start + SNIPPET_CHARS).min(total);
    let start = end.saturating_sub(SNIPPET_CHARS).min(start);
    let body: String = text
        .chars()
        .skip(start)
        .take(end - start)
        .map(|c| if c == '\n' { ' ' } else { c })
        .collect();
    format!(
        "{}{body}{}",
        if start > 0 { "…" } else { "" },
        if end < total { "…" } else { "" }
    )
}

/// Streams the transcript's message records into `f`, with each tool
/// result labelled by the tool that produced it.
fn for_each_entry(path: &Path, mut f: impl FnMut(Entry)) -> Result<(), CoreError> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let mut reader = BufReader::new(file);
    let mut names: HashMap<String, String> = HashMap::new();
    let mut n = 0;
    let mut line = Vec::new();
    loop {
        line.clear();
        match read_bounded_line(&mut reader, &mut line)? {
            None => break,
            Some(false) => continue, // too long: skipped
            Some(true) => {}
        }
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&line) else {
            continue; // a torn last line
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("message") {
            continue;
        }
        let Ok(message) = serde_json::from_value::<Message>(v["message"].clone()) else {
            continue;
        };
        n += 1;
        for call in &message.tool_calls {
            names.insert(call.id.clone(), call.name.clone());
        }
        let tool = match (&message.role, &message.tool_call_id) {
            (Role::Tool, Some(id)) => names.get(id).cloned(),
            _ => None,
        };
        f(Entry { n, message, tool });
    }
    Ok(())
}

/// Reads one line into `buf`. `None` at EOF; `Some(false)` when the line
/// was longer than [`MAX_LINE_BYTES`] and was skipped without buffering it.
fn read_bounded_line(r: &mut impl BufRead, buf: &mut Vec<u8>) -> std::io::Result<Option<bool>> {
    let mut too_long = false;
    let mut any = false;
    loop {
        let chunk = r.fill_buf()?;
        if chunk.is_empty() {
            return Ok(if any { Some(!too_long) } else { None });
        }
        any = true;
        let (take, done) = match chunk.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (chunk.len(), false),
        };
        if !too_long {
            buf.extend_from_slice(&chunk[..take]);
            if buf.len() > MAX_LINE_BYTES {
                too_long = true;
                buf.clear();
            }
        }
        r.consume(take);
        if done {
            return Ok(Some(!too_long));
        }
    }
}

#[async_trait::async_trait]
impl Tool for SearchHistoryTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: SEARCH_HISTORY.into(),
            description:
                "Search this session's full history, including the parts that were \
                compacted out of your context. {\"query\": words} finds the messages containing \
                them (user, assistant and tool results), newest first. {\"ref\": \"r…\"} \
                returns a shortened tool result in full; add \"offset\" to page through a long one."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Words to look for (case-insensitive)."},
                    "limit": {"type": "integer", "description": "Max hits (default 8, max 20)."},
                    "ref": {"type": "string", "description": "A tool result's reference, as given in a shortened result or a search hit."},
                    "offset": {"type": "integer", "description": "With ref: the character to start from."}
                }
            }),
        }
    }

    async fn call(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, CoreError> {
        let reference = args["ref"].as_str().map(str::trim).map(str::to_string);
        let query = args["query"].as_str().unwrap_or("").to_string();
        let limit = args["limit"]
            .as_u64()
            .map(|l| (l as usize).clamp(1, MAX_LIMIT))
            .unwrap_or(DEFAULT_LIMIT);
        let offset = args["offset"].as_u64().unwrap_or(0) as usize;
        // Leave room for the header and the "more" line.
        let page = ctx.max_output_chars.saturating_sub(300).max(1_000);
        let this = SearchHistoryTool {
            path: self.path.clone(),
        };
        let text = tokio::task::spawn_blocking(move || match reference {
            Some(r) if !r.is_empty() => this.fetch(&r, offset, page),
            _ => this.search(&query, limit),
        })
        .await
        .map_err(|e| CoreError::ToolFailed {
            tool: SEARCH_HISTORY.into(),
            message: e.to_string(),
        })??;
        Ok(ToolOutput::ok(text))
    }

    fn changes_files(&self) -> bool {
        false
    }
    fn read_only(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ToolCall;

    fn call(id: &str, name: &str) -> Message {
        Message::assistant(
            None,
            vec![ToolCall {
                id: id.into(),
                name: name.into(),
                arguments: json!({}),
            }],
            None,
        )
    }

    fn session() -> (tempfile::TempDir, Transcript) {
        let dir = tempfile::tempdir().unwrap();
        let t = Transcript::create(dir.path(), "s").unwrap();
        t.log_message(&Message::user("find the staging port"))
            .unwrap();
        t.log_message(&call("1", "read_file")).unwrap();
        t.log_message(&Message::tool_result(
            "1",
            "config: staging port = 5781\nprod port = 443",
        ))
        .unwrap();
        t.log_message(&call("2", SEARCH_HISTORY)).unwrap();
        t.log_message(&Message::tool_result("2", "[message 3] staging port 5781"))
            .unwrap();
        t.log_event("compacted: …").unwrap();
        t.log_message(&Message::assistant(
            Some("staging is on 5781".into()),
            vec![],
            None,
        ))
        .unwrap();
        (dir, t)
    }

    fn ctx() -> ToolContext {
        ToolContext {
            workspace: std::env::temp_dir(),
            max_output_chars: 2_000,
        }
    }

    #[tokio::test]
    async fn query_finds_messages_and_skips_its_own_results() {
        let (_d, t) = session();
        let tool = SearchHistoryTool::new(&t);
        let out = tool
            .call(json!({"query": "staging 5781"}), &ctx())
            .await
            .unwrap()
            .content;
        assert!(out.starts_with("3 of 3 matching"), "{out}");
        assert!(out.contains("[message 3 · tool (read_file)]"), "{out}");
        assert!(out.contains("[message 6 · assistant]"), "{out}");
        assert!(!out.contains("[message 5"), "{out}");
        let r = result_ref("config: staging port = 5781\nprod port = 443");
        assert!(out.contains(&r), "{out}");
        // Newest first among messages matching both words.
        assert!(out.find("message 6").unwrap() < out.find("message 3").unwrap());

        let none = tool
            .call(json!({"query": "kubernetes"}), &ctx())
            .await
            .unwrap();
        assert!(none.content.starts_with("no message"));
    }

    #[tokio::test]
    async fn ref_fetches_the_full_text_in_pages() {
        let dir = tempfile::tempdir().unwrap();
        let t = Transcript::create(dir.path(), "s").unwrap();
        let big: String = (0..3_000).map(|i| format!("{i:05}\n")).collect(); // 18k chars
        t.log_message(&call("a", "run_shell")).unwrap();
        t.log_message(&Message::tool_result("a", big.clone()))
            .unwrap();
        let tool = SearchHistoryTool::new(&t);
        let r = result_ref(&big);
        let mut fetched = String::new();
        let mut offset = 0;
        for _ in 0..50 {
            let out = tool
                .call(json!({"ref": r, "offset": offset}), &ctx())
                .await
                .unwrap()
                .content;
            assert!(out.len() <= 2_000 + 200, "page too long: {}", out.len());
            let (head, rest) = out.split_once('\n').unwrap();
            assert!(
                head.contains("run_shell") && head.contains("18000 chars"),
                "{head}"
            );
            match rest.rsplit_once("\n[more: ") {
                Some((body, more)) => {
                    fetched.push_str(body);
                    offset = more
                        .split("\"offset\": ")
                        .nth(1)
                        .unwrap()
                        .trim_end_matches("}]")
                        .parse()
                        .unwrap();
                }
                None => {
                    fetched.push_str(rest);
                    break;
                }
            }
        }
        assert_eq!(fetched, big);
        let missing = tool
            .call(json!({"ref": "r0000000000000000"}), &ctx())
            .await
            .unwrap();
        assert!(missing.content.starts_with("no tool result with ref"));
    }

    #[test]
    fn overlong_lines_and_torn_lines_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let t = Transcript::create(dir.path(), "s").unwrap();
        t.log_message(&Message::user(format!(
            "needle {}",
            "x".repeat(MAX_LINE_BYTES)
        )))
        .unwrap();
        t.log_message(&Message::user("needle small")).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(t.path())
            .and_then(|mut f| std::io::Write::write_all(&mut f, b"{\"type\":\"mess"))
            .unwrap();
        let tool = SearchHistoryTool::new(&t);
        let out = tool.search("needle", 8).unwrap();
        assert!(out.starts_with("1 of 1"), "{out}");
        assert!(out.contains("[message 1 · user] needle small"), "{out}");
    }

    #[test]
    fn refs_are_stable_and_distinct() {
        assert_eq!(result_ref("abc"), result_ref("abc"));
        assert_ne!(result_ref("abc"), result_ref("abd"));
        assert_eq!(result_ref("").len(), 17);
    }
}
