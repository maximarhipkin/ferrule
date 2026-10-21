//! M54.3: known secret values never reach the transcript or the model.
//!
//! One process-wide hook, installed by the host (ferrule-cli knows the
//! values: the credential proxy's bound secrets and the keys it holds), so
//! this crate needs nothing of the proxy. The loop applies it where a tool
//! call becomes its result (`agent.rs` `Ran::of`), which is what the
//! transcript, the model, the hooks, the trace and the stuck detector all
//! read; [`ToolOutput`](crate::tool::ToolOutput)'s caps and spills apply it
//! before they cut, so a cut never leaves part of a key. With no hook
//! installed (tests, a library user) text passes unchanged.

use std::sync::{Arc, PoisonError, RwLock};

/// Text in, the same text with every known secret value replaced.
pub type Scrubber = Arc<dyn Fn(&str) -> String + Send + Sync>;

static HOOK: RwLock<Option<Scrubber>> = RwLock::new(None);

/// Sets the process's scrubber, replacing any earlier one.
pub fn install(scrubber: Scrubber) {
    *HOOK.write().unwrap_or_else(PoisonError::into_inner) = Some(scrubber);
}

/// `text` with every known secret value replaced; unchanged when no
/// scrubber is installed.
pub fn text(text: String) -> String {
    let hook = HOOK.read().unwrap_or_else(PoisonError::into_inner).clone();
    match hook {
        Some(scrub) => scrub(&text),
        None => text,
    }
}
