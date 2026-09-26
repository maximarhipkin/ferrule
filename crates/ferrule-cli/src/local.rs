//! M34: local model servers (docs/local-models.md): which of Ollama,
//! llama.cpp, LM Studio and vLLM is running, the window each model really
//! gets against the one ferrule plans for, the fix when it's too small,
//! and a tool-calling probe that tells "can't call tools" from "the chat
//! template is broken". Nothing here installs a server or pulls a model;
//! the only change it can make is an Ollama derived model, on a yes.

use crate::config::Config;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Each detection request's budget: a local server answers in
/// milliseconds, and setup and doctor shouldn't hang on a dead port.
pub const BUDGET: Duration = Duration::from_millis(700);
/// Below this the agent can't hold its own prompt and one tool result.
pub const FLOOR: usize = 8_192;
/// Below this: "too small for agent work".
pub const TOO_SMALL: usize = 16_384;
/// Below this it works, but compacts often. Also the window setup offers.
pub const WORKS_WELL: usize = 32_768;
/// How often the gateway re-reads the default model's window.
pub const RECHECK: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Ollama,
    LlamaCpp,
    LmStudio,
    Vllm,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Ollama => "Ollama",
            Kind::LlamaCpp => "llama.cpp",
            Kind::LmStudio => "LM Studio",
            Kind::Vllm => "vLLM",
        }
    }

    /// The provider name setup suggests for it.
    pub fn provider_name(self) -> &'static str {
        match self {
            Kind::Ollama => "ollama",
            Kind::LlamaCpp => "llamacpp",
            Kind::LmStudio => "lmstudio",
            Kind::Vllm => "vllm",
        }
    }

    /// What a request longer than the window meets.
    fn overflow(self) -> &'static str {
        match self {
            Kind::Ollama => "Ollama drops the front of long prompts",
            Kind::LlamaCpp => "llama.cpp shifts or rejects long prompts",
            Kind::LmStudio | Kind::Vllm => "long prompts are rejected",
        }
    }
}

/// One model a server lists, with what it says about its window.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Model {
    pub id: String,
    /// What the model was trained for.
    pub trained: Option<usize>,
    /// What a request gets now (loaded), or will get (a Modelfile's
    /// `num_ctx`). None: the server doesn't say until it loads it.
    pub effective: Option<usize>,
    /// Whether the server says the model does tool calls.
    pub tools: Option<bool>,
    pub loaded: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Server {
    pub kind: Kind,
    /// `http://127.0.0.1:11434`: scheme, host and port.
    pub origin: String,
    pub version: Option<String>,
    pub models: Vec<Model>,
}

impl Server {
    /// The OpenAI-compatible base URL ferrule talks to.
    pub fn base_url(&self) -> String {
        format!("{}/v1", self.origin)
    }

    /// `Ollama 0.34.4 at 127.0.0.1:11434 · 3 models`.
    pub fn describe(&self) -> String {
        let host = self
            .origin
            .split_once("://")
            .map_or(self.origin.as_str(), |(_, h)| h);
        let n = self.models.len();
        format!(
            "{}{} at {host} · {n} model{}",
            self.kind.label(),
            self.version
                .as_deref()
                .map(|v| format!(" {v}"))
                .unwrap_or_default(),
            if n == 1 { "" } else { "s" }
        )
    }

    /// The model the config names: exact, or Ollama's implied `:latest`.
    pub fn model(&self, id: &str) -> Option<&Model> {
        self.models
            .iter()
            .find(|m| m.id == id)
            .or_else(|| self.models.iter().find(|m| same_model(&m.id, id)))
    }
}

fn same_model(a: &str, b: &str) -> bool {
    let bare = |s: &str| s.strip_suffix(":latest").unwrap_or(s).to_string();
    bare(a) == bare(b)
}

/// `http://localhost:11434/v1` → `http://localhost:11434`, when the host is
/// this machine or a private network address; None for anything else,
/// which is a hosted API.
pub fn local_origin(base_url: &str) -> Option<String> {
    let url = url::Url::parse(base_url).ok()?;
    let local = match url.host()? {
        url::Host::Domain(d) => d.eq_ignore_ascii_case("localhost") || d.ends_with(".local"),
        url::Host::Ipv4(ip) => ip.is_loopback() || ip.is_private() || ip.is_link_local(),
        url::Host::Ipv6(ip) => {
            ip.is_loopback() || (ip.segments()[0] & 0xfe00) == 0xfc00 // unique local
        }
    };
    local.then(|| url.origin().ascii_serialization())
}

/// Where to look: Ollama (`OLLAMA_HOST` or its port), llama.cpp, LM
/// Studio and vLLM on their default ports, and every configured provider
/// whose `base_url` is local.
pub fn candidates(cfg: Option<&Config>) -> Vec<String> {
    let ollama = std::env::var("OLLAMA_HOST")
        .ok()
        .and_then(|h| ollama_origin(&h))
        .unwrap_or_else(|| "http://127.0.0.1:11434".into());
    let mut out = vec![
        ollama,
        "http://127.0.0.1:8080".into(),
        "http://127.0.0.1:1234".into(),
        "http://127.0.0.1:8000".into(),
    ];
    for p in cfg.iter().flat_map(|c| c.providers.values()) {
        if let Some(o) = local_origin(&p.base_url) {
            if !out.iter().any(|x| same_origin(x, &o)) {
                out.push(o);
            }
        }
    }
    out
}

