//! The exporter against a mock OTLP collector on 127.0.0.1: what arrives,
//! and that a full queue, a refusing port or a collector that never
//! answers costs the caller nothing.

use ferrule_core::{LedgerRecord, LedgerSink, TraceEvent, TraceLevel};
use ferrule_otel::{read_status, Exporter, Settings};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime};

struct Null;
impl LedgerSink for Null {
    fn record(&self, _: LedgerRecord) {}
}

/// One POST as the collector saw it: its headers and its JSON body.
type Post = (Vec<(String, String)>, Value);

/// Answers every POST with `status`, and hands each body to the test.
fn collector(status: u16) -> (String, mpsc::Receiver<Post>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1/traces", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(conn) = conn else { return };
            let tx = tx.clone();
            std::thread::spawn(move || serve(conn, status, tx));
        }
    });
    (url, rx)
}

fn serve(conn: TcpStream, status: u16, tx: mpsc::Sender<(Vec<(String, String)>, Value)>) {
    let mut reader = BufReader::new(conn.try_clone().unwrap());
    let mut conn = conn;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let mut headers = Vec::new();
        let mut len = 0usize;
        loop {
            let mut h = String::new();
            reader.read_line(&mut h).unwrap();
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                let (k, v) = (k.trim().to_ascii_lowercase(), v.trim().to_string());
                if k == "content-length" {
                    len = v.parse().unwrap();
                }
                headers.push((k, v));
            }
        }
        let mut body = vec![0; len];
        reader.read_exact(&mut body).unwrap();
        let _ = tx.send((headers, serde_json::from_slice(&body).unwrap()));
        let _ = write!(
            conn,
            "HTTP/1.1 {status} X\r\ncontent-length: 2\r\ncontent-type: application/json\r\n\r\n{{}}"
        );
    }
}

/// Accepts connections and never answers.
fn silent_collector() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1/traces", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for conn in listener.incoming().flatten() {
            held.push(conn);
        }
    });
    url
}

/// A port nothing listens on.
fn refused() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}/v1/traces")
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

fn row(session: &str) -> LedgerRecord {
    serde_json::from_value(serde_json::json!({
        "timestamp": chrono_now(),
        "session_id": session,
        "task_shape": "run",
        "provider": "anthropic",
        "model": "claude-x",
        "iteration": 0,
        "input_tokens": 120,
        "cached_input_tokens": 0,
        "output_tokens": 30,
        "tool_calls": 1,
        "latency_ms": 40,
        "outcome": "ok",
        "cost_usd": 0.002,
        "tree": session,
    }))
    .unwrap()
}

fn chrono_now() -> String {
    let d = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap();
    // RFC 3339 without pulling chrono into the test: seconds are enough.
    let secs = d.as_secs();
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (y, m, dd) = civil(days as i64);
    format!(
        "{y:04}-{m:02}-{dd:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

/// Days since 1970-01-01 to a date (Howard Hinnant's algorithm).
fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// One turn with a model call and a tool call.
fn turn(sink: &dyn LedgerSink, session: &str) {
    let content = sink.trace_level() == TraceLevel::Content;
    sink.trace(TraceEvent::TurnStarted {
        session_id: session.into(),
        task_shape: "run".into(),
        origin: None,
        at: SystemTime::now(),
        goal: content.then(|| "read .env; the key is sk-live-SECRET".into()),
    });
    if content {
        sink.trace(TraceEvent::CallContent {
            session_id: session.into(),
            text: "the key is sk-live-SECRET".into(),
        });
    }
    sink.record(row(session));
    sink.trace(TraceEvent::ToolCall {
        session_id: session.into(),
        id: "call_1".into(),
        name: "shell".into(),
        ok: true,
        started: SystemTime::now(),
        elapsed: Duration::from_millis(2),
        arguments: content.then(|| "{\"cmd\":\"cat .env\"}".into()),
        result: content.then(|| "KEY=sk-live-SECRET".into()),
    });
    sink.trace(TraceEvent::TurnFinished {
        session_id: session.into(),
        at: SystemTime::now(),
        ok: true,
        incomplete: None,
    });
}

fn spans(body: &Value) -> Vec<Value> {
    body["resourceSpans"][0]["scopeSpans"][0]["spans"]
        .as_array()
        .unwrap()
        .clone()
}

fn attr<'a>(span: &'a Value, key: &str) -> Option<&'a Value> {
    span["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["key"] == key)
        .map(|a| &a["value"])
}

fn named<'a>(spans: &'a [Value], name: &str) -> &'a Value {
    spans
        .iter()
        .find(|s| s["name"] == name)
        .unwrap_or_else(|| panic!("no {name} span"))
}

