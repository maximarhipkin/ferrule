//! `edit_file`: SEARCH/REPLACE hunks, all or nothing
//! (docs/m29-edit-mechanics.md §1).
//!
//! Each hunk's SEARCH must match exactly one place. Matching climbs a short
//! ladder — exact, then ignoring trailing whitespace, then ignoring a
//! uniform indentation offset (the replacement re-indented by the same
//! offset) — and never goes fuzzy on content: a hunk applied to the wrong
//! place is worse than one that fails. A failure says what to try next and
//! shows the closest region with line numbers; nothing is written unless
//! every hunk applied.

use crate::fs_tools::{resolve, write_atomic};
use ferrule_core::error::CoreError;
use ferrule_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// The diff in the result is cut after this many lines.
const DIFF_LINES: usize = 60;
/// Lines of the file shown around a failed hunk's closest region.
const REGION_LINES: usize = 40;

/// `edit_file`. [`EditFileTool::hiding`] adds paths it refuses even inside
/// the workspace (the sandbox's read denies, as for the other file tools).
#[derive(Default)]
pub struct EditFileTool {
    hidden: Vec<PathBuf>,
}

impl EditFileTool {
    pub fn hiding(hidden: Vec<PathBuf>) -> Self {
        Self { hidden }
    }
}

/// One SEARCH/REPLACE pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub search: String,
    pub replace: String,
}

impl Hunk {
    pub fn new(search: impl Into<String>, replace: impl Into<String>) -> Hunk {
        Hunk {
            search: search.into(),
            replace: replace.into(),
        }
    }
}

/// How a hunk matched: the rung of the ladder it needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rung {
    /// An empty SEARCH on an empty or missing file.
    Created,
    Exact,
    TrailingWhitespace,
    Indentation,
}

#[async_trait::async_trait]
impl Tool for EditFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "edit_file".into(),
            description: "Change part of an existing file with exact SEARCH/REPLACE blocks; cheaper and safer than rewriting it with write_file. \
                Each SEARCH must match exactly one place: copy it from the file, with a line or two of context if needed. \
                Edits apply in order, and nothing is written unless all of them apply. \
                An empty SEARCH on a missing file creates it. Returns a diff."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path relative to workspace (or absolute within it)" },
                    "edits": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "search": { "type": "string", "description": "Exact text to find" },
                                "replace": { "type": "string", "description": "Text to put in its place" }
                            },
                            "required": ["search", "replace"]
                        }
                    }
                },
                "required": ["path", "edits"]
            }),
        }
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        let fail = |message: String| CoreError::ToolFailed {
            tool: "edit_file".into(),
            message,
        };
        let shown = args["path"].as_str().unwrap_or("").to_string();
        if shown.is_empty() {
            return Err(fail("`path` is required".into()));
        }
        let hunks = parse_hunks(&args).map_err(fail)?;
        let path = resolve(&ctx.workspace, &self.hidden, &shown)?;
        let out = tokio::task::spawn_blocking(move || edit_path(&path, &shown, &hunks))
            .await
            .map_err(|e| fail(e.to_string()))?
            .map_err(fail)?;
        Ok(ToolOutput::capped(out, ctx.max_output_chars))
    }
}

/// `edits: [{search, replace}]`, or one `search`/`replace` at the top level
/// (models write it that way often enough; Claude Code's `old_string` /
/// `new_string` are taken too).
pub fn parse_hunks(args: &Value) -> Result<Vec<Hunk>, String> {
    let one = |v: &Value| -> Option<Hunk> {
        let search = v.get("search").or_else(|| v.get("old_string"))?.as_str()?;
        let replace = v.get("replace").or_else(|| v.get("new_string"))?.as_str()?;
        Some(Hunk::new(search, replace))
    };
    let usage = "edit_file needs `edits`: a list of {\"search\": …, \"replace\": …}";
    match args.get("edits") {
        Some(Value::Array(items)) if !items.is_empty() => items
            .iter()
            .enumerate()
            .map(|(i, v)| {
                one(v).ok_or_else(|| {
                    format!(
                        "edit {} of {} needs a string `search` and a string `replace`",
                        i + 1,
                        items.len()
                    )
                })
            })
            .collect(),
        Some(Value::Array(_)) => Err(format!("{usage}; it was empty")),
        _ => one(args).map(|h| vec![h]).ok_or_else(|| usage.to_string()),
    }
}

