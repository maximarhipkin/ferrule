//! The Gemini / Antigravity plan's native Cloud Code HTTPS driver:
//! sends requests directly to `daily-cloudcode-pa.googleapis.com`
//! (`v1internal:loadCodeAssist`, `v1internal:fetchAvailableModels`,
//! `v1internal:retrieveUserQuotaSummary`, `v1internal:streamGenerateContent?alt=sse`)
//! using a signed-in Google account's sealed OAuth token (`PlanAuth`) instead
//! of spawning an external `agy` or `gemini` CLI process.

pub mod version;

use crate::codex::{LimitWindow, PlanAuth, PlanCredentials, RateLimits};
use crate::common::{self, Events, Opened};
use crate::vision::{self, Part};
use crate::DriverOptions;
use ferrule_core::error::CoreError;
use ferrule_core::message::{Message, NativeBlocks, Role, ToolCall, Usage};
use ferrule_core::provider::{CompletionRequest, CompletionResponse, Delta, DeltaSink, Provider};
use ferrule_core::tool::ToolDefinition;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tracing::warn;

pub const DEFAULT_BASE_URL: &str = "https://daily-cloudcode-pa.googleapis.com";
pub const CLIENT_VERSION: &str = "1.4.12";
pub const SIGN_IN_AGAIN: &str =
    "HTTP 401: the Gemini plan refused the sign-in even after a refresh; run `ferrule login gemini`";

/// The Antigravity client version in use now.
pub fn client_version() -> String {
    version::global().current()
}

/// Cached `v1internal:fetchAvailableModels` tiered mappings.
#[derive(Debug, Clone, Default)]
struct ModelCatalog {
    direct: Vec<String>,
    pro_tiered: Option<String>,
    flash_tiered: Option<String>,
    flash_lite_tiered: Option<String>,
}

pub struct CloudCodeProvider {
    name: String,
    base_url: String,
    model: String,
    options: DriverOptions,
    auth: Arc<dyn PlanAuth>,
    client: reqwest::Client,
    identity: Arc<version::ClientIdentity>,
    sees: AtomicBool,
    project_cache: RwLock<Option<String>>,
    catalog_cache: RwLock<Option<ModelCatalog>>,
    last_quota_check: Mutex<Option<Instant>>,
}

impl CloudCodeProvider {
    pub fn new(
        name: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        options: DriverOptions,
        auth: Arc<dyn PlanAuth>,
    ) -> Self {
        let name = name.into();
        let model = model.into();
        let base_url = base_url.into().trim_end_matches('/').to_string();
        let sees = options.vision.unwrap_or(true);
        Self {
            name,
            base_url: if base_url.is_empty() {
                DEFAULT_BASE_URL.into()
            } else {
                base_url
            },
            model,
            options,
            auth,
            client: common::client(),
            identity: version::global(),
            sees: AtomicBool::new(sees),
            project_cache: RwLock::new(None),
            catalog_cache: RwLock::new(None),
            last_quota_check: Mutex::new(None),
        }
    }

    pub fn with_identity(mut self, identity: Arc<version::ClientIdentity>) -> Self {
        self.identity = identity;
        self
    }

    pub fn sees(&self) -> bool {
        self.sees.load(Ordering::Relaxed)
    }

    pub fn refuse_images(&self) {
        self.sees.store(false, Ordering::Relaxed);
    }

    fn request(
        &self,
        method: reqwest::Method,
        url: &str,
        creds: &PlanCredentials,
        version: &str,
    ) -> reqwest::RequestBuilder {
        self.client
            .request(method, url)
            .bearer_auth(&creds.access_token)
            .header(reqwest::header::USER_AGENT, user_agent(version))
            .header("X-Goog-Api-Client", "gl-go/1.23.0 antigravity/1.4.12")
    }

