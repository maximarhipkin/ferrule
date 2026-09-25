//! `ferrule model …`, and the text `/model` and the CLI show.

use super::*;

#[derive(clap::Subcommand)]
pub enum ModelCmd {
    /// The connected models, the default, the fallback list and the pins
    List {
        /// As JSON (the same view the dashboard reads)
        #[arg(long)]
        json: bool,
    },
    /// Set the default model: `provider/model`, a provider, an alias or a
    /// model id only one provider has
    Default { reference: String },
    /// Make one real call to a model (the default without one) and say
    /// plainly what failed
    Test { reference: Option<String> },
    /// Connect another model on a provider you already have:
    /// `ferrule model add openai/gpt-5.2-mini --alias fast`
    Add {
        reference: String,
        #[arg(long)]
        alias: Option<String>,
    },
    /// Disconnect a model (not a provider's own; `ferrule setup` does that)
    Remove { reference: String },
    /// Name a model: `ferrule model alias fast openai/gpt-5.2-mini`
    Alias {
        name: String,
        #[arg(required_unless_present = "remove")]
        reference: Option<String>,
        #[arg(long, conflicts_with = "reference")]
        remove: bool,
    },
    /// Run a chat on a model: `ferrule model pin telegram:42 fast`
    Pin { chat: String, reference: String },
    /// Put a chat back on the default
    Unpin { chat: String },
    /// Where turns go when a model stays down after its retries, in order
    Fallback {
        #[arg(required_unless_present = "off")]
        references: Vec<String>,
        /// No fallback: a model that's down fails its turn
        #[arg(long, conflicts_with = "references")]
        off: bool,
    },
    /// The models your providers offer, with prices per 1M tokens (M22):
    /// each provider's list and OpenRouter's, refetched at most hourly
    Catalog {
        /// Words the id or name must contain
        #[arg(long)]
        search: Option<String>,
        /// Only models that can call tools (ferrule's agents need them)
        #[arg(long)]
        tools: bool,
        /// Sort: in (default), out, context or name
        #[arg(long, default_value = "in")]
        sort: String,
        /// Fetch now, even if the cached list is recent
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        json: bool,
    },
    /// A short curated list by tier, checked against OpenRouter's live list,
    /// with what each would cost a month at your usage
    Recommend {
        #[arg(long)]
        json: bool,
    },
    /// Prices from the catalogs for every connected model without them
    /// (hand-set prices are never touched)
    FillPrices,
}

pub async fn cmd(op: ModelCmd) -> anyhow::Result<()> {
    let models = shared()?;
    if !matches!(
        op,
        ModelCmd::List { .. }
            | ModelCmd::Test { .. }
            | ModelCmd::Catalog { .. }
            | ModelCmd::Recommend { .. }
    ) {
        let (cfg, _) = Config::load()?;
        models.attach_hub(crate::trust::hub(&cfg)?);
    }
    let by = "cli";
    let done = match op {
        ModelCmd::List { json } => {
            let view = models.view();
            if json {
                println!("{}", serde_json::to_string_pretty(&view)?);
            } else {
                print!("{}", render(&view, None));
            }
            return Ok(());
        }
        ModelCmd::Test { reference } => {
            let word = match reference {
                Some(r) => r,
                None => models
                    .view()
                    .default
                    .ok_or_else(|| anyhow::anyhow!("no default model; name one"))?,
            };
            let out = models.test(&word).await;
            if out.ok {
                println!("{}: {}", out.reference, out.said);
                return Ok(());
            }
            anyhow::bail!("{}: {}", out.reference, out.said);
        }
        ModelCmd::Default { reference } => models.set_default(&reference, by)?,
        ModelCmd::Add { reference, alias } => {
            let (p, m) = reference.split_once('/').ok_or_else(|| {
                anyhow::anyhow!("say it as provider/model, like openai/gpt-5.2-mini")
            })?;
            models.add_model(p, m, alias.as_deref(), by)?
        }
        ModelCmd::Remove { reference } => models.remove_model(&reference, by)?,
        ModelCmd::Alias {
            name, reference, ..
        } => models.set_alias(&name, reference.as_deref(), by)?,
        ModelCmd::Pin { chat, reference } => {
            let (ch, id) = split_chat(&chat)?;
            models.pin(ch, id, &reference, by)?
        }
        ModelCmd::Unpin { chat } => {
            let (ch, id) = split_chat(&chat)?;
            models.unpin(ch, id, by)?
        }
        ModelCmd::Fallback { references, off } => {
            models.set_fallback(if off { &[] } else { &references }, by)?
        }
        ModelCmd::Catalog {
            search,
            tools,
            sort,
            refresh,
            json,
        } => {
            let (_, listings) = catalog::listings(refresh).await?;
            let q = catalog::Query {
                search,
                all: !tools,
                sort,
            };
            let f = catalog::filter(&listings, &models.catalog(), &q);
            if json {
                println!("{}", serde_json::to_string_pretty(&f)?);
            } else {
                print!("{}", catalog::render(&f));
            }
            return Ok(());
        }
        ModelCmd::Recommend { json } => {
            let r = catalog::recommended(&models, false).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                print!("{}", catalog::render_recommended(&r));
            }
            return Ok(());
        }
        ModelCmd::FillPrices => {
            let (_, listings) = catalog::listings(false).await?;
            println!("{}", models.fill_prices(&listings, by)?.said);
            return Ok(());
        }
    };
    println!("{}", done.said);
    Ok(())
}

