//! A deliberately small, lenient reader for SKILL.md frontmatter.
//!
//! Skills only need a handful of top-level scalar keys (`name`,
//! `description`, `disable-model-invocation`, ...), so this reads those and
//! skips anything nested (`metadata:` maps) instead of pulling in a full YAML
//! parser. It is lenient on purpose: skills written for other clients often
//! carry YAML their parsers happen to accept, most commonly an unquoted
//! value containing `: ` — agentskills.io's client guide recommends
//! accepting those rather than dropping the skill.

use std::collections::BTreeMap;

#[derive(Debug, Clone, Default)]
pub struct Frontmatter {
    /// Top-level scalar keys. Nested maps are recorded as `""`.
    pub fields: BTreeMap<String, String>,
    /// Markdown after the closing `---`, trimmed.
    pub body: String,
}

impl Frontmatter {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields.get(key).map(|s| s.as_str())
    }

    pub fn flag(&self, key: &str) -> bool {
        matches!(self.get(key), Some("true" | "True" | "TRUE" | "yes"))
    }
}

/// Split a SKILL.md into frontmatter fields and body. `Err` only when there
/// is no frontmatter block at all or it has no readable key — the cases the
/// client guide says to skip rather than load.
pub fn parse(text: &str) -> Result<Frontmatter, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let lines: Vec<&str> = text.lines().map(|l| l.trim_end_matches('\r')).collect();
    if lines.first().map(|l| l.trim_end()) != Some("---") {
        return Err("no frontmatter: file must start with `---`".into());
    }
    let close = lines
        .iter()
        .skip(1)
        .position(|l| matches!(l.trim_end(), "---" | "..."))
        .map(|i| i + 1)
        .ok_or("frontmatter has no closing `---`")?;

    let block = &lines[1..close];
    let mut fields = BTreeMap::new();
    let mut i = 0;
    while i < block.len() {
        let line = block[i];
        i += 1;
        let Some((key, rest)) = top_level_key(line) else {
            continue; // comment, blank, stray indentation: ignore
        };
        // Everything indented below this key belongs to it.
        let start = i;
        while i < block.len() && (block[i].trim().is_empty() || block[i].starts_with([' ', '\t'])) {
            i += 1;
        }
        let cont = &block[start..i];
        fields.insert(key.to_string(), scalar(rest.trim(), cont));
    }
    if fields.is_empty() {
        return Err("frontmatter has no readable `key: value` lines".into());
    }
    let body = lines[close + 1..].join("\n").trim().to_string();
    Ok(Frontmatter { fields, body })
}

fn top_level_key(line: &str) -> Option<(&str, &str)> {
    if line.starts_with([' ', '\t', '#']) {
        return None;
    }
    let colon = line.find(':')?;
    let key = &line[..colon];
    let valid = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    valid.then(|| (key, &line[colon + 1..]))
}

fn scalar(rest: &str, cont: &[&str]) -> String {
    if let Some(indicator) = rest.strip_prefix('|') {
        return block_scalar(indicator, cont, false);
    }
    if let Some(indicator) = rest.strip_prefix('>') {
        return block_scalar(indicator, cont, true);
    }
    if rest.starts_with('"') || rest.starts_with('\'') {
        return quoted(rest, cont);
    }
    if rest.is_empty() {
        // An empty value with indented children is a nested map (e.g.
        // `metadata:`) — not something a skill catalog needs.
        return String::new();
    }
    // Plain scalar, possibly folded over indented continuation lines.
    let mut out = strip_comment(rest).to_string();
    for l in cont.iter().map(|l| l.trim()).filter(|l| !l.is_empty()) {
        out.push(' ');
        out.push_str(strip_comment(l));
    }
    out
}

/// YAML treats ` #` as the start of a comment in plain scalars.
fn strip_comment(s: &str) -> &str {
    match s.find(" #") {
        Some(i) => s[..i].trim_end(),
        None => s,
    }
}

fn block_scalar(indicator: &str, cont: &[&str], folded: bool) -> String {
    let keep_trailing = indicator.contains('+');
    let indent = cont
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    let lines: Vec<&str> = cont.iter().map(|l| l.get(indent..).unwrap_or("")).collect();
    let mut out = if folded {
        // Lines fold into one with spaces; a blank line is a paragraph break.
        let mut s = String::new();
        let mut prev_blank = true;
        for l in &lines {
            if l.trim().is_empty() {
                s.push('\n');
                prev_blank = true;
            } else {
                if !prev_blank {
                    s.push(' ');
                }
                s.push_str(l.trim_end());
                prev_blank = false;
            }
        }
        s
    } else {
        lines.join("\n")
    };
    if !keep_trailing {
        out = out.trim_end().to_string();
    }
    out
}

