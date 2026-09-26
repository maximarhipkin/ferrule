//! OpenClaw's files as a [`Plan`] (the format as read at v2026.9.6:
//! docs/m33-ops.md §3.1).

use super::*;

/// The state directory: `--from`, else `$OPENCLAW_STATE_DIR`, else
/// `~/.openclaw[-$OPENCLAW_PROFILE]` (with `$OPENCLAW_HOME` as `~`), else
/// the legacy `~/.clawdbot`. `None` when there's none.
pub fn home(
    from: Option<&Path>,
    env: &dyn Fn(&str) -> Option<String>,
    home: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(from) = from {
        return Some(from.to_path_buf());
    }
    if let Some(dir) = env("OPENCLAW_STATE_DIR") {
        return Some(expand_tilde(&dir, home));
    }
    let base = env("OPENCLAW_HOME")
        .map(|h| expand_tilde(&h, home))
        .or_else(|| home.map(Path::to_path_buf))?;
    let name = match env("OPENCLAW_PROFILE") {
        Some(p) if p != "default" => format!(".openclaw-{p}"),
        _ => ".openclaw".to_string(),
    };
    [base.join(name), base.join(".clawdbot")]
        .into_iter()
        .find(|d| d.is_dir())
}

pub fn read(
    state: &Path,
    workspace: Option<&Path>,
    env: &dyn Fn(&str) -> Option<String>,
    home: Option<&Path>,
) -> Result<Plan> {
    if !state.is_dir() {
        bail!("{} isn't a directory", state.display());
    }
    let mut plan = Plan::new(Tool::OpenClaw, state.to_path_buf());
    let dotenv: BTreeMap<String, String> = read_text(&mut plan, &state.join(".env"))
        .map(|t| secrets::parse(&t).into_iter().collect())
        .unwrap_or_default();

    let config_path = env("OPENCLAW_CONFIG_PATH")
        .map(|p| expand_tilde(&p, home))
        .or_else(|| {
            ["openclaw.json", "clawdbot.json"]
                .iter()
                .map(|n| state.join(n))
                .find(|p| p.is_file())
        });
    let cfg = match &config_path {
        Some(path) => match read_text(&mut plan, path).map(|t| json5::parse(&t)) {
            Some(Ok(v)) => v,
            Some(Err(e)) => {
                plan.note(format!(
                    "{} doesn't parse ({e}); its settings are skipped",
                    path.display()
                ));
                Value::Null
            }
            None => Value::Null,
        },
        None => {
            plan.note("no openclaw.json; only the workspace is read");
            Value::Null
        }
    };
    if let Some(inc) = cfg.get("$include") {
        plan.note(format!(
            "`$include` ({inc}) isn't followed; settings in the included file aren't imported"
        ));
    }

    let workspace = workspace
        .map(Path::to_path_buf)
        .or_else(|| {
            cfg.pointer("/agents/defaults/workspace")
                .and_then(Value::as_str)
                .map(|w| expand_tilde(w, home))
        })
        .or_else(|| env("OPENCLAW_WORKSPACE_DIR").map(|w| expand_tilde(&w, home)))
        .unwrap_or_else(|| state.join("workspace"));
    if let Some(entries) = cfg.pointer("/agents/entries").and_then(Value::as_object) {
        let others: Vec<&str> = entries
            .iter()
            .filter(|(_, a)| a.get("workspace").is_some())
            .map(|(id, _)| id.as_str())
            .collect();
        if !others.is_empty() {
            plan.note(format!(
                "agents with their own workspace ({}) aren't read; pass --workspace <dir> \
                 to import one",
                others.join(", ")
            ));
        }
    }

    memories(&mut plan, &workspace);
    for (root, label) in [
        (workspace.join("skills"), "workspace/skills"),
        (
            workspace.join(".agents").join("skills"),
            "workspace/.agents/skills",
        ),
        (state.join("skills"), "skills"),
    ] {
        find_skills(&mut plan, &root, label);
    }
    if let Some(home) = home {
        find_skills(
            &mut plan,
            &home.join(".agents").join("skills"),
            "~/.agents/skills",
        );
    }
    channels(&mut plan, &cfg, state, &dotenv);
    providers(&mut plan, &cfg, &dotenv);
    if cfg.pointer("/auth/profiles").is_some() || state.join("agents").is_dir() {
        plan.note(
            "OAuth and token auth profiles (auth-profiles.json) don't carry over; connect \
             those providers in `ferrule setup`",
        );
    }
    if state.join("state").join("openclaw.sqlite").is_file() {
        plan.note(
            "pairing approvals in state/openclaw.sqlite aren't read; add those ids with \
             `ferrule setup`",
        );
    }
    if cfg.pointer("/skills/entries").is_some() {
        plan.note("per-skill keys (skills.entries.*.apiKey/env) aren't imported");
    }
    plan.finish();
    Ok(plan)
}

