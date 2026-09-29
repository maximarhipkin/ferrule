//! Markdown → the HTML Matrix clients show (`org.matrix.custom.html`).
//! The `body` stays the Markdown itself, which reads fine as plain text;
//! `formatted_body` gets a small pass: bold, italic, strike, code spans
//! and fences, links, headings, bullet and numbered lists, quotes. Every
//! character of text is escaped, so nothing the model writes becomes
//! markup it didn't mean.

/// The HTML for `md`.
pub fn to_html(md: &str) -> String {
    let mut out = String::new();
    // The open block: a list (`ul`/`ol`), a quote, or plain lines.
    let mut open: Option<&'static str> = None;
    let mut fence: Option<String> = None;
    let mut code = String::new();
    let close = |out: &mut String, open: &mut Option<&'static str>| {
        match open.take() {
            Some("ul") => out.push_str("</ul>"),
            Some("ol") => out.push_str("</ol>"),
            Some("blockquote") => out.push_str("</blockquote>"),
            _ => {}
        };
    };
    let mut plain_before = false;
    for line in md.split('\n') {
        let trimmed = line.trim_start();
        if let Some(lang) = fence.as_ref() {
            if trimmed.starts_with("```") {
                let class = if lang.is_empty() {
                    String::new()
                } else {
                    format!(" class=\"language-{}\"", escape(lang))
                };
                out.push_str(&format!(
                    "<pre><code{class}>{}</code></pre>",
                    escape(code.trim_end_matches('\n'))
                ));
                code.clear();
                fence = None;
                plain_before = false;
            } else {
                code.push_str(line);
                code.push('\n');
            }
            continue;
        }
        if let Some(lang) = trimmed.strip_prefix("```") {
            close(&mut out, &mut open);
            fence = Some(
                lang.trim()
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '+')
                    .collect(),
            );
            continue;
        }
        let hashes = trimmed.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ') {
            close(&mut out, &mut open);
            let title = trimmed[hashes..].trim().trim_end_matches('#').trim_end();
            out.push_str(&format!("<h{hashes}>{}</h{hashes}>", inline(title)));
            plain_before = false;
            continue;
        }
        let bullet = ["- ", "* ", "+ "]
            .iter()
            .find_map(|b| trimmed.strip_prefix(b));
        let numbered = numbered(trimmed);
        let quote = trimmed
            .strip_prefix("> ")
            .or_else(|| (trimmed == ">").then_some(""));
        let (block, item) = match (bullet, numbered, quote) {
            (Some(rest), _, _) => ("ul", rest),
            (_, Some(rest), _) => ("ol", rest),
            (_, _, Some(rest)) => ("blockquote", rest),
            _ => ("", trimmed),
        };
        if open != Some(block).filter(|b| !b.is_empty()) {
            close(&mut out, &mut open);
            if !block.is_empty() {
                out.push_str(&format!("<{block}>"));
                open = Some(block);
                plain_before = false;
            }
        }
        match block {
            "ul" | "ol" => out.push_str(&format!("<li>{}</li>", inline(item))),
            "blockquote" => {
                if !out.ends_with("<blockquote>") {
                    out.push_str("<br>");
                }
                out.push_str(&inline(item));
            }
            _ => {
                if plain_before {
                    out.push_str("<br>");
                }
                out.push_str(&inline(line.trim_end()));
                plain_before = true;
            }
        }
    }
    if fence.is_some() {
        out.push_str(&format!(
            "<pre><code>{}</code></pre>",
            escape(code.trim_end_matches('\n'))
        ));
    }
    close(&mut out, &mut open);
    out
}

/// `3. item` → `item`.
fn numbered(line: &str) -> Option<&str> {
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 || digits > 9 {
        return None;
    }
    line[digits..]
        .strip_prefix(". ")
        .or_else(|| line[digits..].strip_prefix(") "))
}

pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Code spans kept as `<code>`; the rest styled.
fn inline(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('`') {
        let ticks = rest[start..].chars().take_while(|c| *c == '`').count();
        let fence = &rest[start..start + ticks];
        match rest[start + ticks..].find(fence) {
            Some(len) => {
                out.push_str(&style(&rest[..start]));
                let inner = &rest[start + ticks..start + ticks + len];
                out.push_str(&format!("<code>{}</code>", escape(inner.trim())));
                rest = &rest[start + ticks + len + ticks..];
            }
            None => break,
        }
    }
    out.push_str(&style(rest));
    out
}

