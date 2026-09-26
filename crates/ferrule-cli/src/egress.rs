//! M33 in the binary: `[egress]` plus the endpoints ferrule's own config
//! names, as the policy the credential proxy enforces, and what a refusal
//! leaves behind for the owner (docs/egress.md).

use crate::config::{self, Config, EmbedderChoice};
use anyhow::Result;
use ferrule_core::ledger::EGRESS_DENIED_KIND;
use ferrule_core::{LedgerRecord, LedgerSink};
use ferrule_proxy::{Broker, Denial, EgressPolicy};
use ferrule_trust::audit::Audit;
use serde_json::json;
use std::sync::Arc;

/// The ledger's `provider` on an egress refusal row.
pub const EGRESS_PROVIDER: &str = "egress";

/// `[egress]` with every endpoint the config names let through the private
/// guard (and `default = "deny"`) on its own port: model servers, the
/// embedder, MCP servers, the search backend, the telemetry collector.
pub fn policy(cfg: &Config) -> Result<EgressPolicy> {
    let mut policy = cfg.egress.policy()?;
    for (host, port) in endpoints(cfg) {
        if let Err(e) = policy.allow_endpoint(&host, port) {
            tracing::debug!("egress: not an endpoint ({host}:{port}): {e}");
        }
    }
    Ok(policy)
}

/// host:port of each URL the config names for ferrule to reach.
pub fn endpoints(cfg: &Config) -> Vec<(String, u16)> {
    let mut urls: Vec<String> = cfg.providers.values().map(|p| p.base_url.clone()).collect();
    if let Ok(EmbedderChoice::Endpoint(e)) = cfg.memory.choice(&cfg.providers) {
        urls.push(e.base_url);
    }
    urls.extend(cfg.mcp.servers.iter().filter_map(|s| s.url.clone()));
    if let Ok(Some(s)) = cfg.web_search.settings() {
        urls.push(s.endpoint);
    }
    if let Ok(Some(t)) = cfg.telemetry.traces_url() {
        urls.push(t);
    }
    let mut out: Vec<(String, u16)> = urls.iter().filter_map(|u| host_port(u)).collect();
    out.sort();
    out.dedup();
    out
}

fn host_port(raw: &str) -> Option<(String, u16)> {
    let url = url::Url::parse(raw).ok()?;
    let host = url
        .host_str()?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    Some((host, url.port_or_known_default()?))
}

/// Wires the proxy's refusals to a ledger row and a trust audit event each
/// (the proxy already keeps it to one per source, host and reason per 10 s).
pub fn report_denials(broker: &Broker, sink: Option<Arc<dyn LedgerSink>>) {
    let audit = config::data_dir()
        .ok()
        .map(|d| Audit::new(d.join("trust").join("audit.jsonl")));
    broker.on_deny(move |d| {
        tracing::info!(
            "egress: refused {} {} {}:{} ({})",
            d.source,
            d.method,
            d.host,
            d.port,
            d.reason.as_str()
        );
        if let Some(sink) = &sink {
            sink.record(denial_record(d));
        }
        if let Some(audit) = &audit {
            audit.record(
                chrono::Utc::now(),
                "egress_denied",
                None,
                None,
                json!({
                    "source": d.source.to_string(),
                    "method": d.method,
                    "host": d.host,
                    "port": d.port,
                    "reason": d.reason.as_str(),
                    "detail": d.detail,
                }),
            );
        }
    });
}

/// One refusal's ledger row: host and port only, never a path or query
/// (they can carry data).
pub fn denial_record(d: &Denial) -> LedgerRecord {
    LedgerRecord {
        timestamp: chrono::Utc::now().to_rfc3339(),
        session_id: format!("egress:{}", d.source),
        task_shape: EGRESS_PROVIDER.into(),
        origin: None,
        provider: EGRESS_PROVIDER.into(),
        model: d.host.clone(),
        iteration: 0,
        call_kind: EGRESS_DENIED_KIND.into(),
        input_tokens: 0,
        cached_input_tokens: 0,
        cache_write_input_tokens: 0,
        output_tokens: 0,
        tool_calls: 0,
        latency_ms: 0,
        outcome: "error".into(),
        error_kind: Some(d.reason.as_str().into()),
        error_message: Some(format!("{} {} {}:{}", d.source, d.method, d.host, d.port)),
        cost_usd: None,
        eval: None,
        tree: None,
        route: None,
        speed: None,
    }
}