fn memories(plan: &mut Plan, workspace: &Path) {
    if !workspace.is_dir() {
        plan.note(format!(
            "no workspace at {}; no memories read (pass --workspace <dir>)",
            workspace.display()
        ));
        return;
    }
    for (name, extra) in [
        ("MEMORY.md", &[][..]),
        ("memory.md", &[][..]),
        ("USER.md", &["user"][..]),
    ] {
        let path = workspace.join(name);
        // `memory.md` is the legacy name; on a case-insensitive disk it's
        // MEMORY.md again.
        if name == "memory.md" && plan.memories.iter().any(|e| e.file == "MEMORY.md") {
            continue;
        }
        if let Some(text) = read_text(plan, &path) {
            plan.memories.extend(split_markdown(&text, name, extra, ""));
        }
    }
    let daily = workspace.join("memory");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&daily)
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    files.sort();
    for path in files {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(date) = daily_date(name) else {
            continue;
        };
        if !path.is_file() {
            continue;
        }
        if let Some(text) = read_text(plan, &path) {
            let file = format!("memory/{}", name.replace(',', ""));
            plan.memories.extend(split_markdown(
                &text,
                &file,
                &["daily"],
                &format!("({date}) "),
            ));
        }
    }
    if workspace.join("AGENTS.md").is_file() {
        plan.suggestions.push(format!(
            "{} is operating instructions, not memories: copy it into your project as \
             AGENTS.md and ferrule reads it as context",
            workspace.join("AGENTS.md").display()
        ));
    }
    for name in ["SOUL.md", "IDENTITY.md"] {
        if workspace.join(name).is_file() {
            plan.note(format!("{name} (persona) isn't imported"));
        }
    }
}

/// `2026-09-01.md`, `2026-09-01-standup.md` → `2026-09-01`.
fn daily_date(name: &str) -> Option<&str> {
    let stem = name.strip_suffix(".md")?;
    let date = stem.get(..10)?;
    let b = date.as_bytes();
    let shaped = b.iter().enumerate().all(|(i, c)| match i {
        4 | 7 => *c == b'-',
        _ => c.is_ascii_digit(),
    });
    (shaped && (stem.len() == 10 || stem.as_bytes()[10] == b'-')).then_some(date)
}

fn channels(plan: &mut Plan, cfg: &Value, state: &Path, dotenv: &BTreeMap<String, String>) {
    struct Chan {
        key: &'static str,
        tokens: &'static [(&'static str, &'static str, &'static str)],
        users: List,
    }
    let chans = [
        Chan {
            key: "telegram",
            tokens: &[
                ("botToken", "telegram_token_env", "TELEGRAM_BOT_TOKEN"),
                ("token", "telegram_token_env", "TELEGRAM_BOT_TOKEN"),
            ],
            users: List::TelegramChats,
        },
        Chan {
            key: "discord",
            tokens: &[
                ("token", "discord_token_env", "DISCORD_BOT_TOKEN"),
                ("botToken", "discord_token_env", "DISCORD_BOT_TOKEN"),
            ],
            users: List::DiscordUsers,
        },
        Chan {
            key: "slack",
            tokens: &[
                ("botToken", "slack_bot_token_env", "SLACK_BOT_TOKEN"),
                ("appToken", "slack_app_token_env", "SLACK_APP_TOKEN"),
            ],
            users: List::SlackUsers,
        },
    ];
    for c in &chans {
        let conf = cfg.pointer(&format!("/channels/{}", c.key));
        if conf.and_then(|v| v.get("enabled")) == Some(&Value::Bool(false)) {
            plan.note(format!(
                "channels.{} is disabled there, so it's skipped",
                c.key
            ));
            continue;
        }
        // The channel's tables: its own, then each account's.
        let mut scopes: Vec<(String, &Value)> = Vec::new();
        if let Some(v) = conf {
            scopes.push((format!("channels.{}", c.key), v));
            if let Some(accounts) = v.get("accounts").and_then(Value::as_object) {
                for (id, a) in accounts {
                    scopes.push((format!("channels.{}.accounts.{id}", c.key), a));
                }
            }
        }
        for (origin, v) in &scopes {
            for &(field, key, default_env) in c.tokens {
                if let Some(input) = v.get(field).and_then(secret_input) {
                    let origin = format!("{origin}.{field}");
                    if plan.tokens.iter().any(|(k, _)| *k == key) {
                        plan.note(format!(
                            "{origin}: ferrule runs one {} bot; only the first token is imported",
                            c.key
                        ));
                        continue;
                    }
                    if let Some(env) = take_secret(plan, input, default_env, &origin, dotenv) {
                        plan.token(key, &env);
                    }
                }
            }
            if v.get("tokenFile").is_some() {
                plan.note(format!(
                    "{origin}.tokenFile: a token file doesn't carry over"
                ));
            }
            for field in ["allowFrom", "dm/allowFrom"] {
                if let Some(ids) = v.pointer(&format!("/{field}")) {
                    let origin = format!("{origin}.{}", field.replace('/', "."));
                    allow(plan, c.users, id_list(ids), &origin);
                }
            }
            if c.key == "discord" {
                if let Some(ids) = v.pointer("/dm/groupChannels") {
                    allow(
                        plan,
                        List::DiscordChannels,
                        id_list(ids),
                        &format!("{origin}.dm.groupChannels"),
                    );
                }
            }
            if c.key == "telegram" && v.get("groupAllowFrom").is_some() {
                plan.note(format!(
                    "{origin}.groupAllowFrom lists users allowed in groups; ferrule's list is of \
                     chats, so add the group chat ids with `ferrule setup`"
                ));
            }
        }
        // A token only in <state>/.env, under the name OpenClaw reads.
        for &(_, key, default_env) in c.tokens {
            if !plan.tokens.iter().any(|(k, _)| *k == key) {
                if let Some(value) = dotenv.get(default_env) {
                    plan.secret(default_env, Some(value.clone()), ".env");
                    plan.token(key, default_env);
                }
            }
        }
    }
    // Legacy pairing approvals: credentials/<channel>-<account>-allowFrom.json.
    let creds = state.join("credentials");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&creds)
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    files.sort();
    for path in files {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with("-allowFrom.json") {
            continue;
        }
        let list = if name.starts_with("telegram-") {
            List::TelegramChats
        } else if name.starts_with("discord-") {
            List::DiscordUsers
        } else if name.starts_with("slack-") {
            List::SlackUsers
        } else {
            continue;
        };
        let origin = format!("credentials/{name}");
        match read_text(plan, &path).map(|t| json5::parse(&t)) {
            Some(Ok(v)) => {
                let ids = v.get("allowFrom").map(id_list).unwrap_or_default();
                allow(plan, list, ids, &origin);
            }
            Some(Err(e)) => plan.note(format!("{origin} doesn't parse ({e})")),
            None => {}
        }
    }
}

