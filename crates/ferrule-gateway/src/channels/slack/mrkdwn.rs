//! Markdown → Slack mrkdwn. Models write CommonMark; Slack shows it raw
//! (`**bold**` with its asterisks), so every outgoing text is converted:
//! bold `*b*`, italic `_i_`, strike `~s~`, links `<url|text>`, headings as
//! bold lines, bullets as `•`. Code spans and fences are left as they are
//! (the fence's language tag dropped). `&`, `<` and `>` are Slack's control
//! characters and are escaped everywhere, code included, except inside a
//! link's `<…>`. Quotes, numbered lists and tables stay text.
//!
//! M39: WhatsApp's markup is the same but for links and escaping: it has
//! no link syntax (`text (url)`) and no control characters.

/// Which markup comes out.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Flavor {
    Slack,
    WhatsApp,
}

pub fn convert(md: &str) -> String {
    convert_as(md, Flavor::Slack)
}

/// Markdown → WhatsApp: `*bold*`, `_italic_`, `~strike~`, fences kept.
pub fn whatsapp(md: &str) -> String {
    convert_as(md, Flavor::WhatsApp)
}

fn convert_as(md: &str, f: Flavor) -> String {
    let mut out = Vec::new();
    let mut fenced = false;
    for line in md.split('\n') {
        let trimmed = line.trim_start();
        if let Some(after) = trimmed.strip_prefix("```") {
            let indent = &line[..line.len() - trimmed.len()];
            if fenced {
                out.push(format!("{indent}```{}", escape(after, f)));
            } else {
                // The language tag would show as text in Slack.
                out.push(format!("{indent}```"));
            }
            fenced = !fenced;
            continue;
        }
        if fenced {
            out.push(escape(line, f));
            continue;
        }
        out.push(convert_line(line, f));
    }
    out.join("\n")
}

fn convert_line(line: &str, f: Flavor) -> String {
    let trimmed = line.trim_start();
    let indent = &line[..line.len() - trimmed.len()];
    // A heading, any level: a bold line.
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes) && indent.len() < 4 {
        let rest = &trimmed[hashes..];
        if rest.is_empty() || rest.starts_with(' ') {
            let title = rest.trim().trim_end_matches('#').trim_end();
            if title.is_empty() {
                return String::new();
            }
            // Bold inside a heading would nest: drop it.
            let inner = inline(title, f);
            let inner = inner.trim_matches('*');
            return format!("{indent}*{inner}*");
        }
    }
    // A bullet: `•`, keeping the indent.
    for bullet in ["- ", "* ", "+ "] {
        if let Some(rest) = trimmed.strip_prefix(bullet) {
            return format!("{indent}• {}", inline(rest, f));
        }
    }
    // A quote: the `>` markers stay what they are.
    let quote = trimmed
        .chars()
        .take_while(|c| *c == '>' || *c == ' ')
        .count();
    if quote > 0 && trimmed.starts_with('>') {
        return format!(
            "{indent}{}{}",
            &trimmed[..quote],
            inline(&trimmed[quote..], f)
        );
    }
    format!("{indent}{}", inline(trimmed, f))
}

/// Code spans kept (escaped only); everything between them restyled.
fn inline(text: &str, f: Flavor) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('`') {
        let ticks = rest[start..].chars().take_while(|c| *c == '`').count();
        let fence = &rest[start..start + ticks];
        match rest[start + ticks..].find(fence) {
            Some(len) => {
                out.push_str(&style(&rest[..start], f));
                let end = start + ticks + len + ticks;
                out.push_str(&escape(&rest[start..end], f));
                rest = &rest[end..];
            }
            None => break,
        }
    }
    out.push_str(&style(rest, f));
    out
}

fn escape(s: &str, f: Flavor) -> String {
    if f == Flavor::WhatsApp {
        return s.to_string();
    }
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Restyles text with no code in it.
fn style(s: &str, f: Flavor) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let at = |j: usize| chars.get(j).copied();
        // [text](url)
        if c == '[' {
            if let Some((label, url, next)) = link(&chars, i, f) {
                if f == Flavor::Slack {
                    out.push_str(&format!("<{url}|{}>", style(&label, f)));
                } else if label.trim() == url {
                    out.push_str(&url);
                } else {
                    out.push_str(&format!("{} ({url})", style(&label, f)));
                }
                i = next;
                continue;
            }
        }
        // <https://…>: an autolink is already Slack's syntax.
        if c == '<' {
            let rest: String = chars[i + 1..].iter().collect();
            if rest.starts_with("http://")
                || rest.starts_with("https://")
                || rest.starts_with("mailto:")
            {
                if let Some(end) = rest.find('>') {
                    let url = &rest[..end];
                    if !url.contains(char::is_whitespace) {
                        if f == Flavor::Slack {
                            out.push_str(&format!("<{url}>"));
                        } else {
                            out.push_str(url);
                        }
                        i += end + 2;
                        continue;
                    }
                }
            }
        }
        // **b** / __b__ → *b*
        if (c == '*' || c == '_') && at(i + 1) == Some(c) {
            if let Some(end) = closing(&chars, i + 2, &[c, c]) {
                let inner: String = chars[i + 2..end].iter().collect();
                out.push('*');
                out.push_str(&style(&inner, f));
                out.push('*');
                i = end + 2;
                continue;
            }
        }
        // ~~s~~ → ~s~
        if c == '~' && at(i + 1) == Some('~') {
            if let Some(end) = closing(&chars, i + 2, &['~', '~']) {
                let inner: String = chars[i + 2..end].iter().collect();
                out.push('~');
                out.push_str(&style(&inner, f));
                out.push('~');
                i = end + 2;
                continue;
            }
        }
        // *i* → _i_ (a `*` with a space after it is just a star)
        if c == '*' {
            if let Some(end) = closing(&chars, i + 1, &['*']) {
                let inner: String = chars[i + 1..end].iter().collect();
                out.push('_');
                out.push_str(&style(&inner, f));
                out.push('_');
                i = end + 1;
                continue;
            }
        }
        match c {
            _ if f == Flavor::WhatsApp => out.push(c),
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
        i += 1;
    }
    out
}

