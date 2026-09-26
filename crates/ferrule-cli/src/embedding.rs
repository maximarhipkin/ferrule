//! M30: the embedder `[memory]` asks for, as the CLI builds it. An
//! endpoint's key is a proxy placeholder and each of its requests is a
//! ledger row; any failure means keyword recall, said once in the log and
//! never to the user (docs/m30-vector-recall.md §2.5).

use crate::config::{Config, EmbedderChoice, EndpointSettings};
use crate::ledger::LedgerTag;
use anyhow::{bail, Result};
use ferrule_core::{LedgerRecord, LedgerSink};
use ferrule_embed::{EmbedError, Embedded, Embedder, OpenAiEmbedder, OpenAiSettings, Purpose};
use ferrule_memory::{Hybrid, MemoryStore, QueryVector};
use ferrule_proxy::Broker;
use ferrule_sandbox::Egress;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The ledger's `call_kind` for one embeddings request.
pub const EMBED_CALL_KIND: &str = "embedding";

/// How long a turn waits for a vector before recalling by keyword.
pub const IN_SESSION_TIMEOUT: Duration = Duration::from_secs(5);

/// Stale live facts re-embedded after a session-start recall.
pub const LAZY_BATCH: usize = 32;

/// Texts are clipped to this many characters before embedding.
const MAX_CHARS: usize = 4_000;

/// An embedder and how recall merges with it.
pub struct Embedding {
    embedder: Arc<dyn Embedder>,
    pub hybrid: Hybrid,
    ledger: Option<Recorder>,
    /// Requests a minute for `reindex`; 0: no limit.
    pub max_requests_per_minute: u32,
}

struct Recorder {
    sink: Arc<dyn LedgerSink>,
    session_id: String,
    task_shape: String,
    origin: Option<String>,
    model: String,
    price_input_per_mtok: Option<f64>,
}

impl Embedding {
    pub fn new(embedder: Arc<dyn Embedder>, hybrid: Hybrid) -> Self {
        Self {
            embedder,
            hybrid,
            ledger: None,
            max_requests_per_minute: 0,
        }
    }

    /// The id stored with every vector.
    pub fn model(&self) -> &str {
        self.embedder.model().as_str()
    }

    /// One call, with its ledger row when the backend is a paid endpoint.
    pub async fn embed(&self, texts: &[String], purpose: Purpose) -> Result<Embedded, EmbedError> {
        let clipped: Vec<String> = texts
            .iter()
            .map(|t| t.chars().take(MAX_CHARS).collect())
            .collect();
        let started = Instant::now();
        let result = self.embedder.embed(&clipped, purpose).await;
        if let Some(r) = &self.ledger {
            r.sink.record(embed_record(
                r,
                started.elapsed().as_millis() as u64,
                &result,
            ));
        }
        result
    }

    /// [`Embedding::embed`] inside a turn: at most [`IN_SESSION_TIMEOUT`],
    /// and `None` on any failure, which is logged once per cause.
    pub async fn try_embed(&self, texts: &[String], purpose: Purpose) -> Option<Vec<Vec<f32>>> {
        match tokio::time::timeout(IN_SESSION_TIMEOUT, self.embed(texts, purpose)).await {
            Ok(Ok(e)) => Some(e.vectors),
            Ok(Err(e)) => {
                warn_once(e.kind(), &e.to_string());
                None
            }
            Err(_) => {
                warn_once(
                    "timeout",
                    &format!("no vector within {}s", IN_SESSION_TIMEOUT.as_secs()),
                );
                None
            }
        }
    }

    /// One text's vector, or `None`.
    pub async fn try_one(&self, text: &str, purpose: Purpose) -> Option<Vec<f32>> {
        self.try_embed(&[text.to_string()], purpose)
            .await?
            .into_iter()
            .next()
    }

    /// Embeds up to [`LAZY_BATCH`] live facts that have no vector from this
    /// model yet, newest first. Errors are logged once and dropped.
    pub async fn catch_up(&self, db: &Path) {
        let model = self.model().to_string();
        let path = db.to_path_buf();
        let Ok(Ok(rows)) = tokio::task::spawn_blocking(move || {
            MemoryStore::open(&path).and_then(|s| s.stale_live(&model, LAZY_BATCH))
        })
        .await
        else {
            return;
        };
        if rows.is_empty() {
            return;
        }
        let texts: Vec<String> = rows.iter().map(|r| r.1.clone()).collect();
        let Some(vectors) = self.try_embed(&texts, Purpose::Document).await else {
            return;
        };
        let model = self.model().to_string();
        let path = db.to_path_buf();
        let _ = tokio::task::spawn_blocking(move || {
            let store = MemoryStore::open(&path)?;
            for ((id, content), v) in rows.iter().zip(&vectors) {
                store.set_embedding(*id, content, &model, v)?;
            }
            Ok::<_, ferrule_memory::MemoryError>(())
        })
        .await;
    }
}

