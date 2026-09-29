//! A model without reliable native tool calling sometimes writes the call
//! as message text instead of a structured tool call: `<function=…>
//! <parameter=…>` markup, a `<tool_call>` JSON block (the Qwen/Hermes
//! dialect), or a bare `{"name": …, "arguments": …}` object. Read here,
//! the loop can still run the call instead of ending the turn on it; what
//! won't parse earns the model one nudge (agent.rs). Prior art: OpenHands'
//! NonNativeToolCallingMixin.

use serde_json::{Map, Value};

/// One tool call read out of assistant text.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedCall {
    pub name: String,
    pub arguments: Value,
}

/// What assistant text turned out to carry.
#[derive(Debug, Clone, PartialEq)]
pub enum Salvage {
    /// Tool call(s) parsed; `rest` is the text with their markup cut out
    /// (empty when the message was nothing but the call).
    Calls {
        calls: Vec<ParsedCall>,
        rest: String,
    },
    /// The markers of a call are there but it won't parse: worth a nudge.
    LooksLikeCall,
}

/// Said to the model when its call arrived as text. agent.rs caps how
/// often this goes out per run, so a model that can't do structured calls
/// still gets to answer.
pub const NUDGE: &str = "[ferrule] Your tool call arrived as text, not as a structured tool call, so it did not run. \
Re-emit it as a structured tool call — no `<function=…>` markup, no `<tool_call>` block, no JSON in the message text.";

/// The markers a text-written call carries.
const MARKERS: [&str; 4] = ["<function=", "<tool_call>", "</tool_call>", "<parameter="];

/// Reads tool calls written as text. `None`: no sign of a call, the text
/// is an answer like any other.
pub fn salvage(text: &str) -> Option<Salvage> {
    for read in [function_markup, tool_call_blocks, bare_json] {
        if let Some(found) = read(text) {
            return Some(found);
        }
    }
    MARKERS
        .iter()
        .any(|m| text.contains(m))
        .then_some(Salvage::LooksLikeCall)
}

/// `<function=name><parameter=key>value</parameter>…</function>`, several
/// allowed; the close tag may be missing at the end of the text.
fn function_markup(text: &str) -> Option<Salvage> {
    if !text.contains("<function=") {
        return None;
    }
    let mut calls = Vec::new();
    let mut spans = Vec::new();
    let mut at = 0;
    while let Some(open) = text[at..].find("<function=").map(|i| at + i) {
        let from = open + "<function=".len();
        let Some(gt) = text[from..].find('>').map(|i| from + i) else {
            break;
        };
        let name = text[from..gt].trim().trim_matches('"');
        let body_from = gt + 1;
        let (body, span_end) = match text[body_from..].find("</function>") {
            Some(i) => (
                &text[body_from..body_from + i],
                body_from + i + "</function>".len(),
            ),
            None => (&text[body_from..], text.len()),
        };
        if let Some(call) = parameters_call(name, body) {
            calls.push(call);
            spans.push(open..span_end);
        }
        at = span_end;
    }
    found(calls, spans, text)
}

/// `<tool_call>{…}</tool_call>`, several allowed; the close tag may be
/// missing at the end of the text.
fn tool_call_blocks(text: &str) -> Option<Salvage> {
    if !text.contains("<tool_call>") {
        return None;
    }
    let mut calls = Vec::new();
    let mut spans = Vec::new();
    let mut at = 0;
    while let Some(open) = text[at..].find("<tool_call>").map(|i| at + i) {
        let from = open + "<tool_call>".len();
        let (body, span_end) = match text[from..].find("</tool_call>") {
            Some(i) => (&text[from..from + i], from + i + "</tool_call>".len()),
            None => (&text[from..], text.len()),
        };
        if let JsonCall::Call(call) = json_call(body) {
            calls.push(call);
            spans.push(open..span_end);
        }
        at = span_end;
    }
    found(calls, spans, text)
}

/// The whole message is one JSON call object (one ``` fence around it is
/// allowed). Embedded in prose it stays prose — an example in an answer is
/// not a call.
fn bare_json(text: &str) -> Option<Salvage> {
    let t = text.trim();
    if !t.starts_with('{') && !t.starts_with("```") {
        return None;
    }
    match json_call(t) {
        JsonCall::Call(call) => Some(Salvage::Calls {
            calls: vec![call],
            rest: String::new(),
        }),
        JsonCall::Attempt => Some(Salvage::LooksLikeCall),
        JsonCall::No => None,
    }
}

/// What a blob of JSON-ish text turned out to be.
enum JsonCall {
    Call(ParsedCall),
    /// A `name` is there but the arguments won't read: a call attempt.
    Attempt,
    No,
}

