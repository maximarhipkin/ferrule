//! What an inbound mail is before anything answers it (M39 §5): a loop or a
//! list (never answered), a stranger (dropped), and whether the receiving
//! server vouched for its `From:`. Plus the reply's own text: the quoted
//! history cut off, and an approval's first line alone.

/// Why a mail is never answered, or `None` when it may be.
pub fn loop_reason(h: &[(String, String)], own_loop_mark: &str) -> Option<&'static str> {
    let get = |name: &str| {
        h.iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.trim().to_ascii_lowercase())
    };
    if h.iter().any(|(n, _)| n == "x-ferrule-loop")
        || get("message-id").is_some_and(|id| id.contains(own_loop_mark))
    {
        return Some("ferrule's own mail");
    }
    if get("auto-submitted").is_some_and(|v| !v.is_empty() && !v.starts_with("no")) {
        return Some("an automatic mail (Auto-Submitted)");
    }
    if ["list-id", "list-unsubscribe", "list-post"]
        .iter()
        .any(|n| get(n).is_some())
    {
        return Some("a mailing list's mail");
    }
    if get("precedence").is_some_and(|v| matches!(v.as_str(), "bulk" | "list" | "junk")) {
        return Some("bulk mail (Precedence)");
    }
    if get("return-path").is_some_and(|v| v == "<>" || v.is_empty()) {
        return Some("a bounce (an empty Return-Path)");
    }
    if ["x-autoreply", "x-autorespond"]
        .iter()
        .any(|n| get(n).is_some())
        || get("x-auto-response-suppress").is_some_and(|v| v.contains("all"))
    {
        return Some("an autoresponder's mail");
    }
    let from = get("from").map(|f| bare(&f)).unwrap_or_default();
    let local = from.split('@').next().unwrap_or("");
    let local = local.replace(['-', '_', '.'], "");
    if matches!(local.as_str(), "mailerdaemon" | "postmaster")
        || local.starts_with("noreply")
        || local.starts_with("donotreply")
    {
        return Some("a system address (bounces, no-reply)");
    }
    None
}

/// The bare, lower-cased address in `Name <a@b>`.
pub fn bare(from: &str) -> String {
    ferrule_connections::native::mime::bare(from).to_ascii_lowercase()
}

/// The display name in `Name <a@b>`, else the address.
pub fn display_name(from: &str) -> String {
    match from.rfind('<') {
        Some(at) if at > 0 => {
            let name = from[..at].trim().trim_matches('"').trim();
            if name.is_empty() {
                bare(from)
            } else {
                name.to_string()
            }
        }
        _ => bare(from),
    }
}

/// Whether `address` is on `allowed` (addresses and `@domain` entries,
/// both lower-cased).
pub fn allowed(allowed: &[String], address: &str) -> bool {
    let address = address.to_ascii_lowercase();
    let domain = address.rsplit_once('@').map(|(_, d)| d).unwrap_or("");
    allowed.iter().any(|a| {
        let a = a.trim().to_ascii_lowercase();
        match a.strip_prefix('@') {
            Some(d) => !domain.is_empty() && d == domain,
            None => a == address,
        }
    })
}

/// Whether the receiving server's own `Authentication-Results` (the
/// topmost one: a sender can add their own below it) vouches for the
/// `From:` domain: DMARC passed, or SPF or DKIM passed for that domain.
pub fn authenticated(h: &[(String, String)], from_domain: &str) -> bool {
    let Some((_, ar)) = h.iter().find(|(n, _)| n == "authentication-results") else {
        return false;
    };
    let ar = ar.to_ascii_lowercase();
    let from_domain = from_domain.to_ascii_lowercase();
    if from_domain.is_empty() {
        return false;
    }
    let aligned = |d: &str| {
        let d = d.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.' && c != '-');
        d == from_domain || from_domain.ends_with(&format!(".{d}"))
    };
    // `method=result` then its properties up to the next `;`.
    for clause in ar.split(';').skip(1) {
        let clause = clause.trim();
        let mut words = clause.split_whitespace();
        let Some((method, result)) = words.next().and_then(|w| w.split_once('=')) else {
            continue;
        };
        if result != "pass" {
            continue;
        }
        let prop = |key: &str| -> Option<String> {
            clause.split_whitespace().find_map(|w| {
                w.strip_prefix(key)
                    .map(|v| v.rsplit('@').next().unwrap_or(v).to_string())
            })
        };
        let ok = match method {
            "dmarc" => prop("header.from=").is_none_or(|d| aligned(&d)),
            "spf" => prop("smtp.mailfrom=").is_some_and(|d| aligned(&d)),
            "dkim" => prop("header.d=")
                .or_else(|| prop("header.i="))
                .is_some_and(|d| aligned(&d)),
            _ => false,
        };
        if ok {
            return true;
        }
    }
    false
}

