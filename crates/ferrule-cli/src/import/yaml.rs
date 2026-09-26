//! Enough YAML for Hermes's `config.yaml`: block maps and lists, flow
//! lists and maps, plain and quoted scalars, comments. What it can't read
//! (block scalars, anchors, aliases, tags) becomes a skipped key, named in
//! `Doc::skipped`, rather than an error: the importer wants a handful of
//! keys, not the whole file.

use serde_json::{Map, Value};

pub struct Doc {
    pub value: Value,
    /// Dotted paths of keys that weren't read.
    pub skipped: Vec<String>,
}

pub fn parse(text: &str) -> Result<Doc, String> {
    let mut lines = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        let body = strip_comment(raw);
        if body.trim().is_empty() || body.trim() == "---" || body.trim() == "..." {
            continue;
        }
        let indent = body.len() - body.trim_start().len();
        if body[..indent].contains('\t') {
            return Err(format!("line {}: a tab in the indentation", n + 1));
        }
        lines.push(Line {
            indent,
            text: body.trim().to_string(),
            n: n + 1,
        });
    }
    let mut p = Parser {
        lines,
        at: 0,
        skipped: Vec::new(),
    };
    let value = if p.lines.is_empty() {
        Value::Object(Map::new())
    } else {
        let indent = p.lines[0].indent;
        p.block(indent, "")?
    };
    Ok(Doc {
        value,
        skipped: p.skipped,
    })
}

struct Line {
    indent: usize,
    text: String,
    n: usize,
}

struct Parser {
    lines: Vec<Line>,
    at: usize,
    skipped: Vec<String>,
}

impl Parser {
    fn block(&mut self, indent: usize, path: &str) -> Result<Value, String> {
        if is_item(&self.lines[self.at].text) {
            self.list(indent, path)
        } else {
            self.map(indent, path)
        }
    }

    fn map(&mut self, indent: usize, path: &str) -> Result<Value, String> {
        let mut map = Map::new();
        while let Some(line) = self.lines.get(self.at) {
            if line.indent < indent || is_item(&line.text) {
                break;
            }
            if line.indent > indent {
                // Nothing opened this: skip it rather than guess.
                self.at += 1;
                continue;
            }
            let n = line.n;
            let Some((key, rest)) = split_key(&line.text) else {
                return Err(format!("line {n}: expected `key: value`"));
            };
            let (key, rest) = (unquote(key), rest.to_string());
            let here = join(path, &key);
            self.at += 1;
            let value = self.value_after(indent, &rest, &here, n)?;
            if let Some(v) = value {
                map.insert(key, v);
            }
        }
        Ok(Value::Object(map))
    }

    fn list(&mut self, indent: usize, path: &str) -> Result<Value, String> {
        let mut items = Vec::new();
        while let Some(line) = self.lines.get(self.at) {
            if line.indent != indent || !is_item(&line.text) {
                break;
            }
            let n = line.n;
            let rest = line.text[1..].trim_start().to_string();
            let here = format!("{path}[{}]", items.len());
            if !rest.is_empty() && split_key(&rest).is_some() && !starts_flow(&rest) {
                // `- key: v` opens a map whose keys line up with `key`.
                let offset = line.text.len() - rest.len();
                self.lines[self.at] = Line {
                    indent: indent + offset,
                    text: rest,
                    n,
                };
                items.push(self.map(indent + offset, &here)?);
                continue;
            }
            self.at += 1;
            if let Some(v) = self.value_after(indent, &rest, &here, n)? {
                items.push(v);
            }
        }
        Ok(Value::Array(items))
    }

