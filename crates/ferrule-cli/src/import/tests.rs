use super::*;
use std::fs;

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn no_env(_: &str) -> Option<String> {
    None
}

fn texts(plan: &Plan) -> Vec<&str> {
    plan.memories.iter().map(|e| e.text.as_str()).collect()
}

fn targets(dir: &Path) -> Targets {
    Targets {
        config: dir.join("ferrule").join("config.toml"),
        memory_db: dir.join("data").join("memory.db"),
        secrets: dir.join("private").join("secrets.env"),
    }
}

async fn run_to_string(plan: &Plan, t: &Targets, apply: bool, bind: bool) -> (Counts, String) {
    let mut out = Vec::new();
    let counts = run(
        plan,
        t,
        apply,
        bind,
        Skills::Skip("testing".into()),
        &mut out,
    )
    .await
    .unwrap();
    (counts, String::from_utf8(out).unwrap())
}

const TG_SECRET: &str = "123456:tg-secret-value";
const DISCORD_SECRET: &str = "literal-discord-token-value-xyz";
const ANTHROPIC_SECRET: &str = "sk-ant-secretvalue123";

/// An OpenClaw state directory as v2026.9.6 lays it out.
fn openclaw_fixture(root: &Path) -> PathBuf {
    let state = root.join("state");
    write(
        &state.join("openclaw.json"),
        r#"{
          // written by `openclaw onboard`
          agents: { defaults: { model: { primary: 'anthropic/claude-opus-4-6' } } },
          channels: {
            telegram: {
              botToken: "${TG_TOKEN}",
              allowFrom: [123456789, "tg:-100200300", "*"],
              groupAllowFrom: [1],
            },
            discord: {
              token: 'literal-discord-token-value-xyz',
              allowFrom: ["somename"],
              dm: { allowFrom: ["user:111222333"], groupChannels: ["444555666"] },
            },
            slack: { enabled: false, botToken: "${SLACK_X}" },
          },
          models: {
            providers: {
              local: {
                baseUrl: "http://127.0.0.1:8080/v1",
                api: "openai-completions",
                apiKey: "${LOCAL_KEY}",
                models: [{ id: "qwen" }],
              },
              vault: {
                baseUrl: "https://llm.example.com/v1",
                apiKey: { source: "exec", command: "pass show llm" },
                models: ["big"],
              },
            },
          },
        }"#,
    );
    write(
        &state.join(".env"),
        &format!("TG_TOKEN={TG_SECRET}\nANTHROPIC_API_KEY={ANTHROPIC_SECRET}\n"),
    );
    let ws = state.join("workspace");
    write(
        &ws.join("MEMORY.md"),
        &format!(
            "# Memory\n\n## Preferences\n\n- Prefers tea over coffee\n- Uses vim\n\n\
             A paragraph about the project\nspanning two lines.\n\n---\n\n\
             - The api key is {ANTHROPIC_SECRET}\n- The telegram token is {TG_SECRET}\n\
             - Uses vim\n"
        ),
    );
    write(&ws.join("USER.md"), "- Name is Max\n");
    write(&ws.join("memory").join("2026-09-01.md"), "Shipped M32\n");
    write(&ws.join("memory").join("notes.md"), "Not a daily file\n");
    write(&ws.join("AGENTS.md"), "Be brief.\n");
    write(&ws.join("SOUL.md"), "Calm.\n");
    write(
        &ws.join("skills").join("weather").join("SKILL.md"),
        "---\nname: Weather Tool\ndescription: Looks up the weather\n---\nCall the API.\n",
    );
    write(
        &state
            .join("credentials")
            .join("telegram-default-allowFrom.json"),
        r#"{"version": 1, "allowFrom": ["555"]}"#,
    );
    state
}