/// The reply's own words: the quoted history (`On … wrote:`, `>` lines,
/// Outlook's divider) and the `-- ` signature cut off.
pub fn cut_quoted(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut keep = lines.len();
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        let tl = t.to_ascii_lowercase();
        let header = tl.ends_with("wrote:")
            || t.ends_with("כתב:")
            || t.ends_with("כתב/ה:")
            || (t.contains("בתאריך") && t.contains("מאת"))
            || tl.ends_with("schrieb:");
        if header {
            // Gmail wraps `On …, Name <` / `a@b> wrote:` over two lines.
            keep = if i > 0 && lines[i - 1].trim_start().starts_with("On ") {
                i - 1
            } else {
                i
            };
            break;
        }
        if t.starts_with('>')
            || *line == "-- "
            || t == "--"
            || tl.starts_with("-----original message-----")
            || (t.len() >= 20 && t.chars().all(|c| c == '_'))
        {
            keep = i;
            break;
        }
    }
    lines[..keep].join("\n").trim().to_string()
}

/// The first line alone when it answers an approval (`yes`, `yes a1`,
/// `no a1`), so a signature or anything under it doesn't turn the answer
/// into "something else" (which refuses every open question).
pub fn approval_line(text: &str) -> Option<String> {
    let first = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    let t = first.trim_end_matches(['.', '!']).trim();
    let mut words = t.split_whitespace();
    let word = words.next()?.to_ascii_lowercase();
    let code = words.next();
    if words.next().is_some() || !matches!(word.as_str(), "yes" | "no") {
        return None;
    }
    if code.is_some_and(|c| c.len() > 8 || !c.chars().all(|c| c.is_ascii_alphanumeric())) {
        return None;
    }
    Some(t.to_string())
}