/// Read, apply, write. The error text is what the model sees.
fn edit_path(path: &Path, shown: &str, hunks: &[Hunk]) -> Result<String, String> {
    if path.is_dir() {
        return Err(format!("`{shown}` is a directory"));
    }
    let existing = match std::fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("can't read `{shown}`: {e}")),
    };
    match edit_bytes(existing.as_deref(), shown, hunks)? {
        Edited::Unchanged(message) => Ok(message),
        Edited::Write { bytes, summary } => {
            write_atomic(path, &bytes).map_err(|e| format!("can't write `{shown}`: {e}"))?;
            Ok(summary)
        }
    }
}

/// What [`edit_bytes`] decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edited {
    /// Nothing to write; the message is the tool's result.
    Unchanged(String),
    /// Write `bytes`, then return `summary` (the diff).
    Write { bytes: Vec<u8>, summary: String },
}

/// The whole edit without the file system: `existing` is the file's bytes
/// (`None` = it doesn't exist). Keeps the encoding, BOM and line endings.
/// The remote `edit_file` (ferrule-ssh) reads and writes over SSH and
/// shares this.
pub fn edit_bytes(existing: Option<&[u8]>, shown: &str, hunks: &[Hunk]) -> Result<Edited, String> {
    let (text, enc) = match existing {
        Some(bytes) => decode(bytes).map_err(|why| format!("`{shown}` {why}"))?,
        None => (String::new(), Encoding::Utf8 { bom: false }),
    };
    let crlf = is_crlf(&text);
    let base = if crlf {
        text.replace("\r\n", "\n")
    } else {
        text
    };
    let (new, rungs) = apply_edits(&base, existing.is_none(), shown, hunks)?;
    if new == base && existing.is_some() {
        return Ok(Edited::Unchanged(format!(
            "`{shown}` unchanged: the edits leave it as it was, so nothing was written"
        )));
    }
    let out = if crlf {
        new.replace('\n', "\r\n")
    } else {
        new.clone()
    };
    let bytes = encode(&out, enc).map_err(|c| {
        format!(
            "`{shown}` isn't UTF-8 text, so edit_file keeps its bytes as they are (one byte per character); \
             REPLACE contains `{c}`, which can't be written that way. Nothing was written. \
             Keep REPLACE to plain ASCII, or change this file with the shell tool."
        )
    })?;
    Ok(Edited::Write {
        bytes,
        summary: summary(shown, existing.is_none(), &base, &new, &rungs),
    })
}

/// Apply every hunk in order to `text` (LF line endings). `missing`: the
/// file doesn't exist yet. The error names the hunk and says nothing was
/// written.
pub fn apply_edits(
    text: &str,
    missing: bool,
    shown: &str,
    hunks: &[Hunk],
) -> Result<(String, Vec<Rung>), String> {
    let n = hunks.len();
    let mut text = text.to_string();
    let mut rungs = Vec::with_capacity(n);
    for (k, hunk) in hunks.iter().enumerate() {
        let which = format!("edit {} of {n}", k + 1);
        let search = hunk.search.replace("\r\n", "\n");
        let replace = hunk.replace.replace("\r\n", "\n");
        if search.is_empty() {
            if !text.is_empty() {
                return Err(format!(
                    "{which} failed: SEARCH is empty, which only creates a file, and `{shown}` already has content. Nothing was written.\n\
                     To add text, SEARCH for the lines next to where it goes and repeat them in REPLACE with the new text; \
                     to replace the whole file, use write_file."
                ));
            }
            text = replace;
            rungs.push(Rung::Created);
            continue;
        }
        if missing && text.is_empty() {
            return Err(format!(
                "{which} failed: `{shown}` doesn't exist. Nothing was written.\n\
                 To create it, use an empty SEARCH (or write_file); otherwise check the path with list_dir."
            ));
        }
        match apply_one(&text, &search, &replace) {
            Ok((new, rung)) => {
                text = new;
                rungs.push(rung);
            }
            Err(Miss::Ambiguous(lines)) => {
                return Err(ambiguous(&which, shown, &text, &search, &lines));
            }
            Err(Miss::NotFound) => return Err(not_found(&which, shown, &text, &search, &replace)),
        }
    }
    Ok((text, rungs))
}

#[derive(Debug)]
enum Miss {
    NotFound,
    /// 1-based line numbers where each match starts.
    Ambiguous(Vec<usize>),
}

