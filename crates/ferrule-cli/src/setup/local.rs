//! M34: the local-model part of `ferrule setup`'s provider step
//! (docs/local-models.md): the servers running here, offered as providers
//! with each model's window; after the pick, one tool call and the window
//! the server really gives, a fix offered when it's small (applied only on
//! a yes), and that window written as the model's `context_window`.

use super::{info, ok, put, table, warn, NewProvider, Target};
use crate::local::{self, Kind, Level, Server, Tools};
use anyhow::Result;
use inquire::Confirm;

/// The servers answering here, said out loud; setup offers each.
pub(super) async fn found(http: &reqwest::Client, cfg: &crate::config::Config) -> Vec<Server> {
    let servers = local::detect(http, &local::candidates(Some(cfg))).await;
    for s in &servers {
        info(format!("Found {}", s.describe()));
    }
    servers
}

pub(super) fn label(s: &Server) -> String {
    format!("{} (running here)", s.describe())
}

pub(super) fn provider(s: &Server) -> NewProvider {
    let name = s.kind.provider_name();
    let model = s
        .models
        .iter()
        .find(|m| local::known_good(&m.id))
        .or(s.models.first())
        .map(|m| m.id.clone())
        .unwrap_or_default();
    NewProvider {
        label: s.kind.label().into(),
        name: name.into(),
        base_url: s.base_url(),
        key_env: format!("{}_API_KEY", name.to_ascii_uppercase()),
        profile: "generic".into(),
        model,
        key_url: String::new(),
        needs_key: false,
    }
}

/// Each model with its windows, before the pick; the known-good list when
/// an Ollama server has none of them.
pub(super) async fn describe_models(http: &reqwest::Client, base_url: &str) {
    let Some(origin) = local::local_origin(base_url) else {
        return;
    };
    let Some(s) = local::identify(http, &origin).await else {
        return;
    };
    for m in &s.models {
        let mut bits = Vec::new();
        if let Some(t) = m.trained {
            bits.push(format!("trained {t}"));
        }
        match m.effective {
            Some(e) => bits.push(format!("window {e}")),
            None if s.kind == Kind::Ollama => bits.push("window known once loaded".into()),
            None => {}
        }
        if m.tools == Some(false) {
            bits.push("no tools".into());
        }
        if local::known_good(&m.id) {
            bits.push("known good".into());
        }
        info(format!("  {} · {}", m.id, bits.join(" · ")));
    }
    if s.kind == Kind::Ollama && !s.models.iter().any(|m| local::known_good(&m.id)) {
        info(format!(
            "Models known to call tools well ({}):",
            local::KNOWN_GOOD_AS_OF
        ));
        for k in local::KNOWN_GOOD {
            info(format!(
                "  {} · {} · {} · {}",
                k.model, k.size, k.window, k.note
            ));
        }
        info("ferrule doesn't download models: `ollama pull <model>` first.");
    }
}

/// After the model is picked and the provider written (unsaved): the tool
/// probe, the real window, the fix on a yes, and `context_window`. May
/// switch `np.model` to an Ollama derived model.
pub(super) async fn fit(
    t: &mut Target,
    http: &reqwest::Client,
    np: &mut NewProvider,
    key: &str,
) -> Result<()> {
    let Some(origin) = local::local_origin(&np.base_url) else {
        return Ok(());
    };
    let Some(server) = local::identify(http, &origin).await else {
        return Ok(());
    };
    info(format!(
        "Checking {} on {}: one tool call (this loads the model, which can take a minute) …",
        np.model,
        server.describe()
    ));
    let listed = server.model(&np.model).cloned();
    let tools = match &listed {
        None => None,
        Some(m) if server.kind == Kind::Ollama && m.tools == Some(false) => Some(Tools::Cannot(
            "the model's template has no tool support".into(),
        )),
        Some(_) => Some(
            super::interruptible(local::probe_tools(http, &np.base_url, key, &np.model)).await?,
        ),
    };
    if let Some(tools) = tools {
        say(&tools.finding(server.kind, &np.model));
    }
    // The probe loaded it: Ollama's window is known now.
    let server = if server.kind == Kind::Ollama {
        local::identify(http, &origin).await.unwrap_or(server)
    } else {
        server
    };
    let Some(m) = server.model(&np.model).cloned() else {
        return Ok(());
    };
    let Some(mut window) = m.effective else {
        info("The server doesn't say the model's window; ferrule plans for the profile's.");
        return Ok(());
    };
    if window < local::WORKS_WELL {
        let want = local::wanted(&m);
        warn(format!(
            "{} gets a {window}-token window from {} (trained for {}); an agent turn needs more",
            np.model,
            server.kind.label(),
            m.trained.map_or("?".into(), |t| t.to_string()),
        ));
        if server.kind == Kind::Ollama && want > window {
            let derived = local::derived_name(&np.model, want);
            if Confirm::new(&format!(
                "Make `{derived}`: the same weights with a {want}-token window (your model and Ollama's settings stay as they are)?"
            ))
            .with_default(true)
            .prompt()?
            {
                match local::derive(http, &origin, &np.model, want).await {
                    Ok(name) => {
                        ok(format!("made `{name}`; ferrule uses it"));
                        np.model = name;
                        window = want;
                        put(table(t.root(), &["providers", &np.name])?, "model", np.model.as_str());
                    }
                    Err(e) => warn(format!("Ollama didn't make it: {e}")),
                }
            } else {
                info(format!("Or: {}", local::fix(server.kind, &np.model, want)));
            }
        } else if want > window {
            info(format!(
                "To fix it: {}",
                local::fix(server.kind, &np.model, want)
            ));
            info("Then run `ferrule doctor`; ferrule plans for the window the server gives now.");
        }
    }
    if window < local::FLOOR {
        warn(format!(
            "below {} tokens the agent can't hold its own instructions; pick a bigger window or model",
            local::FLOOR
        ));
    }
    put(
        table(t.root(), &["providers", &np.name, "models", &np.model])?,
        "context_window",
        window as i64,
    );
    ok(format!(
        "ferrule plans for {window} tokens on {} (context_window)",
        np.model
    ));
    Ok(())
}

fn say(f: &local::Finding) {
    match f.level {
        Level::Ok | Level::Note => ok(&f.text),
        Level::Warn | Level::Fail => warn(&f.text),
    }
    if let Some(fix) = &f.fix {
        info(format!("  → {fix}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local::Model;

    #[test]
    fn a_found_server_becomes_a_keyless_generic_provider() {
        let s = Server {
            kind: Kind::Ollama,
            origin: "http://127.0.0.1:11434".into(),
            version: Some("0.34.4".into()),
            models: vec![
                Model {
                    id: "llama3.2:1b".into(),
                    ..Model::default()
                },
                Model {
                    id: "gpt-oss:20b".into(),
                    ..Model::default()
                },
            ],
        };
        let np = provider(&s);
        assert_eq!(
            (np.name.as_str(), np.base_url.as_str(), np.key_env.as_str()),
            ("ollama", "http://127.0.0.1:11434/v1", "OLLAMA_API_KEY")
        );
        assert_eq!(
            np.model, "gpt-oss:20b",
            "a known-good model is the default pick"
        );
        assert!(!np.needs_key && np.profile == "generic");
        assert_eq!(
            label(&s),
            "Ollama 0.34.4 at 127.0.0.1:11434 · 2 models (running here)"
        );
    }
}
