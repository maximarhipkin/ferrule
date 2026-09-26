//! Hermes Agent's files as a [`Plan`] (the format as read at v2026.9.24:
//! docs/m33-ops.md §3.1).

use super::*;

/// The home directory: `--from`, else `$HERMES_HOME`, else `~/.hermes`,
/// else `%LOCALAPPDATA%\hermes`. `None` when there's none.
pub fn home(
    from: Option<&Path>,
    env: &dyn Fn(&str) -> Option<String>,
    home: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(from) = from {
        return Some(from.to_path_buf());
    }
    if let Some(dir) = env("HERMES_HOME") {
        return Some(expand_tilde(&dir, home));
    }
    home.map(|h| h.join(".hermes"))
        .into_iter()
        .chain(env("LOCALAPPDATA").map(|l| PathBuf::from(l).join("hermes")))
        .find(|d| d.is_dir())
}

/// The allowlist variables in `.env`, and where their ids go.
const ENV_LISTS: &[(&str, List)] = &[
    ("TELEGRAM_ALLOWED_USERS", List::TelegramChats),
    ("TELEGRAM_ALLOWED_CHATS", List::TelegramChats),
    ("TELEGRAM_GROUP_ALLOWED_CHATS", List::TelegramChats),
    ("DISCORD_ALLOWED_USERS", List::DiscordUsers),
    ("DISCORD_ALLOWED_CHANNELS", List::DiscordChannels),
    ("SLACK_ALLOWED_USERS", List::SlackUsers),
    ("SLACK_ALLOWED_CHANNELS", List::SlackChannels),
];

/// The channel token variables, and the `[gateway]` key each sets.
const TOKENS: &[(&str, &str)] = &[
    ("TELEGRAM_BOT_TOKEN", "telegram_token_env"),
    ("DISCORD_BOT_TOKEN", "discord_token_env"),
    ("SLACK_BOT_TOKEN", "slack_bot_token_env"),
    ("SLACK_APP_TOKEN", "slack_app_token_env"),
];