/// One hunk: the first rung with any match decides; more than one match
/// there is ambiguous (a looser rung would only match more).
fn apply_one(text: &str, search: &str, replace: &str) -> Result<(String, Rung), Miss> {
    // Rung 1: exact, overlapping occurrences counted.
    let mut starts = Vec::new();
    let mut from = 0;
    while let Some(i) = text[from..].find(search) {
        let at = from + i;
        starts.push(at);
        from = at + text[at..].chars().next().map_or(1, char::len_utf8);
    }
    match starts.as_slice() {
        [at] => {
            let new = format!("{}{replace}{}", &text[..*at], &text[at + search.len()..]);
            return Ok((new, Rung::Exact));
        }
        [] => {}
        many => {
            return Err(Miss::Ambiguous(
                many.iter().map(|&at| line_of(text, at)).collect(),
            ))
        }
    }

    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let wanted: Vec<&str> = search
        .strip_suffix('\n')
        .unwrap_or(search)
        .split('\n')
        .collect();
    if wanted.iter().all(|l| l.trim().is_empty()) || wanted.len() > lines.len() {
        return Err(Miss::NotFound);
    }
    let body = |l: &str| l.strip_suffix('\n').unwrap_or(l).trim_end().to_string();
    let windows = 0..=lines.len() - wanted.len();

    // Rung 2: whole lines, trailing whitespace ignored.
    let found: Vec<usize> = windows
        .clone()
        .filter(|&i| {
            wanted
                .iter()
                .enumerate()
                .all(|(j, w)| body(lines[i + j]) == w.trim_end())
        })
        .collect();
    match found.as_slice() {
        [i] => {
            let new = splice(&lines, *i, wanted.len(), replace, |l| l.to_string());
            return Ok((new, Rung::TrailingWhitespace));
        }
        [] => {}
        many => return Err(Miss::Ambiguous(many.iter().map(|i| i + 1).collect())),
    }

    // Rung 3: a uniform indentation offset. SEARCH minus its common indent,
    // plus one prefix taken from the file, must equal each file line.
    let common = common_indent(&wanted);
    let rest: Vec<Option<&str>> = wanted
        .iter()
        .map(|w| {
            let w = w.trim_end();
            (!w.is_empty()).then(|| &w[common.len()..])
        })
        .collect();
    let first = rest
        .iter()
        .position(Option::is_some)
        .expect("a non-blank line");
    let mut found: Vec<(usize, String)> = Vec::new();
    for i in windows {
        let head = body(lines[i + first]);
        let Some(prefix) = head.strip_suffix(rest[first].unwrap()) else {
            continue;
        };
        if !prefix.chars().all(|c| c == ' ' || c == '\t') {
            continue;
        }
        let fits = rest.iter().enumerate().all(|(j, r)| {
            let line = body(lines[i + j]);
            match r {
                None => line.is_empty(),
                Some(r) => {
                    line.len() == prefix.len() + r.len()
                        && line.starts_with(prefix)
                        && line.ends_with(r)
                }
            }
        });
        if fits {
            found.push((i, prefix.to_string()));
        }
    }
    match found.as_slice() {
        [(i, prefix)] => {
            let reindent = |l: &str| {
                if l.trim().is_empty() {
                    String::new()
                } else if let Some(r) = l.strip_prefix(common.as_str()) {
                    format!("{prefix}{r}")
                } else {
                    format!("{prefix}{}", l.trim_start())
                }
            };
            let new = splice(&lines, *i, wanted.len(), replace, reindent);
            Ok((new, Rung::Indentation))
        }
        [] => Err(Miss::NotFound),
        many => Err(Miss::Ambiguous(many.iter().map(|(i, _)| i + 1).collect())),
    }
}

/// `lines[at..at + n]` (whole lines) replaced by `replace`, each of its
/// lines passed through `line`.
fn splice(
    lines: &[&str],
    at: usize,
    n: usize,
    replace: &str,
    line: impl Fn(&str) -> String,
) -> String {
    let mut out: String = lines[..at].concat();
    if !replace.is_empty() {
        let body = replace.strip_suffix('\n').unwrap_or(replace);
        let new: Vec<String> = body.split('\n').map(line).collect();
        out.push_str(&new.join("\n"));
        // The last matched line's newline stays; at the end of a file
        // without one, none is added.
        if lines[at + n - 1].ends_with('\n') || at + n < lines.len() {
            out.push('\n');
        }
    }
    out.push_str(&lines[at + n..].concat());
    out
}