fn providers(plan: &mut Plan, cfg: &Value, dotenv: &BTreeMap<String, String>) {
    // The default model: "provider/model" or {primary: "provider/model"}.
    let default = match cfg.pointer("/agents/defaults/model") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(v) => v.get("primary").and_then(Value::as_str).map(str::to_string),
        None => None,
    };
    let default = default.and_then(|d| {
        d.split_once('/')
            .map(|(p, m)| (p.to_string(), m.to_string()))
    });
    let configured = cfg
        .pointer("/models/providers")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut names: Vec<String> = configured.keys().cloned().collect();
    if let Some((p, _)) = &default {
        if !names.contains(p) {
            names.push(p.clone());
        }
    }
    for name in names {
        let conf = configured.get(&name).cloned().unwrap_or(Value::Null);
        let origin = format!("models.providers.{name}");
        let model = default
            .as_ref()
            .filter(|(p, _)| *p == name)
            .map(|(_, m)| m.clone())
            .or_else(|| match conf.pointer("/models/0") {
                Some(Value::String(s)) => Some(s.clone()),
                Some(m) => m.get("id").and_then(Value::as_str).map(str::to_string),
                None => None,
            });
        let base_url = conf.get("baseUrl").and_then(Value::as_str);
        let api = conf.get("api").and_then(Value::as_str);
        let mapped = preset(&name);
        let (base_url, api, profile, default_env) = match (mapped, base_url, api) {
            (Some(p), url, _) => (
                preset_url(p, url),
                (p.profile == "anthropic").then_some("anthropic"),
                p.profile.to_string(),
                p.key_env.to_string(),
            ),
            (None, Some(url), Some("openai-completions") | None) => (
                url.to_string(),
                None,
                "generic".to_string(),
                key_env_for(&name),
            ),
            (None, Some(url), Some("anthropic-messages")) => (
                url.to_string(),
                Some("anthropic"),
                "anthropic".to_string(),
                key_env_for(&name),
            ),
            (None, _, Some(other)) => {
                plan.note(format!(
                    "{origin}: the `{other}` API isn't one ferrule maps; add it with \
                     `ferrule setup`"
                ));
                continue;
            }
            (None, None, None) => {
                plan.note(format!(
                    "provider `{name}` isn't a preset ferrule knows and has no baseUrl; add it \
                     with `ferrule setup`"
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
        let key_env = match conf.get("apiKey").and_then(secret_input) {
            Some(input) => {
                match take_secret(
                    plan,
                    input,
                    &default_env,
                    &format!("{origin}.apiKey"),
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
        if default.as_ref().is_some_and(|(p, _)| *p == name) {
            plan.default_provider = Some(ferrule_name);
        }
    }
}