/// `Re: <subject>` without piling up `Re: Re:`.
pub fn re(subject: &str) -> String {
    let mut s = subject.trim();
    loop {
        let lower = s.to_ascii_lowercase();
        let cut = ["re:", "aw:", "fwd:", "fw:"]
            .iter()
            .find(|p| lower.starts_with(*p))
            .map(|p| p.len());
        match cut {
            Some(n) => s = s[n..].trim_start(),
            None => break,
        }
    }
    format!("Re: {s}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn lists_bounces_autoreplies_and_our_own_mail_are_never_answered() {
        let ok = h(&[("from", "Max <max@example.com>"), ("message-id", "<1@x>")]);
        assert_eq!(loop_reason(&ok, ".ferrule@"), None);
        let no = h(&[("from", "max@example.com"), ("auto-submitted", "no")]);
        assert_eq!(loop_reason(&no, ".ferrule@"), None);
        for (name, value) in [
            ("auto-submitted", "auto-replied"),
            ("list-id", "<dev.lists.example.com>"),
            ("list-unsubscribe", "<mailto:u@x>"),
            ("precedence", "bulk"),
            ("return-path", "<>"),
            ("x-autoreply", "yes"),
            ("x-auto-response-suppress", "All"),
            ("x-ferrule-loop", "default"),
            ("message-id", "<3f2a.ferrule@example.com>"),
        ] {
            let mut m = ok.clone();
            m.retain(|(n, _)| n != name);
            m.push((name.into(), value.into()));
            assert!(loop_reason(&m, ".ferrule@").is_some(), "{name}: {value}");
        }
        for from in [
            "MAILER-DAEMON@example.com",
            "postmaster@example.com",
            "No-Reply <no-reply@shop.com>",
            "noreply@github.com",
            "do_not_reply@bank.com",
        ] {
            assert!(
                loop_reason(&h(&[("from", from)]), ".ferrule@").is_some(),
                "{from}"
            );
        }
    }

    #[test]
    fn addresses_and_domains_are_allowed() {
        let list = vec!["Max@Example.com".to_string(), "@team.org".to_string()];
        assert!(allowed(&list, "max@example.com"));
        assert!(allowed(&list, "anyone@team.org"));
        assert!(!allowed(&list, "anyone@evil-team.org"));
        assert!(!allowed(&list, "max@example.com.evil"));
        assert!(!allowed(&list, "team.org"));
        assert_eq!(bare("Max <Max@Example.com>"), "max@example.com");
        assert_eq!(display_name("\"Max A.\" <m@x>"), "Max A.");
        assert_eq!(display_name("m@x"), "m@x");
    }

    #[test]
    fn only_the_receiving_servers_own_results_vouch_for_the_sender() {
        let gmail = "mx.google.com; dkim=pass header.i=@example.com header.s=s1; spf=pass (google.com: domain of max@example.com designates 1.2.3.4) smtp.mailfrom=max@example.com; dmarc=pass (p=NONE) header.from=example.com";
        assert!(authenticated(
            &h(&[("authentication-results", gmail)]),
            "example.com"
        ));
        // Passing for another domain vouches for nothing.
        let other = "mx.google.com; spf=pass smtp.mailfrom=bounce@evil.com; dkim=pass header.d=evil.com; dmarc=fail header.from=example.com";
        assert!(!authenticated(
            &h(&[("authentication-results", other)]),
            "example.com"
        ));
        // A forged header under the receiver's own doesn't count.
        let forged = h(&[
            (
                "authentication-results",
                "mx.google.com; spf=softfail smtp.mailfrom=x@example.com; dmarc=fail header.from=example.com",
            ),
            (
                "authentication-results",
                "mx.google.com; dmarc=pass header.from=example.com",
            ),
        ]);
        assert!(!authenticated(&forged, "example.com"));
        assert!(!authenticated(&h(&[]), "example.com"));
        let sub = "mx; dkim=pass header.d=example.com";
        assert!(authenticated(
            &h(&[("authentication-results", sub)]),
            "mail.example.com"
        ));
    }

    #[test]
    fn the_quoted_history_and_signature_are_cut() {
        let gmail = "Sounds good, go ahead.\n\nOn Mon, Sep 28, 2026 at 10:00 AM Ferrule <\nbot@example.com> wrote:\n> the old text";
        assert_eq!(cut_quoted(gmail), "Sounds good, go ahead.");
        let one = "yes a2\n\nOn Mon, 28 Sep 2026, bot@example.com wrote:\n> Run it?";
        assert_eq!(cut_quoted(one), "yes a2");
        let sig = "Please check the logs.\n-- \nMax\nCEO";
        assert_eq!(cut_quoted(sig), "Please check the logs.");
        let outlook = "Done.\n\n-----Original Message-----\nFrom: bot";
        assert_eq!(cut_quoted(outlook), "Done.");
        let hebrew = "בסדר\n\nבתאריך יום ב׳, 28 בספט׳ 2026 ב-10:00 מאת Ferrule <bot@x.com>:\n> ישן";
        assert_eq!(cut_quoted(hebrew), "בסדר");
        assert_eq!(cut_quoted("a > b is fine"), "a > b is fine");
    }

    #[test]
    fn an_approval_is_the_first_line_alone() {
        assert_eq!(
            approval_line("yes a2\n\nSent from my phone"),
            Some("yes a2".into())
        );
        assert_eq!(approval_line("Yes!"), Some("Yes".into()));
        assert_eq!(approval_line("no k7."), Some("no k7".into()));
        assert_eq!(approval_line("yes, and also check the logs"), None);
        assert_eq!(approval_line("no worries at all"), None);
        assert_eq!(approval_line("hello"), None);
    }

    #[test]
    fn re_doesnt_pile_up() {
        assert_eq!(re("Re: RE: re: hi"), "Re: hi");
        assert_eq!(re("Fwd: news"), "Re: news");
        assert_eq!(re("hi"), "Re: hi");
    }
}