/// `OLLAMA_HOST` as Ollama reads it: `0.0.0.0`, `host:port` or a URL.
fn ollama_origin(h: &str) -> Option<String> {
    let h = h.trim();
    if h.is_empty() {
        return None;
    }
    let with_scheme = if h.contains("://") {
        h.to_string()
    } else {
        format!("http://{h}")
    };
    let mut url = url::Url::parse(&with_scheme).ok()?;
    if url.host_str() == Some("0.0.0.0") {
        url.set_host(Some("127.0.0.1")).ok()?;
    }
    if url.port().is_none() {
        url.set_port(Some(11434)).ok()?;
    }
    Some(url.origin().ascii_serialization())
}

/// `localhost` and `127.0.0.1` are the same server.
fn same_origin(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.replace("://localhost", "://127.0.0.1");
    norm(a) == norm(b)
}

/// Every candidate that answers as a known server, looked at in parallel.
pub async fn detect(http: &reqwest::Client, origins: &[String]) -> Vec<Server> {
    let mut set = tokio::task::JoinSet::new();
    for (i, o) in origins.iter().enumerate() {
        let (http, o) = (http.clone(), o.clone());
        set.spawn(async move { (i, identify(&http, &o).await) });
    }
    let mut found: Vec<(usize, Server)> = Vec::new();
    while let Some(Ok((i, s))) = set.join_next().await {
        found.extend(s.map(|s| (i, s)));
    }
    found.sort_by_key(|(i, _)| *i);
    found.into_iter().map(|(_, s)| s).collect()
}

async fn get(http: &reqwest::Client, url: String) -> Option<Value> {
    let resp = http.get(url).timeout(BUDGET).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json().await.ok()
}

fn num(v: &Value) -> Option<usize> {
    v.as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .filter(|n| *n > 0)
}

/// What answers at `origin`, if it's one of the four. Ollama goes first:
/// it also answers `/v1/models`.
pub async fn identify(http: &reqwest::Client, origin: &str) -> Option<Server> {
    let origin = origin.trim_end_matches('/');
    if let Some(v) = get(http, format!("{origin}/api/version")).await {
        if let Some(version) = v["version"].as_str() {
            return Some(ollama(http, origin, version).await);
        }
    }
    if let Some(props) = get(http, format!("{origin}/props")).await {
        if props.get("default_generation_settings").is_some() {
            let list = get(http, format!("{origin}/v1/models")).await;
            return Some(llama_cpp(origin, &props, list.as_ref()));
        }
    }
    if let Some(v) = get(http, format!("{origin}/api/v1/models")).await {
        if let Some(models) = v["models"].as_array() {
            return Some(lm_studio_v1(origin, models));
        }
    }
    if let Some(v) = get(http, format!("{origin}/api/v0/models")).await {
        if let Some(data) = v["data"].as_array() {
            return Some(lm_studio_v0(origin, data));
        }
    }
    let list = get(http, format!("{origin}/v1/models")).await?;
    let data = list["data"].as_array()?;
    if !data.iter().any(|m| m["owned_by"] == "vllm") {
        return None;
    }
    let version = get(http, format!("{origin}/version"))
        .await
        .and_then(|v| v["version"].as_str().map(String::from));
    Some(Server {
        kind: Kind::Vllm,
        origin: origin.into(),
        version,
        models: data
            .iter()
            .filter_map(|m| {
                Some(Model {
                    id: m["id"].as_str()?.into(),
                    trained: None,
                    effective: num(&m["max_model_len"]),
                    tools: None,
                    loaded: true,
                })
            })
            .collect(),
    })
}

async fn ollama(http: &reqwest::Client, origin: &str, version: &str) -> Server {
    let tags = get(http, format!("{origin}/api/tags")).await;
    let ps = get(http, format!("{origin}/api/ps")).await;
    let names: Vec<String> = tags
        .as_ref()
        .and_then(|t| t["models"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|m| m["name"].as_str().or(m["model"].as_str()).map(String::from))
        .collect();
    let mut set = tokio::task::JoinSet::new();
    for (i, name) in names.into_iter().enumerate() {
        let (http, url) = (http.clone(), format!("{origin}/api/show"));
        set.spawn(async move {
            let show = match http
                .post(url)
                .json(&json!({ "model": name }))
                .timeout(Duration::from_secs(5))
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => r.json().await.unwrap_or(Value::Null),
                _ => Value::Null,
            };
            (i, name, show)
        });
    }
    let mut shows = Vec::new();
    while let Some(Ok(one)) = set.join_next().await {
        shows.push(one);
    }
    shows.sort_by_key(|(i, _, _)| *i);
    let models = shows
        .into_iter()
        .map(|(_, name, show)| ollama_model(name, &show, ps.as_ref()))
        .collect();
    Server {
        kind: Kind::Ollama,
        origin: origin.into(),
        version: Some(version.into()),
        models,
    }
}