    /// The value after `key:` or `-`: inline, or the nested block below.
    /// `None` when it was skipped.
    fn value_after(
        &mut self,
        indent: usize,
        rest: &str,
        path: &str,
        n: usize,
    ) -> Result<Option<Value>, String> {
        let rest = rest.trim();
        if rest.is_empty() {
            return Ok(Some(match self.lines.get(self.at) {
                Some(next) if next.indent > indent => self.block(next.indent, path)?,
                // A list may sit at its key's indent.
                Some(next)
                    if next.indent == indent && is_item(&next.text) && !path.contains('[') =>
                {
                    self.list(indent, path)?
                }
                _ => Value::Null,
            }));
        }
        if matches!(rest.chars().next(), Some('|' | '>' | '&' | '*' | '!')) {
            self.skip_nested(indent);
            self.skipped.push(path.to_string());
            return Ok(None);
        }
        if starts_flow(rest) {
            let mut text = rest.to_string();
            while !balanced(&text) {
                match self.lines.get(self.at) {
                    Some(l) => {
                        text.push(' ');
                        text.push_str(&l.text);
                        self.at += 1;
                    }
                    None => return Err(format!("line {n}: a `[` or `{{` isn't closed")),
                }
            }
            let chars: Vec<char> = text.chars().collect();
            let mut i = 0;
            return flow(&chars, &mut i)
                .map(Some)
                .map_err(|e| format!("line {n}: {e}"));
        }
        Ok(Some(scalar(rest)))
    }

    fn skip_nested(&mut self, indent: usize) {
        while self.lines.get(self.at).is_some_and(|l| l.indent > indent) {
            self.at += 1;
        }
    }
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}

fn is_item(text: &str) -> bool {
    text == "-" || text.starts_with("- ")
}

fn starts_flow(text: &str) -> bool {
    text.starts_with('[') || text.starts_with('{')
}

/// The line without its comment: `#` at the start or after a space,
/// outside quotes.
fn strip_comment(line: &str) -> &str {
    let mut quote = None;
    let mut prev = ' ';
    for (i, c) in line.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, '#') if prev == ' ' || prev == '\t' => return &line[..i],
            _ => {}
        }
        prev = c;
    }
    line
}

/// `key: rest`, splitting at the first `: ` (or a trailing `:`) outside
/// quotes and brackets.
fn split_key(text: &str) -> Option<(&str, &str)> {
    let mut quote = None;
    let mut depth = 0i32;
    let bytes = text.as_bytes();
    for (i, c) in text.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') if i == 0 => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, '[' | '{') => depth += 1,
            (None, ']' | '}') => depth -= 1,
            (None, ':') if depth == 0 && (i + 1 == bytes.len() || bytes[i + 1] == b' ') => {
                let key = text[..i].trim();
                return (!key.is_empty()).then(|| (key, &text[i + 1..]));
            }
            _ => {}
        }
    }
    None
}

fn balanced(text: &str) -> bool {
    let mut quote = None;
    let mut depth = 0i32;
    for c in text.chars() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, '[' | '{') => depth += 1,
            (None, ']' | '}') => depth -= 1,
            _ => {}
        }
    }
    depth <= 0
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        return serde_json::from_str(s).unwrap_or_else(|_| s[1..s.len() - 1].to_string());
    }
    if s.len() >= 2 && s.starts_with('\'') && s.ends_with('\'') {
        return s[1..s.len() - 1].replace("''", "'");
    }
    s.to_string()
}

fn scalar(s: &str) -> Value {
    let s = s.trim();
    if s.starts_with('"') || s.starts_with('\'') {
        return Value::String(unquote(s));
    }
    match s {
        "" | "~" | "null" | "Null" | "NULL" => Value::Null,
        "true" | "True" | "TRUE" => Value::Bool(true),
        "false" | "False" | "FALSE" => Value::Bool(false),
        _ => {
            if let Ok(n) = s.parse::<i64>() {
                Value::from(n)
            } else if s.contains('.') && s.parse::<f64>().is_ok_and(f64::is_finite) {
                Value::from(s.parse::<f64>().unwrap())
            } else {
                Value::String(s.to_string())
            }
        }
    }
}