/// The longest leading whitespace every non-blank line shares.
fn common_indent(lines: &[&str]) -> String {
    let mut common: Option<&str> = None;
    for l in lines.iter().filter(|l| !l.trim().is_empty()) {
        let indent = &l[..l.len() - l.trim_start().len()];
        common = Some(match common {
            None => indent,
            Some(c) => {
                let shared = c
                    .char_indices()
                    .zip(indent.chars())
                    .take_while(|((_, a), b)| a == b)
                    .last()
                    .map_or(0, |((i, a), _)| i + a.len_utf8());
                &c[..shared]
            }
        });
    }
    common.unwrap_or("").to_string()
}

fn line_of(text: &str, byte: usize) -> usize {
    text[..byte].matches('\n').count() + 1
}

/// `   12│text`, long lines cut.
fn numbered(lines: &[&str], from: usize, to: usize) -> String {
    (from..to)
        .map(|i| {
            let l = lines[i].strip_suffix('\n').unwrap_or(lines[i]);
            let l = l.strip_suffix('\r').unwrap_or(l);
            let l: String = if l.chars().count() > 200 {
                l.chars().take(200).chain("…".chars()).collect()
            } else {
                l.to_string()
            };
            format!("{:>6}│{l}\n", i + 1)
        })
        .collect()
}

fn ambiguous(which: &str, shown: &str, text: &str, search: &str, at: &[usize]) -> String {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut unique = at.to_vec();
    unique.dedup();
    let list = unique
        .iter()
        .map(|l| l.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let span = search.trim_end_matches('\n').split('\n').count().min(3);
    let mut out = format!(
        "{which} failed: SEARCH matches {} places in `{shown}` (starting at lines {list}). Nothing was written.\n\
         Add a line or two of the surrounding context to SEARCH so it matches exactly one place.\n",
        at.len()
    );
    for &l in unique.iter().take(4) {
        let from = l - 1;
        out.push_str(&numbered(&lines, from, (from + span).min(lines.len())));
        out.push_str("   ···\n");
    }
    out
}

fn not_found(which: &str, shown: &str, text: &str, search: &str, replace: &str) -> String {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let wanted: Vec<&str> = search.trim_end_matches('\n').split('\n').collect();
    let mut out = format!(
        "{which} failed: SEARCH matches nothing in `{shown}`, even ignoring trailing whitespace and indentation. Nothing was written.\n"
    );
    // The window of SEARCH's length holding the most of its (trimmed,
    // non-blank) lines.
    let set: HashSet<&str> = wanted
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    let hits: Vec<usize> = lines
        .iter()
        .map(|l| usize::from(set.contains(l.trim())))
        .collect();
    let n = wanted.len().clamp(1, lines.len().max(1));
    let mut best = (0, 0);
    let mut sum: usize = hits.iter().take(n).sum();
    if sum > best.0 {
        best = (sum, 0);
    }
    for i in 1..=lines.len().saturating_sub(n) {
        sum = sum + hits[i + n - 1] - hits[i - 1];
        if sum > best.0 {
            best = (sum, i);
        }
    }
    if best.0 == 0 {
        out.push_str("None of SEARCH's lines occur in the file, even trimmed: check the path, or re-read the file with read_file.\n");
    } else {
        let (score, at) = best;
        let from = at.saturating_sub(1);
        let to = (at + n + 1).min(lines.len()).min(from + REGION_LINES);
        out.push_str(&format!(
            "Closest region (lines {}–{}; {score} of {} SEARCH lines appear there):\n",
            at + 1,
            (at + n).min(lines.len()),
            set.len()
        ));
        out.push_str(&numbered(&lines, from, to));
        out.push_str(
            "Re-read those lines with read_file and copy them exactly into SEARCH, or use a shorter SEARCH that is still unique.\n",
        );
    }
    let replace = replace.trim_end_matches('\n');
    if !replace.trim().is_empty() && text.matches(replace).count() == 1 {
        out.push_str(
            "Note: REPLACE's text is already in the file, once; this edit may already have been applied.\n",
        );
    }
    out
}

fn summary(shown: &str, created: bool, old: &str, new: &str, rungs: &[Rung]) -> String {
    let diff = similar::TextDiff::from_lines(old, new);
    let (mut added, mut removed) = (0, 0);
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => added += 1,
            similar::ChangeTag::Delete => removed += 1,
            similar::ChangeTag::Equal => {}
        }
    }
    let mut out = if created {
        format!("created {shown}: {added} lines\n")
    } else {
        let n = rungs.len();
        format!(
            "edited {shown}: {n} edit{}, +{added} -{removed} lines\n",
            if n == 1 { "" } else { "s" }
        )
    };
    for (i, rung) in rungs.iter().enumerate() {
        let how = match rung {
            Rung::TrailingWhitespace => "matched ignoring trailing whitespace",
            Rung::Indentation => "matched ignoring indentation (REPLACE re-indented to match)",
            Rung::Exact | Rung::Created => continue,
        };
        out.push_str(&format!("edit {}: {how}\n", i + 1));
    }
    let text = diff
        .unified_diff()
        .context_radius(2)
        .header(&format!("a/{shown}"), &format!("b/{shown}"))
        .to_string();
    let lines: Vec<&str> = text.lines().collect();
    for l in lines.iter().take(DIFF_LINES) {
        out.push_str(l);
        out.push('\n');
    }
    if lines.len() > DIFF_LINES {
        out.push_str(&format!("… {} more diff lines\n", lines.len() - DIFF_LINES));
    }
    out
}