#[test]
fn an_openclaw_state_dir_becomes_a_plan() {
    let dir = tempfile::tempdir().unwrap();
    let state = openclaw_fixture(dir.path());
    let plan = openclaw::read(&state, None, &no_env, Some(dir.path())).unwrap();

    assert_eq!(
        texts(&plan),
        [
            "Preferences: Prefers tea over coffee",
            "Preferences: Uses vim",
            "Preferences: A paragraph about the project\nspanning two lines.",
            "Name is Max",
            "(2026-09-01) Shipped M32",
        ]
    );
    assert_eq!(plan.withheld, ["MEMORY.md:13", "MEMORY.md:14"]);
    let user = plan.memories.iter().find(|e| e.file == "USER.md").unwrap();
    assert_eq!(user.extra, ["user"]);
    let daily = plan.memories.iter().find(|e| e.extra == ["daily"]).unwrap();
    assert_eq!(daily.file, "memory/2026-09-01.md");

    assert_eq!(plan.allow.telegram_chats, [123456789, -100200300, 555]);
    assert_eq!(plan.allow.discord_users, ["111222333"]);
    assert_eq!(plan.allow.discord_channels, ["444555666"]);
    assert!(plan.allow.slack_users.is_empty());
    assert_eq!(
        plan.tokens,
        [
            ("telegram_token_env", "TG_TOKEN".to_string()),
            ("discord_token_env", "DISCORD_BOT_TOKEN".to_string()),
        ]
    );

    let local = plan.providers.iter().find(|p| p.name == "local").unwrap();
    assert_eq!(local.base_url, "http://127.0.0.1:8080/v1");
    assert_eq!((local.api, local.profile.as_str()), (None, "generic"));
    assert_eq!(
        (local.model.as_str(), local.key_env.as_str()),
        ("qwen", "LOCAL_KEY")
    );
    let anthropic = plan
        .providers
        .iter()
        .find(|p| p.name == "anthropic")
        .unwrap();
    assert_eq!(anthropic.api, Some("anthropic"));
    assert_eq!(anthropic.model, "claude-opus-4-6");
    assert_eq!(anthropic.key_env, "ANTHROPIC_API_KEY");
    assert!(!plan.providers.iter().any(|p| p.name == "vault"));
    assert_eq!(plan.default_provider.as_deref(), Some("anthropic"));

    let secret = |env: &str| plan.secrets.iter().find(|s| s.env == env).unwrap();
    assert_eq!(
        secret("TG_TOKEN").value.as_ref().unwrap().expose(),
        TG_SECRET
    );
    assert_eq!(
        secret("DISCORD_BOT_TOKEN").value.as_ref().unwrap().expose(),
        DISCORD_SECRET
    );
    assert!(secret("LOCAL_KEY").value.is_none());
    assert!(!plan.secrets.iter().any(|s| s.env == "SLACK_X"));

    assert_eq!(plan.skills.len(), 1);
    assert_eq!(plan.skills[0].original, "Weather Tool");
    assert_eq!(plan.skills[0].name, "weather-tool");
    assert_eq!(plan.skills[0].rel, "workspace/skills/weather");

    let notes = plan.notes.join("\n");
    for want in [
        "`*` (anyone)",
        "groupAllowFrom",
        "`somename` isn't an id",
        "channels.slack is disabled",
        "a `exec` secret reference",
        "SOUL.md (persona)",
    ] {
        assert!(notes.contains(want), "no note with {want:?} in:\n{notes}");
    }
    assert_eq!(plan.suggestions.len(), 1);
    assert!(plan.suggestions[0].contains("AGENTS.md"));
    // Values never reach Debug output.
    let debug = format!("{plan:?}");
    assert!(!debug.contains(TG_SECRET) && !debug.contains(DISCORD_SECRET));
}

#[test]
fn openclaw_without_a_config_still_reads_the_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    write(&state.join("workspace").join("MEMORY.md"), "Likes jazz\n");
    let plan = openclaw::read(&state, None, &no_env, None).unwrap();
    assert_eq!(texts(&plan), ["Likes jazz"]);
    assert!(plan.notes.iter().any(|n| n.contains("no openclaw.json")));
    assert!(openclaw::read(&dir.path().join("missing"), None, &no_env, None).is_err());
}

