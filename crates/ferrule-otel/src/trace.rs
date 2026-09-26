//! Ledger rows and trace events in, finished spans out: which session and
//! turn each belongs to, and the GenAI attributes each carries
//! (docs/otel.md has the tree).

use crate::span::{span_id, trace_id, Attr, Span, KIND_CLIENT, KIND_INTERNAL};
use ferrule_core::{LedgerRecord, TraceEvent};
use std::collections::HashMap;
use std::time::{Duration, SystemTime};

/// Sessions held open at once; the least recently used is closed (its
/// span exported) past this.
pub const MAX_SESSIONS: usize = 256;
/// Each content attribute is cut to this many bytes.
pub const MAX_CONTENT: usize = 4096;

/// Turns content into what may leave the machine.
pub type Scrub = std::sync::Arc<dyn Fn(&str) -> String + Send + Sync>;

struct Session {
    trace_id: String,
    span_id: String,
    parent: Option<String>,
    start: SystemTime,
    task_shape: String,
    origin: Option<String>,
    tree: String,
    turns: u64,
    turn: Option<Turn>,
    /// The model's reply waiting for its row (content only).
    reply: Option<String>,
    used: u64,
}

impl Session {
    /// Where a call's or tool's span hangs: the open turn, else the session.
    fn parent_for_child(&self) -> String {
        self.turn
            .as_ref()
            .map_or_else(|| self.span_id.clone(), |t| t.span_id.clone())
    }

    /// A sub-agent's session (`origin = "agent:<parent>"`) ends with its
    /// run; the next run is a new span under the parent's new turn.
    fn is_sub_agent(&self) -> bool {
        self.origin
            .as_deref()
            .is_some_and(|o| o.starts_with("agent:"))
    }
}

struct Turn {
    span_id: String,
    start: SystemTime,
    goal: Option<String>,
    calls: u64,
    input_tokens: u64,
    output_tokens: u64,
    cost_usd: f64,
    priced: bool,
}

/// Open sessions and turns.
pub struct Tracer {
    content: bool,
    scrub: Option<Scrub>,
    sessions: HashMap<String, Session>,
    /// Trace id per run tree, so sub-agents share their root's trace.
    trees: HashMap<String, (String, u64)>,
    tick: u64,
}

impl Tracer {
    pub fn new(content: bool, scrub: Option<Scrub>) -> Self {
        Self {
            content,
            scrub,
            sessions: HashMap::new(),
            trees: HashMap::new(),
            tick: 0,
        }
    }

    pub fn content(&self) -> bool {
        self.content
    }

    pub fn open_sessions(&self) -> usize {
        self.sessions.len()
    }

    /// What `text` becomes on a span: scrubbed, then cut to [`MAX_CONTENT`].
    fn clean(&self, text: &str) -> String {
        let text = match &self.scrub {
            Some(scrub) => scrub(text),
            None => text.to_string(),
        };
        truncate(text)
    }

    /// The session `id`, opened if it isn't: a sub-agent's under its
    /// parent's open turn, in the parent's trace.
    fn session(
        &mut self,
        id: &str,
        tree: &str,
        task_shape: &str,
        origin: Option<&str>,
        at: SystemTime,
        out: &mut Vec<Span>,
    ) -> &mut Session {
        self.tick += 1;
        let tick = self.tick;
        if !self.sessions.contains_key(id) {
            if self.sessions.len() >= MAX_SESSIONS {
                self.evict(out);
            }
            let parent = origin
                .and_then(|o| o.strip_prefix("agent:"))
                .and_then(|p| self.sessions.get(p));
            let (trace, parent_span) = match parent {
                Some(p) => (p.trace_id.clone(), Some(p.parent_for_child())),
                None => (self.trace_for(tree), None),
            };
            self.sessions.insert(
                id.to_string(),
                Session {
                    trace_id: trace,
                    span_id: span_id(),
                    parent: parent_span,
                    start: at,
                    task_shape: task_shape.to_string(),
                    origin: origin.map(str::to_string),
                    tree: tree.to_string(),
                    turns: 0,
                    turn: None,
                    reply: None,
                    used: tick,
                },
            );
        }
        let s = self.sessions.get_mut(id).expect("inserted above");
        s.used = tick;
        s
    }