#[test]
fn a_turn_arrives_as_a_span_tree_without_content() {
    let (url, rx) = collector(200);
    let dir = tempfile::tempdir().unwrap();
    let status_path = dir.path().join("telemetry/status.json");
    let mut s = Settings::new(url.clone(), client());
    s.headers = vec![("authorization".into(), "Bearer FERRULE_PH_x".into())];
    s.status_path = Some(status_path.clone());
    let exporter = Exporter::start(s).unwrap();
    let sink = exporter.sink(Arc::new(Null), "sess-1");
    assert_eq!(sink.trace_level(), TraceLevel::Spans);
    turn(sink.as_ref(), "sess-1");
    assert!(exporter.shutdown(Duration::from_secs(3)));

    let mut all = Vec::new();
    let mut headers = Vec::new();
    while let Ok((h, body)) = rx.recv_timeout(Duration::from_millis(200)) {
        assert_eq!(
            body["resourceSpans"][0]["resource"]["attributes"][0]["value"]["stringValue"],
            "ferrule"
        );
        headers = h;
        all.extend(spans(&body));
    }
    assert!(headers
        .iter()
        .any(|(k, v)| k == "authorization" && v == "Bearer FERRULE_PH_x"));
    assert_eq!(all.len(), 4, "session, turn, chat, tool: {all:#?}");
    let session = named(&all, "ferrule.session");
    let turn = named(&all, "ferrule.turn");
    let chat = named(&all, "chat claude-x");
    let tool = named(&all, "execute_tool shell");
    assert!(session.get("parentSpanId").is_none());
    assert_eq!(turn["parentSpanId"], session["spanId"]);
    assert_eq!(chat["parentSpanId"], turn["spanId"]);
    assert_eq!(tool["parentSpanId"], turn["spanId"]);
    assert!(all.iter().all(|s| s["traceId"] == session["traceId"]));
    assert_eq!(chat["kind"], 3);
    assert_eq!(
        attr(chat, "gen_ai.operation.name").unwrap()["stringValue"],
        "chat"
    );
    assert_eq!(
        attr(chat, "gen_ai.provider.name").unwrap()["stringValue"],
        "anthropic"
    );
    assert_eq!(
        attr(chat, "gen_ai.usage.input_tokens").unwrap()["intValue"],
        "120"
    );
    assert_eq!(
        attr(chat, "ferrule.cost_usd").unwrap()["doubleValue"],
        0.002
    );
    assert_eq!(
        attr(turn, "gen_ai.usage.output_tokens").unwrap()["intValue"],
        "30"
    );
    assert_eq!(
        attr(tool, "gen_ai.tool.name").unwrap()["stringValue"],
        "shell"
    );
    let text = serde_json::to_string(&all).unwrap();
    assert!(
        !text.contains("messages") && !text.contains("arguments"),
        "{text}"
    );

    let status = read_status(&status_path).unwrap();
    assert_eq!((status.exported, status.dropped, status.failed), (4, 0, 0));
    assert_eq!(status.endpoint, url);
    assert!(!std::fs::read_to_string(&status_path)
        .unwrap()
        .contains("FERRULE_PH_x"));
}

#[test]
fn content_is_opt_in_and_goes_through_the_scrubber() {
    let (url, rx) = collector(200);
    let mut s = Settings::new(url, client());
    s.content = true;
    s.scrub = Some(Arc::new(|t: &str| {
        t.replace("sk-live-SECRET", "[redacted]")
    }));
    let exporter = Exporter::start(s).unwrap();
    let sink = exporter.sink(Arc::new(Null), "sess-2");
    assert_eq!(sink.trace_level(), TraceLevel::Content);
    turn(sink.as_ref(), "sess-2");
    assert!(exporter.shutdown(Duration::from_secs(3)));
    let mut all = Vec::new();
    while let Ok((_, body)) = rx.recv_timeout(Duration::from_millis(200)) {
        all.extend(spans(&body));
    }
    let text = serde_json::to_string(&all).unwrap();
    assert!(!text.contains("sk-live-SECRET"), "{text}");
    let chat = named(&all, "chat claude-x");
    assert!(attr(chat, "gen_ai.output.messages").unwrap()["stringValue"]
        .as_str()
        .unwrap()
        .contains("[redacted]"));
    let tool = named(&all, "execute_tool shell");
    assert!(attr(tool, "gen_ai.tool.call.arguments").is_some());
    let turn = named(&all, "ferrule.turn");
    assert!(attr(turn, "gen_ai.input.messages").is_some());
}

