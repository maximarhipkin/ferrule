//! Paste-back, the last resort: the browser lands on a page that doesn't
//! load (the loopback redirect), and the owner pastes its address into the
//! chat. Only `state` and `code` (or `error`) are taken from it.

/// What a pasted redirect carried.
#[derive(Debug, PartialEq)]
pub struct Pasted {
    pub state: String,
    pub code: Option<String>,
    pub error: Option<String>,
}

/// Finds a redirect URL in `text` that carries a `state`. Anything else in
/// the text is ignored.
pub fn parse(text: &str) -> Option<Pasted> {
    text.split_whitespace().find_map(|word| {
        let word = word.trim_matches(|c: char| matches!(c, '<' | '>' | '"' | '\'' | '`'));
        let url = url::Url::parse(word).ok()?;
        if !matches!(url.scheme(), "http" | "https") {
            return None;
        }
        let mut state = None;
        let mut code = None;
        let mut error = None;
        for (k, v) in url.query_pairs() {
            match &*k {
                "state" => state = Some(v.into_owned()),
                "code" => code = Some(v.into_owned()),
                "error" => error = Some(v.into_owned()),
                _ => {}
            }
        }
        let state = state?;
        if code.is_none() && error.is_none() {
            return None;
        }
        Some(Pasted { state, code, error })
    })
}

/// Whether a chat message looks like a pasted redirect, so the channel
/// can keep it away from the model even when it doesn't match a flow.
pub fn looks_like_redirect(text: &str) -> bool {
    parse(text).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pasted_redirect_gives_state_and_code() {
        let p = parse("here: <http://127.0.0.1:8976/callback?code=abc%2F1&state=S123&iss=x> ok")
            .unwrap();
        assert_eq!(p.state, "S123");
        assert_eq!(p.code.as_deref(), Some("abc/1"));
        let e = parse("http://127.0.0.1:8976/callback?error=access_denied&state=S").unwrap();
        assert_eq!(e.error.as_deref(), Some("access_denied"));
        assert!(
            parse("http://127.0.0.1:8976/callback?code=abc").is_none(),
            "no state"
        );
        assert!(parse("just words ?code=1&state=2").is_none());
        assert!(!looks_like_redirect("https://example.com/page"));
    }
}