/// Where `marker` closes emphasis opened just before `from`: the text
/// between is non-empty and neither starts nor ends with a space.
fn closing(chars: &[char], from: usize, marker: &[char]) -> Option<usize> {
    let first = *chars.get(from)?;
    if first.is_whitespace() || marker.contains(&first) {
        return None;
    }
    let mut j = from + 1;
    while j + marker.len() <= chars.len() {
        if chars[j..j + marker.len()] == *marker
            && !chars[j - 1].is_whitespace()
            // `**` isn't a single `*`'s end.
            && (marker.len() > 1 || chars.get(j + 1) != Some(&marker[0]))
        {
            return Some(j);
        }
        j += 1;
    }
    None
}

/// `[label](url)` at `i`: the label, the URL, and the index after it.
fn link(chars: &[char], i: usize, f: Flavor) -> Option<(String, String, usize)> {
    let mut depth = 0;
    let mut j = i;
    let close = loop {
        match chars.get(j)? {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    break j;
                }
            }
            _ => {}
        }
        j += 1;
    };
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let end = (close + 2..chars.len()).find(|&k| chars[k] == ')')?;
    let url: String = chars[close + 2..end].iter().collect();
    let url = url.trim();
    if url.is_empty() || url.contains(char::is_whitespace) {
        return None;
    }
    let label: String = chars[i + 1..close].iter().collect();
    let url = match f {
        Flavor::Slack => url.replace('|', "%7C").replace('>', "%3E"),
        Flavor::WhatsApp => url.to_string(),
    };
    Some((label, url, end + 1))
}

#[cfg(test)]
mod tests {
    use super::{convert, whatsapp};

    #[test]
    fn the_table_of_conversions() {
        let cases = [
            ("**bold** and __also__", "*bold* and *also*"),
            ("*it* and _it_", "_it_ and _it_"),
            ("~~gone~~", "~gone~"),
            (
                "see [the docs](https://x.io/a?b=1&c=2)",
                "see <https://x.io/a?b=1&c=2|the docs>",
            ),
            ("# Title", "*Title*"),
            ("### **Deep** ##", "*Deep*"),
            ("- one\n  * two\n+ three", "• one\n  • two\n• three"),
            ("1. first\n2. second", "1. first\n2. second"),
            ("> quoted **b**", "> quoted *b*"),
            ("a < b && c > d", "a &lt; b &amp;&amp; c &gt; d"),
            ("2 * 3 * 4", "2 * 3 * 4"),
            ("<https://x.io>", "<https://x.io>"),
            ("| a | b |\n|---|---|", "| a | b |\n|---|---|"),
            ("snake_case_name", "snake_case_name"),
        ];
        for (md, want) in cases {
            assert_eq!(convert(md), want, "{md}");
        }
    }

    #[test]
    fn code_is_never_restyled_only_escaped() {
        assert_eq!(
            convert("run `**x** <y>` then **go**"),
            "run `**x** &lt;y&gt;` then *go*"
        );
        assert_eq!(
            convert("```rust\nlet a = **b** && c;\n# not a heading\n```\n**after**"),
            "```\nlet a = **b** &amp;&amp; c;\n# not a heading\n```\n*after*"
        );
    }

    #[test]
    fn whatsapp_has_no_link_syntax_and_no_escapes() {
        let cases = [
            ("**bold**, *it*, ~~gone~~", "*bold*, _it_, ~gone~"),
            (
                "see [the docs](https://x.io/a?b=1&c=2)",
                "see the docs (https://x.io/a?b=1&c=2)",
            ),
            (
                "[https://x.io](https://x.io) and <https://y.io>",
                "https://x.io and https://y.io",
            ),
            ("a < b && c > d", "a < b && c > d"),
            ("## Plan", "*Plan*"),
            ("```sh\nrm -rf **x**\n```", "```\nrm -rf **x**\n```"),
        ];
        for (md, want) in cases {
            assert_eq!(whatsapp(md), want, "{md}");
        }
    }
}
