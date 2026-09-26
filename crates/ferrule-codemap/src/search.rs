//! `code_search`: definitions and references from the tags, and a plain
//! text search for files no grammar covers.

use crate::lang;
use crate::repo::{extension, CodeMap, Snapshot, MAX_FILE_BYTES};
use ferrule_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use ferrule_core::CoreError;
use serde_json::{json, Value};
use std::path::{Component, Path};
use std::sync::Arc;

pub const DEFAULT_MAX_RESULTS: usize = 50;

pub struct CodeSearchTool {
    map: Arc<CodeMap>,
}

impl CodeSearchTool {
    pub fn new(map: Arc<CodeMap>) -> Self {
        Self { map }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SearchKind {
    Definitions,
    References,
    All,
}

fn fail(message: impl Into<String>) -> CoreError {
    CoreError::ToolFailed {
        tool: "code_search".into(),
        message: message.into(),
    }
}

#[async_trait::async_trait]
impl Tool for CodeSearchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "code_search".into(),
            description: "Find where a symbol is defined and used: path:line for each hit, \
                definitions first. Understands Rust, Python, TypeScript/JavaScript, Go and Java; \
                other files get a whole-word text search."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "A symbol name (`Agent`, `run_inner`, `Agent::run`), or any text"},
                    "kind": {"type": "string", "enum": ["definitions", "references", "all"], "description": "Default all"},
                    "path": {"type": "string", "description": "Only under this workspace-relative dir or file"},
                    "max_results": {"type": "integer", "description": "Default 50"}
                },
                "required": ["query"]
            }),
        }
    }

    fn changes_files(&self) -> bool {
        false
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        let query = args["query"]
            .as_str()
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .ok_or_else(|| fail("`query` is required"))?
            .to_string();
        let kind = match args["kind"].as_str().unwrap_or("all") {
            "definitions" | "definition" | "defs" => SearchKind::Definitions,
            "references" | "reference" | "refs" => SearchKind::References,
            "all" | "" => SearchKind::All,
            other => {
                return Err(fail(format!(
                    "unknown kind `{other}`: use definitions, references or all"
                )))
            }
        };
        let scope = scope_of(args["path"].as_str().unwrap_or(""))?;
        let max = args["max_results"]
            .as_u64()
            .map_or(DEFAULT_MAX_RESULTS, |n| n.clamp(1, 1000) as usize);
        let map = self.map.clone();
        let out = tokio::task::spawn_blocking(move || {
            let snapshot = map.refresh();
            search(map.root(), &snapshot, &query, kind, &scope, max)
        })
        .await
        .map_err(|e| fail(format!("search failed: {e}")))?;
        Ok(ToolOutput::capped(out, ctx.max_output_chars))
    }
}

/// `path` as a relative, `/`-separated prefix; nothing outside the
/// workspace.
fn scope_of(path: &str) -> Result<String, CoreError> {
    let path = path.trim().trim_start_matches("./");
    let mut parts = Vec::new();
    for c in Path::new(path).components() {
        match c {
            Component::Normal(p) => parts.push(p.to_string_lossy().into_owned()),
            Component::CurDir => {}
            _ => {
                return Err(fail(format!(
                    "path `{path}` must be relative to the workspace, without `..`"
                )))
            }
        }
    }
    Ok(parts.join("/"))
}

fn in_scope(rel: &str, scope: &str) -> bool {
    scope.is_empty() || rel == scope || rel.strip_prefix(scope).is_some_and(|r| r.starts_with('/'))
}

fn identifier_like(q: &str) -> bool {
    q.chars().all(|c| c.is_alphanumeric() || c == '_')
}

/// `Agent::run`, `self.run`, `pkg.Run` → the last segment.
fn symbol_of(query: &str) -> &str {
    let last = query.rsplit("::").next().unwrap_or(query);
    let last = last.rsplit('.').next().unwrap_or(last);
    last.trim_end_matches("()")
}