/// A vector by `model`, as the store takes it.
pub fn query_vector<'a>(model: &'a str, vector: &'a [f32]) -> QueryVector<'a> {
    QueryVector { model, vector }
}

fn embed_record(
    r: &Recorder,
    latency_ms: u64,
    result: &Result<Embedded, EmbedError>,
) -> LedgerRecord {
    let (outcome, tokens, error_kind, error_message, cost_usd) = match result {
        Ok(e) => (
            "ok",
            e.tokens,
            None,
            None,
            r.price_input_per_mtok
                .map(|p| e.tokens as f64 * p / 1_000_000.0),
        ),
        Err(e) => (
            "error",
            0,
            Some(e.kind().to_string()),
            Some(e.to_string()),
            None,
        ),
    };
    LedgerRecord {
        timestamp: chrono::Utc::now().to_rfc3339(),
        session_id: r.session_id.clone(),
        task_shape: r.task_shape.clone(),
        origin: r.origin.clone(),
        provider: EMBED_CALL_KIND.into(),
        model: r.model.clone(),
        iteration: 0,
        call_kind: EMBED_CALL_KIND.into(),
        input_tokens: tokens,
        cached_input_tokens: 0,
        cache_write_input_tokens: 0,
        output_tokens: 0,
        tool_calls: 0,
        latency_ms,
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

/// Logs `message` once per process per `cause`: an embedder that is down
/// for a whole session is one line, not one per recall.
fn warn_once(cause: &str, message: &str) {
    static SAID: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
    if SAID
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(cause.to_string())
    {
        tracing::warn!("memory: {message}; recalling by keyword");
    }
}

/// Where the ledger rows of an endpoint's requests go.
pub struct LedgerTarget<'a> {
    pub tag: &'a LedgerTag,
    pub session_id: String,
}

/// `[memory]`'s embedder, or `None` for keyword recall: off, the local
/// model not downloaded (or not in this build), or the endpoint's key not
/// reachable through the proxy. Only a config that doesn't validate is an
/// error, and `Config::load` has refused that already.
pub fn build(
    cfg: &Config,
    broker: Option<&Broker>,
    egress: Option<Egress>,
    ledger: Option<LedgerTarget<'_>>,
) -> Result<Option<Embedding>> {
    let hybrid = cfg.memory.hybrid()?;
    match cfg.memory.choice(&cfg.providers)? {
        EmbedderChoice::Off => Ok(None),
        EmbedderChoice::Local => Ok(local(&crate::config::data_dir()?)
            .map_err(|why| warn_once("local", &why))
            .ok()
            .map(|e| Embedding::new(e, hybrid))),
        EmbedderChoice::Endpoint(s) => {
            let Some(embedder) = endpoint(&s, broker, egress)? else {
                return Ok(None);
            };
            let mut emb = Embedding::new(Arc::new(embedder), hybrid);
            emb.max_requests_per_minute = s.max_requests_per_minute;
            emb.ledger = ledger.map(|l| Recorder {
                sink: l.tag.sink.clone(),
                session_id: l.session_id,
                task_shape: l.tag.task_shape.clone(),
                origin: l.tag.origin.clone(),
                model: s.model.clone(),
                price_input_per_mtok: s.price_input_per_mtok,
            });
            Ok(Some(emb))
        }
    }
}

/// The local model if its files are there; nothing is read until the
/// first embed, which checks them.
#[cfg(feature = "local-embed")]
pub fn local(data_dir: &Path) -> Result<Arc<dyn Embedder>, String> {
    use ferrule_embed::download::{presence, Presence, POTION_MULTILINGUAL};
    let spec = POTION_MULTILINGUAL;
    let dir = spec.dir(data_dir);
    match presence(&spec, &dir) {
        Presence::Present => Ok(Arc::new(ferrule_embed::local::StaticEmbedder::new(
            spec, dir,
        ))),
        _ => Err(format!(
            "the local embedding model isn't downloaded ({}); run `ferrule memory model download`",
            dir.display()
        )),
    }
}

#[cfg(not(feature = "local-embed"))]
pub fn local(_data_dir: &Path) -> Result<Arc<dyn Embedder>, String> {
    Err("this ferrule was built without the local embedder (feature `local-embed`)".into())
}

fn endpoint(
    s: &EndpointSettings,
    broker: Option<&Broker>,
    egress: Option<Egress>,
) -> Result<Option<OpenAiEmbedder>> {
    let (key, egress) = match &s.key_env {
        None => (None, egress),
        Some(var) => {
            let Some((placeholder, egress)) = crate::web_search::keyed(broker, var)? else {
                warn_once(
                    "key",
                    &format!("[memory]: {var} isn't set in ferrule's environment"),
                );
                return Ok(None);
            };
            (Some(placeholder), Some(egress))
        }
    };
    let client = ferrule_tools::egress::client_builder(egress.as_ref())?.build()?;
    Ok(Some(OpenAiEmbedder::new(
        OpenAiSettings {
            base_url: s.base_url.clone(),
            model: s.model.clone(),
            dim: s.dim,
            send_dimensions: s.send_dimensions,
            key,
            key_env: s.key_env.clone(),
            timeout: s.timeout,
        },
        client,
    )))
}

/// What `reindex` did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Reindexed {
    pub embedded: usize,
    pub requests: usize,
    pub waits: usize,
}

