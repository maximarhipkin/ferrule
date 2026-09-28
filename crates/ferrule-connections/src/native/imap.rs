//! Just enough IMAP4rev1 for Gmail: log in, find All Mail, search with
//! Gmail's own syntax (`X-GM-RAW`), fetch. Generic over the stream so the
//! tests talk to a plain-TCP mock.

use super::net::{Stream, TIMEOUT};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

/// One untagged or tagged line, with the literals it carried.
#[derive(Debug, Default)]
pub struct Line {
    pub text: String,
    pub literals: Vec<Vec<u8>>,
}

pub struct Imap {
    io: BufReader<Box<dyn Stream>>,
    tag: u32,
}

/// Why a command failed: the server's `NO`/`BAD` text (never a password),
/// or the connection broke.
#[derive(Debug)]
pub enum Error {
    No(String),
    Broken(String),
}

impl Error {
    pub fn text(&self) -> &str {
        match self {
            Error::No(t) | Error::Broken(t) => t,
        }
    }
}

/// A string argument: quoted, or `None` when it needs a literal.
fn quoted(s: &str) -> Option<String> {
    if s.is_ascii() && !s.contains(['\r', '\n']) {
        Some(format!(
            "\"{}\"",
            s.replace('\\', "\\\\").replace('"', "\\\"")
        ))
    } else {
        None
    }
}

impl Imap {
    /// Reads the greeting.
    pub async fn start(stream: Box<dyn Stream>) -> Result<Self, Error> {
        let mut me = Self {
            io: BufReader::new(stream),
            tag: 0,
        };
        let hello = me.line().await?;
        if !hello.text.starts_with("* OK") && !hello.text.starts_with("* PREAUTH") {
            return Err(Error::Broken("the mail server didn't greet ferrule".into()));
        }
        Ok(me)
    }

    async fn raw_line(&mut self) -> Result<String, Error> {
        let mut buf = Vec::new();
        let n = tokio::time::timeout(TIMEOUT, self.io.read_until(b'\n', &mut buf))
            .await
            .map_err(|_| Error::Broken("the mail server stopped answering".into()))?
            .map_err(|_| Error::Broken("the connection to the mail server broke".into()))?;
        if n == 0 {
            return Err(Error::Broken(
                "the mail server closed the connection".into(),
            ));
        }
        Ok(String::from_utf8_lossy(&buf)
            .trim_end_matches(['\r', '\n'])
            .to_string())
    }

    /// A line and any `{n}` literals it announces.
    async fn line(&mut self) -> Result<Line, Error> {
        let mut out = Line::default();
        let mut text = self.raw_line().await?;
        loop {
            let Some(n) = literal_len(&text) else {
                out.text.push_str(&text);
                return Ok(out);
            };
            out.text.push_str(&text);
            let mut lit = vec![0u8; n];
            tokio::time::timeout(TIMEOUT, self.io.read_exact(&mut lit))
                .await
                .map_err(|_| Error::Broken("the mail server stopped answering".into()))?
                .map_err(|_| Error::Broken("the connection to the mail server broke".into()))?;
            out.literals.push(lit);
            text = self.raw_line().await?;
        }
    }

    async fn write(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let w = self.io.get_mut();
        tokio::time::timeout(TIMEOUT, async {
            w.write_all(bytes).await?;
            w.flush().await
        })
        .await
        .map_err(|_| Error::Broken("the mail server stopped answering".into()))?
        .map_err(|_| Error::Broken("the connection to the mail server broke".into()))
    }