    async fn ensure_project(&self, creds: &PlanCredentials, version: &str) -> Option<String> {
        if let Some(id) = creds.account_id.as_ref().filter(|s| !s.is_empty()) {
            return Some(id.clone());
        }
        if let Some(cached) = self
            .project_cache
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return Some(cached);
        }
        let url = format!("{}/v1internal:loadCodeAssist", self.base_url);
        let body = json!({
            "metadata": {
                "ideType": "ANTIGRAVITY",
                "ideVersion": version,
                "platform": platform_enum(),
                "pluginType": "GEMINI",
            }
        });
        let resp = self
            .request(reqwest::Method::POST, &url, creds, version)
            .json(&body)
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let doc: Value = resp.json().await.ok()?;
        let project = doc["cloudaicompanionProject"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                doc["cloudaicompanionProject"]["id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            })?;
        *self
            .project_cache
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Some(project.clone());
        Some(project)
    }

    /// Resolves friendly/tiered model aliases (such as `gemini-4-argon-medium`
    /// or `gemini-3.8-flash-low`) to the concrete Cloud Code model ID and
    /// optional thinking level (`LOW` / `MEDIUM` / `HIGH`).
    fn resolve_model(&self) -> (String, Option<&'static str>) {
        let m = self.model.trim();
        let cat = self
            .catalog_cache
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(c) = &cat {
            if c.direct.iter().any(|d| d == m) {
                return (m.to_string(), None);
            }
        }
        let pro_target = cat
            .as_ref()
            .and_then(|c| c.pro_tiered.clone())
            .unwrap_or_else(|| "gemini-3.8-flash-tiered".to_string());
        let flash_target = cat
            .as_ref()
            .and_then(|c| c.flash_tiered.clone())
            .unwrap_or_else(|| "gemini-3.8-flash-tiered".to_string());
        let lite_target = cat
            .as_ref()
            .and_then(|c| c.flash_lite_tiered.clone())
            .unwrap_or_else(|| "gemini-3.5-flash-lite".to_string());

        match m {
            "gemini-4-argon-high" => (pro_target, Some("HIGH")),
            "gemini-4-argon-medium" | "gemini-4-argon" => (pro_target, Some("MEDIUM")),
            "gemini-4-argon-low" => (pro_target, Some("LOW")),
            "gemini-3.8-flash-high" => (flash_target, Some("HIGH")),
            "gemini-3.8-flash-medium" => (flash_target, Some("MEDIUM")),
            "gemini-3.8-flash-low" => (flash_target, Some("LOW")),
            "gemini-3.1-flash-lite" => (lite_target, None),
            other => (other.to_string(), None),
        }
    }

    /// Fetches `v1internal:fetchAvailableModels` and `v1internal:retrieveUserQuotaSummary`,
    /// updates the cached catalog and quota windows, and returns the ordered model IDs.
    pub async fn list_models(&self) -> Result<Vec<String>, CoreError> {
        let creds = self.auth.credentials().await?;
        let version = self.identity.current();
        let project = self.ensure_project(&creds, &version).await;
        let url = format!("{}/v1internal:fetchAvailableModels", self.base_url);
        let body = match &project {
            Some(p) => json!({ "project": p }),
            None => json!({}),
        };
        let reply = common::send(
            self.request(reqwest::Method::POST, &url, &creds, &version)
                .json(&body),
        )
        .await?;
        let doc = &reply.body;

        let mut direct: Vec<String> = Vec::new();
        if let Some(models_obj) = doc["models"].as_object() {
            for k in models_obj.keys() {
                if !k.starts_with("tab_") && !k.starts_with("chat_") {
                    direct.push(k.clone());
                }
            }
        }
        let pro_tiered = doc["tieredModelIds"]["pro"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(Value::as_str)
            .map(str::to_string);
        let flash_tiered = doc["tieredModelIds"]["flash"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(Value::as_str)
            .map(str::to_string);
        let flash_lite_tiered = doc["tieredModelIds"]["flashLite"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(Value::as_str)
            .map(str::to_string);

        *self
            .catalog_cache
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Some(ModelCatalog {
            direct: direct.clone(),
            pro_tiered: pro_tiered.clone(),
            flash_tiered: flash_tiered.clone(),
            flash_lite_tiered,
        });

        // Build ordered model list: friendly tiered aliases first, then
        // recommended agentModelSorts, then remaining direct chat models.
        let mut out: Vec<String> = Vec::new();
        let mut push_unique = |id: &str| {
            if !id.is_empty() && !out.iter().any(|x| x == id) {
                out.push(id.to_string());
            }
        };
        if pro_tiered.is_some() || flash_tiered.is_some() {
            for alias in [
                "gemini-4-argon-medium",
                "gemini-4-argon-high",
                "gemini-4-argon-low",
                "gemini-3.8-flash-high",
                "gemini-3.8-flash-medium",
                "gemini-3.8-flash-low",
            ] {
                push_unique(alias);
            }
        }
        if let Some(sorts) = doc["agentModelSorts"].as_array() {
            for s in sorts {
                if let Some(groups) = s["groups"].as_array() {
                    for g in groups {
                        if let Some(ids) = g["modelIds"].as_array() {
                            for id in ids.iter().filter_map(Value::as_str) {
                                push_unique(id);
                            }
                        }
                    }
                }
            }
        }
        direct.sort();
        for id in &direct {
            push_unique(id);
        }

        // Also refresh the 5-hour and weekly quota summary.
        self.refresh_quota(&creds, project.as_deref(), &version)
            .await;

        Ok(out)
    }

    async fn refresh_quota(&self, creds: &PlanCredentials, project: Option<&str>, version: &str) {
        let url = format!("{}/v1internal:retrieveUserQuotaSummary", self.base_url);
        let body = match project {
            Some(p) => json!({ "project": p }),
            None => json!({}),
        };
        let Ok(resp) = self
            .request(reqwest::Method::POST, &url, creds, version)
            .timeout(Duration::from_secs(10))
            .json(&body)
            .send()
            .await
        else {
            return;
        };
        if !resp.status().is_success() {
            return;
        }
        let Ok(doc) = resp.json::<Value>().await else {
            return;
        };
        let limits = parse_quota_summary(&doc);
        if !limits.windows.is_empty() {
            self.auth.observe(&limits);
        }
    }

    async fn maybe_refresh_quota(
        &self,
        creds: &PlanCredentials,
        project: Option<&str>,
        version: &str,
    ) {
        {
            let mut last = self
                .last_quota_check
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if last.is_some_and(|at| at.elapsed() < Duration::from_secs(120)) {
                return;
            }
            *last = Some(Instant::now());
        }
        self.refresh_quota(creds, project, version).await;
    }

    /// Sends the `v1internal:streamGenerateContent?alt=sse` request, refreshing
    /// on 401 once, and reads the SSE stream into a [`CompletionResponse`].
    async fn post(
        &self,
        req: &CompletionRequest,
        plain: bool,
    ) -> Result<CompletionResponse, CoreError> {
        let url = format!("{}/v1internal:streamGenerateContent?alt=sse", self.base_url);
        let (resolved_model, thinking_level) = self.resolve_model();
        let inner_request = self.build_request_body(req, &resolved_model, thinking_level, plain);
        let mut refreshed = false;

        loop {
            let creds = self.auth.credentials().await?;
            let version = self.identity.current();
            let project = self.ensure_project(&creds, &version).await;
            let mut envelope = json!({
                "model": resolved_model,
                "userAgent": "antigravity",
                "requestType": "agent",
                "request": inner_request,
            });
            if let Some(p) = &project {
                envelope["project"] = json!(p);
            }
            let rb = self
                .request(reqwest::Method::POST, &url, &creds, &version)
                .header(reqwest::header::ACCEPT, "text/event-stream")
                .json(&envelope);
            let resp = common::connect(rb).await?;
            match resp.status().as_u16() {
                401 if !refreshed => {
                    refreshed = true;
                    self.auth.refused(&creds.access_token).await?;
                    continue;
                }
                401 => return Err(CoreError::Provider(SIGN_IN_AGAIN.into())),
                429 => {
                    let wait = common::retry_after(resp.headers());
                    let text = resp.text().await.unwrap_or_default();
                    return Err(common::transient(
                        format!("HTTP 429: {}", text.chars().take(300).collect::<String>()),
                        wait,
                    ));
                }
                _ => {}
            }
            let quiet = DeltaSink::new(|_| {});
            let sink = req.stream.as_ref().unwrap_or(&quiet);
            let res = match common::opened_as_stream(resp).await? {
                Opened::Events(events) => read_stream(events, sink, &resolved_model).await,
                Opened::Json(reply) => {
                    if let Some(err) = reply.body.get("error").filter(|e| !e.is_null()) {
                        return Err(common::error_in_body(err, reply.wait));
                    }
                    parse_single_response(&reply.body, sink, &resolved_model)
                }
            };
            self.maybe_refresh_quota(&creds, project.as_deref(), &version)
                .await;
            return res;
        }
    }

    fn build_request_body(
        &self,
        req: &CompletionRequest,
        resolved_model: &str,
        thinking_level: Option<&str>,
        plain: bool,
    ) -> Value {
        let sees = self.sees();
        let pixels = ferrule_core::vision::pixel_set(&req.messages);

        // Map tool_call_id -> tool_name from assistant messages.
        let mut tool_names: HashMap<String, String> = HashMap::new();
        for m in &req.messages {
            for tc in &m.tool_calls {
                tool_names.insert(tc.id.clone(), tc.name.clone());
            }
        }

        let mut system_parts: Vec<Value> = Vec::new();
        let mut contents: Vec<Value> = Vec::new();
        let mut idx = 0;

        while idx < req.messages.len() {
            let msg = &req.messages[idx];
            match msg.role {
                Role::System => {
                    if let Some(t) = msg.content.as_deref().filter(|s| !s.is_empty()) {
                        system_parts.push(json!({ "text": t }));
                    }
                    idx += 1;
                }
                Role::User => {
                    let mut parts: Vec<Value> = Vec::new();
                    if let Some(t) = msg.content.as_deref().filter(|s| !s.is_empty()) {
                        parts.push(json!({ "text": t }));
                    }
                    for part in vision::parts(idx, msg, &pixels, sees) {
                        match part {
                            Part::Pixels { mime, data } => {
                                parts.push(json!({
                                    "inlineData": {
                                        "mimeType": mime,
                                        "data": data,
                                    }
                                }));
                            }
                            Part::Note(note) => {
                                parts.push(json!({ "text": note }));
                            }
                        }
                    }
                    if parts.is_empty() {
                        parts.push(json!({ "text": "" }));
                    }
                    contents.push(json!({
                        "role": "user",
                        "parts": parts,
                    }));
                    idx += 1;
                }
                Role::Assistant => {
                    let native_items = (!plain)
                        .then_some(msg.native.as_ref())
                        .flatten()
                        .filter(|n| {
                            n.api == "cloudcode" && n.model == resolved_model && !n.items.is_empty()
                        })
                        .map(|n| n.items.clone());

                    let parts = if let Some(items) = native_items {
                        items
                    } else {
                        let mut p: Vec<Value> = Vec::new();
                        if !plain {
                            if let Some(r) = msg.reasoning.as_deref().filter(|s| !s.is_empty()) {
                                p.push(json!({ "thought": true, "text": r }));
                            }
                        }
                        if let Some(t) = msg.content.as_deref().filter(|s| !s.is_empty()) {
                            p.push(json!({ "text": t }));
                        }
                        for tc in &msg.tool_calls {
                            let args = if tc.arguments.is_object() {
                                tc.arguments.clone()
                            } else {
                                json!({})
                            };
                            p.push(json!({
                                "functionCall": {
                                    "id": tc.id,
                                    "name": tc.name,
                                    "args": args,
                                }
                            }));
                        }
                        if p.is_empty() {
                            p.push(json!({ "text": "" }));
                        }
                        p
                    };
                    contents.push(json!({
                        "role": "model",
                        "parts": parts,
                    }));
                    idx += 1;
                }
                Role::Tool => {
                    // Group consecutive tool results into a single `user` content item.
                    let mut resp_parts: Vec<Value> = Vec::new();
                    let mut extra_pics: Vec<(String, String)> = Vec::new();
                    let mut pic_calls: Vec<String> = Vec::new();
                    while idx < req.messages.len() && req.messages[idx].role == Role::Tool {
                        let tm = &req.messages[idx];
                        let call_id = tm.tool_call_id.clone().unwrap_or_default();
                        let name = tool_names
                            .get(&call_id)
                            .cloned()
                            .unwrap_or_else(|| "tool".into());
                        let (text, pics) = vision::tool_parts(idx, tm, &pixels, sees);
                        if !pics.is_empty() {
                            pic_calls.push(name.clone());
                            extra_pics.extend(pics);
                        }
                        let mut fresp = json!({
                            "name": name,
                            "response": { "result": text },
                        });
                        if !call_id.is_empty() {
                            fresp["id"] = json!(call_id);
                        }
                        resp_parts.push(json!({ "functionResponse": fresp }));
                        idx += 1;
                    }
                    if !extra_pics.is_empty() {
                        resp_parts.push(json!({ "text": vision::tool_images_lead(&pic_calls) }));
                        for (mime, data) in extra_pics {
                            resp_parts.push(json!({
                                "inlineData": {
                                    "mimeType": mime,
                                    "data": data,
                                }
                            }));
                        }
                    }
                    contents.push(json!({
                        "role": "user",
                        "parts": resp_parts,
                    }));
                }
            }
        }

        let mut request = json!({
            "contents": contents,
        });
        if !system_parts.is_empty() {
            request["systemInstruction"] = json!({ "parts": system_parts });
        }
        if !req.tools.is_empty() {
            let decls: Vec<Value> = req.tools.iter().map(tool_declaration).collect();
            request["tools"] = json!([{ "functionDeclarations": decls }]);
        }

        let mut gen_cfg = Map::new();
        if let Some(max) = req.max_output_tokens.or(self.options.max_tokens) {
            gen_cfg.insert("maxOutputTokens".into(), json!(max));
        }
        if let Some(temp) = req.temperature {
            gen_cfg.insert("temperature".into(), json!(temp));
        }
        let level = thinking_level.or_else(|| {
            self.options.effort.as_deref().and_then(|e| {
                match e.trim().to_ascii_lowercase().as_str() {
                    "low" => Some("LOW"),
                    "medium" => Some("MEDIUM"),
                    "high" | "max" => Some("HIGH"),
                    _ => None,
                }
            })
        });
        if let Some(lvl) = level {
            if resolved_model.starts_with("gemini-") {
                gen_cfg.insert(
                    "thinkingConfig".into(),
                    json!({
                        "includeThoughts": true,
                        "thinkingLevel": lvl,
                    }),
                );
            }
        }
        if !gen_cfg.is_empty() {
            request["generationConfig"] = Value::Object(gen_cfg);
        }
        request
    }

    async fn attempt(&self, req: &CompletionRequest) -> Result<CompletionResponse, CoreError> {
        let has_native = req.messages.iter().any(|m| {
            m.native
                .as_ref()
                .is_some_and(|n| n.api == "cloudcode" && !n.items.is_empty())
                || m.reasoning.is_some()
        });
        match self.post(req, false).await {
            Err(CoreError::Provider(m)) if has_native && thought_rejected(&m) => {
                warn!(
                    provider = %self.name,
                    "thought/signature items rejected; retrying once without them"
                );
                self.post(req, true).await
            }
            other => other,
        }
    }
}

#[async_trait::async_trait]
impl Provider for CloudCodeProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn sees_images(&self) -> bool {
        self.sees()
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
        let pixels = vision::has_pixels(&req.messages, self.sees());
        match self.attempt(&req).await {
            Err(e) if pixels && vision::image_rejected(&e) => {
                warn!(provider = %self.name, "photos refused ({e}); sending notes instead");
                self.refuse_images();
                self.attempt(&req).await
            }
            done => done,
        }
    }
}

fn thought_rejected(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("thought") || lower.contains("signature")
}

fn tool_declaration(t: &ToolDefinition) -> Value {
    let mut params = clean_schema(&t.parameters);
    if params.get("type").is_none() {
        if let Some(obj) = params.as_object_mut() {
            obj.insert("type".into(), json!("object"));
        }
    }
    json!({
        "name": t.name,
        "description": t.description,
        "parameters": params,
    })
}

/// Cleans a JSON Schema for Cloud Code (`v1internal:streamGenerateContent`),
/// stripping keywords that protobuf `Schema` rejects (`$schema`,
/// `additionalProperties`, `default`, `examples`, `$id`, `title`).
fn clean_schema(schema: &Value) -> Value {
    match schema {
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                if matches!(
                    k.as_str(),
                    "$schema"
                        | "additionalProperties"
                        | "default"
                        | "examples"
                        | "$id"
                        | "title"
                        | "$defs"
                        | "definitions"
                ) {
                    continue;
                }
                if k == "properties" {
                    if let Some(props) = v.as_object() {
                        let mut clean_props = Map::new();
                        for (pk, pv) in props {
                            clean_props.insert(pk.clone(), clean_schema(pv));
                        }
                        out.insert(k.clone(), Value::Object(clean_props));
                        continue;
                    }
                }
                if k == "type" {
                    if let Some(arr) = v.as_array() {
                        // Handle `["string", "null"]` -> `"type": "string", "nullable": true`
                        let non_null = arr.iter().filter_map(Value::as_str).find(|s| *s != "null");
                        let has_null = arr.iter().any(|x| x.as_str() == Some("null"));
                        if let Some(t) = non_null {
                            out.insert("type".into(), json!(t));
                            if has_null {
                                out.insert("nullable".into(), json!(true));
                            }
                            continue;
                        }
                    }
                }
                out.insert(k.clone(), clean_schema(v));
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(clean_schema).collect()),
        other => other.clone(),
    }
}

async fn read_stream(
    mut events: Events,
    sink: &DeltaSink,
    resolved_model: &str,
) -> Result<CompletionResponse, CoreError> {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut native_parts: Vec<Value> = Vec::new();
    let mut usage = Usage::default();
    let mut stop_reason: Option<String> = None;
    let mut call_seq: usize = 0;

    while let Some(ev) = events.next().await? {
        if ev.data.trim().is_empty() || ev.data.trim() == "[DONE]" {
            continue;
        }
        let doc = ev.json()?;
        if let Some(err) = doc.get("error").filter(|e| !e.is_null()) {
            return Err(common::error_in_body(err, None));
        }
        apply_chunk(
            &doc,
            sink,
            &mut text,
            &mut reasoning,
            &mut tool_calls,
            &mut native_parts,
            &mut usage,
            &mut stop_reason,
            &mut call_seq,
        );
    }

    Ok(build_completion_response(
        text,
        reasoning,
        tool_calls,
        native_parts,
        usage,
        stop_reason,
        resolved_model,
    ))
}

fn parse_single_response(
    doc: &Value,
    sink: &DeltaSink,
    resolved_model: &str,
) -> Result<CompletionResponse, CoreError> {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut native_parts: Vec<Value> = Vec::new();
    let mut usage = Usage::default();
    let mut stop_reason: Option<String> = None;
    let mut call_seq: usize = 0;
    apply_chunk(
        doc,
        sink,
        &mut text,
        &mut reasoning,
        &mut tool_calls,
        &mut native_parts,
        &mut usage,
        &mut stop_reason,
        &mut call_seq,
    );
    Ok(build_completion_response(
        text,
        reasoning,
        tool_calls,
        native_parts,
        usage,
        stop_reason,
        resolved_model,
    ))
}

#[allow(clippy::too_many_arguments)]
fn apply_chunk(
    doc: &Value,
    sink: &DeltaSink,
    text: &mut String,
    reasoning: &mut String,
    tool_calls: &mut Vec<ToolCall>,
    native_parts: &mut Vec<Value>,
    usage: &mut Usage,
    stop_reason: &mut Option<String>,
    call_seq: &mut usize,
) {
    let resp = doc.get("response").unwrap_or(doc);
    if let Some(u) = resp.get("usageMetadata") {
        let prompt = u["promptTokenCount"].as_u64().unwrap_or(0);
        let cand = u["candidatesTokenCount"].as_u64().unwrap_or(0);
        let thoughts = u["thoughtsTokenCount"].as_u64().unwrap_or(0);
        let cached = u["cachedContentTokenCount"].as_u64().unwrap_or(0);
        if prompt > 0 {
            usage.input_tokens = prompt;
        }
        if cand + thoughts > 0 {
            usage.output_tokens = cand + thoughts;
        }
        if cached > 0 {
            usage.cached_input_tokens = cached;
        }
    }

    let Some(candidates) = resp["candidates"].as_array() else {
        return;
    };
    for cand in candidates {
        if let Some(reason) = cand["finishReason"].as_str() {
            *stop_reason = Some(match reason {
                "MAX_TOKENS" => "max_tokens".to_string(),
                "STOP" => "end_turn".to_string(),
                other => other.to_ascii_lowercase(),
            });
        }
        let Some(parts) = cand["content"]["parts"].as_array() else {
            continue;
        };
        for part in parts {
            // Skip empty `{ "text": "" }` keep-alive parts that carry no signature or thought flag.
            let is_empty_plain_text = part
                .as_object()
                .is_some_and(|o| o.len() == 1 && o.get("text").and_then(Value::as_str) == Some(""));
            if !is_empty_plain_text {
                native_parts.push(part.clone());
            }

            let is_thought = part["thought"].as_bool().unwrap_or(false);
            if is_thought {
                if let Some(t) = part["text"].as_str().filter(|s| !s.is_empty()) {
                    reasoning.push_str(t);
                    sink.send(Delta::Progress);
                } else {
                    sink.send(Delta::Progress);
                }
                continue;
            }
            if let Some(fc) = part.get("functionCall") {
                *call_seq += 1;
                let name = fc["name"].as_str().unwrap_or("tool").to_string();
                let id = fc["id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("call_{}", *call_seq));
                let arguments = fc.get("args").cloned().unwrap_or_else(|| json!({}));
                // Ensure the recorded native part has the same `id` if it was synthesized.
                if let Some(last) = native_parts.last_mut() {
                    if last.get("functionCall").is_some()
                        && last["functionCall"]["id"].as_str().is_none()
                    {
                        last["functionCall"]["id"] = json!(id);
                    }
                }
                tool_calls.push(ToolCall {
                    id,
                    name,
                    arguments,
                });
                sink.send(Delta::Progress);
                continue;
            }
            if let Some(t) = part["text"].as_str().filter(|s| !s.is_empty()) {
                text.push_str(t);
                sink.send(Delta::Text(t.to_string()));
            }
        }
    }
}

fn build_completion_response(
    text: String,
    reasoning: String,
    tool_calls: Vec<ToolCall>,
    native_parts: Vec<Value>,
    usage: Usage,
    stop_reason: Option<String>,
    resolved_model: &str,
) -> CompletionResponse {
    let content = (!text.is_empty() || tool_calls.is_empty()).then_some(text);
    let reasoning = (!reasoning.is_empty()).then_some(reasoning);
    let mut message = Message::assistant(content, tool_calls, reasoning);
    if !native_parts.is_empty() {
        message.native = Some(NativeBlocks {
            api: "cloudcode".into(),
            model: resolved_model.to_string(),
            items: native_parts,
        });
    }
    CompletionResponse {
        message,
        usage,
        stop_reason,
    }
}

/// Converts `v1internal:retrieveUserQuotaSummary` JSON into [`RateLimits`]
/// (5-hour = 300 min and weekly = 10080 min windows).
pub fn parse_quota_summary(doc: &Value) -> RateLimits {
    let mut out = RateLimits::default();
    let mut seen_windows: Vec<Option<u64>> = Vec::new();
    let buckets = doc["groups"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|g| g["buckets"].as_array())
        .flatten()
        .chain(doc["buckets"].as_array().into_iter().flatten());

    for b in buckets {
        let Some(rem) = b["remainingFraction"].as_f64() else {
            continue;
        };
        let minutes = match b["window"].as_str() {
            Some("5h") => Some(300),
            Some("weekly") => Some(10_080),
            Some("daily") => Some(1_440),
            _ => None,
        };
        if seen_windows.contains(&minutes) {
            continue;
        }
        seen_windows.push(minutes);
        let used_percent = ((1.0 - rem.clamp(0.0, 1.0)) * 100.0).round();
        let resets_at = b["resetTime"].as_str().and_then(parse_rfc3339_unix);
        out.windows.push(LimitWindow {
            minutes,
            used_percent,
            resets_at,
        });
    }
    // Sort shorter windows (5h) before longer ones (weekly), matching Codex.
    out.windows.sort_by_key(|w| w.minutes.unwrap_or(u64::MAX));
    out
}

/// Parses an ISO-8601 / RFC-3339 UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`) into
/// Unix epoch seconds.
pub fn parse_rfc3339_unix(s: &str) -> Option<u64> {
    let (date, time) = s.trim().split_once('T')?;
    let mut dp = date.split('-');
    let year: i64 = dp.next()?.parse().ok()?;
    let month: u32 = dp.next()?.parse().ok()?;
    let day: u32 = dp.next()?.parse().ok()?;

    let time = time.strip_suffix('Z').unwrap_or(time);
    let time = time.split(['+', '-', '.']).next()?;
    let mut tp = time.split(':');
    let hour: u64 = tp.next()?.parse().ok()?;
    let min: u64 = tp.next()?.parse().ok()?;
    let sec: u64 = tp.next()?.parse().ok()?;

    let days = days_from_civil(year, month, day)?;
    if days < 0 {
        return None;
    }
    Some((days as u64) * 86_400 + hour * 3600 + min * 60 + sec)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u32;
    let m_adj = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * m_adj + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + (doe as i64) - 719_468)
}

fn platform_enum() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "DARWIN_ARM64",
        ("macos", _) => "DARWIN_AMD64",
        ("linux", "aarch64") => "LINUX_ARM64",
        ("windows", _) => "WINDOWS_AMD64",
        _ => "LINUX_AMD64",
    }
}

