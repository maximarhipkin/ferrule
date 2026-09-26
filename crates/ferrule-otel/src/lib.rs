//! M33: the agent's work as OpenTelemetry traces, exported as OTLP/JSON
//! over HTTP (docs/otel.md).
//!
//! [`OtelSink`] sits on the ledger seam, inside the trust sink (so rows
//! arrive stamped with their tree and cost): it hands each row and trace
//! event to the [`Exporter`]'s bounded queue with `try_send` and returns.
//! One thread per process drains the queue, turns rows and events into
//! spans ([`trace::Tracer`]), and POSTs them in batches. A full queue drops
//! and counts; a slow or dead collector costs that thread its time, never a
//! turn's.
//!
//! The encoder is hand-rolled rather than the `opentelemetry` crates: the
//! OTLP/JSON mapping is a small, stable document, and the SDK would bring
//! its own runtime, batch processor and HTTP stack (and gRPC, protobuf)
//! next to the ones the workspace already has (docs/m33-ops.md §2.4).

pub mod span;
pub mod trace;

pub use span::{Attr, Span};
pub use trace::{Scrub, Tracer};

use ferrule_core::{LedgerRecord, LedgerSink, TraceEvent, TraceLevel};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How the exporter runs.
pub struct Settings {
    /// The full URL spans are POSTed to (`…/v1/traces`).
    pub endpoint: String,
    /// Extra request headers, values already expanded.
    pub headers: Vec<(String, String)>,
    /// Put prompts, replies and tool arguments/results on spans (scrubbed).
    pub content: bool,
    pub service_name: String,
    pub service_version: String,
    /// Built by the caller (the egress-policy client).
    pub client: reqwest::Client,
    /// Applied to all content before it goes on a span.
    pub scrub: Option<Scrub>,
    /// Where the counters are written, for `ferrule doctor`.
    pub status_path: Option<PathBuf>,
    /// Rows and events waiting for the export thread.
    pub queue: usize,
    /// Spans per request.
    pub batch: usize,
    /// Longest a span waits before its batch is sent.
    pub interval: Duration,
    /// Per request.
    pub timeout: Duration,
}

impl Settings {
    pub fn new(endpoint: impl Into<String>, client: reqwest::Client) -> Self {
        Self {
            endpoint: endpoint.into(),
            headers: Vec::new(),
            content: false,
            service_name: "ferrule".into(),
            service_version: env!("CARGO_PKG_VERSION").into(),
            client,
            scrub: None,
            status_path: None,
            queue: 2048,
            batch: 512,
            interval: Duration::from_secs(2),
            timeout: Duration::from_secs(10),
        }
    }
}

/// Written at most every [`STATUS_EVERY`] and at shutdown.
pub const STATUS_EVERY: Duration = Duration::from_secs(10);
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

enum Msg {
    Row(Box<LedgerRecord>, Arc<str>),
    Event(TraceEvent, Arc<str>),
}

#[derive(Default)]
struct Stats {
    /// Rows and events the queue refused (full, or the exporter stopped).
    dropped: AtomicU64,
    exported: AtomicU64,
    /// Spans a collector refused or never answered for, and spans thrown
    /// away while backing off.
    failed: AtomicU64,
    last_error: Mutex<Option<String>>,
    last_ok: Mutex<Option<String>>,
}

/// The counters, as `status.json` holds them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub endpoint: String,
    pub exported: u64,
    pub dropped: u64,
    pub failed: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// RFC 3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ok: Option<String>,
    /// RFC 3339: when this was written.
    #[serde(default)]
    pub updated: String,
    #[serde(default)]
    pub pid: u32,
}

/// Reads what an exporter last wrote to `path`.
pub fn read_status(path: &Path) -> Option<Status> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// One per process: the queue, the thread that drains it, the counters.
pub struct Exporter {
    tx: Mutex<Option<SyncSender<Msg>>>,
    done: Mutex<Option<Receiver<()>>>,
    /// Set by [`Exporter::shutdown`]: the export thread gives up sending by
    /// then.
    deadline: Arc<Mutex<Option<Instant>>>,
    stats: Arc<Stats>,
    level: TraceLevel,
    endpoint: String,
}