#[test]
fn a_full_queue_drops_and_counts_without_blocking() {
    let url = silent_collector();
    let mut s = Settings::new(url, client());
    s.queue = 4;
    s.batch = 1;
    let exporter = Exporter::start(s).unwrap();
    let sink = exporter.sink(Arc::new(Null), "sess-3");
    let t = Instant::now();
    for _ in 0..500 {
        sink.record(row("sess-3"));
    }
    assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
    assert!(exporter.status().dropped >= 400, "{:?}", exporter.status());
    let t = Instant::now();
    exporter.shutdown(Duration::from_millis(300));
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
}

#[test]
fn a_refusing_port_costs_the_turn_nothing_and_is_counted() {
    let mut s = Settings::new(refused(), client());
    s.interval = Duration::from_millis(20);
    let exporter = Exporter::start(s).unwrap();
    let sink = exporter.sink(Arc::new(Null), "sess-4");
    let t = Instant::now();
    for i in 0..20 {
        turn(sink.as_ref(), &format!("sess-4-{i}"));
    }
    assert!(
        t.elapsed() < Duration::from_millis(500),
        "{:?}",
        t.elapsed()
    );
    let t = Instant::now();
    exporter.shutdown(Duration::from_secs(3));
    assert!(t.elapsed() < Duration::from_secs(4));
    let status = exporter.status();
    assert!(status.failed > 0, "{status:?}");
    assert_eq!(status.exported, 0);
    assert!(status.last_error.is_some());
}

#[test]
fn a_collector_that_never_answers_is_cut_off_at_the_shutdown_deadline() {
    let mut s = Settings::new(silent_collector(), client());
    s.interval = Duration::from_millis(10);
    let exporter = Exporter::start(s).unwrap();
    let sink = exporter.sink(Arc::new(Null), "sess-5");
    let t = Instant::now();
    turn(sink.as_ref(), "sess-5");
    // The export thread is now stuck in a request for up to 10 s.
    std::thread::sleep(Duration::from_millis(100));
    turn(sink.as_ref(), "sess-5b");
    assert!(t.elapsed() < Duration::from_millis(400));
    let t = Instant::now();
    assert!(!exporter.shutdown(Duration::from_millis(500)));
    let took = t.elapsed();
    assert!(
        took >= Duration::from_millis(450) && took < Duration::from_millis(1500),
        "{took:?}"
    );
    // After shutdown, rows are dropped and counted, not queued.
    let before = exporter.status().dropped;
    sink.record(row("late"));
    assert_eq!(exporter.status().dropped, before + 1);
}

#[test]
fn a_collector_error_status_is_counted_as_failed() {
    let (url, rx) = collector(503);
    let exporter = Exporter::start(Settings::new(url, client())).unwrap();
    let sink = exporter.sink(Arc::new(Null), "sess-6");
    turn(sink.as_ref(), "sess-6");
    exporter.shutdown(Duration::from_secs(3));
    assert!(rx.recv_timeout(Duration::from_secs(1)).is_ok());
    let status = exporter.status();
    assert_eq!(status.exported, 0);
    assert_eq!(status.failed, 4);
    assert!(status.last_error.unwrap().contains("503"));
}

/// A real collector: `FERRULE_OTEL_LIVE_ENDPOINT=http://127.0.0.1:4318 cargo
/// test -p ferrule-otel --test export -- --ignored live`, with e.g. Jaeger
/// (`docker run -p 16686:16686 -p 4318:4318 jaegertracing/all-in-one`)
/// running. `FERRULE_OTEL_LIVE_HEADER=name=value` adds one header for a
/// hosted backend. The trace then shows up as service `ferrule-live-test`.
#[test]
#[ignore = "live: needs FERRULE_OTEL_LIVE_ENDPOINT, a running OTLP/HTTP collector"]
fn live_a_real_collector_takes_the_spans() {
    let base = std::env::var("FERRULE_OTEL_LIVE_ENDPOINT")
        .expect("set FERRULE_OTEL_LIVE_ENDPOINT to the collector's OTLP/HTTP base URL");
    let url = format!("{}/v1/traces", base.trim_end_matches('/'));
    let mut settings = Settings::new(url, client());
    settings.service_name = "ferrule-live-test".into();
    if let Ok(h) = std::env::var("FERRULE_OTEL_LIVE_HEADER") {
        let (k, v) = h
            .split_once('=')
            .expect("FERRULE_OTEL_LIVE_HEADER is name=value");
        settings.headers.push((k.into(), v.into()));
    }
    let exporter = Exporter::start(settings).unwrap();
    let sink = exporter.sink(Arc::new(Null), "live");
    turn(sink.as_ref(), "live");
    assert!(
        exporter.shutdown(Duration::from_secs(10)),
        "the flush timed out"
    );
    let status = exporter.status();
    assert_eq!(status.failed, 0, "{:?}", status.last_error);
    assert!(status.exported >= 3, "{status:?}");
}
