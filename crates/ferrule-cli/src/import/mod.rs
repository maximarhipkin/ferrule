//! M33: `ferrule import openclaw|hermes` (docs/migrate.md). Each source
//! reader turns the other tool's files into a [`Plan`], the same shape for
//! both; [`run`] compares it with what ferrule has, prints what it would do
//! and, with `--apply`, does it. Every step is idempotent, so a second run
//! changes nothing and a run cut short is finished by the next one.

mod hermes;
mod json5;
mod openclaw;
mod yaml;

use crate::config;
use crate::secrets;
use crate::setup::{put, table, tilde};
use anyhow::{anyhow, bail, Context, Result};
use clap::Subcommand;
use ferrule_extensions::{ExtensionManager, Outcome, Review};
use ferrule_memory::MemoryStore;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use toml_edit::{Array, Item};

/// More than this is almost certainly not a memory file.
pub const MAX_MEMORIES: usize = 2000;
pub const MAX_SKILLS: usize = 200;

#[derive(Subcommand)]
pub enum ImportCmd {
    /// From OpenClaw: memories, skills, channel allowlists, providers
    Openclaw {
        /// The state directory (default: $OPENCLAW_STATE_DIR, ~/.openclaw)
        #[arg(long)]
        from: Option<PathBuf>,
        /// The agent workspace (default: the config's, or <state>/workspace)
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Write; without it nothing is written
        #[arg(long)]
        apply: bool,
        /// With --apply: copy the keys and tokens found into ferrule's
        /// secret store
        #[arg(long)]
        bind_secrets: bool,
    },
    /// From Hermes Agent: memories, skills, channel allowlists, providers
    Hermes {
        /// The home directory (default: $HERMES_HOME, ~/.hermes)
        #[arg(long)]
        from: Option<PathBuf>,
        /// A profile under <home>/profiles (default: the active one)
        #[arg(long)]
        profile: Option<String>,
        /// Write; without it nothing is written
        #[arg(long)]
        apply: bool,
        /// With --apply: copy the keys and tokens found into ferrule's
        /// secret store
        #[arg(long)]
        bind_secrets: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    OpenClaw,
    Hermes,
}

impl Tool {
    pub fn name(self) -> &'static str {
        match self {
            Tool::OpenClaw => "openclaw",
            Tool::Hermes => "hermes",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Tool::OpenClaw => "OpenClaw",
            Tool::Hermes => "Hermes Agent",
        }
    }
}

/// A secret value: never printed, not even by `{:?}`.
#[derive(Clone, PartialEq, Eq)]
pub struct Hidden(String);

impl Hidden {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Hidden {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[hidden]")
    }
}

/// One memory to import.
#[derive(Debug, Clone)]
pub struct Entry {
    pub text: String,
    /// `user`, `daily`: on top of `import:<tool>` and `from:<file>`.
    pub extra: Vec<&'static str>,
    /// Relative to the source, `/`-separated.
    pub file: String,
    pub line: usize,
}