#[test]
fn the_state_and_home_dirs_are_found() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    assert_eq!(openclaw::home(None, &no_env, Some(home)), None);
    assert_eq!(hermes::home(None, &no_env, Some(home)), None);

    fs::create_dir_all(home.join(".openclaw")).unwrap();
    fs::create_dir_all(home.join(".openclaw-work")).unwrap();
    fs::create_dir_all(home.join(".hermes")).unwrap();
    assert_eq!(
        openclaw::home(None, &no_env, Some(home)),
        Some(home.join(".openclaw"))
    );
    let profile = |k: &str| (k == "OPENCLAW_PROFILE").then(|| "work".to_string());
    assert_eq!(
        openclaw::home(None, &profile, Some(home)),
        Some(home.join(".openclaw-work"))
    );
    let state_dir = |k: &str| (k == "OPENCLAW_STATE_DIR").then(|| "~/elsewhere".to_string());
    assert_eq!(
        openclaw::home(None, &state_dir, Some(home)),
        Some(home.join("elsewhere"))
    );
    assert_eq!(
        openclaw::home(Some(Path::new("/given")), &state_dir, Some(home)),
        Some(PathBuf::from("/given"))
    );
    assert_eq!(
        hermes::home(None, &no_env, Some(home)),
        Some(home.join(".hermes"))
    );
    let hermes_home = |k: &str| (k == "HERMES_HOME").then(|| "~/h".to_string());
    assert_eq!(
        hermes::home(None, &hermes_home, Some(home)),
        Some(home.join("h"))
    );
}

/// A Hermes home as v2026.9.24 lays it out, with a `work` profile active.
fn hermes_fixture(root: &Path) -> PathBuf {
    let hermes = root.join("hermes");
    write(&hermes.join("active_profile"), "work\n");
    let p = hermes.join("profiles").join("work");
    write(
        &p.join(".env"),
        "TELEGRAM_ALLOWED_USERS=111,222\nDISCORD_ALLOWED_USERS=333\n\
         TELEGRAM_BOT_TOKEN=999:hermes-tg-secret\nOPENROUTER_API_KEY=sk-or-v1-abcdef123456\n\
         GATEWAY_ALLOW_ALL_USERS=true\nSLACK_BOT_TOKEN=xoxb-a,xoxb-b\n",
    );
    write(
        &p.join("config.yaml"),
        "# Hermes config\n\
         model:\n  default: anthropic/claude-sonnet-4.6\n  provider: auto\n\
         providers:\n  mylocal:\n    base_url: http://localhost:9000/v1\n    model: llama\n    \
         api_key: ${MYLOCAL_KEY}\n  nous:\n    model: hermes-4\n\
         platforms:\n  slack:\n    allowed_channels: [C0123ABC, general]\n\
         skills:\n  external_dirs: [~/shared-skills]\n\
         system_prompt: |\n  be nice\n\
         mcp_servers:\n  fs:\n    command: npx\n",
    );
    write(
        &p.join("memories").join("MEMORY.md"),
        "Likes Rust\n§\nWorks at a mat company\n§\n\n",
    );
    write(&p.join("memories").join("USER.md"), "Lives in Haifa\n");
    write(
        &p.join("skills")
            .join("research")
            .join("arxiv")
            .join("SKILL.md"),
        "---\nname: arxiv\ndescription: Searches arXiv\n---\nSearch.\n",
    );
    write(
        &root.join("shared-skills").join("pdf").join("SKILL.md"),
        "---\nname: pdf\ndescription: Reads PDFs\n---\nRead.\n",
    );
    hermes
}