    /// Sends a command made of `parts`: each is an atom (sent as is) or a
    /// string (quoted, or a literal when it isn't plain ASCII). Returns the
    /// untagged lines.
    pub async fn command(&mut self, parts: &[Part<'_>]) -> Result<Vec<Line>, Error> {
        self.tag += 1;
        let tag = format!("a{}", self.tag);
        let mut pending = tag.clone();
        for p in parts {
            pending.push(' ');
            match p {
                Part::Atom(a) => pending.push_str(a),
                Part::Str(s) => match quoted(s) {
                    Some(q) => pending.push_str(&q),
                    None => {
                        let bytes = s.replace(['\r', '\n'], " ");
                        pending.push_str(&format!("{{{}}}\r\n", bytes.len()));
                        self.write(pending.as_bytes()).await?;
                        pending.clear();
                        let go = self.line().await?;
                        if !go.text.starts_with('+') {
                            return Err(Error::No(tagged_text(&go.text)));
                        }
                        pending.push_str(&bytes);
                    }
                },
            }
        }
        pending.push_str("\r\n");
        self.write(pending.as_bytes()).await?;
        let mut untagged = Vec::new();
        loop {
            let line = self.line().await?;
            if let Some(rest) = line.text.strip_prefix(&format!("{tag} ")) {
                return if rest.starts_with("OK") {
                    Ok(untagged)
                } else {
                    Err(Error::No(tagged_text(rest)))
                };
            }
            untagged.push(line);
        }
    }

    pub async fn login(&mut self, user: &str, password: &str) -> Result<(), Error> {
        self.command(&[Part::Atom("LOGIN"), Part::Str(user), Part::Str(password)])
            .await
            .map(|_| ())
    }

    /// The mailbox with every message: the one flagged `\All`, else INBOX.
    /// Gmail names it in the account's language, so it's found, not typed.
    pub async fn all_mail(&mut self) -> Result<String, Error> {
        let lines = self
            .command(&[Part::Atom("LIST"), Part::Str(""), Part::Str("*")])
            .await?;
        for l in &lines {
            if l.text.contains("\\All") {
                if let Some(lit) = l.literals.first() {
                    return Ok(from_utf7_imap(&String::from_utf8_lossy(lit)));
                }
                if let Some(name) = last_string(&l.text) {
                    return Ok(from_utf7_imap(&name));
                }
            }
        }
        Ok("INBOX".into())
    }

    pub async fn examine(&mut self, mailbox: &str) -> Result<(), Error> {
        let name = utf7_imap(mailbox);
        self.command(&[Part::Atom("EXAMINE"), Part::Str(&name)])
            .await
            .map(|_| ())
    }

    /// UIDs matching Gmail's search syntax, oldest first.
    pub async fn search(&mut self, query: &str) -> Result<Vec<u32>, Error> {
        let mut parts = vec![Part::Atom("UID SEARCH")];
        if !query.is_ascii() {
            parts.push(Part::Atom("CHARSET UTF-8"));
        }
        parts.push(Part::Atom("X-GM-RAW"));
        parts.push(Part::Str(query));
        let lines = self.command(&parts).await?;
        let mut uids: Vec<u32> = lines
            .iter()
            .filter_map(|l| l.text.strip_prefix("* SEARCH"))
            .flat_map(|r| r.split_whitespace().filter_map(|n| n.parse().ok()))
            .collect();
        uids.sort_unstable();
        Ok(uids)
    }

    /// `(uid, the literal)` for each of `uids`, fetching `what`
    /// (`BODY.PEEK[]` or a header subset).
    pub async fn fetch(&mut self, uids: &[u32], what: &str) -> Result<Vec<(u32, Vec<u8>)>, Error> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let set: Vec<String> = uids.iter().map(|u| u.to_string()).collect();
        let set = set.join(",");
        let items = format!("(UID {what})");
        let lines = self
            .command(&[
                Part::Atom("UID FETCH"),
                Part::Atom(&set),
                Part::Atom(&items),
            ])
            .await?;
        let mut out = Vec::new();
        for l in lines {
            let Some(uid) = l
                .text
                .split("UID ")
                .nth(1)
                .and_then(|r| r.split(|c: char| !c.is_ascii_digit()).next())
                .and_then(|n| n.parse().ok())
            else {
                continue;
            };
            if let Some(lit) = l.literals.into_iter().next() {
                out.push((uid, lit));
            }
        }
        Ok(out)
    }

    /// The server's capabilities, upper-cased (`IDLE`, `STARTTLS`, …).
    pub async fn capabilities(&mut self) -> Result<Vec<String>, Error> {
        let lines = self.command(&[Part::Atom("CAPABILITY")]).await?;
        Ok(lines
            .iter()
            .filter_map(|l| l.text.strip_prefix("* CAPABILITY"))
            .flat_map(|r| r.split_whitespace().map(|c| c.to_ascii_uppercase()))
            .collect())
    }