/// The file's encoding, kept on write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Utf8 {
        bom: bool,
    },
    Utf16 {
        big_endian: bool,
    },
    /// Not valid UTF-8: one byte per character, so every byte outside an
    /// edit is kept as it was, whatever the real encoding.
    Latin1,
}

/// The text and how to write it back. The error completes "`path` …".
pub fn decode(bytes: &[u8]) -> Result<(String, Encoding), String> {
    if let Some(rest) = bytes.strip_prefix(b"\xEF\xBB\xBF") {
        if let Ok(text) = std::str::from_utf8(rest) {
            return Ok((text.to_string(), Encoding::Utf8 { bom: true }));
        }
    }
    let utf16 = match bytes {
        [0xFF, 0xFE, rest @ ..] => Some((false, rest)),
        [0xFE, 0xFF, rest @ ..] => Some((true, rest)),
        _ => None,
    };
    if let Some((big_endian, rest)) = utf16 {
        let bad = || {
            "starts with a UTF-16 byte-order mark but isn't valid UTF-16; edit it with the shell tool".to_string()
        };
        if rest.len() % 2 != 0 {
            return Err(bad());
        }
        let units: Vec<u16> = rest
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&c| match big_endian {
                true => u16::from_be_bytes(c),
                false => u16::from_le_bytes(c),
            })
            .collect();
        let text = String::from_utf16(&units).map_err(|_| bad())?;
        return Ok((text, Encoding::Utf16 { big_endian }));
    }
    if bytes.contains(&0) {
        return Err("looks binary (it has NUL bytes); edit_file only edits text".into());
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => Ok((text.to_string(), Encoding::Utf8 { bom: false })),
        Err(_) => Ok((bytes.iter().map(|&b| b as char).collect(), Encoding::Latin1)),
    }
}

/// The bytes for `text` in `enc`; `Err(c)` for a character Latin-1 can't
/// hold.
pub fn encode(text: &str, enc: Encoding) -> Result<Vec<u8>, char> {
    Ok(match enc {
        Encoding::Utf8 { bom } => {
            let mut out = Vec::with_capacity(text.len() + 3);
            if bom {
                out.extend_from_slice(b"\xEF\xBB\xBF");
            }
            out.extend_from_slice(text.as_bytes());
            out
        }
        Encoding::Utf16 { big_endian } => {
            let mut out = match big_endian {
                true => vec![0xFE, 0xFF],
                false => vec![0xFF, 0xFE],
            };
            for unit in text.encode_utf16() {
                out.extend_from_slice(&match big_endian {
                    true => unit.to_be_bytes(),
                    false => unit.to_le_bytes(),
                });
            }
            out
        }
        Encoding::Latin1 => text
            .chars()
            .map(|c| u8::try_from(u32::from(c)).map_err(|_| c))
            .collect::<Result<_, _>>()?,
    })
}

