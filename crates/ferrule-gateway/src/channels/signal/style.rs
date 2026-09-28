//! The agent's Markdown as Signal shows it: plain text, and the styled
//! ranges beside it (`textStyle`, "start:length:STYLE" in UTF-16 units).
//! Bold, italic, strikethrough and code; a heading is bold; a link is its
//! text with the address after it. Anything unmatched stays as typed.

/// `text` without its Markdown, and the styles to send with it.
pub fn render(text: &str) -> (String, Vec<String>) {
    let mut out = Styled::default();
    let mut fence = false;
    let mut first = true;
    for line in text.split('\n') {
        if line.trim_start().starts_with("```") {
            fence = !fence;
            continue;
        }
        if !first {
            out.push("\n");
        }
        first = false;
        if fence {
            out.styled(line, "MONOSPACE");
            continue;
        }
        let trimmed = line.trim_start();
        let hashes = trimmed.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ') {
            let start = out.len();
            inline(trimmed[hashes..].trim(), &mut out);
            out.mark(start, "BOLD");
        } else {
            inline(line, &mut out);
        }
    }
    let styles = out
        .styles
        .into_iter()
        .filter(|(_, n, _)| *n > 0)
        .map(|(s, n, k)| format!("{s}:{n}:{k}"))
        .collect();
    (out.text, styles)
}

#[derive(Default)]
struct Styled {
    text: String,
    /// UTF-16 length of `text`.
    units: usize,
    styles: Vec<(usize, usize, &'static str)>,
}

impl Styled {
    fn len(&self) -> usize {
        self.units
    }

    fn push(&mut self, s: &str) {
        self.units += s.encode_utf16().count();
        self.text.push_str(s);
    }

    fn styled(&mut self, s: &str, style: &'static str) {
        let start = self.len();
        self.push(s);
        self.mark(start, style);
    }

    fn mark(&mut self, start: usize, style: &'static str) {
        let n = self.len() - start;
        self.styles.push((start, n, style));
    }
}

/// The markers, longest first.
const MARKS: &[(&str, &str)] = &[
    ("**", "BOLD"),
    ("__", "BOLD"),
    ("~~", "STRIKETHROUGH"),
    ("*", "ITALIC"),
    ("_", "ITALIC"),
];

fn inline(s: &str, out: &mut Styled) {
    let mut i = 0;
    let mut plain = 0;
    while i < s.len() {
        let rest = &s[i..];
        if let Some((len, apply)) = span(s, i) {
            out.push(&s[plain..i]);
            apply(out);
            i += len;
            plain = i;
            continue;
        }
        i += rest.chars().next().map_or(1, char::len_utf8);
    }
    out.push(&s[plain..]);
}

type Apply<'a> = Box<dyn FnOnce(&mut Styled) + 'a>;

/// A styled span starting at byte `i` of `s`: its length and how to write it.
fn span(s: &str, i: usize) -> Option<(usize, Apply<'_>)> {
    let rest = &s[i..];
    if let Some(body) = rest.strip_prefix('`') {
        let end = body.find('`').filter(|e| *e > 0)?;
        let code = &body[..end];
        return Some((end + 2, Box::new(move |o| o.styled(code, "MONOSPACE"))));
    }
    if let Some(body) = rest.strip_prefix('[') {
        let close = body.find("](")?;
        let label = &body[..close];
        let after = &body[close + 2..];
        let end = after.find(')')?;
        let url = &after[..end];
        if label.is_empty() || url.contains(' ') {
            return None;
        }
        return Some((
            1 + close + 2 + end + 1,
            Box::new(move |o| {
                inline(label, o);
                if label != url {
                    o.push(&format!(" ({url})"));
                }
            }),
        ));
    }
    for (mark, style) in MARKS {
        let Some(body) = rest.strip_prefix(mark) else {
            continue;
        };
        // `snake_case` and `2*3*4` aren't emphasis.
        let before = s[..i].chars().next_back();
        if mark.len() == 1 && before.is_some_and(char::is_alphanumeric) {
            continue;
        }
        if body.starts_with(char::is_whitespace) || body.starts_with(mark) {
            continue;
        }
        let end = find_close(body, mark)?;
        let inner = &body[..end];
        if inner.is_empty() || inner.ends_with(char::is_whitespace) {
            continue;
        }
        let style: &'static str = style;
        return Some((
            mark.len() * 2 + end,
            Box::new(move |o| {
                let start = o.len();
                inline(inner, o);
                o.mark(start, style);
            }),
        ));
    }
    None
}

/// Where `mark` closes in `body`: not inside code, and (for a one-letter
/// mark) not followed by a letter.
fn find_close(body: &str, mark: &str) -> Option<usize> {
    let mut i = 0;
    let mut code = false;
    while i < body.len() {
        let rest = &body[i..];
        if rest.starts_with('`') {
            code = !code;
        } else if !code && rest.starts_with(mark) {
            let after = rest[mark.len()..].chars().next();
            let doubled = mark.len() == 1 && rest[1..].starts_with(mark);
            if !doubled && !(mark.len() == 1 && after.is_some_and(char::is_alphanumeric)) {
                return Some(i);
            }
            if doubled {
                i += 2;
                continue;
            }
        }
        i += rest.chars().next().map_or(1, char::len_utf8);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::render;

    #[test]
    fn markdown_becomes_ranges() {
        let (t, s) = render("**done** and *fast*, `x = 1`");
        assert_eq!(t, "done and fast, x = 1");
        assert_eq!(s, ["0:4:BOLD", "9:4:ITALIC", "15:5:MONOSPACE"]);
    }

    #[test]
    fn nesting_headings_links_and_fences() {
        let (t, s) = render("# Title\n**a _b_**\n[docs](https://x.io) ~~old~~");
        assert_eq!(t, "Title\na b\ndocs (https://x.io) old");
        assert!(s.contains(&"0:5:BOLD".to_string()), "{s:?}");
        assert!(s.contains(&"8:1:ITALIC".to_string()), "{s:?}");
        assert!(s.contains(&"6:3:BOLD".to_string()), "{s:?}");
        assert!(s.contains(&"30:3:STRIKETHROUGH".to_string()), "{s:?}");
        let (t, s) = render("see:\n```rust\nlet a = *b*;\n```\nend");
        assert_eq!(t, "see:\nlet a = *b*;\nend");
        assert_eq!(s, ["5:12:MONOSPACE"]);
    }

    #[test]
    fn what_isnt_markdown_stays() {
        for plain in [
            "snake_case_name",
            "2*3*4",
            "a * b * c",
            "unclosed **bold",
            "price_1 and x_2",
        ] {
            let (t, s) = render(plain);
            assert_eq!(t, plain);
            assert!(s.is_empty(), "{plain}: {s:?}");
        }
    }

    #[test]
    fn offsets_are_utf16() {
        let (t, s) = render("😀 **שלום**");
        assert_eq!(t, "😀 שלום");
        assert_eq!(s, ["3:4:BOLD"]);
    }
}