fn quoted(rest: &str, cont: &[&str]) -> String {
    let quote = rest.chars().next().unwrap();
    // A quoted scalar may span lines; lines fold with single spaces.
    let mut raw = rest[1..].to_string();
    let mut lines = cont.iter();
    while !ends_quote(&raw, quote) {
        match lines.next() {
            Some(l) => {
                raw.push(' ');
                raw.push_str(l.trim());
            }
            None => break, // unterminated: keep what we have (lenient)
        }
    }
    let raw = raw.trim_end();
    let inner = if ends_quote(raw, quote) {
        &raw[..raw.len() - 1]
    } else {
        raw
    };
    if quote == '\'' {
        inner.replace("''", "'")
    } else {
        unescape_double(inner)
    }
}

fn ends_quote(s: &str, quote: char) -> bool {
    let s = s.trim_end();
    if !s.ends_with(quote) {
        return false;
    }
    if quote == '\'' {
        // `''` is an escaped quote; an odd run of trailing quotes closes.
        return s.chars().rev().take_while(|c| *c == '\'').count() % 2 == 1;
    }
    let backslashes = s[..s.len() - 1]
        .chars()
        .rev()
        .take_while(|c| *c == '\\')
        .count();
    backslashes % 2 == 0
}

fn unescape_double(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_fields_and_body() {
        let fm =
            parse("---\nname: pdf\ndescription: Extract PDF text.\n---\n\n# PDF\nsteps\n").unwrap();
        assert_eq!(fm.get("name"), Some("pdf"));
        assert_eq!(fm.get("description"), Some("Extract PDF text."));
        assert_eq!(fm.body, "# PDF\nsteps");
    }

    #[test]
    fn unquoted_colon_in_value_is_kept_whole() {
        // Invalid YAML that other clients accept; the guide says load it.
        let fm = parse(
            "---\nname: x\ndescription: Use this skill when: the user asks about PDFs\n---\n",
        )
        .unwrap();
        assert_eq!(
            fm.get("description"),
            Some("Use this skill when: the user asks about PDFs")
        );
    }

    #[test]
    fn quoted_values_unescape() {
        let fm = parse(
            "---\nname: \"a-b\"\ndescription: 'it''s \"fine\": ok'\nx: \"say \\\"hi\\\"\"\n---\n",
        )
        .unwrap();
        assert_eq!(fm.get("name"), Some("a-b"));
        assert_eq!(fm.get("description"), Some("it's \"fine\": ok"));
        assert_eq!(fm.get("x"), Some("say \"hi\""));
    }

    #[test]
    fn multiline_quoted_value_folds() {
        let fm = parse("---\nname: x\ndescription: \"first line\n  second line\"\n---\n").unwrap();
        assert_eq!(fm.get("description"), Some("first line second line"));
    }

    #[test]
    fn block_scalars_literal_and_folded() {
        let fm = parse("---\nname: x\ndescription: >\n  folded one\n  two\n\n  para\nnotes: |\n  keep\n  lines\n---\nbody").unwrap();
        assert_eq!(fm.get("description"), Some("folded one two\npara"));
        assert_eq!(fm.get("notes"), Some("keep\nlines"));
        assert_eq!(fm.body, "body");
    }

    #[test]
    fn nested_map_is_skipped_and_following_keys_still_read() {
        let fm = parse("---\nname: x\nmetadata:\n  author: me\n  version: \"1.0\"\ndisable-model-invocation: true\ndescription: d\n---\n").unwrap();
        assert_eq!(fm.get("metadata"), Some(""));
        assert!(fm.flag("disable-model-invocation"));
        assert_eq!(fm.get("description"), Some("d"));
        assert!(fm.get("author").is_none());
    }

    #[test]
    fn plain_scalar_continuation_and_comment() {
        let fm = parse("---\nname: x # trailing comment\ndescription: one\n  two\n---\n").unwrap();
        assert_eq!(fm.get("name"), Some("x"));
        assert_eq!(fm.get("description"), Some("one two"));
    }

    #[test]
    fn crlf_and_bom_are_tolerated() {
        let fm = parse("\u{feff}---\r\nname: x\r\ndescription: d\r\n---\r\nbody\r\n").unwrap();
        assert_eq!(fm.get("description"), Some("d"));
        assert_eq!(fm.body, "body");
    }

    #[test]
    fn missing_or_unclosed_frontmatter_is_an_error() {
        assert!(parse("# just markdown\n").is_err());
        assert!(parse("---\nname: x\ndescription: d\n").is_err());
        assert!(parse("---\n# only a comment\n---\nbody").is_err());
    }
}
