//! Just enough of RFC 5322/2045/2047 to read a mail and write one: the
//! headers the agent needs, the text of the body (the plain part, else the
//! HTML one without its tags), attachment names, and an outgoing message
//! whose body and subject survive any language.

use base64::Engine;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Message {
    pub from: String,
    pub to: String,
    pub cc: String,
    pub subject: String,
    pub date: String,
    pub message_id: String,
    pub references: String,
    pub text: String,
    pub attachments: Vec<String>,
}

/// Headers, unfolded, names lowercased, in order.
pub fn headers(raw: &[u8]) -> (Vec<(String, String)>, &[u8]) {
    let (head, body) = split(raw);
    let text = String::from_utf8_lossy(head);
    let mut out: Vec<(String, String)> = Vec::new();
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.starts_with([' ', '\t']) {
            if let Some(last) = out.last_mut() {
                last.1.push(' ');
                last.1.push_str(line.trim());
            }
        } else if let Some((name, value)) = line.split_once(':') {
            out.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    (out, body)
}

fn split(raw: &[u8]) -> (&[u8], &[u8]) {
    for (i, w) in raw.windows(2).enumerate() {
        if w == b"\n\n" {
            return (&raw[..i], &raw[i + 2..]);
        }
        if i + 4 <= raw.len() && &raw[i..i + 4] == b"\r\n\r\n" {
            return (&raw[..i], &raw[i + 4..]);
        }
    }
    (raw, &[])
}

fn get<'a>(h: &'a [(String, String)], name: &str) -> &'a str {
    h.iter()
        .find(|(n, _)| n == name)
        .map_or("", |(_, v)| v.as_str())
}

/// A parameter of a structured header: `boundary` of a Content-Type.
pub fn param(value: &str, name: &str) -> Option<String> {
    for part in value.split(';').skip(1) {
        let (k, v) = part.split_once('=')?;
        if k.trim().eq_ignore_ascii_case(name) {
            return Some(v.trim().trim_matches('"').to_string());
        }
    }
    None
}