#[test]
fn a_hermes_profile_becomes_a_plan() {
    let dir = tempfile::tempdir().unwrap();
    let hermes = hermes_fixture(dir.path());
    let plan = hermes::read(&hermes, None, Some(dir.path())).unwrap();
    assert_eq!(plan.home, hermes.join("profiles").join("work"));

    assert_eq!(
        texts(&plan),
        ["Likes Rust", "Works at a mat company", "Lives in Haifa"]
    );
    assert_eq!(plan.memories[1].line, 3);
    assert_eq!(plan.memories[2].extra, ["user"]);
    assert_eq!(plan.memories[2].file, "memories/USER.md");

    assert_eq!(plan.allow.telegram_chats, [111, 222]);
    assert_eq!(plan.allow.discord_users, ["333"]);
    assert_eq!(plan.allow.slack_channels, ["C0123ABC"]);
    assert_eq!(
        plan.tokens,
        [("telegram_token_env", "TELEGRAM_BOT_TOKEN".to_string())]
    );

    let or = plan
        .providers
        .iter()
        .find(|p| p.name == "openrouter")
        .unwrap();
    // OpenRouter keeps the vendor prefix; it routes on it.
    assert_eq!(or.model, "anthropic/claude-sonnet-4.6");
    assert_eq!(or.key_env, "OPENROUTER_API_KEY");
    let local = plan.providers.iter().find(|p| p.name == "mylocal").unwrap();
    assert_eq!(local.base_url, "http://localhost:9000/v1");
    assert_eq!(
        (local.model.as_str(), local.key_env.as_str()),
        ("llama", "MYLOCAL_KEY")
    );
    assert!(!plan.providers.iter().any(|p| p.name == "nous"));
    assert_eq!(plan.default_provider.as_deref(), Some("openrouter"));

    let mut skills: Vec<&str> = plan.skills.iter().map(|s| s.name.as_str()).collect();
    skills.sort();
    assert_eq!(skills, ["arxiv", "pdf"]);

    let notes = plan.notes.join("\n");
    for want in [
        "system_prompt",
        "`nous` signs in with OAuth",
        "GATEWAY_ALLOW_ALL_USERS",
        "several workspaces",
        "`general` isn't an id",
        "mcp_servers",
    ] {
        assert!(notes.contains(want), "no note with {want:?} in:\n{notes}");
    }
}

#[test]
fn a_hermes_profile_is_picked_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let hermes = hermes_fixture(dir.path());
    write(&hermes.join("memories").join("MEMORY.md"), "At the root\n");
    let root = hermes::read(&hermes, Some("default"), None).unwrap();
    assert_eq!(texts(&root), ["At the root"]);
    let err = hermes::read(&hermes, Some("nope"), None).unwrap_err();
    assert!(err.to_string().contains("no Hermes profile `nope`"));
}

#[tokio::test]
async fn a_dry_run_writes_nothing_and_shows_no_secret() {
    let dir = tempfile::tempdir().unwrap();
    let state = openclaw_fixture(dir.path());
    let plan = openclaw::read(&state, None, &no_env, Some(dir.path())).unwrap();
    let out_dir = dir.path().join("out");
    let t = targets(&out_dir);
    let (counts, out) = run_to_string(&plan, &t, false, false).await;

    assert!(!out_dir.exists(), "a dry run created {}", out_dir.display());
    assert_eq!(counts.added, 5);
    // 3 telegram + 1 discord user + 1 discord channel + 2 tokens + 2
    // providers + the default.
    assert_eq!(counts.config_changes, 10);
    assert!(out.contains("dry run, nothing written"));
    assert!(out.contains("weather-tool"));
    assert!(out.contains("TG_TOKEN: found in"));
    assert!(out.contains("LOCAL_KEY: referenced in"));
    for secret in [TG_SECRET, DISCORD_SECRET, ANTHROPIC_SECRET] {
        assert!(!out.contains(secret), "the output shows a secret:\n{out}");
    }
}