impl Exporter {
    /// Starts the export thread.
    pub fn start(settings: Settings) -> std::io::Result<Arc<Self>> {
        let (tx, rx) = mpsc::sync_channel(settings.queue.max(1));
        let (done_tx, done_rx) = mpsc::channel();
        let stats = Arc::new(Stats::default());
        let deadline = Arc::new(Mutex::new(None));
        let level = if settings.content {
            TraceLevel::Content
        } else {
            TraceLevel::Spans
        };
        let endpoint = settings.endpoint.clone();
        let worker = Worker {
            tracer: Tracer::new(settings.content, settings.scrub.clone()),
            settings,
            stats: stats.clone(),
            deadline: deadline.clone(),
            pending: Vec::new(),
            oldest: None,
            retry_at: None,
            backoff: BACKOFF_MIN,
            status_at: None,
        };
        std::thread::Builder::new()
            .name("ferrule-otel".into())
            .spawn(move || {
                worker.run(rx);
                let _ = done_tx.send(());
            })?;
        Ok(Arc::new(Self {
            tx: Mutex::new(Some(tx)),
            done: Mutex::new(Some(done_rx)),
            deadline,
            stats,
            level,
            endpoint,
        }))
    }

    pub fn level(&self) -> TraceLevel {
        self.level
    }

    fn send(&self, msg: Msg) {
        let tx = self.tx.lock().unwrap_or_else(|e| e.into_inner());
        let full = match tx.as_ref() {
            Some(tx) => match tx.try_send(msg) {
                Ok(()) => false,
                Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => true,
            },
            None => true,
        };
        if full {
            self.stats.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// A sink for one run tree, in front of `inner`.
    pub fn sink(self: &Arc<Self>, inner: Arc<dyn LedgerSink>, tree: &str) -> Arc<OtelSink> {
        Arc::new(OtelSink {
            inner,
            exporter: self.clone(),
            tree: tree.into(),
        })
    }

    /// The counters now.
    pub fn status(&self) -> Status {
        self.stats.snapshot(&self.endpoint)
    }

    /// Closes what's open, sends what's queued, and waits for the export
    /// thread — but no longer than `deadline`. Returns whether it finished
    /// in time. Later rows are dropped (and counted).
    pub fn shutdown(&self, deadline: Duration) -> bool {
        let until = Instant::now() + deadline;
        *self.deadline.lock().unwrap_or_else(|e| e.into_inner()) = Some(until);
        // Dropping the sender is the signal: the thread drains the queue,
        // then sees the channel close.
        self.tx.lock().unwrap_or_else(|e| e.into_inner()).take();
        let Some(done) = self.done.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return true;
        };
        let left = until.saturating_duration_since(Instant::now());
        !matches!(done.recv_timeout(left), Err(RecvTimeoutError::Timeout))
    }
}

impl Stats {
    fn snapshot(&self, endpoint: &str) -> Status {
        Status {
            endpoint: endpoint.to_string(),
            exported: self.exported.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            last_error: self
                .last_error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            last_ok: self
                .last_ok
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            updated: chrono::Utc::now().to_rfc3339(),
            pid: std::process::id(),
        }
    }
}

/// The ledger seam: every row and trace event goes to the exporter, then
/// on to `inner`.
pub struct OtelSink {
    inner: Arc<dyn LedgerSink>,
    exporter: Arc<Exporter>,
    tree: Arc<str>,
}

impl LedgerSink for OtelSink {
    fn record(&self, record: LedgerRecord) {
        self.exporter
            .send(Msg::Row(Box::new(record.clone()), self.tree.clone()));
        self.inner.record(record);
    }

    fn trace_level(&self) -> TraceLevel {
        self.exporter.level.max(self.inner.trace_level())
    }

    fn trace(&self, event: TraceEvent) {
        if self.inner.trace_level() == TraceLevel::Off {
            self.exporter.send(Msg::Event(event, self.tree.clone()));
        } else {
            self.exporter
                .send(Msg::Event(event.clone(), self.tree.clone()));
            self.inner.trace(event);
        }
    }
}

struct Worker {
    settings: Settings,
    tracer: Tracer,
    stats: Arc<Stats>,
    deadline: Arc<Mutex<Option<Instant>>>,
    pending: Vec<Span>,
    /// When the oldest pending span was made.
    oldest: Option<Instant>,
    /// Backing off after a failed send: nothing is sent before this.
    retry_at: Option<Instant>,
    backoff: Duration,
    status_at: Option<Instant>,
}

impl Worker {
    fn run(mut self, rx: Receiver<Msg>) {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                self.error(format!("no runtime for the exporter: {e}"));
                self.write_status();
                return;
            }
        };
        loop {
            let wait = self
                .oldest
                .map(|o| self.settings.interval.saturating_sub(o.elapsed()))
                .unwrap_or(STATUS_EVERY);
            match rx.recv_timeout(wait) {
                Ok(msg) => {
                    let spans = match msg {
                        Msg::Row(r, tree) => self.tracer.row(&r, &tree),
                        Msg::Event(e, tree) => self.tracer.event(&e, &tree),
                    };
                    self.add(spans);
                    if self.pending.len() >= self.settings.batch {
                        self.flush(&rt);
                    }
                }
                Err(RecvTimeoutError::Timeout) => self.flush(&rt),
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if self.status_at.is_none_or(|t| t.elapsed() >= STATUS_EVERY) {
                self.write_status();
            }
        }
        let spans = self.tracer.close_all("shutdown");
        self.add(spans);
        // At shutdown a backoff doesn't hold the last batch back.
        self.retry_at = None;
        while !self.pending.is_empty() {
            let before = self.pending.len();
            self.flush(&rt);
            if self.pending.len() >= before {
                break;
            }
        }
        self.write_status();
    }