fn split_chat(chat: &str) -> anyhow::Result<(&str, &str)> {
    chat.split_once(':')
        .filter(|(c, id)| !c.is_empty() && !id.is_empty())
        .ok_or_else(|| anyhow::anyhow!("say the chat as channel:chat, like telegram:42"))
}

/// The view as text; `chat` marks that chat's pin.
pub fn render(view: &ModelsView, chat: Option<(&str, &str)>) -> String {
    let mut out = String::new();
    match &view.default {
        Some(d) => out.push_str(&format!("Default: {d}\n")),
        None => out.push_str("Default: none\n"),
    }
    if let Some((ch, id)) = chat {
        let pin = view.pins.iter().find(|p| p.channel == ch && p.chat == id);
        match pin {
            Some(p) => match &p.resolved {
                Some(r) => out.push_str(&format!("This chat: pinned to {r}\n")),
                None => out.push_str(&format!(
                    "This chat: pinned to {}, which isn't connected anymore, so on the default\n",
                    p.reference
                )),
            },
            None => out.push_str("This chat: on the default\n"),
        }
    }
    out.push_str("\nConnected:\n");
    for m in &view.models {
        let mut notes = Vec::new();
        if m.default {
            notes.push("default".to_string());
        }
        if let Some(r) = m.fallback_rank {
            notes.push(format!("fallback {r}"));
        }
        if !m.aliases.is_empty() {
            notes.push(format!("alias {}", m.aliases.join(", ")));
        }
        if !m.key_present {
            notes.push(format!("key missing (${})", m.key_env));
        }
        if let Some(secs) = m.down_secs {
            notes.push(format!(
                "down: {}, retried in {} min",
                m.down_reason.as_deref().unwrap_or("?"),
                secs.div_ceil(60)
            ));
        }
        if let Some(p) = m.pricing {
            notes.push(format!(
                "${}/${}/${} per M in/cached/out",
                p.input, p.cached_input, p.output
            ));
        }
        let mark = if m.default { "*" } else { "-" };
        let notes = if notes.is_empty() {
            String::new()
        } else {
            format!(" ({})", notes.join("; "))
        };
        out.push_str(&format!("{mark} {}{notes}\n", m.reference));
    }
    if view.fallback.is_empty() {
        out.push_str("\nFallback: off\n");
    } else {
        out.push_str(&format!("\nFallback: {}\n", view.fallback.join(" → ")));
    }
    if chat.is_none() && !view.pins.is_empty() {
        out.push_str("\nPinned chats:\n");
        for p in &view.pins {
            let r = p
                .resolved
                .as_deref()
                .unwrap_or("not connected, on the default");
            out.push_str(&format!("- {}:{} → {r}\n", p.channel, p.chat));
        }
    }
    if !view.problems.is_empty() {
        out.push_str("\nProblems:\n");
        for p in &view.problems {
            out.push_str(&format!("- {p}\n"));
        }
    }
    out
}