#[tokio::test]
async fn apply_is_idempotent_and_supersedes_an_edited_entry() {
    let dir = tempfile::tempdir().unwrap();
    let state = openclaw_fixture(dir.path());
    let t = targets(&dir.path().join("out"));
    let read = || openclaw::read(&state, None, &no_env, Some(dir.path())).unwrap();

    let (first, _) = run_to_string(&read(), &t, true, false).await;
    assert_eq!((first.added, first.kept, first.superseded), (5, 0, 0));
    assert_eq!(first.config_changes, 10);
    assert_eq!(first.skills_skipped, 1);
    let config = fs::read_to_string(&t.config).unwrap();
    for want in [
        "telegram_allowed_chats = [123456789, -100200300, 555]",
        "discord_allowed_users = [\"111222333\"]",
        "telegram_token_env = \"TG_TOKEN\"",
        "[providers.anthropic]",
        "default_provider = \"anthropic\"",
    ] {
        assert!(config.contains(want), "no {want:?} in:\n{config}");
    }
    for secret in [TG_SECRET, DISCORD_SECRET, ANTHROPIC_SECRET] {
        assert!(!config.contains(secret));
    }
    assert!(
        !t.secrets.exists(),
        "secrets written without --bind-secrets"
    );
    let store = MemoryStore::open(&t.memory_db).unwrap();
    let user = store
        .live_tagged(&["import:openclaw", "from:USER.md"])
        .unwrap();
    assert_eq!(user.len(), 1);
    assert!(user[0].tags.iter().any(|t| t == "user"));
    drop(store);

    let (second, out) = run_to_string(&read(), &t, true, false).await;
    assert_eq!((second.added, second.kept, second.superseded), (0, 5, 0));
    assert_eq!(second.config_changes, 0);
    assert!(out.contains("nothing to change"));
    assert_eq!(fs::read_to_string(&t.config).unwrap(), config);

    let memory = state.join("workspace").join("MEMORY.md");
    let text = fs::read_to_string(&memory).unwrap();
    fs::write(&memory, text.replace("Prefers tea", "Prefers green tea")).unwrap();
    let (third, _) = run_to_string(&read(), &t, true, false).await;
    assert_eq!((third.added, third.kept, third.superseded), (0, 4, 1));
    let store = MemoryStore::open(&t.memory_db).unwrap();
    let live: Vec<String> = store
        .live_tagged(&["import:openclaw", "from:MEMORY.md"])
        .unwrap()
        .into_iter()
        .map(|m| m.content)
        .collect();
    assert_eq!(live.len(), 3);
    assert!(live.contains(&"Preferences: Prefers green tea over coffee".to_string()));
    assert!(!live.contains(&"Preferences: Prefers tea over coffee".to_string()));
}

#[tokio::test]
async fn bind_secrets_copies_values_once() {
    let dir = tempfile::tempdir().unwrap();
    let state = openclaw_fixture(dir.path());
    let plan = openclaw::read(&state, None, &no_env, Some(dir.path())).unwrap();
    let t = targets(&dir.path().join("out"));
    let (first, out) = run_to_string(&plan, &t, true, true).await;
    assert_eq!(first.secrets_written, 3);
    assert!(!out.contains(TG_SECRET));
    let stored: BTreeMap<String, String> = secrets::read(&t.secrets).unwrap().into_iter().collect();
    assert_eq!(stored.get("TG_TOKEN").map(String::as_str), Some(TG_SECRET));
    assert_eq!(
        stored.get("DISCORD_BOT_TOKEN").map(String::as_str),
        Some(DISCORD_SECRET)
    );
    assert_eq!(
        stored.get("ANTHROPIC_API_KEY").map(String::as_str),
        Some(ANTHROPIC_SECRET)
    );
    assert!(!stored.contains_key("LOCAL_KEY"));
    let (second, out) = run_to_string(&plan, &t, true, true).await;
    assert_eq!(second.secrets_written, 0);
    assert!(out.contains("TG_TOKEN: already in ferrule's secret store"));
}

#[tokio::test]
async fn allowlists_join_what_the_config_has() {
    let dir = tempfile::tempdir().unwrap();
    let state = openclaw_fixture(dir.path());
    let plan = openclaw::read(&state, None, &no_env, Some(dir.path())).unwrap();
    let t = targets(&dir.path().join("out"));
    write(
        &t.config,
        "# mine\n[gateway]\ntelegram_allowed_chats = [42, 123456789]\ntelegram_token_env = \"MY_TG\"\n",
    );
    let (counts, _) = run_to_string(&plan, &t, true, false).await;
    // -100200300 and 555 are new; the token is already set.
    assert_eq!(counts.config_changes, 2 + 2 + 1 + 2 + 1);
    let config = fs::read_to_string(&t.config).unwrap();
    assert!(config.contains("# mine\n[gateway]"), "{config}");
    assert!(config.contains("telegram_allowed_chats = [42, 123456789, -100200300, 555]"));
    assert!(config.contains("telegram_token_env = \"MY_TG\""));
    assert!(!config.contains("\"TG_TOKEN\""));
}