#[derive(Debug, Clone)]
pub struct FoundSkill {
    pub dir: PathBuf,
    /// Where it was found, for the lock's source label.
    pub rel: String,
    /// The name in its SKILL.md.
    pub original: String,
    /// The name ferrule will use.
    pub name: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Allow {
    pub telegram_chats: Vec<i64>,
    pub discord_users: Vec<String>,
    pub discord_channels: Vec<String>,
    pub slack_users: Vec<String>,
    pub slack_channels: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Provider {
    pub name: String,
    pub base_url: String,
    /// `anthropic` or `None` (chat).
    pub api: Option<&'static str>,
    pub profile: String,
    pub key_env: String,
    pub model: String,
}

#[derive(Debug, Clone)]
pub struct Secret {
    pub env: String,
    /// `None`: referenced, but the value isn't in the source.
    pub value: Option<Hidden>,
    /// Where it was found (a file, a config key); no value.
    pub origin: String,
}

/// Everything a source says, in ferrule's terms. Built by the readers,
/// finished by [`Plan::finish`].
#[derive(Debug, Clone)]
pub struct Plan {
    pub tool: Tool,
    pub home: PathBuf,
    pub memories: Vec<Entry>,
    /// `file:line` of entries held back for carrying a secret.
    pub withheld: Vec<String>,
    pub skills: Vec<FoundSkill>,
    pub allow: Allow,
    /// `[gateway]` key (`telegram_token_env`, …) → env name.
    pub tokens: Vec<(&'static str, String)>,
    pub providers: Vec<Provider>,
    pub default_provider: Option<String>,
    pub secrets: Vec<Secret>,
    /// Found but not imported, and why.
    pub notes: Vec<String>,
    pub suggestions: Vec<String>,
}

impl Plan {
    pub fn new(tool: Tool, home: PathBuf) -> Self {
        Self {
            tool,
            home,
            memories: Vec::new(),
            withheld: Vec::new(),
            skills: Vec::new(),
            allow: Allow::default(),
            tokens: Vec::new(),
            providers: Vec::new(),
            default_provider: None,
            secrets: Vec::new(),
            notes: Vec::new(),
            suggestions: Vec::new(),
        }
    }

    pub fn note(&mut self, text: impl Into<String>) {
        let text = text.into();
        if !self.notes.contains(&text) {
            self.notes.push(text);
        }
    }

    /// Record a secret under `env`; a value found later fills in a
    /// reference found earlier.
    pub fn secret(&mut self, env: &str, value: Option<String>, origin: &str) {
        let value = value.filter(|v| !v.is_empty()).map(Hidden::new);
        match self.secrets.iter_mut().find(|s| s.env == env) {
            Some(s) => {
                if s.value.is_none() {
                    s.value = value;
                }
            }
            None => self.secrets.push(Secret {
                env: env.to_string(),
                value,
                origin: origin.to_string(),
            }),
        }
    }

    /// Set a channel token's env name, once.
    pub fn token(&mut self, key: &'static str, env: &str) {
        if !self.tokens.iter().any(|(k, _)| *k == key) {
            self.tokens.push((key, env.to_string()));
        }
    }

    pub fn provider(&mut self, p: Provider) {
        if !self.providers.iter().any(|q| q.name == p.name) {
            self.providers.push(p);
        }
    }

    /// Hold back memories that carry a secret, drop duplicates, cap the
    /// sizes, and give skills names ferrule accepts.
    pub fn finish(&mut self) {
        let values: Vec<String> = self
            .secrets
            .iter()
            .filter_map(|s| s.value.as_ref())
            .map(|v| v.expose().to_string())
            .filter(|v| v.len() >= 6)
            .collect();
        let redactor = ferrule_gateway::Redactor::default();
        let mut kept: Vec<Entry> = Vec::new();
        for e in std::mem::take(&mut self.memories) {
            let secretish = e.text.split_whitespace().any(config::looks_like_key)
                || redactor.redact(&e.text) != e.text
                || values.iter().any(|v| e.text.contains(v.as_str()));
            if secretish {
                self.withheld.push(format!("{}:{}", e.file, e.line));
            } else if !kept
                .iter()
                .any(|k| ferrule_memory::same_fact(&k.text, &e.text))
            {
                kept.push(e);
            }
        }
        if kept.len() > MAX_MEMORIES {
            self.note(format!(
                "{} memories found; only the first {MAX_MEMORIES} are imported",
                kept.len()
            ));
            kept.truncate(MAX_MEMORIES);
        }
        self.memories = kept;
        if self.skills.len() > MAX_SKILLS {
            self.note(format!(
                "{} skills found; only the first {MAX_SKILLS} are imported",
                self.skills.len()
            ));
            self.skills.truncate(MAX_SKILLS);
        }
        let mut taken = HashSet::new();
        for s in &mut self.skills {
            let mut name = skill_name(&s.original);
            if !taken.insert(name.clone()) {
                name = with_hash(&name, &s.rel);
                taken.insert(name.clone());
            }
            s.name = name;
        }
    }
}

// ── Reading helpers shared by the sources ──────────────────────────────

/// A file's text; `None` when it isn't there. Other read errors are notes.
pub(crate) fn read_text(plan: &mut Plan, path: &Path) -> Option<String> {
    match std::fs::read(path) {
        Ok(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            plan.note(format!("couldn't read {}: {e}", path.display()));
            None
        }
    }
}

/// `path` relative to `base` with `/` separators; commas go, since tags
/// are stored comma-joined.
pub(crate) fn rel(base: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(base).unwrap_or(path);
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
        .replace(',', "")
}

/// A leading `~/` (or `~`) as `home`.
pub(crate) fn expand_tilde(path: &str, home: Option<&Path>) -> PathBuf {
    match (path.strip_prefix('~'), home) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with(['/', '\\']) => {
            home.join(rest.trim_start_matches(['/', '\\']))
        }
        _ => PathBuf::from(path),
    }
}

/// Markdown split into memories: at top-level list items, otherwise at
/// blank-line paragraphs. An H2+ heading prefixes the entries under it;
/// an H1 is the file's title and is dropped.
pub(crate) fn split_markdown(
    text: &str,
    file: &str,
    extra: &[&'static str],
    prefix: &str,
) -> Vec<Entry> {
    let mut out = Vec::new();
    let mut heading = String::new();
    let mut cur: Option<(String, usize)> = None;
    let mut fence = false;
    let flush = |cur: &mut Option<(String, usize)>, heading: &str, out: &mut Vec<Entry>| {
        if let Some((body, line)) = cur.take() {
            let body = body.trim();
            if body.is_empty() || body.chars().all(|c| c == '-' || c == '*' || c == '_') {
                return;
            }
            let text = if heading.is_empty() {
                format!("{prefix}{body}")
            } else {
                format!("{prefix}{heading}: {body}")
            };
            out.push(Entry {
                text,
                extra: extra.to_vec(),
                file: file.to_string(),
                line,
            });
        }
    };
    for (i, raw) in text.lines().enumerate() {
        let n = i + 1;
        let line = raw.trim_end();
        if line.trim_start().starts_with("```") {
            fence = !fence;
        }
        if !fence {
            if let Some(h) = heading_text(line) {
                flush(&mut cur, &heading, &mut out);
                heading = h;
                continue;
            }
            if line.trim().is_empty() {
                flush(&mut cur, &heading, &mut out);
                continue;
            }
            if let Some(item) = list_item(line) {
                flush(&mut cur, &heading, &mut out);
                cur = Some((item.to_string(), n));
                continue;
            }
            if line.trim().chars().all(|c| c == '-') && line.trim().len() >= 3 {
                flush(&mut cur, &heading, &mut out);
                continue;
            }
        }
        match &mut cur {
            Some((body, _)) => {
                body.push('\n');
                body.push_str(line.trim());
            }
            None => cur = Some((line.trim().to_string(), n)),
        }
    }
    flush(&mut cur, &heading, &mut out);
    out
}

fn heading_text(line: &str) -> Option<String> {
    let hashes = line.bytes().take_while(|b| *b == b'#').count();
    if hashes == 0 || hashes > 6 || !line[hashes..].starts_with(' ') {
        return None;
    }
    Some(if hashes == 1 {
        String::new()
    } else {
        line[hashes..].trim().trim_end_matches(':').to_string()
    })
}

/// The text of a top-level list item (`- `, `* `, `+ `, `1. `).
fn list_item(line: &str) -> Option<&str> {
    if let Some(rest) = line
        .strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .or_else(|| line.strip_prefix("+ "))
    {
        return Some(rest.trim());
    }
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0 && line[digits..].starts_with(". ") {
        return Some(line[digits + 2..].trim());
    }
    None
}

/// Entries joined by a separator line (Hermes's `§`), with line numbers.
pub(crate) fn split_on(text: &str, sep: &str, file: &str, extra: &[&'static str]) -> Vec<Entry> {
    let mut out = Vec::new();
    let mut body = String::new();
    let mut start = 1;
    let text = text.replace("\r\n", "\n");
    let lines: Vec<&str> = text.lines().collect();
    let mut push = |body: &mut String, start: usize| {
        let t = body.trim();
        if !t.is_empty() {
            out.push(Entry {
                text: t.to_string(),
                extra: extra.to_vec(),
                file: file.to_string(),
                line: start,
            });
        }
        body.clear();
    };
    for (i, line) in lines.iter().enumerate() {
        if line.trim() == sep {
            push(&mut body, start);
            start = i + 2;
            continue;
        }
        if body.is_empty() && line.trim().is_empty() {
            start = i + 2;
            continue;
        }
        body.push_str(line);
        body.push('\n');
    }
    push(&mut body, start);
    out
}

/// Every `<dir>/SKILL.md` under `root`, at most four levels down, skipping
/// dot-directories.
pub(crate) fn find_skills(plan: &mut Plan, root: &Path, label: &str) {
    fn walk(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
        if depth > 4 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        let mut dirs: Vec<PathBuf> = rd
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter(|e| {
                let n = e.file_name();
                let n = n.to_string_lossy();
                !n.starts_with('.') && n != "_org" && n != "node_modules"
            })
            .map(|e| e.path())
            .collect();
        dirs.sort();
        for d in dirs {
            if d.join("SKILL.md").is_file() {
                found.push(d);
            } else {
                walk(&d, depth + 1, found);
            }
        }
    }
    let mut found = Vec::new();
    walk(root, 1, &mut found);
    for dir in found {
        if plan.skills.iter().any(|s| s.dir == dir) {
            continue;
        }
        let text = std::fs::read_to_string(dir.join("SKILL.md")).unwrap_or_default();
        let original = ferrule_skills::frontmatter::parse(&text)
            .ok()
            .and_then(|fm| fm.get("name").map(str::to_string))
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| {
                dir.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            });
        plan.skills.push(FoundSkill {
            rel: format!("{label}/{}", rel(root, &dir)),
            dir,
            original,
            name: String::new(),
        });
    }
}

/// A name ferrule's skill rule accepts: lowercase `a-z0-9-_`, no `__`,
/// starting with a letter or digit, at most 40 characters.
pub(crate) fn skill_name(original: &str) -> String {
    let mut name = String::new();
    for c in original.trim().chars().flat_map(char::to_lowercase) {
        let c = if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' {
            c
        } else {
            '-'
        };
        if c == '_' && name.ends_with('_') {
            continue;
        }
        name.push(c);
    }
    let name = name.trim_matches(|c| c == '-' || c == '_').to_string();
    let name = if name.is_empty() {
        "skill".to_string()
    } else {
        name
    };
    if name.len() > 40 {
        with_hash(&name, original)
    } else {
        name
    }
}

/// `name` cut to fit, then `-` and six hex digits of `seed`.
fn with_hash(name: &str, seed: &str) -> String {
    let hash = &ferrule_extensions::scan::bytes_digest(seed.as_bytes())[..6];
    let keep = name.len().min(33);
    let base = name[..keep].trim_end_matches(['-', '_']);
    format!("{base}-{hash}")
}

/// A secret field as the sources write it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SecretIn {
    Literal(String),
    /// `${VAR}` or `{source: "env", id: VAR}`.
    Env(String),
    /// `{source: store|file|exec, …}`: not something ferrule can follow.
    NotPortable(String),
}

pub(crate) fn secret_input(v: &Value) -> Option<SecretIn> {
    match v {
        Value::String(s) if s.trim().is_empty() => None,
        Value::String(s) => {
            let t = s.trim();
            match t.strip_prefix("${").and_then(|r| r.strip_suffix('}')) {
                Some(var) if secrets::valid_name(var) => Some(SecretIn::Env(var.to_string())),
                _ => Some(SecretIn::Literal(t.to_string())),
            }
        }
        Value::Object(m) => {
            let source = m.get("source").and_then(Value::as_str).unwrap_or("");
            let id = m.get("id").and_then(Value::as_str).unwrap_or("");
            if source == "env" && secrets::valid_name(id) {
                Some(SecretIn::Env(id.to_string()))
            } else {
                Some(SecretIn::NotPortable(if source.is_empty() {
                    "an object".to_string()
                } else {
                    source.to_string()
                }))
            }
        }
        _ => None,
    }
}

/// Record a secret field: the env name ferrule will read it from, or
/// `None` when it can't be carried over (noted). `default_env` names a
/// literal; `dotenv` is the source's own `.env`.
pub(crate) fn take_secret(
    plan: &mut Plan,
    input: SecretIn,
    default_env: &str,
    origin: &str,
    dotenv: &BTreeMap<String, String>,
) -> Option<String> {
    match input {
        SecretIn::Literal(value) => {
            plan.secret(default_env, Some(value), origin);
            Some(default_env.to_string())
        }
        SecretIn::Env(var) => {
            plan.secret(&var, dotenv.get(&var).cloned(), origin);
            Some(var)
        }
        SecretIn::NotPortable(kind) => {
            plan.note(format!(
                "{origin}: a `{kind}` secret reference doesn't carry over; set its value in \
                 ferrule by hand"
            ));
            None
        }
    }
}

/// An allowlist value: a list, a JSON list in a string, or a comma list.
pub(crate) fn id_list(v: &Value) -> Vec<String> {
    match v {
        Value::Array(items) => items
            .iter()
            .filter_map(|i| match i {
                Value::String(s) => Some(s.trim().to_string()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            })
            .filter(|s| !s.is_empty())
            .collect(),
        Value::Number(n) => vec![n.to_string()],
        Value::String(s) => {
            let t = s.trim();
            if t.starts_with('[') {
                if let Ok(v @ Value::Array(_)) = serde_json::from_str::<Value>(t) {
                    return id_list(&v);
                }
            }
            t.split(',')
                .map(|p| p.trim().trim_matches(|c| c == '"' || c == '\'').to_string())
                .filter(|p| !p.is_empty())
                .collect()
        }
        _ => Vec::new(),
    }
}

/// Which allowlist an id goes to.
#[derive(Debug, Clone, Copy)]
pub(crate) enum List {
    TelegramChats,
    DiscordUsers,
    DiscordChannels,
    SlackUsers,
    SlackChannels,
}

/// Add ids to a list, noting what ferrule can't take: `*`, and ids that
/// aren't the platform's shape.
pub(crate) fn allow(plan: &mut Plan, list: List, ids: Vec<String>, origin: &str) {
    for raw in ids {
        let id = raw
            .trim()
            .trim_start_matches("telegram:")
            .trim_start_matches("tg:")
            .trim_start_matches("discord:")
            .trim_start_matches("slack:")
            .trim_start_matches("user:")
            .trim_start_matches("channel:")
            .trim_start_matches('@')
            .to_string();
        if id == "*" {
            plan.note(format!(
                "{origin}: `*` (anyone) isn't imported; ferrule's allowlists name ids"
            ));
            continue;
        }
        let ok = match list {
            List::TelegramChats => match id.parse::<i64>() {
                Ok(n) => {
                    if !plan.allow.telegram_chats.contains(&n) {
                        plan.allow.telegram_chats.push(n);
                    }
                    continue;
                }
                Err(_) => false,
            },
            List::DiscordUsers | List::DiscordChannels => {
                !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())
            }
            List::SlackUsers | List::SlackChannels => {
                !id.is_empty()
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
            }
        };
        if !ok {
            plan.note(format!(
                "{origin}: `{id}` isn't an id ferrule can use (names are resolved at run \
                 time there; ferrule needs the numeric id)"
            ));
            continue;
        }
        let target = match list {
            List::DiscordUsers => &mut plan.allow.discord_users,
            List::DiscordChannels => &mut plan.allow.discord_channels,
            List::SlackUsers => &mut plan.allow.slack_users,
            List::SlackChannels => &mut plan.allow.slack_channels,
            List::TelegramChats => unreachable!(),
        };
        if !target.contains(&id) {
            target.push(id);
        }
    }
}

/// A preset by the name the sources use for it.
pub(crate) fn preset(name: &str) -> Option<&'static crate::setup::Preset> {
    let name = match name.to_ascii_lowercase().as_str() {
        "google" | "gemini" | "google-gemini" => "gemini",
        "moonshot" | "kimi" | "moonshotai" => "kimi",
        other => return crate::setup::preset(other),
    };
    crate::setup::preset(name)
}