    fn trace_for(&mut self, tree: &str) -> String {
        let tick = self.tick;
        if self.trees.len() >= MAX_SESSIONS * 2 && !self.trees.contains_key(tree) {
            if let Some(oldest) = self
                .trees
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(k, _)| k.clone())
            {
                self.trees.remove(&oldest);
            }
        }
        let entry = self
            .trees
            .entry(tree.to_string())
            .or_insert_with(|| (trace_id(), tick));
        entry.1 = tick;
        entry.0.clone()
    }

    fn evict(&mut self, out: &mut Vec<Span>) {
        let oldest = self
            .sessions
            .iter()
            .min_by_key(|(_, s)| s.used)
            .map(|(k, _)| k.clone());
        if let Some(id) = oldest {
            self.close(&id, SystemTime::now(), "evicted", out);
        }
    }

    /// Ends session `id`: its open turn (if any) and then the session.
    fn close(&mut self, id: &str, at: SystemTime, why: &str, out: &mut Vec<Span>) {
        let Some(mut s) = self.sessions.remove(id) else {
            return;
        };
        if let Some(turn) = s.turn.take() {
            let mut span = turn_span(id, &s, turn, at);
            span.set("ferrule.closed", why);
            out.push(span);
        }
        let mut span = Span {
            trace_id: s.trace_id.clone(),
            span_id: s.span_id.clone(),
            parent: s.parent.clone(),
            name: "ferrule.session".into(),
            kind: KIND_INTERNAL,
            start: s.start,
            end: at,
            attrs: vec![],
            error: None,
        };
        span.set("gen_ai.conversation.id", id);
        span.set("ferrule.task_shape", s.task_shape.as_str());
        if let Some(o) = &s.origin {
            span.set("ferrule.origin", o.as_str());
        }
        span.set("ferrule.tree", s.tree.as_str());
        span.set("ferrule.turns", s.turns);
        if why != "finished" {
            span.set("ferrule.closed", why);
        }
        out.push(span);
    }

    /// Closes every open session (shutdown).
    pub fn close_all(&mut self, why: &str) -> Vec<Span> {
        let mut out = Vec::new();
        let now = SystemTime::now();
        let ids: Vec<String> = self.sessions.keys().cloned().collect();
        for id in ids {
            self.close(&id, now, why, &mut out);
        }
        out
    }

    /// One ledger row: a model-call span, when it's a model call.
    pub fn row(&mut self, r: &LedgerRecord, tree: &str) -> Vec<Span> {
        let mut out = Vec::new();
        if r.is_bookkeeping() {
            return out;
        }
        let end = chrono::DateTime::parse_from_rfc3339(&r.timestamp)
            .ok()
            .and_then(|t| {
                SystemTime::UNIX_EPOCH.checked_add(Duration::from_nanos(
                    t.timestamp_nanos_opt().unwrap_or(0).max(0) as u64,
                ))
            })
            .unwrap_or_else(SystemTime::now);
        let start = end
            .checked_sub(Duration::from_millis(r.latency_ms))
            .unwrap_or(end);
        let tree = r.tree.as_deref().unwrap_or(tree);
        let content = self.content;
        let s = self.session(
            &r.session_id,
            tree,
            &r.task_shape,
            r.origin.as_deref(),
            start,
            &mut out,
        );
        let reply = s.reply.take();
        let mut span = Span {
            trace_id: s.trace_id.clone(),
            span_id: span_id(),
            parent: Some(s.parent_for_child()),
            name: format!("chat {}", r.model),
            kind: KIND_CLIENT,
            start,
            end,
            attrs: vec![],
            error: None,
        };
        if let Some(t) = &mut s.turn {
            t.calls += 1;
            t.input_tokens += r.input_tokens;
            t.output_tokens += r.output_tokens;
            if let Some(c) = r.cost_usd {
                t.cost_usd += c;
                t.priced = true;
            }
        }
        span.set("gen_ai.operation.name", "chat");
        span.set("gen_ai.provider.name", r.provider.as_str());
        span.set("gen_ai.request.model", r.model.as_str());
        span.set("gen_ai.response.model", r.model.as_str());
        span.set("gen_ai.conversation.id", r.session_id.as_str());
        span.set("gen_ai.usage.input_tokens", r.input_tokens);
        span.set("gen_ai.usage.output_tokens", r.output_tokens);
        if r.cached_input_tokens > 0 {
            span.set("ferrule.usage.cached_input_tokens", r.cached_input_tokens);
        }
        if r.cache_write_input_tokens > 0 {
            span.set(
                "ferrule.usage.cache_write_input_tokens",
                r.cache_write_input_tokens,
            );
        }
        span.set("ferrule.call_kind", r.call_kind.as_str());
        span.set("ferrule.iteration", r.iteration);
        span.set("ferrule.tool_calls", r.tool_calls);
        if let Some(c) = r.cost_usd {
            span.set("ferrule.cost_usd", c);
        }
        if let Some(route) = &r.route {
            span.set("ferrule.route.tier", route.tier.to_string());
        }
        if let Some(ms) = r.speed.as_ref().and_then(|s| s.first_token_ms) {
            span.set("ferrule.first_token_ms", ms);
        }
        match r.outcome.as_str() {
            "ok" => {}
            "retried" => span.set("ferrule.retried", true),
            _ => {
                let kind = r.error_kind.clone().unwrap_or_else(|| "error".into());
                span.set("error.type", kind.as_str());
                span.error = Some(kind);
            }
        }
        if content {
            if let Some(text) = reply {
                let text = self.clean(&text);
                span.set("gen_ai.output.messages", messages("assistant", &text));
            }
        }
        out.push(span);
        out
    }

    /// One trace event: a turn opened or closed, a tool call, a reply.
    pub fn event(&mut self, e: &TraceEvent, tree: &str) -> Vec<Span> {
        let mut out = Vec::new();
        match e {
            TraceEvent::TurnStarted {
                session_id,
                task_shape,
                origin,
                at,
                goal,
            } => {
                let goal = goal
                    .as_deref()
                    .filter(|_| self.content)
                    .map(|g| self.clean(g));
                let s = self.session(
                    session_id,
                    tree,
                    task_shape,
                    origin.as_deref(),
                    *at,
                    &mut out,
                );
                // A turn left open (a run that never reported its end).
                if let Some(old) = s.turn.take() {
                    let mut span = turn_span(session_id, s, old, *at);
                    span.set("ferrule.closed", "superseded");
                    out.push(span);
                }
                s.turns += 1;
                s.turn = Some(Turn {
                    span_id: span_id(),
                    start: *at,
                    goal,
                    calls: 0,
                    input_tokens: 0,
                    output_tokens: 0,
                    cost_usd: 0.0,
                    priced: false,
                });
            }
            TraceEvent::ToolCall {
                session_id,
                id,
                name,
                ok,
                started,
                elapsed,
                arguments,
                result,
            } => {
                let content = self.content;
                let arguments = arguments
                    .as_deref()
                    .filter(|_| content)
                    .map(|a| self.clean(a));
                let result = result.as_deref().filter(|_| content).map(|r| self.clean(r));
                let s = self.session(session_id, tree, "run", None, *started, &mut out);
                let mut span = Span {
                    trace_id: s.trace_id.clone(),
                    span_id: span_id(),
                    parent: Some(s.parent_for_child()),
                    name: format!("execute_tool {name}"),
                    kind: KIND_INTERNAL,
                    start: *started,
                    end: *started + *elapsed,
                    attrs: vec![],
                    error: (!ok).then(|| "tool_error".to_string()),
                };
                span.set("gen_ai.operation.name", "execute_tool");
                span.set("gen_ai.tool.name", name.as_str());
                span.set("gen_ai.tool.call.id", id.as_str());
                span.set("gen_ai.conversation.id", session_id.as_str());
                let (kind, server) = tool_type(name);
                span.set("gen_ai.tool.type", kind);
                if let Some(server) = server {
                    span.set("ferrule.mcp.server", server);
                }
                if !ok {
                    span.set("error.type", "tool_error");
                }
                if let Some(a) = arguments {
                    span.set("gen_ai.tool.call.arguments", a);
                }
                if let Some(r) = result {
                    span.set("gen_ai.tool.call.result", r);
                }
                out.push(span);
            }
            TraceEvent::CallContent { session_id, text } => {
                if self.content {
                    if let Some(s) = self.sessions.get_mut(session_id) {
                        s.reply = Some(text.clone());
                    }
                }
            }
            TraceEvent::TurnFinished {
                session_id,
                at,
                ok,
                incomplete,
            } => {
                let Some(s) = self.sessions.get_mut(session_id) else {
                    return out;
                };
                if let Some(turn) = s.turn.take() {
                    let mut span = turn_span(session_id, s, turn, *at);
                    if !ok {
                        span.error = Some("turn_failed".into());
                        span.set("error.type", "turn_failed");
                    }
                    if let Some(why) = incomplete {
                        span.set("ferrule.incomplete", why.as_str());
                    }
                    out.push(span);
                }
                if s.is_sub_agent() {
                    self.close(session_id, *at, "finished", &mut out);
                }
            }
        }
        out
    }
}