    /// STARTTLS on a plain connection: the same session, secured.
    pub async fn starttls(mut self, host: &str) -> Result<Self, Error> {
        self.command(&[Part::Atom("STARTTLS")]).await?;
        // Nothing may follow the OK before the handshake, so the buffer is
        // empty.
        let plain = self.io.into_inner();
        let secure = super::net::upgrade(plain, host)
            .await
            .map_err(Error::Broken)?;
        Ok(Self {
            io: BufReader::new(secure),
            tag: self.tag,
        })
    }

    /// Opens `mailbox` read-write: its `(UIDVALIDITY, UIDNEXT)`.
    pub async fn select(&mut self, mailbox: &str) -> Result<(u32, u32), Error> {
        let name = utf7_imap(mailbox);
        let lines = self
            .command(&[Part::Atom("SELECT"), Part::Str(&name)])
            .await?;
        let code = |key: &str| {
            lines.iter().find_map(|l| {
                let at = l.text.find(&format!("[{key} "))?;
                l.text[at + key.len() + 2..]
                    .split(|c: char| !c.is_ascii_digit())
                    .next()?
                    .parse()
                    .ok()
            })
        };
        Ok((
            code("UIDVALIDITY").unwrap_or(0),
            code("UIDNEXT").unwrap_or(0),
        ))
    }

    /// UIDs above `after`, oldest first (`UID n:*` names the newest mail
    /// even when it's older than `n`, so that one is left out).
    pub async fn uids_after(&mut self, after: u32) -> Result<Vec<u32>, Error> {
        let range = format!("{}:*", after.saturating_add(1));
        let lines = self
            .command(&[Part::Atom("UID SEARCH UID"), Part::Atom(&range)])
            .await?;
        let mut uids: Vec<u32> = lines
            .iter()
            .filter_map(|l| l.text.strip_prefix("* SEARCH"))
            .flat_map(|r| r.split_whitespace().filter_map(|n| n.parse().ok()))
            .filter(|&u| u > after)
            .collect();
        uids.sort_unstable();
        Ok(uids)
    }

    /// Marks `uid` read.
    pub async fn mark_seen(&mut self, uid: u32) -> Result<(), Error> {
        let uid = uid.to_string();
        self.command(&[
            Part::Atom("UID STORE"),
            Part::Atom(&uid),
            Part::Atom("+FLAGS.SILENT (\\Seen)"),
        ])
        .await
        .map(|_| ())
    }

    /// `NOOP`: keeps the session and asks for news.
    pub async fn noop(&mut self) -> Result<(), Error> {
        self.command(&[Part::Atom("NOOP")]).await.map(|_| ())
    }

    /// RFC 2177 IDLE for at most `max`: `true` when new mail arrived
    /// (`* n EXISTS`), `false` when the time ran out.
    pub async fn idle(&mut self, max: std::time::Duration) -> Result<bool, Error> {
        self.tag += 1;
        let tag = format!("a{}", self.tag);
        self.write(format!("{tag} IDLE\r\n").as_bytes()).await?;
        let go = self.line().await?;
        if !go.text.starts_with('+') {
            return Err(Error::No(tagged_text(
                go.text.strip_prefix(&format!("{tag} ")).unwrap_or(&go.text),
            )));
        }
        let until = tokio::time::Instant::now() + max;
        let mut woke = false;
        // Outside the loop: a line cut off by the timeout isn't lost.
        let mut buf = Vec::new();
        while !woke {
            buf.clear();
            let read = tokio::time::timeout_at(until, self.io.read_until(b'\n', &mut buf)).await;
            match read {
                Err(_) => break,
                Ok(Err(_)) | Ok(Ok(0)) => {
                    return Err(Error::Broken(
                        "the mail server closed the connection".into(),
                    ))
                }
                Ok(Ok(_)) => {
                    let line = String::from_utf8_lossy(&buf);
                    woke = line.starts_with("* ") && line.trim_end().ends_with("EXISTS");
                }
            }
        }
        self.write(b"DONE\r\n").await?;
        loop {
            let line = self.line().await?;
            if let Some(rest) = line.text.strip_prefix(&format!("{tag} ")) {
                return if rest.starts_with("OK") {
                    Ok(woke)
                } else {
                    Err(Error::No(tagged_text(rest)))
                };
            }
            if line.text.starts_with("* ") && line.text.ends_with("EXISTS") {
                woke = true;
            }
        }
    }