/// A preset's URL, unless the source points somewhere else (a proxy, a
/// regional host): the same host written without `/v1` is still the preset.
pub(crate) fn preset_url(p: &crate::setup::Preset, given: Option<&str>) -> String {
    let host = |u: &str| {
        reqwest::Url::parse(u)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
    };
    match given {
        Some(u) if host(u) != host(p.base_url) => u.trim_end_matches('/').to_string(),
        _ => p.base_url.to_string(),
    }
}

/// A provider name ferrule accepts.
pub(crate) fn provider_name(name: &str) -> String {
    let n: String = name
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let n = n.trim_matches('-').to_string();
    if n.is_empty() {
        "imported".into()
    } else {
        n
    }
}

/// `NAME_API_KEY` for a provider name.
pub(crate) fn key_env_for(name: &str) -> String {
    let up: String = name
        .to_ascii_uppercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("{}_API_KEY", up.trim_matches('_'))
}

// ── Comparing with ferrule, and applying ───────────────────────────────

/// Where ferrule keeps what the import writes.
pub struct Targets {
    pub config: PathBuf,
    pub memory_db: PathBuf,
    pub secrets: PathBuf,
}

impl Targets {
    pub fn real() -> Result<Self> {
        let config = match config::config_path()? {
            Some(p) => p,
            None => config::global_config_path()?,
        };
        let data = config::data_dir()?;
        Ok(Self {
            config,
            memory_db: data.join("memory.db"),
            secrets: secrets::path()?,
        })
    }
}