fn turn_span(session_id: &str, s: &Session, turn: Turn, end: SystemTime) -> Span {
    let mut span = Span {
        trace_id: s.trace_id.clone(),
        span_id: turn.span_id,
        parent: Some(s.span_id.clone()),
        name: "ferrule.turn".into(),
        kind: KIND_INTERNAL,
        start: turn.start,
        end,
        attrs: vec![],
        error: None,
    };
    span.set("gen_ai.conversation.id", session_id);
    span.set("ferrule.task_shape", s.task_shape.as_str());
    if let Some(o) = &s.origin {
        span.set("ferrule.origin", o.as_str());
    }
    span.set("ferrule.model_calls", turn.calls);
    span.set("gen_ai.usage.input_tokens", turn.input_tokens);
    span.set("gen_ai.usage.output_tokens", turn.output_tokens);
    if turn.priced {
        span.set("ferrule.cost_usd", turn.cost_usd);
    }
    if let Some(goal) = turn.goal {
        span.set("gen_ai.input.messages", messages("user", &goal));
    }
    span
}

/// `gen_ai.tool.type`, and the MCP server for an MCP tool
/// (`mcp__<server>__<tool>`).
fn tool_type(name: &str) -> (&'static str, Option<&str>) {
    if let Some(rest) = name.strip_prefix("mcp__") {
        return ("extension", rest.split("__").next());
    }
    match name {
        "spawn_agent" | "wait_agent" | "resume_agent" | "close_agent" | "list_agents" => {
            ("agent", None)
        }
        _ if name.contains("__") => ("extension", None),
        _ => ("function", None),
    }
}

