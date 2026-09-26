//! M33 in the binary: `[telemetry]` as one OTLP exporter per process,
//! started the first time an agent is equipped, and its counters for
//! `ferrule doctor` and the gateway's `/status` (docs/otel.md).

use crate::config::{self, Config};
use anyhow::{anyhow, Result};
use ferrule_otel::{Exporter, Scrub, Settings, Status};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// How long exit waits for the last spans.
pub const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(3);

static EXPORTER: OnceLock<Option<Arc<Exporter>>> = OnceLock::new();

/// Where the export thread keeps its counters.
pub fn status_path() -> Result<PathBuf> {
    Ok(config::data_dir()?.join("telemetry").join("status.json"))
}

/// The process's exporter, started on first use; `None` when `[telemetry]`
/// has no endpoint, or it couldn't start (said once, on stderr).
pub fn exporter(cfg: &Config) -> Option<Arc<Exporter>> {
    EXPORTER
        .get_or_init(|| match start(cfg) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("ferrule: telemetry is off: {e:#}");
                None
            }
        })
        .clone()
}

fn start(cfg: &Config) -> Result<Option<Arc<Exporter>>> {
    let Some(url) = cfg.telemetry.traces_url()? else {
        return Ok(None);
    };
    let sandbox = crate::shared_sandbox(cfg)?;
    let client = ferrule_tools::egress::client_builder(sandbox.egress())
        .and_then(|b| b.build())
        .map_err(|e| anyhow!("building the exporter's HTTP client: {e}"))?;
    let mut settings = Settings::new(url, client);
    settings.headers = cfg
        .telemetry
        .headers
        .iter()
        .map(|(k, v)| Ok((k.clone(), expand(v, |n| sandbox.child_env_var(n))?)))
        .collect::<Result<_>>()?;
    settings.content = cfg.telemetry.content;
    if let Some(name) = &cfg.telemetry.service_name {
        settings.service_name = name.clone();
    }
    settings.scrub = Some(scrubber(cfg)?);
    settings.status_path = Some(status_path()?);
    Ok(Some(Exporter::start(settings)?))
}

/// Real secret values to their placeholders (the proxy's bound secrets),
/// then everything the gateway's redactor knows: channel tokens, provider
/// keys, token shapes.
fn scrubber(cfg: &Config) -> Result<Scrub> {
    let broker = crate::shared_broker(cfg)?;
    let redactor = crate::health::redactor(cfg);
    Ok(Arc::new(move |text: &str| {
        let text = match broker {
            Some(b) => b.scrub_text(text),
            None => text.to_string(),
        };
        redactor.redact(&text)
    }))
}

/// `${VAR}` → its value as a command would see it: a `[secrets]` variable
/// is its placeholder.
fn expand(value: &str, lookup: impl Fn(&str) -> Option<String>) -> Result<String> {
    let mut out = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| anyhow!("[telemetry] headers: unclosed `${{` in `{value}`"))?;
        let name = &after[..end];
        let var = lookup(name).ok_or_else(|| {
            anyhow!(
                "[telemetry] headers: ${{{name}}} isn't set, or looks secret and isn't in \
                 [secrets]"
            )
        })?;
        out.push_str(&var);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// The live counters, when this process exports.
pub fn live_status() -> Option<Status> {
    EXPORTER.get()?.as_ref().map(|e| e.status())
}

/// Sends what's left, waiting at most [`SHUTDOWN_DEADLINE`]. Safe to call
/// more than once, and when export is off.
pub fn shutdown() {
    if let Some(Some(e)) = EXPORTER.get() {
        if !e.shutdown(SHUTDOWN_DEADLINE) {
            tracing::debug!("telemetry: the last spans didn't go out in time");
        }
    }
}

/// One line for `/status`, when this process exports.
pub fn status_line() -> Option<String> {
    live_status().map(|s| describe(&s))
}

/// `exported 40 spans, dropped 0, failed 3 (last error: …)`.
pub fn describe(s: &Status) -> String {
    let mut line = format!(
        "{} — exported {} spans, dropped {}, failed {}",
        s.endpoint, s.exported, s.dropped, s.failed
    );
    if let Some(e) = &s.last_error {
        line.push_str(&format!(" (last error: {e})"));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_variables_expand_and_a_missing_one_is_an_error() {
        let lookup = |n: &str| (n == "HNY_KEY").then(|| "FERRULE_PH_abc".to_string());
        assert_eq!(
            expand("Bearer ${HNY_KEY}", lookup).unwrap(),
            "Bearer FERRULE_PH_abc"
        );
        assert!(expand("${NOPE}", lookup).is_err());
        assert!(expand("${HNY_KEY", lookup).is_err());
    }

    #[test]
    fn a_status_line_names_the_counters() {
        let s = Status {
            endpoint: "http://127.0.0.1:4318/v1/traces".into(),
            exported: 40,
            dropped: 2,
            failed: 3,
            last_error: Some("can't connect to the collector".into()),
            ..Status::default()
        };
        assert_eq!(
            describe(&s),
            "http://127.0.0.1:4318/v1/traces — exported 40 spans, dropped 2, failed 3 \
             (last error: can't connect to the collector)"
        );
    }
}