/// How skills are handled on `--apply`.
pub enum Skills<'a> {
    /// Not installed; the reason is said once.
    Skip(String),
    /// Each is reviewed and confirmed through `confirm`.
    Install {
        manager: Arc<ExtensionManager>,
        confirm: &'a (dyn Fn(&Review) -> bool + Sync),
    },
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Counts {
    pub added: usize,
    pub kept: usize,
    pub superseded: usize,
    pub skills_installed: usize,
    pub skills_skipped: usize,
    pub config_changes: usize,
    pub secrets_written: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Action {
    Keep,
    Add,
    Supersede(i64),
}

/// What happens to each memory, given the store as it is.
fn memory_actions(plan: &Plan, store: Option<&MemoryStore>) -> Result<Vec<Action>> {
    let Some(store) = store else {
        return Ok(vec![Action::Add; plan.memories.len()]);
    };
    let tool_tag = format!("import:{}", plan.tool.name());
    let mut actions = vec![Action::Add; plan.memories.len()];
    let files: BTreeSet<&str> = plan.memories.iter().map(|e| e.file.as_str()).collect();
    for file in files {
        let from = format!("from:{file}");
        let old = store.live_tagged(&[&tool_tag, &from])?;
        let mut used: HashSet<i64> = HashSet::new();
        let idx: Vec<usize> = (0..plan.memories.len())
            .filter(|&i| plan.memories[i].file == file)
            .collect();
        for &i in &idx {
            if let Some(id) = store.known(&plan.memories[i].text)? {
                used.insert(id);
                actions[i] = Action::Keep;
            }
        }
        for &i in &idx {
            if actions[i] != Action::Add {
                continue;
            }
            if let Some(o) = old.iter().find(|o| {
                !used.contains(&o.id)
                    && ferrule_memory::resembles(&o.content, &plan.memories[i].text)
            }) {
                used.insert(o.id);
                actions[i] = Action::Supersede(o.id);
            }
        }
    }
    Ok(actions)
}

/// The `[gateway]` and provider changes the plan makes to `cfg`.
#[derive(Debug, Default, PartialEq)]
struct ConfigDiff {
    allow: Allow,
    tokens: Vec<(&'static str, String)>,
    providers: Vec<Provider>,
    default_provider: Option<String>,
}

impl ConfigDiff {
    fn len(&self) -> usize {
        let a = &self.allow;
        a.telegram_chats.len()
            + a.discord_users.len()
            + a.discord_channels.len()
            + a.slack_users.len()
            + a.slack_channels.len()
            + self.tokens.len()
            + self.providers.len()
            + usize::from(self.default_provider.is_some())
    }
}

fn config_diff(plan: &Plan, cfg: &config::Config) -> ConfigDiff {
    let g = &cfg.gateway;
    let new_str = |have: &[String], want: &[String]| -> Vec<String> {
        want.iter().filter(|w| !have.contains(w)).cloned().collect()
    };
    let allow = Allow {
        telegram_chats: plan
            .allow
            .telegram_chats
            .iter()
            .filter(|w| !g.telegram_allowed_chats.contains(w))
            .copied()
            .collect(),
        discord_users: new_str(&g.discord_allowed_users, &plan.allow.discord_users),
        discord_channels: new_str(&g.discord_allowed_channels, &plan.allow.discord_channels),
        slack_users: new_str(&g.slack_allowed_users, &plan.allow.slack_users),
        slack_channels: new_str(&g.slack_allowed_channels, &plan.allow.slack_channels),
    };
    let tokens = plan
        .tokens
        .iter()
        .filter(|(key, _)| {
            match *key {
                "telegram_token_env" => &g.telegram_token_env,
                "discord_token_env" => &g.discord_token_env,
                "slack_bot_token_env" => &g.slack_bot_token_env,
                _ => &g.slack_app_token_env,
            }
            .is_none()
        })
        .cloned()
        .collect();
    let providers: Vec<Provider> = plan
        .providers
        .iter()
        .filter(|p| !cfg.providers.contains_key(&p.name))
        .cloned()
        .collect();
    let default_provider = plan.default_provider.clone().filter(|d| {
        cfg.default_provider.is_none()
            && cfg.models.default.is_none()
            && (cfg.providers.contains_key(d) || providers.iter().any(|p| &p.name == d))
    });
    ConfigDiff {
        allow,
        tokens,
        providers,
        default_provider,
    }
}

fn write_config(root: &mut dyn toml_edit::TableLike, d: &ConfigDiff) -> Result<()> {
    fn extend(
        g: &mut dyn toml_edit::TableLike,
        key: &str,
        new: Vec<toml_edit::Value>,
    ) -> Result<()> {
        if new.is_empty() {
            return Ok(());
        }
        let mut arr = match g.get(key) {
            Some(item) => item
                .as_array()
                .cloned()
                .ok_or_else(|| anyhow!("[gateway] {key} isn't a list"))?,
            None => Array::new(),
        };
        for v in new {
            arr.push(v);
        }
        put(g, key, arr);
        Ok(())
    }
    {
        let g = table(root, &["gateway"])?;
        let a = &d.allow;
        let strs = |v: &[String]| v.iter().map(|s| s.as_str().into()).collect::<Vec<_>>();
        extend(
            g,
            "telegram_allowed_chats",
            a.telegram_chats.iter().map(|n| (*n).into()).collect(),
        )?;
        extend(g, "discord_allowed_users", strs(&a.discord_users))?;
        extend(g, "discord_allowed_channels", strs(&a.discord_channels))?;
        extend(g, "slack_allowed_users", strs(&a.slack_users))?;
        extend(g, "slack_allowed_channels", strs(&a.slack_channels))?;
        for (key, env) in &d.tokens {
            put(g, key, env.as_str());
        }
    }
    for p in &d.providers {
        let t = table(root, &["providers", &p.name])?;
        put(t, "base_url", p.base_url.as_str());
        put(t, "api_key_env", p.key_env.as_str());
        put(t, "model", p.model.as_str());
        put(t, "profile", p.profile.as_str());
        if let Some(api) = p.api {
            put(t, "api", api);
        }
    }
    if let Some(name) = &d.default_provider {
        put(root, "default_provider", name.as_str());
    }
    // An empty [gateway] made implicit above isn't written.
    if let Some(Item::Table(g)) = root.get_mut("gateway") {
        if g.is_empty() {
            g.set_implicit(true);
        }
    }
    Ok(())
}

/// The config as it is (an empty one when there's no file yet).
fn current_config(path: &Path) -> Result<config::Config> {
    crate::setup::Target::load(path.to_path_buf())?.config()
}

/// Compare, print the summary to `out`, and with `apply` write.
pub async fn run(
    plan: &Plan,
    targets: &Targets,
    apply: bool,
    bind_secrets: bool,
    skills: Skills<'_>,
    out: &mut dyn std::io::Write,
) -> Result<Counts> {
    let mut counts = Counts::default();
    let mut notes = plan.notes.clone();
    let head = if apply {
        "applying"
    } else {
        "dry run, nothing written; re-run with --apply to write"
    };
    writeln!(
        out,
        "Import from {} ({}) — {head}",
        plan.tool.label(),
        tilde(&plan.home)
    )?;

    // Memories.
    let store = if apply {
        if let Some(dir) = targets.memory_db.parent() {
            std::fs::create_dir_all(dir)?;
        }
        Some(MemoryStore::open(&targets.memory_db)?)
    } else if targets.memory_db.exists() {
        Some(MemoryStore::open(&targets.memory_db)?)
    } else {
        None
    };
    let actions = memory_actions(plan, store.as_ref())?;
    let tool_tag = format!("import:{}", plan.tool.name());
    for (e, a) in plan.memories.iter().zip(&actions) {
        match a {
            Action::Keep => counts.kept += 1,
            Action::Add => counts.added += 1,
            Action::Supersede(_) => counts.superseded += 1,
        }
        if !apply {
            continue;
        }
        let store = store.as_ref().expect("opened on apply");
        let from = format!("from:{}", e.file);
        let mut tags: Vec<&str> = vec![&tool_tag, &from];
        tags.extend(e.extra.iter().copied());
        let replaces: &[i64] = match a {
            Action::Keep => continue,
            Action::Add => &[],
            Action::Supersede(id) => std::slice::from_ref(id),
        };
        store
            .insert(&e.text, &tags, replaces)
            .with_context(|| format!("importing {}:{}", e.file, e.line))?;
    }
    let verb = |n: usize, now: &str, later: &str| {
        if apply {
            format!("{n} {now}")
        } else {
            format!("{n} {later}")
        }
    };
    writeln!(
        out,
        "\nMemories: {}, {} already known, {}",
        verb(counts.added, "added", "to add"),
        counts.kept,
        verb(
            counts.superseded,
            "updated (the old import superseded)",
            "to update (superseding the old import)"
        ),
    )?;
    if !plan.withheld.is_empty() {
        writeln!(
            out,
            "  held back {} that look like they carry a secret: {}",
            plan.withheld.len(),
            plan.withheld.join(", ")
        )?;
    }

    // Skills.
    writeln!(out, "\nSkills: {}", plan.skills.len())?;
    let staging = Staging::new()?;
    let mut skip_said = false;
    for s in &plan.skills {
        let label = if s.name == s.original {
            s.name.clone()
        } else {
            format!("{} → {}", s.original, s.name)
        };
        let staged = match stage_skill(s, staging.path()) {
            Ok(dir) => dir,
            Err(e) => {
                writeln!(out, "  {label}: can't be imported: {e:#}")?;
                counts.skills_skipped += 1;
                continue;
            }
        };
        let verdict = match ferrule_extensions::skill::inspect(&staged) {
            Ok(c) => scan_verdict(&c.findings),
            Err(e) => {
                writeln!(out, "  {label}: can't be imported: {e}")?;
                counts.skills_skipped += 1;
                continue;
            }
        };
        if !apply {
            writeln!(
                out,
                "  {label} (scan: {verdict}; you confirm each one on --apply)"
            )?;
            continue;
        }
        match &skills {
            Skills::Skip(why) => {
                counts.skills_skipped += 1;
                if !skip_said {
                    notes.push(format!("skills not installed: {why}"));
                    skip_said = true;
                }
            }
            Skills::Install { manager, confirm } => {
                let source = format!("import:{}:{}", plan.tool.name(), s.rel);
                match manager
                    .install_local_skill(&staged, &source, |r| confirm(r))
                    .await
                {
                    Ok(Outcome::Installed { name, warnings, .. }) => {
                        counts.skills_installed += 1;
                        writeln!(
                            out,
                            "  {label}: installed as `{name}` ({warnings} warning(s))"
                        )?;
                    }
                    Ok(Outcome::Pending { .. }) => {
                        counts.skills_skipped += 1;
                        writeln!(out, "  {label}: waiting for approval")?;
                    }
                    Err(e) => {
                        counts.skills_skipped += 1;
                        writeln!(out, "  {label}: skipped: {e}")?;
                    }
                }
            }
        }
    }

    // Config.
    let diff = config_diff(plan, &current_config(&targets.config)?);
    counts.config_changes = diff.len();
    writeln!(out, "\nConfig ({}):", tilde(&targets.config))?;
    if diff.len() == 0 {
        writeln!(out, "  nothing to change")?;
    }
    let a = &diff.allow;
    let lists: [(&str, Vec<String>); 5] = [
        (
            "telegram_allowed_chats",
            a.telegram_chats.iter().map(i64::to_string).collect(),
        ),
        ("discord_allowed_users", a.discord_users.clone()),
        ("discord_allowed_channels", a.discord_channels.clone()),
        ("slack_allowed_users", a.slack_users.clone()),
        ("slack_allowed_channels", a.slack_channels.clone()),
    ];
    for (key, ids) in lists.iter().filter(|(_, ids)| !ids.is_empty()) {
        writeln!(out, "  [gateway] {key} += {}", ids.join(", "))?;
    }
    for (key, env) in &diff.tokens {
        writeln!(out, "  [gateway] {key} = \"{env}\"")?;
    }
    for p in &diff.providers {
        writeln!(
            out,
            "  [providers.{}] {} at {} (key: ${})",
            p.name, p.model, p.base_url, p.key_env
        )?;
    }
    if let Some(d) = &diff.default_provider {
        writeln!(out, "  default_provider = \"{d}\"")?;
    }
    if apply && diff.len() > 0 {
        if let Some(dir) = targets.config.parent() {
            std::fs::create_dir_all(dir)?;
        }
        crate::tasks_admin::edit_config(&targets.config, |t| write_config(t.root(), &diff))?;
    }

    // Secrets.
    if !plan.secrets.is_empty() {
        let have: HashSet<String> = secrets::read(&targets.secrets)?
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        writeln!(
            out,
            "\nSecrets (names only; values are never shown or put in the config):"
        )?;
        for s in &plan.secrets {
            let state = if have.contains(&s.env) {
                "already in ferrule's secret store".to_string()
            } else if s.value.is_none() {
                format!(
                    "referenced in {}, but its value isn't there: export it, or save it with \
                     `ferrule setup`",
                    s.origin
                )
            } else if apply && bind_secrets {
                let v = s.value.as_ref().expect("checked");
                secrets::set(&targets.secrets, &s.env, v.expose())?;
                counts.secrets_written += 1;
                "copied into ferrule's secret store".to_string()
            } else if apply {
                format!(
                    "found in {}; not copied (re-run with --bind-secrets to copy it, or export \
                     it)",
                    s.origin
                )
            } else {
                format!("found in {}; --apply --bind-secrets copies it", s.origin)
            };
            writeln!(out, "  {}: {state}", s.env)?;
        }
    }

    if !notes.is_empty() {
        writeln!(out, "\nNot imported:")?;
        for n in &notes {
            writeln!(out, "  - {n}")?;
        }
    }
    if !plan.suggestions.is_empty() {
        writeln!(out, "\nBy hand:")?;
        for s in &plan.suggestions {
            writeln!(out, "  - {s}")?;
        }
    }
    Ok(counts)
}

/// A scratch directory for staged skills, removed when dropped.
struct Staging(PathBuf);

impl Staging {
    fn new() -> Result<Self> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("ferrule-import-{}-{nanos}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(Self(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A copy of the skill whose SKILL.md carries the name ferrule will use.
fn stage_skill(s: &FoundSkill, staging: &Path) -> Result<PathBuf> {
    let dest = staging.join(&s.name);
    ferrule_extensions::skill::copy_skill(&s.dir, &dest)?;
    if s.name != s.original {
        let path = dest.join("SKILL.md");
        let text = std::fs::read_to_string(&path)?;
        std::fs::write(&path, rename_skill(&text, &s.name))?;
    }
    Ok(dest)
}

/// The frontmatter's `name:` set to `name` (added when missing).
fn rename_skill(text: &str, name: &str) -> String {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    if lines.first().map(|l| l.trim()) != Some("---") {
        return format!("---\nname: {name}\n---\n{text}");
    }
    let end = lines
        .iter()
        .skip(1)
        .position(|l| l.trim() == "---")
        .map(|p| p + 1)
        .unwrap_or(lines.len());
    match lines[1..end].iter().position(|l| l.starts_with("name:")) {
        Some(i) => lines[i + 1] = format!("name: {name}"),
        None => lines.insert(1, format!("name: {name}")),
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

fn scan_verdict(findings: &[ferrule_extensions::Finding]) -> String {
    let blocks = ferrule_extensions::scan::blocks(findings).count();
    match (findings.len(), blocks) {
        (0, _) => "clean".into(),
        (_, 0) => format!("{} warning(s)", findings.len()),
        (_, b) => format!("BLOCKED by {b} finding(s); installing needs `waive`"),
    }
}

// ── The command ────────────────────────────────────────────────────────

fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// The plan for a command, reading the real environment.
pub fn plan_for(cmd: &ImportCmd) -> Result<Plan> {
    let home = dirs::home_dir();
    match cmd {
        ImportCmd::Openclaw {
            from, workspace, ..
        } => {
            let state =
                openclaw::home(from.as_deref(), &env_var, home.as_deref()).ok_or_else(|| {
                    anyhow!("no OpenClaw state directory found (~/.openclaw); pass --from <dir>")
                })?;
            openclaw::read(&state, workspace.as_deref(), &env_var, home.as_deref())
        }
        ImportCmd::Hermes { from, profile, .. } => {
            let root = hermes::home(from.as_deref(), &env_var, home.as_deref())
                .ok_or_else(|| anyhow!("no Hermes home found (~/.hermes); pass --from <dir>"))?;
            hermes::read(&root, profile.as_deref(), home.as_deref())
        }
    }
}

pub async fn command(cmd: ImportCmd) -> Result<()> {
    let (apply, bind) = match &cmd {
        ImportCmd::Openclaw {
            apply,
            bind_secrets,
            ..
        }
        | ImportCmd::Hermes {
            apply,
            bind_secrets,
            ..
        } => (*apply, *bind_secrets),
    };
    if bind && !apply {
        bail!("--bind-secrets writes, so it needs --apply");
    }
    let plan = plan_for(&cmd)?;
    apply_plan(&plan, apply, bind).await.map(|_| ())
}

/// Run a plan against the real ferrule, confirming skills at the terminal.
pub async fn apply_plan(plan: &Plan, apply: bool, bind: bool) -> Result<Counts> {
    apply_to(plan, Targets::real()?, apply, bind).await
}

async fn apply_to(plan: &Plan, targets: Targets, apply: bool, bind: bool) -> Result<Counts> {
    let confirm = |r: &Review| crate::self_extend::confirm_at_terminal(r);
    let skills = if !apply || plan.skills.is_empty() {
        Skills::Skip(String::new())
    } else if !crate::setup::has_terminal() {
        Skills::Skip("each skill is confirmed at a terminal; re-run there".into())
    } else if !targets.config.exists() {
        Skills::Skip("there's no ferrule config yet; run `ferrule setup`, then re-run".into())
    } else {
        Skills::Install {
            manager: crate::self_extend::owner_manager(Path::new("."))?,
            confirm: &confirm,
        }
    };
    let mut stdout = std::io::stdout();
    let counts = run(plan, &targets, apply, bind, skills, &mut stdout).await?;
    if apply {
        println!(
            "\nDone: {} memories added, {} updated; {} skill(s) installed; {} config change(s); \
             {} secret(s) copied. Running it again changes nothing.",
            counts.added,
            counts.superseded,
            counts.skills_installed,
            counts.config_changes,
            counts.secrets_written
        );
    }
    Ok(counts)
}

// ── In `ferrule setup` ─────────────────────────────────────────────────

/// The OpenClaw and Hermes directories on this machine.
pub(crate) fn detected() -> Vec<(Tool, PathBuf)> {
    let home = dirs::home_dir();
    let mut found = Vec::new();
    if let Some(d) = openclaw::home(None, &env_var, home.as_deref()).filter(|d| d.is_dir()) {
        found.push((Tool::OpenClaw, d));
    }
    if let Some(d) = hermes::home(None, &env_var, home.as_deref()).filter(|d| d.is_dir()) {
        found.push((Tool::Hermes, d));
    }
    found
}

pub(crate) fn setup_summary() -> String {
    let found = detected();
    if found.is_empty() {
        return "no OpenClaw or Hermes found".into();
    }
    found
        .iter()
        .map(|(tool, dir)| format!("{} at {}", tool.label(), tilde(dir)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Offer each detected source: the dry run first, then the import, and
/// the keys only on a second yes.
pub(crate) async fn setup_step(t: &mut crate::setup::Target, first: bool) -> Result<()> {
    use inquire::Confirm;
    let found = detected();
    if found.is_empty() {
        if !first {
            crate::setup::info(
                "no OpenClaw (~/.openclaw) or Hermes (~/.hermes) found here; \
                 `ferrule import openclaw|hermes --from <dir>` reads one from elsewhere",
            );
        }
        return Ok(());
    }
    let home = dirs::home_dir();
    for (tool, dir) in found {
        let look = Confirm::new(&format!(
            "Found {} at {}. See what can be brought over from it?",
            tool.label(),
            tilde(&dir)
        ))
        .with_default(true)
        .prompt()?;
        if !look {
            continue;
        }
        let plan = match tool {
            Tool::OpenClaw => openclaw::read(&dir, None, &env_var, home.as_deref()),
            Tool::Hermes => hermes::read(&dir, None, home.as_deref()),
        }?;
        let targets = || -> Result<Targets> {
            Ok(Targets {
                config: t.path.clone(),
                ..Targets::real()?
            })
        };
        println!();
        apply_to(&plan, targets()?, false, false).await?;
        println!();
        if !Confirm::new("Import this?").with_default(true).prompt()? {
            continue;
        }
        let have: HashSet<String> = secrets::read(&targets()?.secrets)?
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        let keys = plan
            .secrets
            .iter()
            .filter(|s| s.value.is_some() && !have.contains(&s.env))
            .count();
        let bind = keys > 0
            && Confirm::new(&format!(
                "Also copy the {} found into ferrule's private secret store?",
                plural(keys, "key or token", "keys and tokens")
            ))
            .with_default(false)
            .prompt()?;
        println!();
        apply_to(&plan, targets()?, true, bind).await?;
        t.reload()?;
    }
    Ok(())
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

#[cfg(test)]
mod tests;