fn whole_word(line: &str, word: &str) -> bool {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    line.match_indices(word).any(|(at, _)| {
        !line[..at].chars().next_back().is_some_and(ident)
            && !line[at + word.len()..].chars().next().is_some_and(ident)
    })
}

/// The search itself, on a snapshot. Blocking (reads files for the
/// reference lines and the text search).
pub fn search(
    root: &Path,
    snapshot: &Snapshot,
    query: &str,
    kind: SearchKind,
    scope: &str,
    max: usize,
) -> String {
    let symbol = symbol_of(query);
    let tagged = identifier_like(symbol) && !symbol.is_empty();
    let mut defs = Vec::new();
    let mut refs = Vec::new();
    if tagged {
        for (rel, tags) in snapshot.files.iter().filter(|(r, _)| in_scope(r, scope)) {
            if kind != SearchKind::References {
                for d in tags.defs.iter().filter(|d| d.name == symbol) {
                    defs.push(format!(
                        "{rel}:{}  def {} {} — {}",
                        d.line,
                        d.kind.as_str(),
                        d.name,
                        d.text
                    ));
                }
            }
            if kind != SearchKind::Definitions {
                let lines: Vec<u32> = tags
                    .refs
                    .iter()
                    .filter(|r| r.name == symbol)
                    .map(|r| r.line)
                    .collect();
                if !lines.is_empty() {
                    let text = std::fs::read_to_string(root.join(rel)).unwrap_or_default();
                    let src: Vec<&str> = text.lines().collect();
                    let mut last = 0;
                    for l in lines {
                        if l == last {
                            continue;
                        }
                        last = l;
                        let shown = src
                            .get(l as usize - 1)
                            .map(|s| crate::tags::clip(s.trim()))
                            .unwrap_or_default();
                        refs.push(format!("{rel}:{l}  ref — {shown}"));
                    }
                }
            }
        }
    }
    // Text search: files no grammar covers (and every file, for a query
    // that isn't a symbol).
    let mut text_hits = Vec::new();
    let mut stopped_early = false;
    if kind != SearchKind::Definitions {
        for rel in snapshot.all_files.iter().filter(|r| in_scope(r, scope)) {
            let covered = extension(rel).and_then(lang::for_extension).is_some();
            if tagged && covered {
                continue;
            }
            let path = root.join(rel);
            if std::fs::metadata(&path).map_or(true, |m| m.len() > MAX_FILE_BYTES) {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            if bytes.contains(&0) {
                continue;
            }
            let text = String::from_utf8_lossy(&bytes);
            for (i, line) in text.lines().enumerate() {
                let hit = if identifier_like(query) {
                    whole_word(line, query)
                } else {
                    line.contains(query)
                };
                if hit {
                    text_hits.push(format!(
                        "{rel}:{}  text — {}",
                        i + 1,
                        crate::tags::clip(line.trim())
                    ));
                }
            }
            if text_hits.len() > max * 4 {
                stopped_early = true;
                break;
            }
        }
    }
    let total = defs.len() + refs.len() + text_hits.len();
    if total == 0 {
        let where_ = if scope.is_empty() {
            String::new()
        } else {
            format!(" under `{scope}`")
        };
        return format!(
            "no results for `{query}`{where_}. Try kind=all, a shorter name, or shell grep for partial words."
        );
    }
    let at_least = if stopped_early { "at least " } else { "" };
    let mut head = format!(
        "{at_least}{total} results ({} definitions, {} references, {} text matches)",
        defs.len(),
        refs.len(),
        text_hits.len()
    );
    if total > max {
        head.push_str(&format!(
            "; showing the first {max}, narrow with `path` or `kind`"
        ));
    }
    let body: Vec<String> = defs
        .into_iter()
        .chain(refs)
        .chain(text_hits)
        .take(max)
        .collect();
    format!("{head}\n{}", body.join("\n"))
}