/// Consecutive 429s before `reindex` gives up (it resumes next time).
const MAX_RETRIES: usize = 6;

/// Embeds every row without a vector from `emb`'s model, `batch` at a
/// time, in id order. Each batch is stored before the next is asked for,
/// so an interrupted run loses at most one batch and the next run starts
/// from the rows still stale. Keeps to `max_requests_per_minute` and waits
/// out a 429 (its `Retry-After`, else a doubling backoff).
pub async fn reindex(
    db: &Path,
    emb: &Embedding,
    batch: usize,
    mut progress: impl FnMut(usize, usize),
) -> Result<Reindexed> {
    let store = MemoryStore::open(db)?;
    let model = emb.model().to_string();
    let total = store.embedding_counts(&model)?.stale;
    let gap = match emb.max_requests_per_minute {
        0 => Duration::ZERO,
        n => Duration::from_secs(60) / n,
    };
    let mut done = Reindexed::default();
    let mut after = 0;
    let mut last: Option<Instant> = None;
    loop {
        let rows = store.stale_rows(&model, after, batch.max(1))?;
        let Some(&(last_id, _)) = rows.last() else {
            break;
        };
        let texts: Vec<String> = rows.iter().map(|r| r.1.clone()).collect();
        let mut retries = 0;
        let mut backoff = Duration::from_secs(5);
        let vectors = loop {
            if let Some(at) = last {
                tokio::time::sleep(gap.saturating_sub(at.elapsed())).await;
            }
            last = Some(Instant::now());
            done.requests += 1;
            match emb.embed(&texts, Purpose::Document).await {
                Ok(e) => break e.vectors,
                Err(EmbedError::RateLimited { retry_after, .. }) if retries < MAX_RETRIES => {
                    retries += 1;
                    done.waits += 1;
                    let wait = retry_after.map_or(backoff, Duration::from_secs);
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                    tokio::time::sleep(wait).await;
                }
                Err(e) => bail!(
                    "{e}. {} of {total} rows embedded; `ferrule memory reindex` picks up from there",
                    done.embedded
                ),
            }
        };
        for ((id, content), v) in rows.iter().zip(&vectors) {
            // A row changed since it was read stays stale for next time.
            if store.set_embedding(*id, content, &model, v)? {
                done.embedded += 1;
            }
        }
        after = last_id;
        progress(done.embedded, total);
    }
    Ok(done)
}

/// The embedder for a `ferrule memory` command, its paid calls on the
/// ledger like a session's. `None`: keyword only.
async fn for_cli(cfg: &Config) -> Result<Option<Embedding>> {
    let (broker, egress) = cli_egress(cfg)?;
    let sink = crate::ledger::build_sink(cfg);
    let tag = crate::ledger::LedgerTag::new(&sink, "memory", None);
    build(
        cfg,
        broker,
        egress,
        tag.as_ref().map(|tag| LedgerTarget {
            tag,
            session_id: "memory".into(),
        }),
    )
}

/// The credential proxy, and the egress through it, for a command run
/// outside a session: without `[secrets]`, a direct connection.
fn cli_egress(cfg: &Config) -> Result<(Option<&'static Broker>, Option<Egress>)> {
    let broker = crate::shared_broker(cfg)?;
    let egress = match broker {
        Some(b) => Some(Egress {
            proxy_url: b.proxy_url(),
            ca_cert_pem: std::fs::read_to_string(b.ca_cert_path())?,
        }),
        None => None,
    };
    Ok((broker, egress))
}