/// For `ferrule doctor` and the dashboard: refusals in the ledger since
/// `since`, and the hosts refused most, most first.
pub fn recent_denials(
    records: &[LedgerRecord],
    since: chrono::DateTime<chrono::Utc>,
) -> (usize, Vec<(String, usize)>) {
    let mut hosts: std::collections::BTreeMap<String, usize> = Default::default();
    let mut total = 0;
    for r in records.iter().filter(|r| r.call_kind == EGRESS_DENIED_KIND) {
        let Ok(at) = chrono::DateTime::parse_from_rfc3339(&r.timestamp) else {
            continue;
        };
        if at < since {
            continue;
        }
        total += 1;
        *hosts.entry(r.model.clone()).or_default() += 1;
    }
    let mut top: Vec<(String, usize)> = hosts.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    top.truncate(5);
    (total, top)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(toml: &str) -> Config {
        toml::from_str::<Config>(toml).unwrap().finish().unwrap()
    }

    #[test]
    fn configured_endpoints_are_let_through_on_their_port() {
        let c = cfg(r#"
            [providers.local]
            base_url = "http://localhost:11434/v1"
            api_key_env = "OLLAMA_KEY"
            model = "qwen3"
            [providers.cloud]
            base_url = "https://api.example.com/v1"
            api_key_env = "CLOUD_KEY"
            model = "m"
            [[mcp.servers]]
            name = "nas"
            url = "http://[fd00::5]:8123/mcp"
            [web_search]
            provider = "searxng"
            endpoint = "http://192.168.1.20:8888"
        "#);
        let eps = endpoints(&c);
        for want in [
            ("localhost".to_string(), 11434),
            ("api.example.com".to_string(), 443),
            ("fd00::5".to_string(), 8123),
            ("192.168.1.20".to_string(), 8888),
        ] {
            assert!(eps.contains(&want), "{want:?} not in {eps:?}");
        }
        let p = policy(&c).unwrap();
        assert_eq!(p.implicit().len(), eps.len());
        // The owner's own rules still count as rules; endpoints alone don't.
        assert!(!p.has_rules());
    }

    #[test]
    fn a_bad_egress_section_is_a_config_error() {
        for bad in [
            "[egress]\ndefault = \"block\"",
            "[egress]\nprivate = \"deny\"",
            "[egress]\nallow = [\"*\"]",
            "[egress]\ndeny = [\"10.0.0.0/33\"]",
        ] {
            let parsed: Config = toml::from_str(bad).unwrap();
            assert!(parsed.finish().is_err(), "{bad}");
        }
        let c = cfg("[egress]\ndefault = \"deny\"\nallow = [\"*.github.com\", \"10.0.0.0/8\"]");
        assert!(c.egress.policy().unwrap().has_rules());
    }

    #[test]
    fn a_denial_row_says_where_but_not_what() {
        let d = Denial {
            source: ferrule_proxy::Source::Tool,
            host: "169.254.169.254".into(),
            port: 80,
            method: "GET".into(),
            reason: ferrule_proxy::egress::Reason::Private,
            detail: "169.254.169.254 (cloud metadata)".into(),
        };
        let r = denial_record(&d);
        assert!(r.is_bookkeeping());
        assert_eq!(r.error_kind.as_deref(), Some("private_address"));
        assert_eq!(
            r.error_message.as_deref(),
            Some("tool GET 169.254.169.254:80")
        );
        let (n, top) = recent_denials(
            &[r.clone(), r],
            chrono::Utc::now() - chrono::Duration::hours(1),
        );
        assert_eq!(n, 2);
        assert_eq!(top, [("169.254.169.254".to_string(), 2)]);
    }
}