/// One Ollama model from `/api/show` and `/api/ps`: trained from
/// `model_info["<arch>.context_length"]`; effective from `/api/ps` when
/// it's loaded, else a Modelfile's `num_ctx`.
fn ollama_model(id: String, show: &Value, ps: Option<&Value>) -> Model {
    let trained = show["model_info"].as_object().and_then(|info| {
        info.iter()
            .find(|(k, _)| k.ends_with(".context_length"))
            .and_then(|(_, v)| num(v))
    });
    let num_ctx = show["parameters"].as_str().and_then(|p| {
        p.lines().find_map(|l| {
            let mut w = l.split_whitespace();
            (w.next() == Some("num_ctx"))
                .then(|| w.next()?.parse().ok())
                .flatten()
        })
    });
    let running = ps
        .and_then(|p| p["models"].as_array())
        .into_iter()
        .flatten()
        .find(|m| {
            [&m["name"], &m["model"]]
                .iter()
                .any(|n| n.as_str().is_some_and(|n| same_model(n, &id)))
        });
    let tools = show["capabilities"]
        .as_array()
        .map(|c| c.iter().any(|x| x == "tools"));
    Model {
        effective: running.and_then(|m| num(&m["context_length"])).or(num_ctx),
        loaded: running.is_some(),
        id,
        trained,
        tools,
    }
}

/// llama-server serves one model: `/v1/models` names it and its trained
/// size; `/props` has the per-slot `n_ctx` a request gets.
fn llama_cpp(origin: &str, props: &Value, list: Option<&Value>) -> Server {
    let first = list.and_then(|l| l["data"].get(0));
    let id = first
        .and_then(|m| m["id"].as_str())
        .or(props["model_alias"].as_str())
        .or(props["model_path"].as_str())
        .unwrap_or("default")
        .to_string();
    let caps = &props["chat_template_caps"];
    let tools = caps["supports_tool_calls"]
        .as_bool()
        .or(caps["supports_tools"].as_bool());
    Server {
        kind: Kind::LlamaCpp,
        origin: origin.into(),
        version: props["build_info"].as_str().map(String::from),
        models: vec![Model {
            id,
            trained: first.and_then(|m| num(&m["meta"]["n_ctx_train"])),
            effective: num(&props["default_generation_settings"]["n_ctx"]),
            tools,
            loaded: true,
        }],
    }
}

fn lm_studio_v1(origin: &str, models: &[Value]) -> Server {
    Server {
        kind: Kind::LmStudio,
        origin: origin.into(),
        version: None,
        models: models
            .iter()
            .filter(|m| m["type"].as_str().is_none_or(|t| t == "llm"))
            .filter_map(|m| {
                let loaded = m["loaded_instances"]
                    .as_array()
                    .and_then(|l| l.first())
                    .map(|i| num(&i["config"]["context_length"]));
                Some(Model {
                    id: m["key"].as_str()?.into(),
                    trained: num(&m["max_context_length"]),
                    effective: loaded.flatten(),
                    tools: m["capabilities"]["trained_for_tool_use"].as_bool(),
                    loaded: loaded.is_some(),
                })
            })
            .collect(),
    }
}