/// Bytes in `charset` as text. UTF-8 and ASCII exactly, Latin-1 and the
/// Hebrew code pages' letters by table; anything else is read as UTF-8.
pub fn decode_charset(bytes: &[u8], charset: &str) -> String {
    let cs = charset.to_ascii_lowercase();
    let single = |b: u8| -> char {
        match b {
            0xE0..=0xFA if cs.contains("1255") || cs.contains("8859-8") => {
                char::from_u32(0x05D0 + u32::from(b - 0xE0)).unwrap_or('?')
            }
            _ => char::from(b),
        }
    };
    if cs.contains("8859") || cs.contains("1252") || cs.contains("1255") || cs == "latin1" {
        bytes.iter().map(|&b| single(b)).collect()
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

/// RFC 2047 encoded words (`=?UTF-8?B?…?=`) in a header, decoded.
pub fn decode_words(value: &str) -> String {
    let mut out = String::new();
    let mut rest = value;
    let mut last_was_word = false;
    while let Some(start) = rest.find("=?") {
        let (before, tail) = rest.split_at(start);
        let word = tail[2..].splitn(3, '?').collect::<Vec<_>>();
        let decoded = (word.len() == 3)
            .then(|| {
                let end = word[2].find("?=")?;
                let (charset, enc, text) = (word[0], word[1], &word[2][..end]);
                let bytes = match enc {
                    "B" | "b" => base64::engine::general_purpose::STANDARD
                        .decode(text.trim())
                        .ok()?,
                    "Q" | "q" => quoted_printable(&text.replace('_', " "), false),
                    _ => return None,
                };
                let used = 2 + charset.len() + 1 + enc.len() + 1 + end + 2;
                Some((decode_charset(&bytes, charset), used))
            })
            .flatten();
        match decoded {
            Some((text, used)) => {
                // Whitespace between two encoded words isn't text.
                if !(last_was_word && before.trim().is_empty()) {
                    out.push_str(before);
                }
                out.push_str(&text);
                rest = &tail[used..];
                last_was_word = true;
            }
            None => {
                out.push_str(before);
                out.push_str("=?");
                rest = &tail[2..];
                last_was_word = false;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Quoted-printable; `soft` joins `=` line ends (bodies, not headers).
pub fn quoted_printable(text: &str, soft: bool) -> Vec<u8> {
    let b = text.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'=' {
            if soft && b.get(i + 1) == Some(&b'\n') {
                i += 2;
                continue;
            }
            if soft && b.get(i + 1) == Some(&b'\r') && b.get(i + 2) == Some(&b'\n') {
                i += 3;
                continue;
            }
            if let Some(h) = b.get(i + 1..i + 3) {
                if let Ok(v) = u8::from_str_radix(&String::from_utf8_lossy(h), 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

fn transfer_decode(body: &[u8], encoding: &str) -> Vec<u8> {
    match encoding.to_ascii_lowercase().as_str() {
        "base64" => {
            let clean: Vec<u8> = body
                .iter()
                .copied()
                .filter(|c| !c.is_ascii_whitespace())
                .collect();
            base64::engine::general_purpose::STANDARD
                .decode(&clean)
                .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(&clean))
                .unwrap_or_default()
        }
        "quoted-printable" => quoted_printable(&String::from_utf8_lossy(body), true),
        _ => body.to_vec(),
    }
}

/// An HTML body as readable text: tags gone, block ends as line breaks,
/// the common entities decoded, blank runs collapsed.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::new();
    let mut rest = html;
    while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..lt]);
        let Some(gt) = rest[lt..].find('>') else {
            rest = "";
            break;
        };
        let tag = rest[lt + 1..lt + gt].to_ascii_lowercase();
        let name: String = tag
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        rest = &rest[lt + gt + 1..];
        if matches!(name.as_str(), "style" | "script" | "head") && !tag.starts_with('/') {
            let close = format!("</{name}");
            match rest.to_ascii_lowercase().find(&close) {
                Some(at) => rest = &rest[at..],
                None => rest = "",
            }
            continue;
        }
        if matches!(
            name.as_str(),
            "br" | "p" | "div" | "tr" | "li" | "h1" | "h2" | "h3" | "table"
        ) {
            out.push('\n');
        }
    }
    out.push_str(rest);
    let out = out
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&");
    let mut lines: Vec<&str> = Vec::new();
    for line in out.lines().map(str::trim) {
        if line.is_empty() && lines.last().is_none_or(|l| l.is_empty()) {
            continue;
        }
        lines.push(line);
    }
    lines.join("\n").trim().to_string()
}

struct Parts {
    plain: Option<String>,
    html: Option<String>,
    attachments: Vec<String>,
}

fn walk(raw: &[u8], parts: &mut Parts, depth: usize) {
    let (h, body) = headers(raw);
    let ctype = get(&h, "content-type");
    let ctype_l = ctype.to_ascii_lowercase();
    let disposition = get(&h, "content-disposition");
    if ctype_l.starts_with("multipart/") && depth < 8 {
        let Some(boundary) = param(ctype, "boundary") else {
            return;
        };
        let delim = format!("--{boundary}");
        let text = body;
        let mut pieces = Vec::new();
        let mut at = 0;
        let mut starts = Vec::new();
        while let Some(i) = find(&text[at..], delim.as_bytes()) {
            starts.push(at + i);
            at += i + delim.len();
        }
        for w in starts.windows(2) {
            let piece = &text[w[0] + delim.len()..w[1]];
            pieces.push(trim_crlf(piece));
        }
        for piece in pieces {
            walk(piece, parts, depth + 1);
        }
        return;
    }
    let name = param(disposition, "filename").or_else(|| param(ctype, "name"));
    if disposition.to_ascii_lowercase().starts_with("attachment") || name.is_some() {
        parts
            .attachments
            .push(decode_words(&name.unwrap_or_else(|| "(unnamed)".into())));
        return;
    }
    let charset = param(ctype, "charset").unwrap_or_else(|| "utf-8".into());
    let bytes = transfer_decode(body, get(&h, "content-transfer-encoding"));
    let text = decode_charset(&bytes, &charset);
    if ctype_l.is_empty() || ctype_l.starts_with("text/plain") {
        parts.plain.get_or_insert(text);
    } else if ctype_l.starts_with("text/html") {
        parts.html.get_or_insert(text);
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn trim_crlf(b: &[u8]) -> &[u8] {
    let b = b
        .strip_prefix(b"\r\n")
        .or(b.strip_prefix(b"\n"))
        .unwrap_or(b);
    b.strip_suffix(b"\r\n")
        .or(b.strip_suffix(b"\n"))
        .unwrap_or(b)
}

pub fn parse(raw: &[u8]) -> Message {
    let (h, _) = headers(raw);
    let mut parts = Parts {
        plain: None,
        html: None,
        attachments: Vec::new(),
    };
    walk(raw, &mut parts, 0);
    let text = match (parts.plain, parts.html) {
        (Some(p), _) if !p.trim().is_empty() => p,
        (_, Some(h)) => html_to_text(&h),
        (p, None) => p.unwrap_or_default(),
    };
    Message {
        from: decode_words(get(&h, "from")),
        to: decode_words(get(&h, "to")),
        cc: decode_words(get(&h, "cc")),
        subject: decode_words(get(&h, "subject")),
        date: get(&h, "date").to_string(),
        message_id: get(&h, "message-id").to_string(),
        references: get(&h, "references").to_string(),
        text: text.replace("\r\n", "\n").trim().to_string(),
        attachments: parts.attachments,
    }
}

/// A header value safe to send: non-ASCII becomes one encoded word.
fn encode_header(value: &str) -> String {
    let clean: String = value.chars().filter(|c| !c.is_control()).collect();
    if clean.is_ascii() {
        clean
    } else {
        format!(
            "=?UTF-8?B?{}?=",
            base64::engine::general_purpose::STANDARD.encode(clean.as_bytes())
        )
    }
}

/// Addresses as given, one line: no CR/LF can reach the headers.
pub fn clean_addresses(list: &[String]) -> Vec<String> {
    list.iter()
        .map(|a| a.chars().filter(|c| !c.is_control()).collect::<String>())
        .map(|a| a.trim().to_string())
        .filter(|a| !a.is_empty())
        .collect()
}

/// The bare address in `Name <a@b>` or `a@b`.
pub fn bare(address: &str) -> String {
    match (address.rfind('<'), address.rfind('>')) {
        (Some(l), Some(r)) if l < r => address[l + 1..r].trim().to_string(),
        _ => address.trim().to_string(),
    }
}

pub struct Outgoing<'a> {
    pub from: &'a str,
    pub to: &'a [String],
    pub cc: &'a [String],
    pub subject: &'a str,
    pub body: &'a str,
    pub in_reply_to: Option<&'a str>,
    pub references: Option<&'a str>,
    /// `<…@host>`; made up when `None`.
    pub message_id: Option<String>,
    pub date: String,
}

/// The whole message, CRLF line ends, body base64 so any text is safe.
pub fn build(m: &Outgoing) -> String {
    let mut out = String::new();
    let mut h = |name: &str, value: &str| {
        out.push_str(name);
        out.push_str(": ");
        out.push_str(value);
        out.push_str("\r\n");
    };
    h("From", &encode_header(m.from));
    h("To", &clean_addresses(m.to).join(", "));
    if !m.cc.is_empty() {
        h("Cc", &clean_addresses(m.cc).join(", "));
    }
    h("Subject", &encode_header(m.subject));
    h("Date", &m.date);
    let host = bare(m.from)
        .split('@')
        .nth(1)
        .unwrap_or("ferrule.local")
        .to_string();
    let id = m.message_id.clone().unwrap_or_else(|| {
        let r: [u8; 12] = crate::seal::random();
        format!(
            "<{}@{host}>",
            r.iter().map(|b| format!("{b:02x}")).collect::<String>()
        )
    });
    h("Message-ID", &id);
    if let Some(r) = m.in_reply_to.filter(|r| !r.is_empty()) {
        h("In-Reply-To", &encode_header(r));
        let refs = match m.references.filter(|r| !r.is_empty()) {
            Some(prev) => format!("{prev} {r}"),
            None => r.to_string(),
        };
        h("References", &encode_header(&refs));
    }
    h("MIME-Version", "1.0");
    h("Content-Type", "text/plain; charset=utf-8");
    h("Content-Transfer-Encoding", "base64");
    out.push_str("\r\n");
    let body = base64::engine::general_purpose::STANDARD.encode(m.body.as_bytes());
    for chunk in body.as_bytes().chunks(76) {
        out.push_str(std::str::from_utf8(chunk).unwrap_or(""));
        out.push_str("\r\n");
    }
    out
}

/// RFC 5322 date for now, in UTC.
pub fn date_now() -> String {
    let secs = crate::store::now() as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil(days);
    let wd = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][days.rem_euclid(7) as usize];
    let mon = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][(m - 1) as usize];
    format!(
        "{wd}, {d:02} {mon} {y} {:02}:{:02}:{:02} +0000",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 as (year, month, day).
pub fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_words_and_hebrew_code_pages_decode() {
        assert_eq!(
            decode_words("=?UTF-8?B?16nXnNeV150=?= =?UTF-8?Q?_world?="),
            "שלום world"
        );
        assert_eq!(
            decode_words("Re: =?iso-8859-8?Q?=F9=EC=E5=ED?="),
            "Re: שלום"
        );
        assert_eq!(decode_words("plain =? not a word"), "plain =? not a word");
    }

    #[test]
    fn a_multipart_mail_gives_its_plain_text_and_attachment_names() {
        let raw = "From: =?UTF-8?B?157Xp9eh?= <max@example.com>\r\nTo: a@b.c\r\nSubject: hi\r\n\
            Message-ID: <1@x>\r\nContent-Type: multipart/mixed; boundary=\"B1\"\r\n\r\n\
            --B1\r\nContent-Type: multipart/alternative; boundary=B2\r\n\r\n\
            --B2\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>html</p>\r\n\
            --B2\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\n\
            caf=C3=A9 =\r\nok\r\n--B2--\r\n\
            --B1\r\nContent-Type: application/pdf; name=\"inv.pdf\"\r\nContent-Disposition: attachment; filename=\"inv.pdf\"\r\n\r\nJVBE\r\n--B1--\r\n";
        let m = parse(raw.as_bytes());
        assert_eq!(m.from, "מקס <max@example.com>");
        assert_eq!(m.text, "café ok");
        assert_eq!(m.attachments, ["inv.pdf"]);
        assert_eq!(m.message_id, "<1@x>");
    }

    #[test]
    fn an_html_only_mail_reads_as_text() {
        let raw = b"Subject: x\nContent-Type: text/html\n\n<html><head><style>p{}</style></head><body><p>Hello&nbsp;there</p><br>bye &amp; thanks</body></html>";
        assert_eq!(parse(raw).text, "Hello there\n\nbye & thanks");
    }

    #[test]
    fn an_outgoing_mail_survives_any_language_and_no_header_injection() {
        let to = vec!["x@y.z\r\nBcc: evil@e.vil".to_string()];
        let text = build(&Outgoing {
            from: "max@example.com",
            to: &to,
            cc: &[],
            subject: "שלום",
            body: "גוף ההודעה",
            in_reply_to: Some("<1@x>"),
            references: None,
            message_id: Some("<2@x>".into()),
            date: date_now(),
        });
        assert!(!text.contains("\r\nBcc:"), "{text}");
        let m = parse(text.as_bytes());
        assert_eq!(m.subject, "שלום");
        assert_eq!(m.text, "גוף ההודעה");
        assert!(text.contains("In-Reply-To: <1@x>\r\nReferences: <1@x>\r\n"));
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(20_723), (2026, 9, 27));
    }
}