/// A `{"name": …, "arguments"|"parameters"|"input": …}` object. The
/// arguments may themselves be a string of JSON.
fn json_call(body: &str) -> JsonCall {
    let parsed: Result<Value, _> = serde_json::from_str(unfence(body));
    let value = match parsed {
        Ok(value) => value,
        Err(_) => {
            let t = unfence(body).trim();
            let attempted = t.starts_with('{') && t.contains("\"name\"");
            return if attempted {
                JsonCall::Attempt
            } else {
                JsonCall::No
            };
        }
    };
    let Some(object) = value.as_object() else {
        return JsonCall::No;
    };
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if name.is_empty() {
        return JsonCall::No;
    }
    let raw = ["arguments", "parameters", "input"]
        .iter()
        .find_map(|key| object.get(*key));
    let arguments = match raw {
        None => Value::Object(Map::new()),
        Some(v) if v.is_object() => v.clone(),
        Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
            Ok(v) if v.is_object() => v,
            _ => return JsonCall::Attempt,
        },
        Some(_) => return JsonCall::Attempt,
    };
    JsonCall::Call(ParsedCall {
        name: name.to_string(),
        arguments,
    })
}

/// One ``` fence around the whole text comes off (` ```json ` included).
fn unfence(text: &str) -> &str {
    let t = text.trim();
    let Some(rest) = t.strip_prefix("```") else {
        return t;
    };
    let Some(inner) = rest.strip_suffix("```") else {
        return t;
    };
    match inner.find('\n') {
        Some(i) => inner[i + 1..].trim(),
        None => inner.trim(),
    }
}

/// A `<function=name>` body: `<parameter=key>value</parameter>` pairs, a
/// JSON object, or nothing at all (a no-argument call). Parameter values
/// read as JSON when they parse (`10`, `true`, `["a"]`), else stay text.
fn parameters_call(name: &str, body: &str) -> Option<ParsedCall> {
    if name.is_empty() {
        return None;
    }
    if body.contains("<parameter=") {
        let mut arguments = Map::new();
        let mut at = 0;
        while let Some(open) = body[at..].find("<parameter=").map(|i| at + i) {
            let from = open + "<parameter=".len();
            let Some(gt) = body[from..].find('>').map(|i| from + i) else {
                break;
            };
            let key = body[from..gt].trim().trim_matches('"');
            let value_from = gt + 1;
            let raw = match body[value_from..].find("</parameter>") {
                Some(i) => {
                    let v = &body[value_from..value_from + i];
                    at = value_from + i + "</parameter>".len();
                    v
                }
                None => {
                    let v = &body[value_from..];
                    at = body.len();
                    v
                }
            };
            if key.is_empty() {
                return None;
            }
            let raw = raw.trim();
            let value = serde_json::from_str(raw).unwrap_or(Value::String(raw.to_string()));
            arguments.insert(key.to_string(), value);
        }
        return Some(ParsedCall {
            name: name.to_string(),
            arguments: Value::Object(arguments),
        });
    }
    let body = body.trim();
    if body.is_empty() {
        return Some(ParsedCall {
            name: name.to_string(),
            arguments: Value::Object(Map::new()),
        });
    }
    // The tag already named the call; the body is the arguments object.
    match serde_json::from_str::<Value>(body) {
        Ok(v) if v.is_object() => Some(ParsedCall {
            name: name.to_string(),
            arguments: v,
        }),
        _ => None,
    }
}

/// What a markup scan found: the calls and the text around them, or
/// nothing when none parsed.
fn found(
    calls: Vec<ParsedCall>,
    spans: Vec<std::ops::Range<usize>>,
    text: &str,
) -> Option<Salvage> {
    if calls.is_empty() {
        return None;
    }
    Some(Salvage::Calls {
        calls,
        rest: without(text, spans),
    })
}