fn lm_studio_v0(origin: &str, data: &[Value]) -> Server {
    Server {
        kind: Kind::LmStudio,
        origin: origin.into(),
        version: None,
        models: data
            .iter()
            .filter(|m| m["type"].as_str().is_none_or(|t| t == "llm" || t == "vlm"))
            .filter_map(|m| {
                Some(Model {
                    id: m["id"].as_str()?.into(),
                    trained: num(&m["max_context_length"]),
                    effective: num(&m["loaded_context_length"]),
                    tools: m["capabilities"]
                        .as_array()
                        .map(|c| c.iter().any(|x| x == "tool_use")),
                    loaded: m["state"] == "loaded",
                })
            })
            .collect(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Note,
    Warn,
    Fail,
}

/// One thing doctor or setup says about a local model.
#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub level: Level,
    pub text: String,
    pub fix: Option<String>,
    /// The `/status` line, when it's something to fix.
    pub status: Option<String>,
}

/// The window a fix should ask for: 32K, or the whole trained window if
/// that's smaller.
pub fn wanted(m: &Model) -> usize {
    m.trained.map_or(WORKS_WELL, |t| t.min(WORKS_WELL))
}

/// How to give `model` a window of `n`, for this server. Printed, never
/// applied, except Ollama's derived model, which setup makes on a yes.
pub fn fix(kind: Kind, model: &str, n: usize) -> String {
    match kind {
        Kind::Ollama => format!(
            "`ferrule setup` → Model provider can make `{}` (the same weights with num_ctx {n}); \
             or set OLLAMA_CONTEXT_LENGTH={n} in the Ollama server's environment and restart it; \
             or a Modelfile with `FROM {model}` and `PARAMETER num_ctx {n}`",
            derived_name(model, n)
        ),
        Kind::LlamaCpp => format!(
            "restart llama-server with `-c {n} -np 1` (the window is split across --parallel slots)"
        ),
        Kind::LmStudio => format!("`lms load {model} --context-length {n}`"),
        Kind::Vllm => format!("restart vLLM with `--max-model-len {n}`"),
    }
}

/// `qwen3-coder:30b` → `qwen3-coder:30b-32k`.
pub fn derived_name(model: &str, n: usize) -> String {
    format!("{model}-{}k", n / 1024)
}

/// The window findings for `model` on `server`, planned against
/// `planned` (the profile's window, or the configured `context_window`).
pub fn window_findings(server: &Server, model: &str, planned: usize) -> Vec<Finding> {
    let kind = server.kind;
    let Some(m) = server.model(model) else {
        return vec![Finding {
            level: Level::Warn,
            text: format!("{} doesn't list `{model}`", server.describe()),
            fix: Some(match kind {
                Kind::Ollama => {
                    format!("`ollama pull {model}`, or pick a listed model in `ferrule setup`")
                }
                _ => "load it on the server, or pick a listed model in `ferrule setup`".into(),
            }),
            status: Some(format!(
                "local: {model} isn't on the {} server",
                kind.label()
            )),
        }];
    };
    let trained = m
        .trained
        .map(|t| format!(", trained {t}"))
        .unwrap_or_default();
    let Some(e) = m.effective else {
        let text = if kind == Kind::Ollama {
            format!("{model}: the window is known once Ollama loads it (its default is 4096 on most laptops){trained}; `ferrule doctor --ping-models` loads it")
        } else {
            format!("{model}: the server doesn't say its window{trained}")
        };
        return vec![Finding {
            level: Level::Note,
            text,
            fix: None,
            status: None,
        }];
    };
    let fixed = fix(kind, model, wanted(m));
    if e < planned {
        return vec![Finding {
            level: Level::Fail,
            text: format!(
                "{model}: the server gives {e} tokens{trained}, but ferrule plans for {planned} — {}",
                kind.overflow()
            ),
            fix: Some(format!(
                "{fixed}; or tell ferrule the real window: `context_window = {e}` under [providers.<name>.models.\"{model}\"]"
            )),
            status: Some(format!(
                "local: {model} window {e} < {planned} planned ({})",
                kind.overflow()
            )),
        }];
    }
    let (level, text) = if e < FLOOR {
        (
            Level::Fail,
            format!("{model}: window {e}, below the {FLOOR} the agent needs"),
        )
    } else if e < TOO_SMALL {
        (
            Level::Warn,
            format!("{model}: window {e} is too small for agent work"),
        )
    } else if e < WORKS_WELL {
        (
            Level::Warn,
            format!("{model}: window {e} works, but compacts often"),
        )
    } else {
        (Level::Ok, format!("{model}: window {e}{trained}"))
    };
    let status = (level != Level::Ok).then(|| format!("local: {model} window {e} (small)"));
    vec![Finding {
        level,
        fix: (e < TOO_SMALL).then_some(fixed),
        text,
        status,
    }]
}

/// Make `<model>-32k` on an Ollama server: the same weights with a bigger
/// `num_ctx`. The user's model and the server's settings stay as they are.
pub async fn derive(
    http: &reqwest::Client,
    origin: &str,
    model: &str,
    n: usize,
) -> Result<String, String> {
    let name = derived_name(model, n);
    let resp = http
        .post(format!("{}/api/create", origin.trim_end_matches('/')))
        .json(&json!({
            "model": name,
            "from": model,
            "parameters": { "num_ctx": n },
            "stream": false,
        }))
        .timeout(Duration::from_secs(120))
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("HTTP {}: {}", status.as_u16(), snippet(&body)));
    }
    Ok(name)
}

/// What the tool probe found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tools {
    Works,
    /// The server or the model can't: the reason.
    Cannot(String),
    /// The model tried, in text; the chat template didn't parse it.
    TemplateBroken,
    /// Couldn't tell (the server's down, an odd answer).
    Unknown(String),
}

impl Tools {
    pub fn finding(&self, kind: Kind, model: &str) -> Finding {
        match self {
            Tools::Works => Finding {
                level: Level::Ok,
                text: format!("{model}: calls tools"),
                fix: None,
                status: None,
            },
            Tools::Cannot(why) => Finding {
                level: Level::Warn,
                text: format!("{model}: can't call tools ({why})"),
                fix: Some(if kind == Kind::Vllm {
                    "start vLLM with `--enable-auto-tool-choice --tool-call-parser <the model's parser>`, or pick a model from docs/local-models.md".into()
                } else {
                    "pick a tools-capable model; docs/local-models.md lists some".into()
                }),
                status: Some(format!("local: {model} can't call tools")),
            },
            Tools::TemplateBroken => Finding {
                level: Level::Warn,
                text: format!("{model}: can't call tools (template broken) — the model tried, but the server's chat template didn't turn it into a tool call"),
                fix: Some(match kind {
                    Kind::Ollama => format!("update Ollama and re-pull the model (`ollama pull {model}`)"),
                    Kind::LlamaCpp => "update llama.cpp; start it with `--jinja`, or `--chat-template-file` with the model's official template".into(),
                    _ => "update the server, and use the model's official chat template".into(),
                }),
                status: Some(format!("local: {model} can't call tools (template broken)")),
            },
            Tools::Unknown(why) => Finding {
                level: Level::Note,
                text: format!("{model}: the tool probe couldn't tell ({why})"),
                fix: None,
                status: None,
            },
        }
    }
}

const PROBE_TOOL: &str = "lookup_order";