/// `ferrule memory search`: by meaning too when there is an embedder.
pub async fn search(
    store: &MemoryStore,
    query: &str,
    limit: usize,
) -> Result<Vec<ferrule_memory::Memory>> {
    let emb = match Config::load() {
        Ok((cfg, _)) => for_cli(&cfg).await?,
        Err(_) => None,
    };
    let Some(emb) = emb else {
        return Ok(store.recall(query, limit)?);
    };
    let v = emb.try_one(query, Purpose::Query).await;
    let qv = v.as_deref().map(|v| query_vector(emb.model(), v));
    Ok(store.recall_hybrid(query, qv, limit, &emb.hybrid)?)
}

/// `ferrule memory reindex`.
pub async fn reindex_cmd(batch: usize) -> Result<()> {
    let (cfg, _) = Config::load()?;
    if matches!(cfg.memory.choice(&cfg.providers)?, EmbedderChoice::Off) {
        bail!("[memory] embedder is off, so there is nothing to embed with; set it to \"local\" or \"openai\" (`ferrule setup`)");
    }
    let Some(emb) = for_cli(&cfg).await? else {
        bail!("the [memory] embedder isn't available (the warning above says why; `ferrule doctor` too)");
    };
    let db = crate::config::data_dir()?.join("memory.db");
    eprintln!("embedding with {}", emb.model());
    let done = reindex(&db, &emb, batch, |done, total| {
        eprint!("\r{done}/{total}");
    })
    .await;
    eprintln!();
    let done = done?;
    println!(
        "{} memories embedded in {} requests{}",
        done.embedded,
        done.requests,
        if done.waits > 0 {
            format!(", {} rate-limit waits", done.waits)
        } else {
            String::new()
        }
    );
    Ok(())
}

/// Megabytes, for download sizes.
pub fn mb(bytes: u64) -> String {
    format!("{} MB", (bytes + 500_000) / 1_000_000)
}

/// Downloads the local model to the data dir, through the credential
/// proxy when there is one, and checks every file's SHA-256 against the
/// pin. Progress goes to stderr.
#[cfg(feature = "local-embed")]
pub async fn fetch_model(cfg: &Config) -> Result<std::path::PathBuf> {
    use ferrule_embed::download::{download, verify, HUGGING_FACE, POTION_MULTILINGUAL};
    let spec = POTION_MULTILINGUAL;
    let dir = spec.dir(&crate::config::data_dir()?);
    let (_, egress) = cli_egress(cfg)?;
    // No overall timeout: half a gigabyte takes what it takes.
    let client = ferrule_tools::egress::client_builder(egress.as_ref())?
        .connect_timeout(Duration::from_secs(30))
        .build()?;
    let base = std::env::var("FERRULE_EMBED_MODEL_BASE").unwrap_or_else(|_| HUGGING_FACE.into());
    let mut shown = (String::new(), u64::MAX);
    download(&client, &spec, &base, &dir, |file, done, total| {
        let pct = done * 100 / total.max(1);
        if shown.0 != file || shown.1 != pct {
            eprint!("\r{file}: {pct}% of {}   ", mb(total));
            shown = (file.to_string(), pct);
        }
    })
    .await
    .map_err(|e| {
        eprintln!();
        anyhow::anyhow!("model download failed: {e}")
    })?;
    eprintln!();
    verify(&spec, &dir).map_err(|e| anyhow::anyhow!(e))?;
    Ok(dir)
}

#[cfg(not(feature = "local-embed"))]
pub async fn fetch_model(_cfg: &Config) -> Result<std::path::PathBuf> {
    bail!("this ferrule was built without the local embedder (feature `local-embed`)")
}