/// The text with `spans` cut out, trimmed.
fn without(text: &str, mut spans: Vec<std::ops::Range<usize>>) -> String {
    spans.sort_by_key(|s| s.start);
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for span in spans {
        if span.start >= at && span.end <= text.len() {
            out.push_str(&text[at..span.start]);
            at = span.end;
        }
    }
    out.push_str(&text[at..]);
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn calls(s: Salvage) -> (Vec<ParsedCall>, String) {
        match s {
            Salvage::Calls { calls, rest } => (calls, rest),
            other => panic!("expected calls, got {other:?}"),
        }
    }

    #[test]
    fn function_markup_with_parameters_parses() {
        // The shape that ended 13 eval runs.
        let (found, rest) = calls(
            salvage("<function=list_dir><parameter=path>logs</parameter></function>").unwrap(),
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "list_dir");
        assert_eq!(found[0].arguments, json!({"path": "logs"}));
        assert_eq!(rest, "");
    }

    #[test]
    fn parameter_values_read_as_json_when_they_parse() {
        let (found, _) = calls(
            salvage(
                "<function=read_file><parameter=path>Cargo.toml</parameter>\
                 <parameter=offset>10</parameter><parameter=recursive>true</parameter>\
                 <parameter=names>[\"a\", \"b\"]</parameter></function>",
            )
            .unwrap(),
        );
        assert_eq!(
            found[0].arguments,
            json!({"path": "Cargo.toml", "offset": 10, "recursive": true, "names": ["a", "b"]})
        );
    }

    #[test]
    fn function_markup_with_a_json_body_parses() {
        let (found, _) =
            calls(salvage("<function=shell>{\"command\": \"ls -la\"}</function>").unwrap());
        assert_eq!(found[0].name, "shell");
        assert_eq!(found[0].arguments, json!({"command": "ls -la"}));
    }

    #[test]
    fn a_no_argument_call_has_empty_arguments() {
        let (found, _) = calls(salvage("<function=now></function>").unwrap());
        assert_eq!(found[0].arguments, json!({}));
    }

    #[test]
    fn a_missing_close_tag_still_salvages() {
        let (found, _) =
            calls(salvage("<function=list_dir><parameter=path>logs</parameter>").unwrap());
        assert_eq!(found[0].name, "list_dir");
        assert_eq!(found[0].arguments, json!({"path": "logs"}));
    }

    #[test]
    fn surrounding_prose_survives_the_markup_coming_out() {
        let (found, rest) = calls(
            salvage("Let me look at the logs.\n<function=list_dir><parameter=path>logs</parameter></function>\nThat should show it.")
                .unwrap(),
        );
        assert_eq!(found[0].name, "list_dir");
        assert_eq!(rest, "Let me look at the logs.\n\nThat should show it.");
    }

    #[test]
    fn several_function_blocks_all_parse() {
        let (found, rest) = calls(
            salvage("<function=list_dir><parameter=path>a</parameter></function><function=read_file><parameter=path>b.txt</parameter></function>")
                .unwrap(),
        );
        assert_eq!(found.len(), 2);
        assert_eq!(found[1].name, "read_file");
        assert_eq!(rest, "");
    }

    #[test]
    fn a_tool_call_block_parses() {
        let (found, rest) = calls(
            salvage("<tool_call>\n{\"name\": \"list_dir\", \"arguments\": {\"path\": \"logs\"}}\n</tool_call>")
                .unwrap(),
        );
        assert_eq!(found[0].name, "list_dir");
        assert_eq!(found[0].arguments, json!({"path": "logs"}));
        assert_eq!(rest, "");
    }

    #[test]
    fn hermes_parameters_and_stringified_arguments_parse() {
        let (found, _) = calls(
            salvage("<tool_call>{\"name\": \"search\", \"parameters\": \"{\\\"q\\\": \\\"x\\\"}\"}</tool_call>")
                .unwrap(),
        );
        assert_eq!(found[0].arguments, json!({"q": "x"}));
    }

    #[test]
    fn an_unclosed_tool_call_block_still_salvages() {
        let (found, _) = calls(
            salvage("<tool_call>{\"name\": \"list_dir\", \"arguments\": {\"path\": \".\"}}")
                .unwrap(),
        );
        assert_eq!(found[0].name, "list_dir");
    }

    #[test]
    fn a_bare_json_object_parses() {
        let (found, rest) = calls(
            salvage("{\"name\": \"list_dir\", \"arguments\": {\"path\": \"logs\"}}").unwrap(),
        );
        assert_eq!(found[0].name, "list_dir");
        assert_eq!(found[0].arguments, json!({"path": "logs"}));
        assert_eq!(rest, "");
    }

    #[test]
    fn a_fenced_json_object_parses() {
        let (found, _) = calls(
            salvage("```json\n{\"name\": \"read_file\", \"input\": {\"path\": \"a.txt\"}}\n```")
                .unwrap(),
        );
        assert_eq!(found[0].name, "read_file");
        assert_eq!(found[0].arguments, json!({"path": "a.txt"}));
    }

    #[test]
    fn a_name_without_arguments_gets_an_empty_object() {
        let (found, _) = calls(salvage("{\"name\": \"now\"}").unwrap());
        assert_eq!(found[0].arguments, json!({}));
    }

    #[test]
    fn broken_markup_is_a_nudge() {
        assert_eq!(
            salvage("<tool_call>{not json}</tool_call>"),
            Some(Salvage::LooksLikeCall)
        );
        assert_eq!(
            salvage("<function=><parameter=path>x</parameter></function>"),
            Some(Salvage::LooksLikeCall)
        );
        assert_eq!(
            salvage("<function=list_dir><parameter=></parameter></function>"),
            Some(Salvage::LooksLikeCall)
        );
        // A stray close tag, the other shape the eval saw.
        assert_eq!(
            salvage("I tried: </tool_call>"),
            Some(Salvage::LooksLikeCall)
        );
        assert_eq!(
            salvage("{\"name\": \"x\", \"arguments\": \"not json\"}"),
            Some(Salvage::LooksLikeCall)
        );
        assert_eq!(
            salvage("{\"name\": \"list_dir\", \"arguments\": {\"path\":"),
            Some(Salvage::LooksLikeCall)
        );
    }

    #[test]
    fn plain_answers_are_left_alone() {
        assert_eq!(salvage("I'll check the logs now."), None);
        assert_eq!(salvage("Done — three files changed."), None);
        // An example inside prose is not a call.
        assert_eq!(
            salvage("Call it like {\"name\": \"list_dir\", \"arguments\": {\"path\": \".\"}} — that lists files."),
            None
        );
        // No name, no call.
        assert_eq!(salvage("{\"path\": \"logs\"}"), None);
        assert_eq!(salvage(""), None);
    }
}