/// Every `\n` is part of a `\r\n` (and there is at least one).
fn is_crlf(text: &str) -> bool {
    let lf = text.matches('\n').count();
    lf > 0 && text.matches("\r\n").count() == lf
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn ctx(dir: &Path) -> ToolContext {
        ToolContext {
            workspace: dir.to_path_buf(),
            max_output_chars: 30_000,
        }
    }

    fn apply(text: &str, hunks: &[(&str, &str)]) -> Result<(String, Vec<Rung>), String> {
        let hunks: Vec<Hunk> = hunks.iter().map(|(s, r)| Hunk::new(*s, *r)).collect();
        apply_edits(text, false, "f.txt", &hunks)
    }

    async fn call(dir: &Path, args: Value) -> Result<String, String> {
        EditFileTool::default()
            .call(args, &ctx(dir))
            .await
            .map(|o| o.content)
            .map_err(|e| e.to_string())
    }

    #[test]
    fn exact_match_replaces_in_place() {
        let (out, rungs) = apply("a\nb\nc\n", &[("b\n", "B\n")]).unwrap();
        assert_eq!(out, "a\nB\nc\n");
        assert_eq!(rungs, vec![Rung::Exact]);
        // Mid-line text matches exactly too.
        let (out, _) = apply("let x = foo(1);\n", &[("foo(1)", "bar(2)")]).unwrap();
        assert_eq!(out, "let x = bar(2);\n");
    }

    #[test]
    fn ambiguous_match_names_every_place_and_writes_nothing() {
        let text = "x = 1\ny = 2\nx = 1\nz = 3\nx = 1\n";
        let err = apply(text, &[("x = 1", "x = 9")]).unwrap_err();
        assert!(err.starts_with("edit 1 of 1 failed: SEARCH matches 3 places in `f.txt` (starting at lines 1, 3, 5). Nothing was written."), "{err}");
        assert!(
            err.contains("Add a line or two of the surrounding context to SEARCH"),
            "{err}"
        );
        assert!(err.contains("     3│x = 1"), "{err}");
        // Overlapping occurrences count as two.
        let err = apply("aaa\n", &[("aa", "b")]).unwrap_err();
        assert!(err.contains("matches 2 places"), "{err}");
    }

    #[test]
    fn missing_match_shows_the_closest_region_with_line_numbers() {
        let text =
            "fn a() {}\n\nfn main() {\n    let total = 1;\n    println!(\"{}\", total);\n}\n";
        let search = "fn main() {\n    let total = 2;\n    println!(\"{}\", total);\n}\n";
        let err = apply(text, &[("x", "y"), (search, "")]).unwrap_err();
        assert!(err.starts_with("edit 1 of 2 failed"), "{err}");
        let err = apply(text, &[(search, "")]).unwrap_err();
        assert!(err.contains("edit 1 of 1 failed: SEARCH matches nothing in `f.txt`, even ignoring trailing whitespace and indentation. Nothing was written."), "{err}");
        assert!(
            err.contains("Closest region (lines 3–6; 3 of 4 SEARCH lines appear there):"),
            "{err}"
        );
        assert!(err.contains("     4│    let total = 1;"), "{err}");
        assert!(err.contains("Re-read those lines with read_file"), "{err}");

        let err = apply(text, &[("nothing like it", "z")]).unwrap_err();
        assert!(
            err.contains("None of SEARCH's lines occur in the file"),
            "{err}"
        );
    }

    #[test]
    fn an_already_applied_edit_is_pointed_out() {
        let err = apply("value = 2\n", &[("value = 1", "value = 2")]).unwrap_err();
        assert!(err.contains("may already have been applied"), "{err}");
    }

    #[test]
    fn trailing_whitespace_is_forgiven() {
        let (out, rungs) = apply("a  \nb\t\nc\n", &[("a\nb\n", "A\nB\n")]).unwrap();
        assert_eq!(out, "A\nB\nc\n");
        assert_eq!(rungs, vec![Rung::TrailingWhitespace]);
    }

    #[test]
    fn indentation_is_forgiven_and_reapplied() {
        let text =
            "impl X {\n    fn f(&self) {\n        if y {\n            go();\n        }\n    }\n}\n";
        // The model quoted the block at column 0.
        let search = "if y {\n    go();\n}\n";
        let replace = "if y {\n    go();\n    again();\n}\n";
        let (out, rungs) = apply(text, &[(search, replace)]).unwrap();
        assert_eq!(rungs, vec![Rung::Indentation]);
        assert_eq!(
            out,
            "impl X {\n    fn f(&self) {\n        if y {\n            go();\n            again();\n        }\n    }\n}\n"
        );
        // Tabs in the file are what gets re-applied.
        let (out, _) = apply("fn f() {\n\tx();\n}\n", &[("x();", "y();\nz();")]).unwrap();
        assert_eq!(out, "fn f() {\n\ty();\nz();\n}\n", "exact mid-line match");
        let (out, rungs) = apply(
            "fn f() {\n\tx();\n\tw();\n}\n",
            &[("  x();\n  w();\n", "  y();\n")],
        )
        .unwrap();
        assert_eq!(rungs, vec![Rung::Indentation]);
        assert_eq!(out, "fn f() {\n\ty();\n}\n");
    }

    #[test]
    fn indentation_must_shift_uniformly_and_content_never_goes_fuzzy() {
        // One line off by a different amount: not the same block.
        let text = "    a\n      b\n";
        let err = apply(text, &[("a\nb\n", "c\n")]).unwrap_err();
        assert!(err.contains("matches nothing"), "{err}");
        // A one-character content difference never matches.
        let err = apply("let value = 1;\n", &[("let valeu = 1;", "x")]).unwrap_err();
        assert!(err.contains("matches nothing"), "{err}");
        // Ambiguous at the indentation rung is still ambiguous.
        let err = apply("  a\n    a\n", &[("a\n", "b\n")]).unwrap_err();
        assert!(err.contains("matches 2 places"), "{err}");
    }

    #[test]
    fn hunks_apply_in_order_and_all_or_nothing() {
        let (out, _) = apply("a\nb\n", &[("a", "x"), ("x\nb", "y\nz")]).unwrap();
        assert_eq!(out, "y\nz\n");
        let err = apply("a\nb\n", &[("a", "x"), ("nope", "q")]).unwrap_err();
        assert!(err.starts_with("edit 2 of 2 failed"), "{err}");
    }

    #[test]
    fn empty_search_creates_but_never_clobbers() {
        let hunks = vec![Hunk::new("", "new\n")];
        let (out, rungs) = apply_edits("", true, "n.txt", &hunks).unwrap();
        assert_eq!((out.as_str(), rungs), ("new\n", vec![Rung::Created]));
        let err = apply("old\n", &[("", "new\n")]).unwrap_err();
        assert!(
            err.contains("SEARCH is empty, which only creates a file"),
            "{err}"
        );
        let err = apply_edits("", true, "n.txt", &[Hunk::new("x", "y")]).unwrap_err();
        assert!(err.contains("`n.txt` doesn't exist"), "{err}");
    }

    #[test]
    fn deleting_lines_and_the_last_line_without_newline() {
        let (out, _) = apply("a\nb  \nc\n", &[("b\n", "")]).unwrap();
        assert_eq!(out, "a\nc\n");
        let (out, _) = apply("a\n  end", &[("end", "fin")]).unwrap();
        assert_eq!(out, "a\n  fin");
        let (out, rungs) = apply("a\n  b \n  end", &[("b\nend", "b\nfin")]).unwrap();
        assert_eq!(rungs, vec![Rung::Indentation]);
        assert_eq!(out, "a\n  b\n  fin");
    }

    #[tokio::test]
    async fn crlf_and_bom_survive_an_edit() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("w.txt");
        std::fs::write(&p, b"\xEF\xBB\xBFone\r\ntwo\r\nthree\r\n").unwrap();
        let out = call(
            dir.path(),
            json!({"path": "w.txt", "edits": [{"search": "two\n", "replace": "2\n2b\n"}]}),
        )
        .await
        .unwrap();
        assert!(
            out.starts_with("edited w.txt: 1 edit, +2 -1 lines"),
            "{out}"
        );
        assert!(out.contains("-two\n+2\n+2b"), "{out}");
        assert_eq!(
            std::fs::read(&p).unwrap(),
            b"\xEF\xBB\xBFone\r\n2\r\n2b\r\nthree\r\n"
        );
    }

    #[tokio::test]
    async fn utf16_and_non_utf8_bytes_are_kept() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("u16.txt");
        let mut bytes = vec![0xFF, 0xFE];
        for u in "héllo\r\nwörld\r\n".encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        std::fs::write(&p, &bytes).unwrap();
        call(
            dir.path(),
            json!({"path": "u16.txt", "search": "wörld", "replace": "wèlt"}),
        )
        .await
        .unwrap();
        let mut want = vec![0xFF, 0xFE];
        for u in "héllo\r\nwèlt\r\n".encode_utf16() {
            want.extend_from_slice(&u.to_le_bytes());
        }
        assert_eq!(std::fs::read(&p).unwrap(), want);

        // Latin-1 (not UTF-8): the bytes outside the edit are untouched.
        let p = dir.path().join("l1.txt");
        std::fs::write(&p, b"caf\xE9 = 1\nna\xEFve = 2\n").unwrap();
        call(
            dir.path(),
            json!({"path": "l1.txt", "search": "= 2", "replace": "= 3"}),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"caf\xE9 = 1\nna\xEFve = 3\n");
        // A character Latin-1 can't hold is refused, and nothing is written.
        let err = call(
            dir.path(),
            json!({"path": "l1.txt", "search": "= 3", "replace": "= €"}),
        )
        .await
        .unwrap_err();
        assert!(err.contains("isn't UTF-8 text"), "{err}");
        assert_eq!(std::fs::read(&p).unwrap(), b"caf\xE9 = 1\nna\xEFve = 3\n");

        std::fs::write(dir.path().join("bin"), b"a\0b").unwrap();
        let err = call(
            dir.path(),
            json!({"path": "bin", "search": "a", "replace": "c"}),
        )
        .await
        .unwrap_err();
        assert!(err.contains("looks binary"), "{err}");
    }

    #[tokio::test]
    async fn a_failed_hunk_leaves_the_file_untouched() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("a.py");
        std::fs::write(&p, "x = 1\ny = 2\n").unwrap();
        let err = call(
            dir.path(),
            json!({"path": "a.py", "edits": [
                {"search": "x = 1", "replace": "x = 10"},
                {"search": "z = 3", "replace": "z = 30"}
            ]}),
        )
        .await
        .unwrap_err();
        assert!(err.contains("edit 2 of 2 failed"), "{err}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "x = 1\ny = 2\n");
    }

    #[tokio::test]
    async fn creates_a_missing_file_with_parents() {
        let dir = TempDir::new().unwrap();
        let out = call(
            dir.path(),
            json!({"path": "src/new/mod.rs", "edits": [{"search": "", "replace": "pub fn a() {}\n"}]}),
        )
        .await
        .unwrap();
        assert!(out.starts_with("created src/new/mod.rs: 1 lines"), "{out}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/new/mod.rs")).unwrap(),
            "pub fn a() {}\n"
        );
        // No temp file left behind.
        let names: Vec<_> = std::fs::read_dir(dir.path().join("src/new"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
    }

    #[tokio::test]
    async fn escapes_and_hidden_paths_are_refused() {
        let root = TempDir::new().unwrap();
        let ws = root.path().join("ws");
        std::fs::create_dir_all(ws.join("keys")).unwrap();
        std::fs::write(root.path().join("outside.txt"), "secret").unwrap();
        std::fs::write(ws.join("keys/k"), "secret").unwrap();
        let err = call(
            &ws,
            json!({"path": "../outside.txt", "search": "secret", "replace": "x"}),
        )
        .await
        .unwrap_err();
        assert!(err.contains("escapes workspace"), "{err}");
        let abs = root.path().join("outside.txt");
        let err = call(
            &ws,
            json!({"path": abs.to_str().unwrap(), "search": "", "replace": "x"}),
        )
        .await
        .unwrap_err();
        assert!(err.contains("escapes workspace"), "{err}");
        let tool = EditFileTool::hiding(vec![ws.join("keys")]);
        let err = tool
            .call(
                json!({"path": "keys/k", "search": "secret", "replace": "x"}),
                &ctx(&ws),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("off limits"), "{err}");
        assert_eq!(
            std::fs::read_to_string(ws.join("keys/k")).unwrap(),
            "secret"
        );
        assert_eq!(std::fs::read_to_string(&abs).unwrap(), "secret");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_out_of_the_workspace_is_an_escape() {
        let root = TempDir::new().unwrap();
        let ws = root.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(root.path().join("t.txt"), "a").unwrap();
        std::os::unix::fs::symlink(root.path().join("t.txt"), ws.join("link")).unwrap();
        let err = call(&ws, json!({"path": "link", "search": "a", "replace": "b"}))
            .await
            .unwrap_err();
        assert!(err.contains("escapes workspace"), "{err}");
        assert_eq!(
            std::fs::read_to_string(root.path().join("t.txt")).unwrap(),
            "a"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn permissions_survive_the_atomic_write() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("run.sh");
        std::fs::write(&p, "echo a\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        call(
            dir.path(),
            json!({"path": "run.sh", "search": "a", "replace": "b"}),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn arguments_take_the_common_shapes() {
        assert_eq!(
            parse_hunks(&json!({"old_string": "a", "new_string": "b"})).unwrap(),
            vec![Hunk::new("a", "b")]
        );
        assert!(parse_hunks(&json!({"edits": []}))
            .unwrap_err()
            .contains("it was empty"));
        assert!(parse_hunks(&json!({"edits": [{"search": "a"}]}))
            .unwrap_err()
            .contains("edit 1 of 1 needs"));
    }

    #[test]
    fn the_tool_writes_and_is_not_read_only() {
        let t = EditFileTool::default();
        assert!(t.changes_files());
        assert!(!t.read_only());
    }
}
