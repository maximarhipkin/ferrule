//! M57.7: a forked child (`spawn_agent {fork: true}`) starts from the end
//! of its parent's conversation instead of from its task alone. The copy
//! is read from the parent's transcript, folds applied, so it is what the
//! parent's model sees; it is cut to a token budget, never between a tool
//! call and its results, and the supervisor logs it into the child's own
//! transcript, so a resumed child replays the same history.

use ferrule_core::message::{Message, Role};

/// The end of `history` a forked child starts from, about `budget`
/// estimated tokens at most, and how many messages it was taken from.
/// System prompts are left out: the child has its own.
pub(crate) fn tail(history: Vec<Message>, budget: usize) -> (Vec<Message>, usize) {
    let mut messages: Vec<Message> = history
        .into_iter()
        .filter(|m| m.role != Role::System)
        .collect();
    // The parent is in the middle of the turn that calls spawn_agent: that
    // call has no result yet, and the turn isn't the child's to finish.
    if let Some(at) = messages.iter().rposition(|m| !m.tool_calls.is_empty()) {
        let answered = |id: &str| {
            messages[at + 1..]
                .iter()
                .any(|m| m.tool_call_id.as_deref() == Some(id))
        };
        if !messages[at].tool_calls.iter().all(|c| answered(&c.id)) {
            messages.truncate(at);
        }
    }
    let total = messages.len();
    // At ~4 chars a token, one message's text gets at most a quarter of
    // the budget, so a single big read can't crowd out everything before.
    let longest = budget;
    let mut used = 0;
    let mut start = messages.len();
    while start > 0 {
        let m = &mut messages[start - 1];
        shrink(m, longest);
        let cost = m.est_tokens();
        if used + cost > budget {
            break;
        }
        used += cost;
        start -= 1;
    }
    // Never a result whose call was left out: the copy opens with the
    // first message after them.
    while start < messages.len() && messages[start].role == Role::Tool {
        start += 1;
    }
    (messages.split_off(start), total)
}

/// Drops what belongs to the parent's own model (its reasoning, and the
/// provider blocks bound to the prefix they were made under, as a
/// compaction does for its kept tail) and cuts the text at `max` chars.
fn shrink(m: &mut Message, max: usize) {
    m.reasoning = None;
    m.native = None;
    let Some(text) = &m.content else {
        return;
    };
    let n = text.chars().count();
    if n > max {
        let mut cut: String = text.chars().take(max).collect();
        cut.push_str(&format!(
            "\n…[cut at {max} of {n} chars for the fork; the rest is in the parent's transcript]"
        ));
        m.content = Some(cut);
    }
}
