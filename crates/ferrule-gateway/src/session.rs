/// Deterministic, filesystem-safe session id for a (channel, chat) pair.
/// Used both as the router's in-memory lane key and as the JSONL transcript
/// file stem, so a restart resumes the exact same session.
pub fn session_id(channel: &str, chat_id: &str) -> String {
    format!("{}__{}", sanitize(channel), sanitize(chat_id))
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_pair_is_deterministic() {
        assert_eq!(session_id("telegram", "12345"), session_id("telegram", "12345"));
    }

    #[test]
    fn different_chats_get_different_ids() {
        assert_ne!(session_id("telegram", "1"), session_id("telegram", "2"));
    }

    #[test]
    fn unsafe_characters_are_sanitized_for_filenames() {
        let id = session_id("local", "some/../weird:chat id");
        assert!(!id.contains('/'));
        assert!(!id.contains(':'));
        assert!(!id.contains(' '));
    }
}