/// One chat call with one tool, at temperature 0; loads the model if the
/// server loads on demand.
pub async fn probe_tools(http: &reqwest::Client, base_url: &str, key: &str, model: &str) -> Tools {
    let body = json!({
        "model": model,
        "messages": [{"role": "user", "content": "Look up order A-1729 with the lookup_order tool"}],
        "tools": [{
            "type": "function",
            "function": {
                "name": PROBE_TOOL,
                "description": "Look up an order by its id",
                "parameters": {
                    "type": "object",
                    "properties": {"order_id": {"type": "string"}},
                    "required": ["order_id"]
                }
            }
        }],
        "temperature": 0,
        "max_tokens": 256,
        "stream": false,
    });
    let resp = http
        .post(format!(
            "{}/chat/completions",
            base_url.trim_end_matches('/')
        ))
        .bearer_auth(key)
        .json(&body)
        .timeout(Duration::from_secs(180))
        .send()
        .await;
    match resp {
        Err(e) => Tools::Unknown(e.without_url().to_string()),
        Ok(r) => {
            let status = r.status().as_u16();
            let text = r.text().await.unwrap_or_default();
            classify(status, &text)
        }
    }
}

/// The probe's answer, judged (design §13).
pub fn classify(status: u16, body: &str) -> Tools {
    if !(200..300).contains(&status) {
        let low = body.to_lowercase();
        if low.contains("does not support tools") {
            return Tools::Cannot("the model doesn't support tools".into());
        }
        if low.contains("enable-auto-tool-choice") || low.contains("tool-call-parser") {
            return Tools::Cannot("the server was started without tool calling".into());
        }
        if low.contains("tool") && low.contains("jinja") {
            return Tools::Cannot("the server needs --jinja for tools".into());
        }
        return Tools::Unknown(format!("HTTP {status}: {}", snippet(body)));
    }
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return Tools::Unknown("the answer isn't JSON".into());
    };
    let msg = &v["choices"][0]["message"];
    if msg["tool_calls"]
        .as_array()
        .is_some_and(|calls| !calls.is_empty())
    {
        return Tools::Works;
    }
    let said = [&msg["content"], &msg["reasoning_content"]]
        .iter()
        .filter_map(|c| c.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    const SHAPES: &[&str] = &[
        "{\"name\"",
        "<tool_call>",
        "[TOOL_CALLS]",
        "<|python_tag|>",
        "\"arguments\"",
    ];
    if said.contains(PROBE_TOOL) && SHAPES.iter().any(|s| said.contains(s)) {
        Tools::TemplateBroken
    } else {
        Tools::Cannot("the model answered in text and ignored the tool".into())
    }
}

