//! Tags: a file's top-level definitions and the identifiers it references,
//! from one tree-sitter parse and the language's query.

use crate::lang::{kind_of, Lang};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Function,
    Method,
    Class,
    Struct,
    Enum,
    Trait,
    Interface,
    Type,
    Module,
    Const,
    Macro,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Function => "function",
            Kind::Method => "method",
            Kind::Class => "class",
            Kind::Struct => "struct",
            Kind::Enum => "enum",
            Kind::Trait => "trait",
            Kind::Interface => "interface",
            Kind::Type => "type",
            Kind::Module => "module",
            Kind::Const => "const",
            Kind::Macro => "macro",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Def {
    pub name: String,
    pub kind: Kind,
    /// 1-based: the line of the name.
    pub line: u32,
    /// That line, name is on, trimmed, at most [`MAX_TEXT`] chars.
    pub text: String,
}

/// A referenced identifier and its 1-based line.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Ref {
    pub name: String,
    pub line: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileTags {
    /// In line order.
    pub defs: Vec<Def>,
    /// Sorted, one per (name, line).
    pub refs: Vec<Ref>,
}

pub const MAX_TEXT: usize = 100;

/// Compiled queries, one per language (compiling one takes milliseconds;
/// a refresh may parse thousands of files).
fn query_for(lang: &'static Lang) -> Option<&'static Query> {
    static CACHE: OnceLock<Mutex<HashMap<&'static str, Option<&'static Query>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
    *cache.entry(lang.name).or_insert_with(|| {
        match Query::new(&(lang.grammar)(), &lang.query.concat()) {
            Ok(q) => Some(Box::leak(Box::new(q))),
            Err(e) => {
                tracing::warn!("codemap: the {} tag query doesn't compile: {e}", lang.name);
                None
            }
        }
    })
}

/// The tags of `source`, or `None` if it couldn't be parsed at all.
pub fn extract(lang: &'static Lang, source: &str) -> Option<FileTags> {
    let query = query_for(lang)?;
    let mut parser = Parser::new();
    parser.set_language(&(lang.grammar)()).ok()?;
    let tree = parser.parse(source, None)?;
    let names = query.capture_names();
    let bytes = source.as_bytes();

    // name node's byte range → (pattern index, def kind + def node | ref)
    type Hit<'t> = (usize, Option<(Kind, Node<'t>)>, Node<'t>);
    let mut seen: HashMap<(usize, usize), Hit> = HashMap::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), bytes);
    while let Some(m) = matches.next() {
        let mut name = None;
        let mut def = None;
        let mut is_ref = false;
        for c in m.captures {
            match names[c.index as usize] {
                "name" => name = Some(c.node),
                "ref" => is_ref = true,
                other => {
                    if let Some(kind) = kind_of(other) {
                        def = Some((kind, c.node));
                    }
                }
            }
        }
        let Some(name) = name else { continue };
        if def.is_none() && !is_ref {
            continue;
        }
        let key = (name.start_byte(), name.end_byte());
        let entry = (m.pattern_index, def, name);
        match seen.get(&key) {
            Some((idx, _, _)) if *idx <= m.pattern_index => {}
            _ => {
                seen.insert(key, entry);
            }
        }
    }

    let lines: Vec<&str> = source.lines().collect();
    let mut tags = FileTags::default();
    for (_, def, name) in seen.into_values() {
        let Ok(text) = name.utf8_text(bytes) else {
            continue;
        };
        if text.is_empty() {
            continue;
        }
        let line = name.start_position().row as u32 + 1;
        match def {
            Some((kind, node)) => {
                if is_local(lang, node) {
                    continue;
                }
                let kind = if kind == Kind::Function && in_container(lang, node) {
                    Kind::Method
                } else {
                    kind
                };
                // The name's line, not the node's first: a Java annotation
                // or a decorator may start the node a line earlier.
                let shown = lines.get(line as usize - 1).map(|l| l.trim()).unwrap_or("");
                tags.defs.push(Def {
                    name: text.to_string(),
                    kind,
                    line,
                    text: clip(shown),
                });
            }
            None => tags.refs.push(Ref {
                name: text.to_string(),
                line,
            }),
        }
    }
    tags.defs
        .sort_by(|a, b| (a.line, &a.name).cmp(&(b.line, &b.name)));
    tags.defs.dedup();
    tags.refs.sort();
    tags.refs.dedup();
    Some(tags)
}

fn is_local(lang: &Lang, node: Node) -> bool {
    let mut up = node.parent();
    while let Some(n) = up {
        if lang.local_scopes.contains(&n.kind()) {
            return true;
        }
        up = n.parent();
    }
    false
}

fn in_container(lang: &Lang, node: Node) -> bool {
    let mut up = node.parent();
    while let Some(n) = up {
        if lang.containers.contains(&n.kind()) {
            return true;
        }
        up = n.parent();
    }
    false
}

pub(crate) fn clip(line: &str) -> String {
    match line.char_indices().nth(MAX_TEXT) {
        Some((at, _)) => format!("{}…", &line[..at]),
        None => line.to_string(),
    }
}