fn flow(c: &[char], i: &mut usize) -> Result<Value, String> {
    skip_ws(c, i);
    match c.get(*i) {
        Some('[') => {
            *i += 1;
            let mut items = Vec::new();
            loop {
                skip_ws(c, i);
                match c.get(*i) {
                    Some(']') => {
                        *i += 1;
                        return Ok(Value::Array(items));
                    }
                    Some(',') => *i += 1,
                    Some(_) => items.push(flow(c, i)?),
                    None => return Err("a `[` isn't closed".into()),
                }
            }
        }
        Some('{') => {
            *i += 1;
            let mut map = Map::new();
            loop {
                skip_ws(c, i);
                match c.get(*i) {
                    Some('}') => {
                        *i += 1;
                        return Ok(Value::Object(map));
                    }
                    Some(',') => *i += 1,
                    Some(_) => {
                        let key = match flow_scalar(c, i, true) {
                            Value::String(s) => s,
                            other => other.to_string(),
                        };
                        skip_ws(c, i);
                        let value = if c.get(*i) == Some(&':') {
                            *i += 1;
                            flow(c, i)?
                        } else {
                            Value::Null
                        };
                        map.insert(key, value);
                    }
                    None => return Err("a `{` isn't closed".into()),
                }
            }
        }
        _ => Ok(flow_scalar(c, i, false)),
    }
}

/// A scalar inside `[…]`/`{…}`: quoted, or plain up to `,`, `]`, `}` (and
/// `:` for a key).
fn flow_scalar(c: &[char], i: &mut usize, key: bool) -> Value {
    skip_ws(c, i);
    if let Some(&q) = c.get(*i).filter(|q| **q == '"' || **q == '\'') {
        let start = *i;
        *i += 1;
        while *i < c.len() {
            if c[*i] == '\\' && q == '"' {
                *i += 2;
                continue;
            }
            if c[*i] == q {
                if q == '\'' && c.get(*i + 1) == Some(&'\'') {
                    *i += 2;
                    continue;
                }
                *i += 1;
                break;
            }
            *i += 1;
        }
        let s: String = c[start..(*i).min(c.len())].iter().collect();
        return Value::String(unquote(&s));
    }
    let start = *i;
    while *i < c.len() && !matches!(c[*i], ',' | ']' | '}') && !(key && c[*i] == ':') {
        if c[*i] == ':' && c.get(*i + 1).is_some_and(|n| n.is_whitespace()) {
            break;
        }
        *i += 1;
    }
    let s: String = c[start..*i].iter().collect();
    scalar(&s)
}

fn skip_ws(c: &[char], i: &mut usize) {
    while c.get(*i).is_some_and(|c| c.is_whitespace()) {
        *i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hermes_style_config_reads() {
        let text = r#"
_config_version: 46
model:
  default: "anthropic/claude-opus-4.6"   # the default
  provider: auto
providers:
  myprov:
    base_url: https://llm.example.com/v1
    api_key: "${MYPROV_KEY}"
    api_mode: chat_completions
    extra_headers: {X-Team: core, X-Tags: [a, "b c"]}
skills:
  external_dirs:
  - ~/.agents/skills
  - '/opt/it''s'
gateway:
  platforms:
    telegram:
      extra:
        allow_from: ["123456789", 42]
        group_allowed_chats: '["-1001234567890"]'
mcp_servers:
  - name: files
    command: npx
    args: [-y, server]
  - plain
prompt: |
  multi
  line
anchored: &a {x: 1}
url_with_hash: http://h/#frag
"#;
        let doc = parse(text).unwrap();
        let v = &doc.value;
        assert_eq!(v["_config_version"], json!(46));
        assert_eq!(v["model"]["default"], json!("anthropic/claude-opus-4.6"));
        assert_eq!(v["model"]["provider"], json!("auto"));
        assert_eq!(v["providers"]["myprov"]["api_key"], json!("${MYPROV_KEY}"));
        assert_eq!(
            v["providers"]["myprov"]["extra_headers"],
            json!({"X-Team": "core", "X-Tags": ["a", "b c"]})
        );
        assert_eq!(
            v["skills"]["external_dirs"],
            json!(["~/.agents/skills", "/opt/it's"])
        );
        assert_eq!(
            v["gateway"]["platforms"]["telegram"]["extra"]["allow_from"],
            json!(["123456789", 42])
        );
        assert_eq!(
            v["mcp_servers"],
            json!([{"name": "files", "command": "npx", "args": ["-y", "server"]}, "plain"])
        );
        assert_eq!(v["url_with_hash"], json!("http://h/#frag"));
        assert!(v.get("prompt").is_none());
        assert_eq!(doc.skipped, ["prompt", "anchored"]);
        assert!(parse("a: [1, 2").is_err());
        assert!(parse("just words").is_err());
        assert_eq!(parse("").unwrap().value, json!({}));
    }
}
