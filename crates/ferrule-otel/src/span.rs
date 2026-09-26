//! One finished span and its OTLP/JSON encoding (OTLP 1.x, the JSON
//! mapping of `ExportTraceServiceRequest`: ids as lowercase hex, 64-bit
//! integers as strings, enums as numbers).

use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

/// `SPAN_KIND_INTERNAL`: a turn, a session, a tool call.
pub const KIND_INTERNAL: u8 = 1;
/// `SPAN_KIND_CLIENT`: a call out to a model provider.
pub const KIND_CLIENT: u8 = 3;

/// An attribute value.
#[derive(Debug, Clone, PartialEq)]
pub enum Attr {
    Str(String),
    Int(i64),
    Double(f64),
    Bool(bool),
}

impl From<&str> for Attr {
    fn from(v: &str) -> Self {
        Attr::Str(v.to_string())
    }
}
impl From<String> for Attr {
    fn from(v: String) -> Self {
        Attr::Str(v)
    }
}
impl From<u64> for Attr {
    fn from(v: u64) -> Self {
        Attr::Int(v.min(i64::MAX as u64) as i64)
    }
}
impl From<usize> for Attr {
    fn from(v: usize) -> Self {
        Attr::from(v as u64)
    }
}
impl From<f64> for Attr {
    fn from(v: f64) -> Self {
        Attr::Double(v)
    }
}
impl From<bool> for Attr {
    fn from(v: bool) -> Self {
        Attr::Bool(v)
    }
}

/// A finished span.
#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    /// 32 hex digits.
    pub trace_id: String,
    /// 16 hex digits.
    pub span_id: String,
    pub parent: Option<String>,
    pub name: String,
    pub kind: u8,
    pub start: SystemTime,
    pub end: SystemTime,
    pub attrs: Vec<(String, Attr)>,
    /// Status `ERROR` with this message; `None` is `UNSET`.
    pub error: Option<String>,
}

impl Span {
    pub fn attr(&self, key: &str) -> Option<&Attr> {
        self.attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn set(&mut self, key: &str, value: impl Into<Attr>) {
        self.attrs.push((key.to_string(), value.into()));
    }

    fn to_json(&self) -> Value {
        let mut span = json!({
            "traceId": self.trace_id,
            "spanId": self.span_id,
            "name": self.name,
            "kind": self.kind,
            "startTimeUnixNano": nanos(self.start),
            "endTimeUnixNano": nanos(self.end.max(self.start)),
            "attributes": attrs(&self.attrs),
        });
        if let Some(parent) = &self.parent {
            span["parentSpanId"] = json!(parent);
        }
        if let Some(message) = &self.error {
            span["status"] = json!({"code": 2, "message": message});
        }
        span
    }
}

/// A new random trace id.
pub fn trace_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// A new random span id.
pub fn span_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..16].to_string()
}

fn nanos(t: SystemTime) -> String {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        .to_string()
}

fn attrs(list: &[(String, Attr)]) -> Value {
    Value::Array(
        list.iter()
            .map(|(k, v)| {
                let value = match v {
                    Attr::Str(s) => json!({"stringValue": s}),
                    Attr::Int(i) => json!({"intValue": i.to_string()}),
                    Attr::Double(d) if d.is_finite() => json!({"doubleValue": d}),
                    Attr::Double(d) => json!({"stringValue": d.to_string()}),
                    Attr::Bool(b) => json!({"boolValue": b}),
                };
                json!({"key": k, "value": value})
            })
            .collect(),
    )
}

/// The request body for `POST /v1/traces`.
pub fn encode(service_name: &str, service_version: &str, spans: &[Span]) -> Value {
    let resource = [
        ("service.name".to_string(), Attr::from(service_name)),
        ("service.version".to_string(), Attr::from(service_version)),
        ("telemetry.sdk.name".to_string(), Attr::from("ferrule-otel")),
    ];
    json!({
        "resourceSpans": [{
            "resource": {"attributes": attrs(&resource)},
            "scopeSpans": [{
                "scope": {"name": "ferrule", "version": service_version},
                "spans": spans.iter().map(Span::to_json).collect::<Vec<_>>(),
            }],
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_span_encodes_to_the_otlp_json_mapping() {
        let start = UNIX_EPOCH + Duration::from_millis(1_700_000_000_123);
        let mut s = Span {
            trace_id: trace_id(),
            span_id: span_id(),
            parent: Some("00f067aa0ba902b7".into()),
            name: "chat m".into(),
            kind: KIND_CLIENT,
            start,
            end: start + Duration::from_millis(5),
            attrs: vec![],
            error: Some("rate_limited".into()),
        };
        s.set("gen_ai.usage.input_tokens", 12u64);
        s.set("ferrule.cost_usd", 0.25);
        s.set("ferrule.retried", true);
        s.set("gen_ai.request.model", "m");
        assert_eq!(s.trace_id.len(), 32);
        assert_eq!(s.span_id.len(), 16);
        let body = encode("ferrule", "0.3.0", &[s]);
        let span = &body["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert_eq!(span["startTimeUnixNano"], "1700000000123000000");
        assert_eq!(span["endTimeUnixNano"], "1700000000128000000");
        assert_eq!(span["kind"], 3);
        assert_eq!(span["parentSpanId"], "00f067aa0ba902b7");
        assert_eq!(span["status"]["code"], 2);
        let a = &span["attributes"];
        assert_eq!(a[0]["value"]["intValue"], "12");
        assert_eq!(a[1]["value"]["doubleValue"], 0.25);
        assert_eq!(a[2]["value"]["boolValue"], true);
        assert_eq!(a[3]["value"]["stringValue"], "m");
        assert_eq!(
            body["resourceSpans"][0]["resource"]["attributes"][0]["value"]["stringValue"],
            "ferrule"
        );
    }
}