/// A GenAI semconv message list with one text part, as a JSON string.
fn messages(role: &str, text: &str) -> String {
    serde_json::json!([{"role": role, "parts": [{"type": "text", "content": text}]}]).to_string()
}

fn truncate(mut text: String) -> String {
    if text.len() > MAX_CONTENT {
        let mut cut = MAX_CONTENT;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push('…');
    }
    text
}

impl Attr {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Attr::Str(s) => Some(s),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn row(session: &str, model: &str, origin: Option<&str>) -> LedgerRecord {
        LedgerRecord {
            timestamp: chrono::Utc::now().to_rfc3339(),
            session_id: session.into(),
            task_shape: "run".into(),
            origin: origin.map(Into::into),
            provider: "anthropic".into(),
            model: model.into(),
            iteration: 0,
            call_kind: "turn".into(),
            input_tokens: 100,
            cached_input_tokens: 40,
            cache_write_input_tokens: 0,
            output_tokens: 20,
            tool_calls: 1,
            latency_ms: 250,
            outcome: "ok".into(),
            error_kind: None,
            error_message: None,
            cost_usd: Some(0.01),
            eval: None,
            tree: Some("root".into()),
            route: None,
            speed: None,
        }
    }

    fn started(session: &str, origin: Option<&str>, goal: Option<&str>) -> TraceEvent {
        TraceEvent::TurnStarted {
            session_id: session.into(),
            task_shape: "run".into(),
            origin: origin.map(Into::into),
            at: SystemTime::now(),
            goal: goal.map(Into::into),
        }
    }

