//! M63.3: just enough XML for WebDAV's `multistatus` answers (CalDAV's
//! PROPFIND and REPORT). A small tree of elements by local name (the
//! namespace prefix dropped: servers pick their own), their attributes,
//! their text with entities and CDATA decoded. Not a validating parser: a
//! DOCTYPE's internal subset, processing instructions and comments are
//! skipped, and an answer that doesn't close is an error.

/// One element.
#[derive(Debug, Default, Clone)]
pub struct Node {
    /// The local name (`href` for `D:href`).
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Node>,
    /// The text directly inside it, decoded.
    pub text: String,
}

impl Node {
    pub fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }

    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }

    /// The first element named `name` at any depth below, depth first.
    pub fn find(&self, name: &str) -> Option<&Node> {
        for c in &self.children {
            if c.name == name {
                return Some(c);
            }
            if let Some(found) = c.find(name) {
                return Some(found);
            }
        }
        None
    }

    /// Every element named `name` at any depth below.
    pub fn find_all<'a>(&'a self, name: &str, out: &mut Vec<&'a Node>) {
        for c in &self.children {
            if c.name == name {
                out.push(c);
            }
            c.find_all(name, out);
        }
    }

    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn trimmed(&self) -> &str {
        self.text.trim()
    }
}

fn local(name: &str) -> String {
    name.rsplit(':').next().unwrap_or(name).to_string()
}

/// Decodes the five named entities and numeric references; anything else
/// is left as written.
pub fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let after = &rest[at..];
        let Some(end) = after.find(';').filter(|e| *e <= 10) else {
            out.push('&');
            rest = &after[1..];
            continue;
        };
        let entity = &after[1..end];
        let decoded = match entity {
            "lt" => Some('<'),
            "gt" => Some('>'),
            "amp" => Some('&'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &after[end + 1..];
            }
            None => {
                out.push('&');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Escapes text for an element's content or an attribute.
pub fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The document's root element.
pub fn parse(text: &str) -> Result<Node, String> {
    let bad = || "the server's answer isn't XML ferrule can read".to_string();
    let mut stack: Vec<Node> = vec![Node::default()];
    let mut rest = text;
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix("<!--") {
            rest = &r[r.find("-->").ok_or_else(bad)? + 3..];
        } else if let Some(r) = rest.strip_prefix("<![CDATA[") {
            let end = r.find("]]>").ok_or_else(bad)?;
            stack.last_mut().ok_or_else(bad)?.text.push_str(&r[..end]);
            rest = &r[end + 3..];
        } else if let Some(r) = rest.strip_prefix("<?") {
            rest = &r[r.find("?>").ok_or_else(bad)? + 2..];
        } else if let Some(r) = rest.strip_prefix("<!") {
            rest = &r[r.find('>').ok_or_else(bad)? + 1..];
        } else if let Some(r) = rest.strip_prefix("</") {
            let end = r.find('>').ok_or_else(bad)?;
            let node = stack.pop().ok_or_else(bad)?;
            if stack.is_empty() || node.name != local(r[..end].trim()) {
                return Err(bad());
            }
            stack.last_mut().ok_or_else(bad)?.children.push(node);
            rest = &r[end + 1..];
        } else if let Some(r) = rest.strip_prefix('<') {
            let end = tag_end(r).ok_or_else(bad)?;
            let inner = &r[..end];
            let (inner, closed) = match inner.strip_suffix('/') {
                Some(i) => (i, true),
                None => (inner, false),
            };
            let node = element(inner);
            if closed {
                stack.last_mut().ok_or_else(bad)?.children.push(node);
            } else {
                stack.push(node);
            }
            rest = &r[end + 1..];
        } else {
            let end = rest.find('<').unwrap_or(rest.len());
            stack
                .last_mut()
                .ok_or_else(bad)?
                .text
                .push_str(&unescape(&rest[..end]));
            rest = &rest[end..];
        }
    }
    if stack.len() != 1 {
        return Err(bad());
    }
    let doc = stack.pop().ok_or_else(bad)?;
    doc.children.into_iter().next().ok_or_else(bad)
}

/// Where a tag's `>` is, outside quoted attribute values.
fn tag_end(s: &str) -> Option<usize> {
    let mut quote = None;
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), _) if c == q => quote = None,
            (None, '>') => return Some(i),
            _ => {}
        }
    }
    None
}

/// `name a="b" c='d'` as an element (namespace declarations left out).
fn element(inner: &str) -> Node {
    let inner = inner.trim();
    let split = inner
        .find(|c: char| c.is_whitespace())
        .unwrap_or(inner.len());
    let mut node = Node {
        name: local(&inner[..split]),
        ..Default::default()
    };
    let mut rest = inner[split..].trim_start();
    while let Some(eq) = rest.find('=') {
        let name = rest[..eq].trim();
        let after = rest[eq + 1..].trim_start();
        let Some(q) = after.chars().next().filter(|c| *c == '"' || *c == '\'') else {
            break;
        };
        let Some(close) = after[1..].find(q) else {
            break;
        };
        let value = unescape(&after[1..1 + close]);
        if name != "xmlns" && !name.starts_with("xmlns:") {
            node.attrs.push((local(name), value));
        }
        rest = after[close + 2..].trim_start();
    }
    node
}