#[test]
fn memories_that_carry_a_secret_are_held_back() {
    let mut plan = Plan::new(Tool::Hermes, PathBuf::new());
    plan.secret("MY_PASS", Some("hunter2hunter2".into()), ".env");
    for (i, text) in [
        "The key is ghp_notarealtoken",
        "My password is hunter2hunter2",
        "Likes long walks",
        "likes long walks",
    ]
    .into_iter()
    .enumerate()
    {
        plan.memories.push(Entry {
            text: text.into(),
            extra: Vec::new(),
            file: "memories/MEMORY.md".into(),
            line: i + 1,
        });
    }
    plan.finish();
    assert_eq!(texts(&plan), ["Likes long walks"]);
    assert_eq!(
        plan.withheld,
        ["memories/MEMORY.md:1", "memories/MEMORY.md:2"]
    );
}

#[test]
fn skill_names_become_ones_ferrule_accepts() {
    assert_eq!(skill_name("Weather Tool"), "weather-tool");
    assert_eq!(skill_name("My__Skill!"), "my_skill");
    assert_eq!(skill_name("  "), "skill");
    let long = skill_name(&"x".repeat(60));
    assert_eq!(long.len(), 40);
    assert!(long.starts_with(&"x".repeat(33)));

    let mut plan = Plan::new(Tool::OpenClaw, PathBuf::new());
    for rel in ["a/web", "b/web"] {
        plan.skills.push(FoundSkill {
            dir: PathBuf::from(rel),
            rel: rel.into(),
            original: "Web".into(),
            name: String::new(),
        });
    }
    plan.finish();
    assert_eq!(plan.skills[0].name, "web");
    assert!(plan.skills[1].name.starts_with("web-") && plan.skills[1].name.len() == 10);

    assert_eq!(
        rename_skill("---\nname: Web\ndescription: d\n---\nbody\n", "web"),
        "---\nname: web\ndescription: d\n---\nbody\n"
    );
    assert_eq!(
        rename_skill("---\ndescription: d\n---\nbody", "web"),
        "---\nname: web\ndescription: d\n---\nbody\n"
    );
    assert_eq!(rename_skill("body\n", "web"), "---\nname: web\n---\nbody\n");
}

#[test]
fn secret_fields_and_id_lists_read_every_form() {
    use serde_json::json;
    assert_eq!(
        secret_input(&json!("${TOKEN}")),
        Some(SecretIn::Env("TOKEN".into()))
    );
    assert_eq!(
        secret_input(&json!({"source": "env", "id": "TOKEN"})),
        Some(SecretIn::Env("TOKEN".into()))
    );
    assert_eq!(
        secret_input(&json!({"source": "file", "path": "/x"})),
        Some(SecretIn::NotPortable("file".into()))
    );
    assert_eq!(
        secret_input(&json!(" abc ")),
        Some(SecretIn::Literal("abc".into()))
    );
    assert_eq!(secret_input(&json!("")), None);
    assert_eq!(format!("{:?}", Hidden::new("abc")), "[hidden]");

    assert_eq!(id_list(&json!([1, "2", " "])), ["1", "2"]);
    assert_eq!(id_list(&json!("[3, \"4\"]")), ["3", "4"]);
    assert_eq!(id_list(&json!("5, '6' ,")), ["5", "6"]);
    assert_eq!(id_list(&json!(7)), ["7"]);
}

#[test]
fn markdown_splits_into_entries() {
    let text =
        "# Title\n\nIntro line\n\n## Work\n- one\n  more of one\n1. two\n\n```\n- in code\n```\n";
    let got: Vec<(String, usize)> = split_markdown(text, "f.md", &[], "")
        .into_iter()
        .map(|e| (e.text, e.line))
        .collect();
    assert_eq!(
        got,
        [
            ("Intro line".to_string(), 3),
            ("Work: one\nmore of one".to_string(), 6),
            ("Work: two".to_string(), 8),
            ("Work: ```\n- in code\n```".to_string(), 10),
        ]
    );
}