pub fn read(root: &Path, profile: Option<&str>, home: Option<&Path>) -> Result<Plan> {
    if !root.is_dir() {
        bail!("{} isn't a directory", root.display());
    }
    let active = std::fs::read_to_string(root.join("active_profile"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let dir = match profile.map(str::to_string).or(active) {
        None => root.to_path_buf(),
        Some(p) if p == "default" => root.to_path_buf(),
        Some(p) => {
            let dir = root.join("profiles").join(&p);
            if !dir.is_dir() {
                bail!(
                    "there's no Hermes profile `{p}` ({} is missing)",
                    dir.display()
                );
            }
            dir
        }
    };
    let mut plan = Plan::new(Tool::Hermes, dir.clone());
    let dotenv: BTreeMap<String, String> = read_text(&mut plan, &dir.join(".env"))
        .map(|t| secrets::parse(&t).into_iter().collect())
        .unwrap_or_default();
    let cfg = match read_text(&mut plan, &dir.join("config.yaml")).map(|t| yaml::parse(&t)) {
        Some(Ok(doc)) => {
            if !doc.skipped.is_empty() {
                plan.note(format!(
                    "config.yaml: {} use YAML this reader skips (block text, anchors); \
                     none of them is a setting the import reads",
                    doc.skipped.join(", ")
                ));
            }
            doc.value
        }
        Some(Err(e)) => {
            plan.note(format!(
                "config.yaml doesn't parse ({e}); its settings are skipped"
            ));
            Value::Null
        }
        None => Value::Null,
    };

    for (name, extra) in [("MEMORY.md", &[][..]), ("USER.md", &["user"][..])] {
        let file = format!("memories/{name}");
        if let Some(text) = read_text(&mut plan, &dir.join("memories").join(name)) {
            plan.memories.extend(split_on(&text, "§", &file, extra));
        }
    }
    find_skills(&mut plan, &dir.join("skills"), "skills");
    if let Some(Value::Array(extra)) = cfg.pointer("/skills/external_dirs") {
        for d in extra.iter().filter_map(Value::as_str) {
            find_skills(&mut plan, &expand_tilde(d, home), d);
        }
    }
    channels(&mut plan, &cfg, &dotenv);
    providers(&mut plan, &cfg, &dotenv);
    if dir.join("SOUL.md").is_file() {
        plan.note("SOUL.md (persona) isn't imported");
    }
    if dir.join("auth.json").is_file() {
        plan.note(
            "OAuth logins in auth.json (Nous Portal, Codex) don't carry over; connect those \
             providers in `ferrule setup`",
        );
    }
    if cfg.get("mcp_servers").is_some() {
        plan.note("mcp_servers aren't imported; add them with `ferrule mcp add`");
    }
    plan.finish();
    Ok(plan)
}

fn channels(plan: &mut Plan, cfg: &Value, dotenv: &BTreeMap<String, String>) {
    for (var, list) in ENV_LISTS {
        if let Some(v) = dotenv.get(*var) {
            allow(
                plan,
                *list,
                id_list(&Value::String(v.clone())),
                &format!(".env {var}"),
            );
        }
    }
    for (var, value) in dotenv {
        let anyone = var.ends_with("_ALLOW_ALL_USERS") || var.starts_with("GATEWAY_ALLOW");
        if anyone && matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes") {
            plan.note(format!(
                ".env {var}: \"anyone\" isn't imported; ferrule's allowlists name ids"
            ));
        }
    }
    // The YAML forms: gateway.platforms.<p>[.extra], platforms.<p>, <p>.
    let fields: &[(&str, &[(&str, List)])] = &[
        (
            "telegram",
            &[
                ("allow_from", List::TelegramChats),
                ("allowed_users", List::TelegramChats),
                ("allowed_chats", List::TelegramChats),
                ("group_allowed_chats", List::TelegramChats),
            ],
        ),
        (
            "discord",
            &[
                ("allow_from", List::DiscordUsers),
                ("allowed_users", List::DiscordUsers),
                ("allowed_channels", List::DiscordChannels),
            ],
        ),
        (
            "slack",
            &[
                ("allow_from", List::SlackUsers),
                ("allowed_users", List::SlackUsers),
                ("allowed_channels", List::SlackChannels),
            ],
        ),
    ];
    for (platform, keys) in fields {
        for base in [
            format!("/gateway/platforms/{platform}"),
            format!("/gateway/platforms/{platform}/extra"),
            format!("/platforms/{platform}"),
            format!("/platforms/{platform}/extra"),
            format!("/{platform}"),
        ] {
            let Some(conf) = cfg.pointer(&base) else {
                continue;
            };
            let origin = format!(
                "config.yaml {}",
                base.trim_start_matches('/').replace('/', ".")
            );
            for (key, list) in *keys {
                if let Some(ids) = conf.get(*key) {
                    allow(plan, *list, id_list(ids), &format!("{origin}.{key}"));
                }
            }
            if let Some(input) = conf.get("token").and_then(secret_input) {
                let (var, key) = TOKENS
                    .iter()
                    .find(|(v, _)| v.starts_with(&platform.to_ascii_uppercase()))
                    .expect("every platform has a token");
                if let Some(env) = take_secret(plan, input, var, &format!("{origin}.token"), dotenv)
                {
                    plan.token(key, &env);
                }
            }
        }
    }
    for (var, key) in TOKENS {
        let Some(value) = dotenv.get(*var) else {
            continue;
        };
        if *var == "SLACK_BOT_TOKEN" && value.contains(',') {
            plan.note(
                ".env SLACK_BOT_TOKEN holds several workspaces' tokens; ferrule runs one, so \
                 set it by hand",
            );
            continue;
        }
        plan.secret(var, Some(value.clone()), ".env");
        plan.token(key, var);
    }
}

fn providers(plan: &mut Plan, cfg: &Value, dotenv: &BTreeMap<String, String>) {
    // model: "name" or {default, provider, base_url, api_key, api_mode}.
    let (model, provider, base_url) = match cfg.get("model") {
        Some(Value::String(m)) => (Some(m.clone()), None, None),
        Some(m) => (
            m.get("default")
                .or_else(|| m.get("model"))
                .and_then(Value::as_str)
                .map(str::to_string),
            m.get("provider")
                .and_then(Value::as_str)
                .map(str::to_string),
            m.get("base_url")
                .and_then(Value::as_str)
                .map(str::to_string),
        ),
        None => (None, None, None),
    };
    let model_conf = cfg.get("model").cloned().unwrap_or(Value::Null);
    let provider = provider.unwrap_or_else(|| "auto".into());
    // `auto` is OpenRouter when its key is there, else the model's vendor.
    let provider = if provider == "auto" {
        if dotenv.contains_key("OPENROUTER_API_KEY") {
            "openrouter".to_string()
        } else {
            model
                .as_deref()
                .and_then(|m| m.split_once('/'))
                .map(|(p, _)| p.to_string())
                .unwrap_or_else(|| "openrouter".into())
        }
    } else {
        provider
    };
    let named = cfg
        .get("providers")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    // The default model's provider first, then each named one.
    let mut todo: Vec<(String, Value, Option<String>, bool)> = Vec::new();
    let custom_default =
        provider == "custom" || (base_url.is_some() && !named.contains_key(&provider));
    if custom_default {
        let mut conf = model_conf.clone();
        if let (Value::Object(m), Some(url)) = (&mut conf, &base_url) {
            m.insert("base_url".into(), Value::String(url.clone()));
        }
        todo.push(("custom".into(), conf, model.clone(), true));
    } else {
        let conf = named.get(&provider).cloned().unwrap_or(Value::Null);
        todo.push((provider.clone(), conf, model.clone(), true));
    }
    for (name, conf) in &named {
        if !todo.iter().any(|(n, ..)| n == name) {
            let m = conf
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string);
            todo.push((name.clone(), conf.clone(), m, false));
        }
    }

    for (name, conf, model, is_default) in todo {
        let origin = if is_default && !named.contains_key(&name) {
            "config.yaml model".to_string()
        } else {
            format!("config.yaml providers.{name}")
        };
        let mapped = preset(&name);
        let url = conf.get("base_url").and_then(Value::as_str);
        let mode = conf
            .get("api_mode")
            .and_then(Value::as_str)
            .unwrap_or("chat_completions");
        if matches!(
            name.as_str(),
            "nous" | "openai-codex" | "copilot" | "qwen-oauth"
        ) {
            plan.note(format!(
                "{origin}: `{name}` signs in with OAuth, which doesn't carry over; connect a \
                 provider with a key in `ferrule setup`"
            ));
            continue;
        }
        if conf.get("key_cmd").is_some() {
            plan.note(format!(
                "{origin}: a key from `key_cmd` doesn't carry over; add the provider with \
                 `ferrule setup`"
            ));
            continue;
        }
        let (api, profile) = match mode {
            "chat_completions" => match mapped {
                Some(p) => (
                    (p.profile == "anthropic").then_some("anthropic"),
                    p.profile.to_string(),
                ),
                None => (None, "generic".to_string()),
            },
            "anthropic_messages" => (Some("anthropic"), "anthropic".to_string()),
            other => {
                plan.note(format!(
                    "{origin}: api_mode `{other}` isn't one ferrule maps; add it with \
                     `ferrule setup`"
                ));
                continue;
            }
        };
        let base_url = match (mapped, url) {
            (Some(p), u) => preset_url(p, u),
            (None, Some(u)) => u.to_string(),
            (None, None) => {
                plan.note(format!(
                    "{origin}: provider `{name}` isn't a preset ferrule knows and has no \
                     base_url; add it with `ferrule setup`"
                ));
                continue;
            }
        };
        let Some(model) = model.filter(|m| !m.is_empty()).or_else(|| {
            mapped
                .map(|p| p.model.to_string())
                .filter(|m| !m.is_empty())
        }) else {
            plan.note(format!(
                "{origin}: no model named, so it isn't imported; add it with `ferrule setup`"
            ));
            continue;
        };
        // A preset's vendor prefix only means something to OpenRouter.
        let model = match (mapped.map(|p| p.name), model.split_once('/')) {
            (Some(p), Some((vendor, rest)))
                if p != "openrouter" && preset(vendor).is_some_and(|v| v.name == p) =>
            {
                rest.to_string()
            }
            _ => model,
        };
        let default_env = conf
            .get("key_env")
            .and_then(Value::as_str)
            .filter(|v| secrets::valid_name(v))
            .map(str::to_string)
            .or_else(|| mapped.map(|p| p.key_env.to_string()))
            .unwrap_or_else(|| key_env_for(&name));
        let key_env = match conf.get("api_key").and_then(secret_input) {
            Some(input) => {
                match take_secret(
                    plan,
                    input,
                    &default_env,
                    &format!("{origin}.api_key"),
                    dotenv,
                ) {
                    Some(env) => env,
                    None => continue,
                }
            }
            None => {
                if let Some(v) = dotenv.get(&default_env) {
                    plan.secret(&default_env, Some(v.clone()), ".env");
                }
                default_env
            }
        };
        let ferrule_name = provider_name(mapped.map(|p| p.name).unwrap_or(&name));
        plan.provider(Provider {
            name: ferrule_name.clone(),
            base_url,
            api,
            profile,
            key_env,
            model,
        });
        if is_default {
            plan.default_provider = Some(ferrule_name);
        }
    }
}