    pub async fn logout(mut self) {
        let _ = self.command(&[Part::Atom("LOGOUT")]).await;
    }
}

pub enum Part<'a> {
    Atom(&'a str),
    Str(&'a str),
}

fn literal_len(line: &str) -> Option<usize> {
    let open = line.rfind('{')?;
    let inner = line[open + 1..].strip_suffix('}')?;
    inner.trim_end_matches('+').parse().ok()
}

/// A tagged `NO`/`BAD` line's text without the status word.
fn tagged_text(rest: &str) -> String {
    rest.trim_start_matches("NO")
        .trim_start_matches("BAD")
        .trim()
        .to_string()
}

fn last_string(text: &str) -> Option<String> {
    let t = text.trim_end();
    if let Some(body) = t.strip_suffix('"') {
        let start = body.rfind(" \"")?;
        return Some(
            body[start + 2..]
                .replace("\\\"", "\"")
                .replace("\\\\", "\\"),
        );
    }
    t.rsplit(' ').next().map(String::from)
}

/// Modified UTF-7 (RFC 3501 §5.1.3), for a mailbox name that isn't ASCII
/// — and back.
pub fn utf7_imap(name: &str) -> String {
    use base64::Engine;
    let mut out = String::new();
    let mut run: Vec<u16> = Vec::new();
    let flush = |run: &mut Vec<u16>, out: &mut String| {
        if run.is_empty() {
            return;
        }
        let bytes: Vec<u8> = run.iter().flat_map(|u| u.to_be_bytes()).collect();
        let b = base64::engine::general_purpose::STANDARD_NO_PAD.encode(bytes);
        out.push('&');
        out.push_str(&b.replace('/', ","));
        out.push('-');
        run.clear();
    };
    for c in name.chars() {
        if (' '..='~').contains(&c) {
            flush(&mut run, &mut out);
            out.push(c);
            if c == '&' {
                out.push('-');
            }
        } else {
            let mut buf = [0u16; 2];
            run.extend_from_slice(c.encode_utf16(&mut buf));
        }
    }
    flush(&mut run, &mut out);
    out
}

pub fn from_utf7_imap(name: &str) -> String {
    use base64::Engine;
    let mut out = String::new();
    let mut rest = name;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        let Some(end) = after.find('-') else {
            out.push_str(&rest[at..]);
            return out;
        };
        let chunk = &after[..end];
        if chunk.is_empty() {
            out.push('&');
        } else if let Ok(bytes) =
            base64::engine::general_purpose::STANDARD_NO_PAD.decode(chunk.replace(',', "/"))
        {
            let units: Vec<u16> = bytes
                .chunks(2)
                .filter(|c| c.len() == 2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            out.push_str(&String::from_utf16_lossy(&units));
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mailbox_names_round_trip_through_modified_utf7() {
        for name in ["INBOX", "[Gmail]/All Mail", "[Gmail]/כל הדואר", "A&B"] {
            assert_eq!(from_utf7_imap(&utf7_imap(name)), name);
        }
        assert_eq!(utf7_imap("A&B"), "A&-B");
    }

    #[test]
    fn literals_and_strings_are_read() {
        assert_eq!(literal_len("* 1 FETCH (UID 5 BODY[] {12}"), Some(12));
        assert_eq!(literal_len("* OK"), None);
        assert_eq!(
            last_string(r#"* LIST (\HasNoChildren \All) "/" "[Gmail]/All Mail""#).unwrap(),
            "[Gmail]/All Mail"
        );
        assert_eq!(quoted("a\"b").unwrap(), r#""a\"b""#);
        assert!(quoted("שלום").is_none());
    }
}