    fn finished(session: &str) -> TraceEvent {
        TraceEvent::TurnFinished {
            session_id: session.into(),
            at: SystemTime::now(),
            ok: true,
            incomplete: None,
        }
    }

    fn tool(session: &str, name: &str) -> TraceEvent {
        TraceEvent::ToolCall {
            session_id: session.into(),
            id: "call_1".into(),
            name: name.into(),
            ok: true,
            started: SystemTime::now(),
            elapsed: Duration::from_millis(3),
            arguments: Some("{\"cmd\":\"cat .env\"}".into()),
            result: Some("KEY=sk-live-123".into()),
        }
    }

    fn by_name<'a>(spans: &'a [Span], name: &str) -> &'a Span {
        spans
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("no {name} in {spans:#?}"))
    }

    #[test]
    fn a_turn_holds_its_calls_and_tools_and_a_sub_agent_nests_under_it() {
        let mut t = Tracer::new(false, None);
        let mut spans = Vec::new();
        spans.extend(t.event(&started("root", None, Some("the goal")), "root"));
        spans.extend(t.row(&row("root", "claude", None), "root"));
        spans.extend(t.event(&tool("root", "mcp__github__search"), "root"));
        // A sub-agent in the same tree, spawned from the open turn.
        spans.extend(t.event(&started("agent-a", Some("agent:root"), None), "root"));
        spans.extend(t.row(&row("agent-a", "haiku", Some("agent:root")), "root"));
        spans.extend(t.event(&finished("agent-a"), "root"));
        spans.extend(t.event(&finished("root"), "root"));
        spans.extend(t.close_all("shutdown"));

        let turns: Vec<&Span> = spans.iter().filter(|s| s.name == "ferrule.turn").collect();
        assert_eq!(turns.len(), 2);
        let root_turn = turns
            .iter()
            .find(|s| s.attr("gen_ai.conversation.id") == Some(&Attr::from("root")))
            .unwrap();
        let chat = by_name(&spans, "chat claude");
        assert_eq!(chat.parent.as_ref(), Some(&root_turn.span_id));
        assert_eq!(chat.kind, KIND_CLIENT);
        assert_eq!(
            chat.attr("gen_ai.usage.input_tokens"),
            Some(&Attr::Int(100))
        );
        assert_eq!(chat.attr("ferrule.cost_usd"), Some(&Attr::Double(0.01)));
        assert!(chat.attr("gen_ai.output.messages").is_none());
        let tool = by_name(&spans, "execute_tool mcp__github__search");
        assert_eq!(
            tool.attr("gen_ai.tool.type"),
            Some(&Attr::from("extension"))
        );
        assert_eq!(tool.attr("ferrule.mcp.server"), Some(&Attr::from("github")));
        assert!(
            tool.attr("gen_ai.tool.call.arguments").is_none(),
            "no content by default"
        );
        assert!(root_turn.attr("gen_ai.input.messages").is_none());

        let sessions: Vec<&Span> = spans
            .iter()
            .filter(|s| s.name == "ferrule.session")
            .collect();
        assert_eq!(sessions.len(), 2);
        let sub = sessions.iter().find(|s| s.parent.is_some()).unwrap();
        assert_eq!(
            sub.parent.as_ref(),
            Some(&root_turn.span_id),
            "under the spawning turn"
        );
        assert!(
            sub.attr("ferrule.closed").is_none(),
            "closed when its run finished"
        );
        let root = sessions.iter().find(|s| s.parent.is_none()).unwrap();
        assert_eq!(root.attr("ferrule.closed"), Some(&Attr::from("shutdown")));
        assert!(
            spans.iter().all(|s| s.trace_id == root.trace_id),
            "one trace per tree"
        );
        assert_eq!(root_turn.parent.as_ref(), Some(&root.span_id));
        assert_eq!(t.open_sessions(), 0);
    }

    #[test]
    fn content_is_opt_in_and_scrubbed() {
        let scrub: Scrub = std::sync::Arc::new(|s: &str| s.replace("sk-live-123", "FERRULE_PH_1"));
        let mut t = Tracer::new(true, Some(scrub));
        let mut spans = Vec::new();
        spans.extend(t.event(&started("s", None, Some("deploy with sk-live-123")), "s"));
        spans.extend(t.event(&tool("s", "shell"), "s"));
        t.event(
            &TraceEvent::CallContent {
                session_id: "s".into(),
                text: "x".repeat(10_000),
            },
            "s",
        );
        spans.extend(t.row(&row("s", "m", None), "s"));
        spans.extend(t.event(&finished("s"), "s"));
        let all = format!("{spans:?}");
        assert!(!all.contains("sk-live-123"), "{all}");
        let tool = by_name(&spans, "execute_tool shell");
        assert_eq!(tool.attr("gen_ai.tool.type"), Some(&Attr::from("function")));
        assert!(tool
            .attr("gen_ai.tool.call.result")
            .and_then(Attr::as_str)
            .unwrap()
            .contains("FERRULE_PH_1"));
        let chat = by_name(&spans, "chat m");
        let out = chat
            .attr("gen_ai.output.messages")
            .and_then(Attr::as_str)
            .unwrap();
        assert!(out.len() < MAX_CONTENT + 200, "cut to size");
        let turn = by_name(&spans, "ferrule.turn");
        assert!(turn
            .attr("gen_ai.input.messages")
            .and_then(Attr::as_str)
            .unwrap()
            .contains("FERRULE_PH_1"));
    }

    #[test]
    fn a_failed_call_is_an_error_and_bookkeeping_rows_are_skipped() {
        let mut t = Tracer::new(false, None);
        let mut r = row("s", "m", None);
        r.outcome = "error".into();
        r.error_kind = Some("rate_limited".into());
        let spans = t.row(&r, "s");
        assert_eq!(spans[0].error.as_deref(), Some("rate_limited"));
        let mut r = row("s", "m", None);
        r.call_kind = "egress_denied".into();
        assert!(t.row(&r, "s").is_empty());
    }

    #[test]
    fn sessions_past_the_cap_are_closed_oldest_first() {
        let mut t = Tracer::new(false, None);
        let mut closed = Vec::new();
        for i in 0..MAX_SESSIONS + 3 {
            closed.extend(t.event(&started(&format!("s{i}"), None, None), &format!("s{i}")));
        }
        assert_eq!(t.open_sessions(), MAX_SESSIONS);
        let evicted: Vec<_> = closed
            .iter()
            .filter(|s| s.name == "ferrule.session")
            .map(|s| s.attr("gen_ai.conversation.id").cloned().unwrap())
            .collect();
        assert_eq!(evicted, ["s0", "s1", "s2"].map(Attr::from));
    }
}