    fn add(&mut self, spans: Vec<Span>) {
        if spans.is_empty() {
            return;
        }
        self.oldest.get_or_insert_with(Instant::now);
        self.pending.extend(spans);
    }

    /// Sends up to one batch of what's pending, unless backing off; while
    /// backing off, a full batch is thrown away (and counted) so pending
    /// never grows past one batch.
    fn flush(&mut self, rt: &tokio::runtime::Runtime) {
        if self.pending.is_empty() {
            self.oldest = None;
            return;
        }
        let n = self.pending.len().min(self.settings.batch);
        let backing_off = self.retry_at.is_some_and(|t| Instant::now() < t);
        let deadline = *self.deadline.lock().unwrap_or_else(|e| e.into_inner());
        let left = deadline.map(|d| d.saturating_duration_since(Instant::now()));
        if backing_off || left == Some(Duration::ZERO) {
            if self.pending.len() >= self.settings.batch || deadline.is_some() {
                self.pending.drain(..n);
                self.stats.failed.fetch_add(n as u64, Ordering::Relaxed);
            }
            if self.pending.is_empty() {
                self.oldest = None;
            }
            return;
        }
        let batch: Vec<Span> = self.pending.drain(..n).collect();
        self.oldest = (!self.pending.is_empty()).then(Instant::now);
        let timeout = left.map_or(self.settings.timeout, |l| l.min(self.settings.timeout));
        match rt.block_on(self.post(&batch, timeout)) {
            Ok(()) => {
                self.stats.exported.fetch_add(n as u64, Ordering::Relaxed);
                *self.stats.last_ok.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(chrono::Utc::now().to_rfc3339());
                self.retry_at = None;
                self.backoff = BACKOFF_MIN;
            }
            Err(e) => {
                self.stats.failed.fetch_add(n as u64, Ordering::Relaxed);
                tracing::debug!("OTLP export failed: {e}");
                self.error(e);
                self.retry_at = Some(Instant::now() + self.backoff);
                self.backoff = (self.backoff * 2).min(BACKOFF_MAX);
            }
        }
    }

    async fn post(&self, batch: &[Span], timeout: Duration) -> Result<(), String> {
        let body = span::encode(
            &self.settings.service_name,
            &self.settings.service_version,
            batch,
        );
        let mut req = self
            .settings
            .client
            .post(&self.settings.endpoint)
            .timeout(timeout)
            .json(&body);
        for (k, v) in &self.settings.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let resp = req.send().await.map_err(|e| {
            if e.is_timeout() {
                format!("no answer from the collector in {}s", timeout.as_secs_f32())
            } else if e.is_connect() {
                "can't connect to the collector".to_string()
            } else {
                format!("request failed: {}", without_url(&e))
            }
        })?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        Err(format!("the collector answered {status}"))
    }

    fn error(&self, e: String) {
        *self
            .stats
            .last_error
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(e);
    }

    /// Replaces `status.json` in one step (tmp + rename).
    fn write_status(&mut self) {
        self.status_at = Some(Instant::now());
        let Some(path) = &self.settings.status_path else {
            return;
        };
        let status = self.stats.snapshot(&self.settings.endpoint);
        let Ok(text) = serde_json::to_string_pretty(&status) else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        if std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// A reqwest error's text without the URL (the endpoint is in the status
/// already, and a URL can carry a token in its query).
fn without_url(e: &reqwest::Error) -> String {
    let mut text = e.to_string();
    if let Some(url) = e.url() {
        text = text.replace(url.as_str(), "the endpoint");
    }
    text
}
