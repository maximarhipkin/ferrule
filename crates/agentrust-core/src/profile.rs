use serde::{Deserialize, Serialize};

/// Per-model harness profile. This is the "best harness for each model" unit:
/// how much context the model really has, when to compact, whether reasoning
/// survives across turns, and how the system prompt is shaped.
///
/// Defaults follow the production compaction playbook: trigger at ~70-75% of
/// the window, keep a response reserve so the model never runs out of room
/// mid-reasoning, and keep the system prefix byte-stable for prompt caching.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessProfile {
    pub name: String,
    /// Total context window in tokens.
    pub context_window: usize,
    /// Tokens kept free for the model's response + reasoning.
    pub output_reserve: usize,
    /// Fraction of (window - reserve) at which compaction triggers. 0.70-0.75
    /// is the production sweet spot; compacting at 95%+ produces degraded,
    /// "context-anxious" summaries.
    pub compaction_threshold: f32,
    /// Preserve reasoning/thinking content across turns (Kimi K2 Thinking,
    /// OpenAI retained reasoning). False for models that neither emit nor
    /// accept reasoning content.
    pub retain_reasoning: bool,
    /// Extra system-prompt block tuned for this model family.
    pub system_directive: String,
}

impl HarnessProfile {
    /// Token budget at which the loop must compact.
    pub fn compaction_trigger_tokens(&self) -> usize {
        ((self.context_window.saturating_sub(self.output_reserve)) as f32 * self.compaction_threshold) as usize
    }

    /// Kimi K2.x: 256K window, interleaved thinking is the trained behavior,
    /// implicit context caching rewards a stable prefix.
    pub fn kimi() -> Self {
        Self {
            name: "kimi".into(),
            context_window: 256_000,
            output_reserve: 33_000,
            compaction_threshold: 0.72,
            retain_reasoning: true,
            system_directive: "You are Kimi, an agentic coding assistant. Think step by step, \
                               use tools decisively, and keep going across many tool calls until \
                               the task is genuinely complete."
                .into(),
        }
    }

    /// OpenAI GPT via Chat Completions-compatible shim. Native retained
    /// reasoning + server compaction live on the Responses API (separate
    /// driver); here we at least avoid stripping anything the server returns.
    pub fn openai() -> Self {
        Self {
            name: "openai".into(),
            context_window: 400_000,
            output_reserve: 64_000,
            compaction_threshold: 0.70,
            retain_reasoning: true,
            system_directive: "You are a precise autonomous agent. Prefer tool calls over \
                               speculation, verify claims with evidence, and persist key \
                               facts instead of re-deriving them."
                .into(),
        }
    }

    /// Anthropic Claude via an OpenAI-compatible gateway (direct Messages API
    /// driver with cache breakpoints is on the roadmap).
    pub fn anthropic_compatible() -> Self {
        Self {
            name: "anthropic".into(),
            context_window: 200_000,
            output_reserve: 32_000,
            compaction_threshold: 0.72,
            retain_reasoning: true,
            system_directive: "You are Claude working inside an agent runtime. Use tools when \
                               they reduce uncertainty; be concise in final answers."
                .into(),
        }
    }

    /// Conservative generic fallback for unknown OpenAI-compatible endpoints.
    pub fn generic() -> Self {
        Self {
            name: "generic".into(),
            context_window: 128_000,
            output_reserve: 16_000,
            compaction_threshold: 0.70,
            retain_reasoning: false,
            system_directive: String::new(),
        }
    }

    pub fn by_name(name: &str) -> Self {
        match name {
            "kimi" => Self::kimi(),
            "openai" => Self::openai(),
            "anthropic" => Self::anthropic_compatible(),
            _ => Self::generic(),
        }
    }
}

/// Structured compaction template. Checklist sections prevent the silent
/// information loss that freeform summarization causes.
pub const COMPACTION_TEMPLATE: &str = "\
Summarize this agent session so work can continue seamlessly. Fill every section \
(write \"none\" if empty):

## Session Intent
## Files Touched (full paths)
## Key Decisions
## Important Facts Established
## Active Goals
## Next Steps

Conversation to compress:
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_is_seventyish_percent_of_usable_window() {
        let k = HarnessProfile::kimi();
        let t = k.compaction_trigger_tokens();
        let usable = k.context_window - k.output_reserve;
        let ratio = t as f32 / usable as f32;
        assert!(ratio > 0.65 && ratio < 0.78, "ratio {ratio}");
    }

    #[test]
    fn unknown_profile_falls_back_to_generic() {
        assert_eq!(HarnessProfile::by_name("whatever").name, "generic");
        assert_eq!(HarnessProfile::by_name("kimi").name, "kimi");
    }
}
