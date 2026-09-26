//! M28: the `web_search` tool as the CLI builds it: its key is a proxy
//! placeholder, each search is a ledger row, and the daily cap and the
//! kill switch are asked first (docs/web-search.md).

use crate::config::Config;
use crate::ledger::LedgerTag;
use anyhow::Result;
use ferrule_core::LedgerRecord;
use ferrule_proxy::Broker;
use ferrule_sandbox::Egress;
use ferrule_tools::search::SearchCall;
use ferrule_tools::WebSearchTool;
use ferrule_trust::meter::SEARCH_CALL_KIND;
use ferrule_trust::Hub;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a search that passed the gate counts against the cap before
/// it's in the ledger.
const IN_FLIGHT_FOR: Duration = Duration::from_secs(120);

/// Takes a place under `cap` for one search, counting the `recorded` ones
/// and those still `flying`; false when there's none left.
fn reserve(flying: &mut Vec<Instant>, recorded: u64, cap: u64, now: Instant) -> bool {
    flying.retain(|at| now.saturating_duration_since(*at) < IN_FLIGHT_FOR);
    if recorded + flying.len() as u64 >= cap {
        return false;
    }
    flying.push(now);
    true
}

/// The tool, or `None` when `[web_search]` is off or its key can't be
/// reached through the proxy (said once on stderr).
pub fn tool(
    cfg: &Config,
    broker: Option<&Broker>,
    egress: Option<Egress>,
    hub: Arc<Hub>,
    ledger: &LedgerTag,
    tree: &str,
    session_id: String,
) -> Result<Option<WebSearchTool>> {
    let Some(mut settings) = cfg.web_search.settings()? else {
        return Ok(None);
    };
    let egress = match &settings.key_env {
        None => egress,
        Some(var) => {
            let Some((placeholder, egress)) = keyed(broker, var)? else {
                warn_once(&format!(
                    "[web_search]: {var} isn't set in ferrule's environment, so there's no web_search tool"
                ));
                return Ok(None);
            };
            settings.key = Some(placeholder);
            Some(egress)
        }
    };
    let cap = cfg.web_search.max_searches_per_day;
    let price = cfg.web_search.price_per_search_usd;
    let tz = cfg.trust.timezone.clone();
    let gate_hub = hub.clone();
    let gate_tree = tree.to_string();
    // Searches passed but not yet in the ledger: M27 runs read-only calls
    // side by side, and each must count against the cap. One a stopped
    // turn dropped never reaches `record`, so a reservation lapses after
    // `IN_FLIGHT_FOR` (a search times out long before).
    let in_flight: Arc<Mutex<Vec<Instant>>> = Arc::default();
    let gate_flight = in_flight.clone();
    let gate = Arc::new(move || {
        if let Some(stop) = gate_hub.check(&gate_tree, None, false) {
            return Err(stop);
        }
        if cap == 0 {
            return Ok(());
        }
        // An unreadable ledger refuses: the cap can't be told apart from
        // no cap.
        let (day, _) = gate_hub
            .today(None)
            .map_err(|e| format!("the daily search cap can't be checked: {e}"))?;
        if !reserve(
            &mut gate_flight.lock().unwrap(),
            day.searches,
            cap,
            Instant::now(),
        ) {
            return Err(format!(
                "the daily search cap of {cap} is reached ([web_search] max_searches_per_day); it resets at midnight {tz}"
            ));
        }
        Ok(())
    });
    let sink = ledger.sink.clone();
    let (shape, origin) = (ledger.task_shape.clone(), ledger.origin.clone());
    let record = Arc::new(move |call: SearchCall| {
        sink.record(search_record(
            &session_id,
            &shape,
            origin.clone(),
            &call,
            price,
        ));
        let mut flying = in_flight.lock().unwrap();
        if !flying.is_empty() {
            flying.remove(0);
        }
    });
    Ok(Some(
        WebSearchTool::new(settings, egress)
            .with_gate(gate)
            .with_recorder(record),
    ))
}

/// The key's placeholder, and the proxy to send it through.
pub(crate) fn keyed(broker: Option<&Broker>, var: &str) -> Result<Option<(String, Egress)>> {
    let Some(broker) = broker else {
        return Ok(None);
    };
    let Some(info) = broker.secrets().into_iter().find(|s| s.name == var) else {
        return Ok(None);
    };
    Ok(Some((info.placeholder, crate::tool_egress(broker)?)))
}

fn warn_once(msg: &str) {
    static SAID: std::sync::Once = std::sync::Once::new();
    SAID.call_once(|| eprintln!("ferrule: {msg}"));
}

/// One search's ledger row. Every search is one, failed ones included;
/// only a search that answered is charged.
pub fn search_record(
    session_id: &str,
    task_shape: &str,
    origin: Option<String>,
    call: &SearchCall,
    price: Option<f64>,
) -> LedgerRecord {
    let (outcome, error_kind, error_message, cost_usd) = match &call.error {
        None => ("ok", None, None, Some(price.unwrap_or(0.0))),
        Some((kind, msg)) => ("error", Some(kind.clone()), Some(msg.clone()), None),
    };
    LedgerRecord {
        timestamp: chrono::Utc::now().to_rfc3339(),
        session_id: session_id.to_string(),
        task_shape: task_shape.to_string(),
        origin,
        provider: SEARCH_CALL_KIND.into(),
        model: call.provider.into(),
        iteration: 0,
        call_kind: SEARCH_CALL_KIND.into(),
        input_tokens: 0,
        cached_input_tokens: 0,
        cache_write_input_tokens: 0,
        output_tokens: 0,
        tool_calls: 0,
        latency_ms: call.latency_ms,
        outcome: outcome.into(),
        error_kind,
        error_message,
        cost_usd,
        eval: None,
        tree: None,
        route: None,
        speed: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrule_trust::Spend;

    #[test]
    fn searches_in_flight_count_against_the_cap_until_they_lapse() {
        let now = Instant::now();
        let mut flying = Vec::new();
        assert!(reserve(&mut flying, 1, 3, now));
        assert!(reserve(&mut flying, 1, 3, now));
        assert!(!reserve(&mut flying, 1, 3, now), "1 recorded + 2 flying");
        // Recorded: the ledger has it, the reservation goes.
        flying.remove(0);
        assert!(!reserve(&mut flying, 2, 3, now));
        // A dropped search's reservation lapses.
        assert!(reserve(&mut flying, 2, 3, now + IN_FLIGHT_FOR));
    }

    #[test]
    fn a_search_row_counts_as_a_search_and_is_charged_only_when_it_answered() {
        let ok = SearchCall {
            provider: "brave",
            latency_ms: 42,
            error: None,
        };
        let r = search_record("s1", "run", None, &ok, Some(0.005));
        assert_eq!(
            (r.call_kind.as_str(), r.model.as_str()),
            ("web_search", "brave")
        );
        assert_eq!(r.cost_usd, Some(0.005));
        assert_eq!(Spend::of(&r).searches, 1);
        let failed = SearchCall {
            error: Some(("rate_limited".into(), "wait 3 s".into())),
            ..ok
        };
        let r = search_record(
            "s1",
            "gateway",
            Some("telegram".into()),
            &failed,
            Some(0.005),
        );
        assert!(r.is_error() && r.cost_usd.is_none());
        assert_eq!(r.error_kind.as_deref(), Some("rate_limited"));
        assert_eq!(Spend::of(&r).searches, 1);
        let free = search_record("s1", "run", None, &ok, None);
        assert_eq!(free.cost_usd, Some(0.0));
    }
}