fn user_agent(version: &str) -> String {
    format!(
        "antigravity/{version} {}/{} ferrule/{}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        env!("CARGO_PKG_VERSION"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_summary_parses_5h_and_weekly_windows() {
        let doc = json!({
            "groups": [{
                "buckets": [
                    {
                        "bucketId": "3p-weekly",
                        "window": "weekly",
                        "resetTime": "2026-11-10T17:39:16Z",
                        "remainingFraction": 0.75
                    },
                    {
                        "bucketId": "3p-5h",
                        "window": "5h",
                        "resetTime": "2026-11-03T22:39:16Z",
                        "remainingFraction": 0.9
                    }
                ]
            }]
        });
        let limits = parse_quota_summary(&doc);
        assert_eq!(limits.windows.len(), 2);
        assert_eq!(limits.windows[0].minutes, Some(300));
        assert_eq!(limits.windows[0].used_percent, 10.0);
        assert!(limits.windows[0].resets_at.is_some());
        assert_eq!(limits.windows[1].minutes, Some(10_080));
        assert_eq!(limits.windows[1].used_percent, 25.0);
    }

    #[test]
    fn clean_schema_strips_unsupported_keywords_and_normalizes_nullable_types() {
        let raw = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "path": {
                    "type": ["string", "null"],
                    "default": ".",
                    "description": "File path"
                }
            },
            "required": ["path"]
        });
        let cleaned = clean_schema(&raw);
        assert!(cleaned.get("$schema").is_none());
        assert!(cleaned.get("additionalProperties").is_none());
        assert_eq!(cleaned["properties"]["path"]["type"], "string");
        assert_eq!(cleaned["properties"]["path"]["nullable"], true);
        assert!(cleaned["properties"]["path"].get("default").is_none());
    }
}