/// `ferrule memory model download`.
pub async fn download_cmd(yes: bool) -> Result<()> {
    use ferrule_embed::download::{presence, Presence, POTION_MULTILINGUAL};
    let (cfg, _) = Config::load()?;
    let spec = POTION_MULTILINGUAL;
    let dir = spec.dir(&crate::config::data_dir()?);
    if presence(&spec, &dir) != Presence::Present && !yes {
        if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            bail!(
                "this downloads {} to {}; pass --yes to go ahead",
                mb(spec.total_size()),
                dir.display()
            );
        }
        let go = inquire::Confirm::new(&format!(
            "Download {} ({}, pinned revision {}) to {}?",
            spec.repo,
            mb(spec.total_size()),
            &spec.revision[..7],
            dir.display()
        ))
        .with_default(true)
        .prompt()?;
        if !go {
            return Ok(());
        }
    }
    let dir = fetch_model(&cfg).await?;
    println!("model ready at {} (checksums match)", dir.display());
    if cfg.memory.embedder != "local" {
        println!("to use it, set `embedder = \"local\"` under [memory] in the config (or run `ferrule setup`), then `ferrule memory reindex`");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrule_embed::{FakeEmbedder, ModelId};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Fails its `fail_at`th call (1-based) with `error`, else embeds.
    struct Flaky {
        inner: FakeEmbedder,
        calls: AtomicUsize,
        fail_at: usize,
        error: fn() -> EmbedError,
    }

    #[async_trait::async_trait]
    impl Embedder for Flaky {
        fn model(&self) -> &ModelId {
            self.inner.model()
        }
        async fn embed(&self, texts: &[String], p: Purpose) -> Result<Embedded, EmbedError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) + 1 == self.fail_at {
                return Err((self.error)());
            }
            self.inner.embed(texts, p).await
        }
    }

    fn store_with(dir: &Path, n: usize) -> std::path::PathBuf {
        let db = dir.join("memory.db");
        let store = MemoryStore::open(&db).unwrap();
        for i in 0..n {
            store
                .remember(&format!("fact number {i} about topic {}", i * 7), &[])
                .unwrap();
        }
        db
    }

    #[tokio::test]
    async fn reindex_stops_on_an_error_and_resumes_where_it_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let db = store_with(dir.path(), 10);
        let flaky = Arc::new(Flaky {
            inner: FakeEmbedder::new("f", 64),
            calls: AtomicUsize::new(0),
            fail_at: 3,
            error: || EmbedError::Transport {
                host: "h".into(),
                message: "reset".into(),
            },
        });
        let emb = Embedding::new(flaky.clone(), Hybrid::default());
        let err = reindex(&db, &emb, 3, |_, _| {}).await.unwrap_err();
        assert!(err.to_string().contains("6 of 10 rows embedded"), "{err}");
        let counts = MemoryStore::open(&db)
            .unwrap()
            .embedding_counts(emb.model())
            .unwrap();
        assert_eq!((counts.live_embedded, counts.stale), (6, 4));

        // The next run does only what's left.
        let mut seen = Vec::new();
        let done = reindex(&db, &emb, 3, |d, t| seen.push((d, t)))
            .await
            .unwrap();
        assert_eq!((done.embedded, done.requests), (4, 2));
        assert_eq!(seen, [(3, 4), (4, 4)]);
        let again = reindex(&db, &emb, 3, |_, _| {}).await.unwrap();
        assert_eq!(again, Reindexed::default());
    }

    #[tokio::test]
    async fn reindex_waits_out_a_429_and_carries_on() {
        let dir = tempfile::tempdir().unwrap();
        let db = store_with(dir.path(), 4);
        let emb = Embedding::new(
            Arc::new(Flaky {
                inner: FakeEmbedder::new("f", 64),
                calls: AtomicUsize::new(0),
                fail_at: 2,
                error: || EmbedError::RateLimited {
                    host: "h".into(),
                    retry_after: Some(0),
                },
            }),
            Hybrid::default(),
        );
        let done = reindex(&db, &emb, 2, |_, _| {}).await.unwrap();
        assert_eq!(
            done,
            Reindexed {
                embedded: 4,
                requests: 3,
                waits: 1
            }
        );
    }

    #[tokio::test]
    async fn a_failing_embedder_is_none_and_its_paid_call_is_an_error_row() {
        #[derive(Default)]
        struct Rows(Mutex<Vec<LedgerRecord>>);
        impl LedgerSink for Rows {
            fn record(&self, r: LedgerRecord) {
                self.0.lock().unwrap().push(r);
            }
        }
        let fake = Arc::new(FakeEmbedder::new("f", 8));
        let rows = Arc::new(Rows::default());
        let mut emb = Embedding::new(fake.clone(), Hybrid::default());
        emb.ledger = Some(Recorder {
            sink: rows.clone(),
            session_id: "s".into(),
            task_shape: "run".into(),
            origin: None,
            model: "m".into(),
            price_input_per_mtok: Some(2.0),
        });
        assert!(emb.try_one("hello", Purpose::Query).await.is_some());
        fake.set_failing(true);
        assert!(emb.try_one("hello", Purpose::Query).await.is_none());
        let rows = rows.0.lock().unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.call_kind == EMBED_CALL_KIND));
        assert_eq!(rows[0].outcome, "ok");
        assert_eq!(
            rows[0].cost_usd,
            Some(rows[0].input_tokens as f64 * 2.0 / 1e6)
        );
        assert_eq!(rows[1].outcome, "error");
        assert_eq!(rows[1].cost_usd, None);
    }
}
