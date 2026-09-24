//! M19b: the gateway is never silently deaf. What the owner can see from a
//! phone (a receipt, `/status`, a watchdog message, a restart notice) and
//! what watches the process from outside (systemd's watchdog, a heartbeat).
//! See docs/m19b-reliability.md.

use std::time::Duration;

/// Replaces secrets with `[redacted]` in anything the owner is shown:
/// `/status`, the status file, the heartbeat, the running marker. Knows
/// the configured secret values, and the shape of a Telegram bot token.
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    secrets: Vec<String>,
}

impl Redactor {
    /// `secrets`: values to hide. Short ones (under 6 characters) would
    /// hide ordinary words, so they're skipped.
    pub fn new(secrets: impl IntoIterator<Item = String>) -> Self {
        let mut secrets: Vec<String> = secrets.into_iter().filter(|s| s.len() >= 6).collect();
        // Longest first, so a secret containing another goes whole.
        secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        secrets.dedup();
        Self { secrets }
    }

    pub fn redact(&self, text: &str) -> String {
        let mut out = text.to_string();
        for s in &self.secrets {
            if out.contains(s.as_str()) {
                out = out.replace(s.as_str(), "[redacted]");
            }
        }
        redact_bot_tokens(&out)
    }
}

/// `123456789:AAH…` (a bot id, a colon, 30+ token characters).
fn redact_bot_tokens(text: &str) -> String {
    let b = text.as_bytes();
    let token_char = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'-';
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut copied = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() && (i == 0 || !b[i - 1].is_ascii_digit()) {
            let mut j = i;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if j - i >= 6 && j < b.len() && b[j] == b':' {
                let mut k = j + 1;
                while k < b.len() && token_char(b[k]) {
                    k += 1;
                }
                if k - j > 30 {
                    out.push_str(&text[copied..i]);
                    out.push_str("[redacted]");
                    copied = k;
                    i = k;
                    continue;
                }
            }
            i = j;
            continue;
        }
        i += 1;
    }
    out.push_str(&text[copied..]);
    out
}

/// The first `max` characters of `text`, with "…" when cut.
pub fn clip(text: &str, max: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// "45 s", "12 min", "3 h 5 min".
pub fn human(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s} s")
    } else if s < 3600 {
        format!("{} min", s / 60)
    } else {
        format!("{} h {} min", s / 3600, (s % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_hides_configured_values_and_bot_tokens() {
        let r = Redactor::new(["sk-live-abcdef".to_string(), "abc".to_string()]);
        let text = "key sk-live-abcdef, url https://api.telegram.org/bot123456789:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsawQ/getUpdates, abc stays";
        let out = r.redact(text);
        assert_eq!(
            out,
            "key [redacted], url https://api.telegram.org/bot[redacted]/getUpdates, abc stays"
        );
        // Times and ids aren't tokens.
        assert_eq!(
            r.redact("at 12:30, chat -100123456:x"),
            "at 12:30, chat -100123456:x"
        );
    }

    #[test]
    fn clip_and_human() {
        assert_eq!(clip("hello", 10), "hello");
        assert_eq!(clip("héllo world", 5), "héllo…");
        assert_eq!(human(Duration::from_secs(5)), "5 s");
        assert_eq!(human(Duration::from_secs(600)), "10 min");
        assert_eq!(human(Duration::from_secs(3900)), "1 h 5 min");
    }
}