/// Where the `pair` closing a span that opens before `from` is: not right
/// after the opener, and not after a space.
fn closing(chars: &[char], from: usize, pair: &[char]) -> Option<usize> {
    if chars.get(from).is_none_or(|c| c.is_whitespace()) {
        return None;
    }
    let n = pair.len();
    (from + 1..=chars.len().saturating_sub(n))
        .find(|&j| chars[j..j + n] == *pair && !chars[j - 1].is_whitespace())
}

fn safe_url(url: &str) -> bool {
    ["https://", "http://", "mailto:", "matrix:"]
        .iter()
        .any(|p| url.starts_with(p))
        && !url.contains(char::is_whitespace)
}

/// Text with no code in it.
fn style(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let at = |j: usize| chars.get(j).copied();
        if c == '[' {
            if let Some(close) = chars[i..].iter().position(|&c| c == ']').map(|p| p + i) {
                if at(close + 1) == Some('(') {
                    if let Some(end) = chars[close..].iter().position(|&c| c == ')') {
                        let end = end + close;
                        let label: String = chars[i + 1..close].iter().collect();
                        let url: String = chars[close + 2..end].iter().collect();
                        if safe_url(&url) {
                            out.push_str(&format!(
                                "<a href=\"{}\">{}</a>",
                                escape(&url),
                                style(&label)
                            ));
                            i = end + 1;
                            continue;
                        }
                    }
                }
            }
        }
        if c == '<' {
            let rest: String = chars[i + 1..].iter().collect();
            if let Some(end) = rest.find('>') {
                let url = &rest[..end];
                if safe_url(url) {
                    out.push_str(&format!("<a href=\"{}\">{}</a>", escape(url), escape(url)));
                    i += url.chars().count() + 2;
                    continue;
                }
            }
        }
        let start = i;
        for (pair, tag) in [
            (['*', '*'], "strong"),
            (['_', '_'], "strong"),
            (['~', '~'], "del"),
        ] {
            if c == pair[0] && at(i + 1) == Some(pair[1]) {
                if let Some(end) = closing(&chars, i + 2, &pair) {
                    let inner: String = chars[i + 2..end].iter().collect();
                    out.push_str(&format!("<{tag}>{}</{tag}>", style(&inner)));
                    i = end + 2;
                    break;
                }
            }
        }
        if i != start {
            continue;
        }
        // A `_` inside a word (snake_case) is just a character.
        let word_before = i > 0 && chars[i - 1].is_alphanumeric();
        if (c == '*' || (c == '_' && !word_before)) && at(i + 1) != Some(c) {
            if let Some(end) = closing(&chars, i + 1, &[c]) {
                let after_word = at(end + 1).is_some_and(char::is_alphanumeric);
                if !(c == '_' && after_word) {
                    let inner: String = chars[i + 1..end].iter().collect();
                    out.push_str(&format!("<em>{}</em>", style(&inner)));
                    i = end + 1;
                    continue;
                }
            }
        }
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::to_html;

    #[test]
    fn inline_styles_links_and_escaping() {
        assert_eq!(
            to_html("**bold** and *it* and ~~no~~ `a<b>`"),
            "<strong>bold</strong> and <em>it</em> and <del>no</del> <code>a&lt;b&gt;</code>"
        );
        assert_eq!(
            to_html("see [the docs](https://x.org/a?b=1&c=2) or <https://y.org>"),
            "see <a href=\"https://x.org/a?b=1&amp;c=2\">the docs</a> or <a href=\"https://y.org\">https://y.org</a>"
        );
        assert_eq!(
            to_html("[bad](javascript:alert(1)) <script>"),
            "[bad](javascript:alert(1)) &lt;script&gt;"
        );
        assert_eq!(
            to_html("snake_case_name and 2 * 3"),
            "snake_case_name and 2 * 3"
        );
    }

    #[test]
    fn blocks_become_lists_quotes_headings_and_code() {
        assert_eq!(
            to_html("# Title\nline one\nline two\n- a\n- **b**\n1. x\n2. y\n> q1\n> q2\n```rust\nlet x = 1 < 2;\n```\nafter"),
            "<h1>Title</h1>line one<br>line two<ul><li>a</li><li><strong>b</strong></li></ul><ol><li>x</li><li>y</li></ol><blockquote>q1<br>q2</blockquote><pre><code class=\"language-rust\">let x = 1 &lt; 2;</code></pre>after"
        );
        assert_eq!(
            to_html("```\nopen fence"),
            "<pre><code>open fence</code></pre>"
        );
    }
}
