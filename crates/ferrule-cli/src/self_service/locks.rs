//! What a managed bot's policy refuses, said before the owner is asked:
//! a card nobody can approve into effect is only noise. The handlers keep
//! their own locks; this is the same answer, earlier.

use super::Op;
use crate::dashboard::Ctx;
use crate::managed::{self, Policy};

const UPDATE_WHY: &str = "the panel updates a bot by changing its image";

/// A model's provider name and kind, when it can be told.
pub(super) type KindOf<'a> = &'a dyn Fn(&str) -> Option<(String, String)>;

impl Op {
    /// Why `p` refuses this op, or `None` (the handler's own lock still
    /// applies to everything not listed).
    pub fn managed_refusal_under(&self, p: &Policy, kind_of: KindOf<'_>) -> Option<String> {
        match self {
            Op::Update | Op::UpdateCheck => {
                Some(managed::refused("Updating", UPDATE_WHY).to_string())
            }
            Op::ConfigSet { key, .. } if key.starts_with("update.") => {
                Some(managed::refused("Updating", UPDATE_WHY).to_string())
            }
            Op::HooksTrust { .. } => managed::hooks_off_under(p),
            Op::Caps { caps } => caps
                .iter()
                .find_map(|(key, new)| managed::cap_refusal_under(p, key, *new)),
            Op::McpOn { .. } | Op::SkillOn { .. } if !p.extensions => {
                Some(format!("extensions are off on a managed bot: {}", p.why()))
            }
            Op::ModelDefault { model } => model_refusal(p, model, kind_of),
            Op::ModelHere { model: Some(model) } => model_refusal(p, model, kind_of),
            Op::ModelFallback { models } => models
                .iter()
                .find_map(|model| model_refusal(p, model, kind_of)),
            _ => None,
        }
    }
}

fn model_refusal(p: &Policy, model: &str, kind_of: KindOf<'_>) -> Option<String> {
    let (name, kind) = kind_of(model)?;
    managed::kind_refusal_under(p, &name, &kind)
}

/// This process's policy applied to `op`; `None` outside managed mode.
pub(super) fn refusal(op: &Op, ctx: &Ctx) -> Option<String> {
    let p = managed::policy()?;
    op.managed_refusal_under(&p, &|m| provider_of(ctx, m))
}

fn provider_of(ctx: &Ctx, model: &str) -> Option<(String, String)> {
    let view = ctx.models.as_ref()?.view();
    let row = view.models.iter().find(|r| {
        r.reference == model || r.model == model || r.aliases.iter().any(|a| a == model)
    })?;
    let kind = match row.plan.as_deref() {
        Some("chatgpt") => "chatgpt".to_string(),
        Some("claude-code") => "claude".to_string(),
        _ => {
            let (cfg, _) = crate::config::Config::load().ok()?;
            managed::provider_kind(cfg.providers.get(&row.provider)?)
        }
    };
    Some((row.provider.clone(), kind))
}