fn snippet(s: &str) -> String {
    let s = s.trim();
    match s.char_indices().nth(160) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// A model that should work, and where that came from.
pub struct KnownGood {
    pub model: &'static str,
    pub size: &'static str,
    pub window: &'static str,
    pub note: &'static str,
}

/// When and how the list below was checked (docs/local-models.md).
pub const KNOWN_GOOD_AS_OF: &str =
    "2026-09-26, from the Ollama library (tagged `tools`); not run through ferrule's probe here";

pub const KNOWN_GOOD: &[KnownGood] = &[
    KnownGood {
        model: "qwen3-coder:30b",
        size: "19 GB",
        window: "256K",
        note: "MoE, fast on 24 GB+",
    },
    KnownGood {
        model: "gpt-oss:20b",
        size: "14 GB",
        window: "128K",
        note: "reasoning; fits 16 GB",
    },
    KnownGood {
        model: "devstral:24b",
        size: "14 GB",
        window: "128K",
        note: "agentic coding",
    },
    KnownGood {
        model: "qwen3.6:27b",
        size: "18 GB",
        window: "256K",
        note: "general + tools",
    },
    KnownGood {
        model: "granite4.1:8b",
        size: "5.3 GB",
        window: "128K",
        note: "small machines; weaker tool use",
    },
    KnownGood {
        model: "granite4.1:3b",
        size: "2.1 GB",
        window: "128K",
        note: "small machines; weaker tool use",
    },
];

/// Whether `model` is on the list (any tag of the same family counts
/// only when it's the same tag).
pub fn known_good(model: &str) -> bool {
    KNOWN_GOOD.iter().any(|k| same_model(k.model, model))
}

/// Everything about one configured local model: its window against the
/// plan and, with `probe`, whether it calls tools. The probe loads the
/// model, so Ollama's window is re-read after it.
pub async fn check_model(
    http: &reqwest::Client,
    server: &Server,
    base_url: &str,
    key: &str,
    model: &str,
    planned: usize,
    probe: bool,
) -> Vec<Finding> {
    if !probe {
        return window_findings(server, model, planned);
    }
    let listed = server.model(model);
    let tools = if server.kind == Kind::Ollama && listed.and_then(|m| m.tools) == Some(false) {
        Tools::Cannot("the model's template has no tool support".into())
    } else if listed.is_none() {
        return window_findings(server, model, planned);
    } else {
        probe_tools(http, base_url, key, model).await
    };
    let reread = if server.kind == Kind::Ollama {
        identify(http, &server.origin).await
    } else {
        None
    };
    let mut out = window_findings(reread.as_ref().unwrap_or(server), model, planned);
    out.push(tools.finding(server.kind, model));
    out
}

/// The gateway's `/status` section for the default model, when it's on a
/// local server: its window at start and every [`RECHECK`], the tool
/// probe once at start. Lines only when something's wrong.
pub fn status_section(cfg: &Config) -> Option<Arc<dyn Fn() -> Vec<String> + Send + Sync>> {
    let cat = crate::models::Catalog::from_config(cfg);
    let entry = cat.default_entry().ok()?.0.clone();
    let origin = local_origin(&entry.base_url)?;
    let handle = tokio::runtime::Handle::try_current().ok()?;
    let lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let out = lines.clone();
    handle.spawn(async move {
        let http = crate::probe::client();
        let planned = entry.harness().context_window;
        let key = entry.key().unwrap_or_else(|| "none".into());
        let mut first = true;
        loop {
            let now = match identify(&http, &origin).await {
                None => vec![format!(
                    "local: nothing answers at {origin} for {}",
                    entry.model
                )],
                Some(server) => check_model(
                    &http,
                    &server,
                    &entry.base_url,
                    &key,
                    &entry.model,
                    planned,
                    first,
                )
                .await
                .into_iter()
                .filter_map(|f| f.status)
                .collect(),
            };
            // The probe's verdict stands until the gateway restarts.
            {
                let mut held = lines.lock().unwrap_or_else(|p| p.into_inner());
                let tools: Vec<String> = held
                    .iter()
                    .filter(|l| l.contains("tools"))
                    .cloned()
                    .collect();
                *held = now;
                if !first {
                    held.extend(tools);
                }
            }
            first = false;
            tokio::time::sleep(RECHECK).await;
        }
    });
    Some(Arc::new(move || {
        out.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    /// A tiny HTTP server answering `(method, path)` with canned JSON
    /// bodies; anything else is a 404. Returns its origin.
    fn serve(routes: Vec<(&'static str, &'static str, u16, String)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                if reader.read_line(&mut first).is_err() {
                    continue;
                }
                let mut len = 0usize;
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
                        break;
                    }
                    if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0; len];
                let _ = reader.read_exact(&mut body);
                let mut parts = first.split_whitespace();
                let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                let body = String::from_utf8_lossy(&body);
                // `/api/show` is keyed by the model in the body: "POST
                // /api/show qwen3-coder".
                let hit = routes.iter().find(|(m, p, _, _)| {
                    *m == method
                        && match p.split_once(' ') {
                            Some((path2, needle)) => path2 == path && body.contains(needle),
                            None => *p == path,
                        }
                });
                let (code, text) = match hit {
                    Some((_, _, c, t)) => (*c, t.clone()),
                    None => (404, "{\"error\":\"not found\"}".into()),
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {code} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                    text.len()
                );
            }
        });
        origin
    }

    fn ollama_server(ps: &str) -> String {
        serve(vec![
            ("GET", "/api/version", 200, r#"{"version":"0.34.4"}"#.into()),
            (
                "GET",
                "/api/tags",
                200,
                r#"{"models":[{"name":"qwen3-coder:30b","model":"qwen3-coder:30b","size":18556688736},{"name":"gemma-plain:latest","model":"gemma-plain:latest"}]}"#.into(),
            ),
            (
                "POST",
                "/api/show qwen3-coder",
                200,
                r#"{"modelfile":"FROM x","parameters":"temperature 0.7\ntop_p 0.8","details":{"family":"qwen3moe"},"model_info":{"general.architecture":"qwen3moe","qwen3moe.context_length":262144,"qwen3moe.embedding_length":2048},"capabilities":["completion","tools"]}"#.into(),
            ),
            (
                "POST",
                "/api/show gemma-plain",
                200,
                r#"{"parameters":"num_ctx 16384\nstop \"<end>\"","model_info":{"general.architecture":"gemma3","gemma3.context_length":131072},"capabilities":["completion"]}"#.into(),
            ),
            ("GET", "/api/ps", 200, ps.into()),
        ])
    }

    fn http() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    #[tokio::test]
    async fn ollama_windows_come_from_show_and_ps() {
        let origin = ollama_server(
            r#"{"models":[{"name":"qwen3-coder:30b","model":"qwen3-coder:30b","size":19000000000,"context_length":4096,"expires_at":"2026-09-26T12:00:00Z"}]}"#,
        );
        let s = identify(&http(), &origin).await.unwrap();
        assert_eq!(s.kind, Kind::Ollama);
        assert_eq!(s.version.as_deref(), Some("0.34.4"));
        assert!(
            s.describe().starts_with("Ollama 0.34.4 at 127.0.0.1:"),
            "{}",
            s.describe()
        );
        assert!(s.describe().ends_with("· 2 models"));
        let q = s.model("qwen3-coder:30b").unwrap();
        assert_eq!(
            (q.trained, q.effective, q.tools, q.loaded),
            (Some(262_144), Some(4096), Some(true), true)
        );
        // `:latest` is implied, and a Modelfile's num_ctx is the window.
        let g = s.model("gemma-plain").unwrap();
        assert_eq!(
            (g.trained, g.effective, g.tools, g.loaded),
            (Some(131_072), Some(16_384), Some(false), false)
        );
        // 4096 against generic's 128K: the silent-truncation failure.
        let f = &window_findings(&s, "qwen3-coder:30b", 128_000)[0];
        assert_eq!(f.level, Level::Fail);
        assert!(f.text.contains("gives 4096") && f.text.contains("drops the front"));
        assert!(f.fix.as_ref().unwrap().contains("qwen3-coder:30b-32k"));
        assert_eq!(
            f.status.as_deref(),
            Some("local: qwen3-coder:30b window 4096 < 128000 planned (Ollama drops the front of long prompts)")
        );
        // Told the truth (context_window = 4096): below the floor.
        assert_eq!(
            window_findings(&s, "qwen3-coder:30b", 4096)[0].level,
            Level::Fail
        );
        // Not loaded, no Modelfile value: unknown until it loads.
        let idle = identify(&http(), &ollama_server(r#"{"models":[]}"#))
            .await
            .unwrap();
        let f = &window_findings(&idle, "qwen3-coder:30b", 128_000)[0];
        assert_eq!(f.level, Level::Note);
        assert!(f.text.contains("known once Ollama loads it"));
        // A model it doesn't have.
        let f = &window_findings(&idle, "llama9", 128_000)[0];
        assert_eq!(f.level, Level::Warn);
        assert!(f.fix.as_ref().unwrap().contains("ollama pull llama9"));
    }

    #[tokio::test]
    async fn llama_cpp_reads_props_and_the_trained_size() {
        let origin = serve(vec![
            (
                "GET",
                "/props",
                200,
                r#"{"default_generation_settings":{"n_ctx":8192,"params":{"temperature":0.8}},"total_slots":4,"model_path":"/models/qwen.gguf","chat_template":"...","chat_template_caps":{"supports_tool_calls":true,"supports_tools":true},"build_info":"b6700-abc123"}"#.into(),
            ),
            (
                "GET",
                "/v1/models",
                200,
                r#"{"object":"list","data":[{"id":"qwen.gguf","object":"model","created":1,"owned_by":"llamacpp","meta":{"vocab_type":2,"n_vocab":151936,"n_ctx_train":131072,"n_embd":2048,"n_params":30532122624,"size":18000000000}}]}"#.into(),
            ),
        ]);
        let s = identify(&http(), &origin).await.unwrap();
        assert_eq!(s.kind, Kind::LlamaCpp);
        assert_eq!(s.version.as_deref(), Some("b6700-abc123"));
        let m = s.model("qwen.gguf").unwrap();
        assert_eq!(
            (m.trained, m.effective, m.tools),
            (Some(131_072), Some(8192), Some(true))
        );
        let f = &window_findings(&s, "qwen.gguf", 8192)[0];
        assert_eq!(f.level, Level::Warn);
        assert!(f.text.contains("too small for agent work"));
        assert!(f.fix.as_ref().unwrap().contains("-c 32768 -np 1"));
    }

    #[tokio::test]
    async fn lm_studio_both_rest_versions() {
        let v1 = serve(vec![(
            "GET",
            "/api/v1/models",
            200,
            r#"{"models":[{"type":"llm","publisher":"qwen","key":"qwen/qwen3-coder-30b","display_name":"Qwen3 Coder 30B","architecture":"qwen3moe","quantization":{"name":"Q4_K_M","bits_per_weight":4},"size_bytes":18000000000,"params_string":"30B","loaded_instances":[{"id":"qwen/qwen3-coder-30b","config":{"context_length":4096,"eval_batch_size":512,"flash_attention":false,"num_experts":8,"offload_kv_cache_to_gpu":true}}],"max_context_length":262144,"format":"gguf","capabilities":{"vision":false,"trained_for_tool_use":true},"description":null},{"type":"embedding","key":"text-embedding-nomic","max_context_length":2048,"loaded_instances":[]}]}"#.into(),
        )]);
        let s = identify(&http(), &v1).await.unwrap();
        assert_eq!(s.kind, Kind::LmStudio);
        assert_eq!(s.models.len(), 1, "embeddings aren't chat models");
        let m = &s.models[0];
        assert_eq!(
            (m.trained, m.effective, m.tools, m.loaded),
            (Some(262_144), Some(4096), Some(true), true)
        );
        let f = &window_findings(&s, "qwen/qwen3-coder-30b", 128_000)[0];
        assert!(f
            .fix
            .as_ref()
            .unwrap()
            .contains("lms load qwen/qwen3-coder-30b --context-length 32768"));

        let v0 = serve(vec![(
            "GET",
            "/api/v0/models",
            200,
            r#"{"object":"list","data":[{"id":"qwen2.5-7b-instruct","object":"model","type":"llm","publisher":"lmstudio-community","arch":"qwen2","compatibility_type":"gguf","quantization":"Q4_K_M","state":"loaded","max_context_length":32768,"loaded_context_length":32768,"capabilities":["tool_use"]}]}"#.into(),
        )]);
        let s = identify(&http(), &v0).await.unwrap();
        assert_eq!(s.kind, Kind::LmStudio);
        let m = &s.models[0];
        assert_eq!(
            (m.effective, m.tools, m.loaded),
            (Some(32_768), Some(true), true)
        );
        assert_eq!(window_findings(&s, &m.id, 32_768)[0].level, Level::Ok);
    }

    #[tokio::test]
    async fn vllm_is_told_apart_from_other_openai_servers() {
        let origin = serve(vec![
            (
                "GET",
                "/v1/models",
                200,
                r#"{"object":"list","data":[{"id":"Qwen/Qwen3-Coder-30B-A3B-Instruct","object":"model","created":1,"owned_by":"vllm","root":"Qwen/Qwen3-Coder-30B-A3B-Instruct","parent":null,"max_model_len":65536,"permission":[]}]}"#.into(),
            ),
            ("GET", "/version", 200, r#"{"version":"0.30.0"}"#.into()),
        ]);
        let s = identify(&http(), &origin).await.unwrap();
        assert_eq!((s.kind, s.version.as_deref()), (Kind::Vllm, Some("0.30.0")));
        assert_eq!(s.models[0].effective, Some(65_536));
        // An OpenAI-compatible server that isn't one of the four.
        let other = serve(vec![(
            "GET",
            "/v1/models",
            200,
            r#"{"data":[{"id":"m","owned_by":"someone"}]}"#.into(),
        )]);
        assert!(identify(&http(), &other).await.is_none());
        // Nothing listening at all.
        let dead = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", l.local_addr().unwrap())
        };
        assert!(detect(&http(), &[dead, origin]).await.len() == 1);
    }

    #[test]
    fn the_probe_tells_the_three_outcomes_apart() {
        let works = r#"{"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"lookup_order","arguments":"{\"order_id\":\"A-1729\"}"}}]},"finish_reason":"tool_calls"}]}"#;
        assert_eq!(classify(200, works), Tools::Works);
        let broken = r#"{"choices":[{"message":{"role":"assistant","content":"<tool_call>\n{\"name\": \"lookup_order\", \"arguments\": {\"order_id\": \"A-1729\"}}\n</tool_call>"},"finish_reason":"stop"}]}"#;
        assert_eq!(classify(200, broken), Tools::TemplateBroken);
        let mistral = r#"{"choices":[{"message":{"content":"[TOOL_CALLS]lookup_order{\"order_id\":\"A-1729\"}"}}]}"#;
        assert_eq!(classify(200, mistral), Tools::TemplateBroken);
        let ignored = r#"{"choices":[{"message":{"content":"I can't look up orders, but A-1729 sounds like an order id."}}]}"#;
        assert!(
            matches!(classify(200, ignored), Tools::Cannot(w) if w.contains("ignored the tool"))
        );
        let ollama = r#"{"error":{"message":"registry.ollama.ai/library/gemma:2b does not support tools","type":"api_error"}}"#;
        assert!(
            matches!(classify(400, ollama), Tools::Cannot(w) if w.contains("doesn't support tools"))
        );
        let vllm = r#"{"object":"error","message":"\"auto\" tool choice requires --enable-auto-tool-choice and --tool-call-parser to be set","type":"BadRequestError","code":400}"#;
        assert!(
            matches!(classify(400, vllm), Tools::Cannot(w) if w.contains("without tool calling"))
        );
        let llama = r#"{"error":{"code":500,"message":"tools param requires --jinja flag","type":"server_error"}}"#;
        assert!(matches!(classify(500, llama), Tools::Cannot(w) if w.contains("--jinja")));
        assert!(matches!(classify(502, "bad gateway"), Tools::Unknown(_)));
        let f = Tools::TemplateBroken.finding(Kind::LlamaCpp, "m");
        assert_eq!(
            f.status.as_deref(),
            Some("local: m can't call tools (template broken)")
        );
    }

    #[tokio::test]
    async fn the_probe_sends_one_tool_and_the_derive_asks_for_num_ctx() {
        let origin = serve(vec![
            (
                "POST",
                "/v1/chat/completions lookup_order",
                200,
                r#"{"choices":[{"message":{"tool_calls":[{"id":"c","type":"function","function":{"name":"lookup_order","arguments":"{}"}}]}}]}"#.into(),
            ),
            (
                "POST",
                "/api/create \"num_ctx\":32768",
                200,
                r#"{"status":"success"}"#.into(),
            ),
        ]);
        assert_eq!(
            probe_tools(&http(), &format!("{origin}/v1"), "none", "m").await,
            Tools::Works
        );
        assert_eq!(
            derive(&http(), &origin, "qwen3-coder:30b", 32_768).await,
            Ok("qwen3-coder:30b-32k".into())
        );
        // Ollama without the "tools" capability isn't even called.
        let s = Server {
            kind: Kind::Ollama,
            origin: origin.clone(),
            version: None,
            models: vec![Model {
                id: "plain".into(),
                effective: Some(65_536),
                tools: Some(false),
                ..Model::default()
            }],
        };
        let found = check_model(&http(), &s, &s.base_url(), "none", "plain", 65_536, true).await;
        assert!(found
            .iter()
            .any(|f| f.text.contains("template has no tool support")));
    }

    #[test]
    fn local_origins_and_candidates() {
        assert_eq!(
            local_origin("http://localhost:11434/v1").as_deref(),
            Some("http://localhost:11434")
        );
        assert_eq!(
            local_origin("http://192.168.1.20:8000/v1").as_deref(),
            Some("http://192.168.1.20:8000")
        );
        assert!(local_origin("https://api.openai.com/v1").is_none());
        assert!(local_origin("https://8.8.8.8/v1").is_none());
        assert_eq!(
            ollama_origin("0.0.0.0").as_deref(),
            Some("http://127.0.0.1:11434")
        );
        assert_eq!(
            ollama_origin("http://box.local:9999").as_deref(),
            Some("http://box.local:9999")
        );
        assert!(same_origin(
            "http://localhost:1234",
            "http://127.0.0.1:1234"
        ));
        assert_eq!(derived_name("qwen3-coder", 32_768), "qwen3-coder-32k");
        assert!(known_good("qwen3-coder:30b") && !known_good("qwen3-coder:480b"));
        let m = Model {
            trained: Some(8192),
            ..Model::default()
        };
        assert_eq!(wanted(&m), 8192);
        assert_eq!(wanted(&Model::default()), WORKS_WELL);
    }
}
