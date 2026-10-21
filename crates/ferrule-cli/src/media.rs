//! M51: what `[media]` becomes at gateway start (`docs/m51-media.md`) —
//! the shared backend + jobs table behind `generate_image`,
//! `generate_video`, M51.4's `speak` and the video poller. Off unless `[media] provider`
//! names a connected provider with an OpenRouter-compatible media
//! endpoint.

use crate::{config, ledger, web_search};
use anyhow::Result;
use ferrule_core::LedgerRecord;
use ferrule_gateway::tools::{MediaLimits, MediaPoller, MediaShared, MediaSpend};
use ferrule_media::openrouter::{MediaSettings, OpenRouterMedia};
use ferrule_media::JobStore;
use ferrule_tools::egress;
use std::sync::Arc;
use std::time::Duration;

/// The shared handle, or `None` when media is off or its key can't be
/// reached through the proxy (said once on stderr).
pub fn shared(cfg: &config::Config) -> Option<Arc<MediaShared>> {
    let id = cfg.media.provider.as_deref()?;
    let Some(p) = cfg.providers.get(id) else {
        warn_once(&format!(
            "[media]: there is no [providers.{id}] table, so no media tools"
        ));
        return None;
    };
    if p.plan.is_some() {
        warn_once(&format!(
            "[media]: {id} is a subscription plan, which has no media endpoint, so no media tools"
        ));
        return None;
    }
    if p.base_url.is_empty() {
        warn_once(&format!("[media]: {id} has no base_url, so no media tools"));
        return None;
    }
    // The proxy's placeholder for the provider's key + an egress client
    // through the proxy, exactly like web_search and the embedder.
    let (key, key_env) = if p.api_key_env.is_empty() {
        (None, None)
    } else {
        let broker = crate::shared_broker(cfg).ok().flatten();
        match web_search::keyed(broker, &p.api_key_env) {
            Ok(Some((placeholder, _))) => (Some(placeholder), Some(p.api_key_env.clone())),
            _ => {
                warn_once(&format!(
                    "[media]: {} isn't set in ferrule's environment, so no media tools",
                    p.api_key_env
                ));
                return None;
            }
        }
    };
    let client = match client(cfg) {
        Ok(c) => c,
        Err(e) => {
            warn_once(&format!("[media]: {e:#}, so no media tools"));
            return None;
        }
    };
    let backend = Arc::new(OpenRouterMedia::new(
        MediaSettings {
            base_url: p.base_url.clone(),
            image_model: cfg.media.image_model.clone(),
            video_model: cfg.media.video_model.clone(),
            speech_model: cfg.media.speech_model.clone(),
            key,
            key_env,
            // A render submit is quick; an image can take a couple of
            // minutes. Downloads get their own bound in the backend.
            timeout: Duration::from_secs(180),
        },
        client,
    ));
    let store = match config::data_dir()
        .and_then(|d| JobStore::open(&d.join("media.db")).map_err(anyhow::Error::from))
    {
        Ok(s) => Arc::new(s),
        Err(e) => {
            warn_once(&format!(
                "[media]: the jobs table can't open: {e:#}, so no media tools"
            ));
            return None;
        }
    };
    let shared = MediaShared::new(
        backend,
        store,
        cfg.media.image_model.clone(),
        cfg.media.video_model.clone(),
        cfg.media.max_per_day,
        ledger::build_sink(cfg),
    );
    shared.set_limits(MediaLimits {
        image_models: cfg.media.image_models.clone(),
        video_models: cfg.media.video_models.clone(),
        max_videos_per_day: cfg.media.max_videos_per_day,
        max_usd_per_video: cfg.media.max_usd_per_video,
    });
    // Media dollars answer to [trust]'s caps and count toward them.
    match crate::trust::hub(cfg) {
        Ok(hub) => shared.set_spend(Arc::new(HubSpend(hub))),
        Err(e) => {
            warn_once(&format!(
                "[media]: the trust hub can't start: {e:#}, so no media tools"
            ));
            return None;
        }
    }
    // M51.4: `speak` unless the speech model is set to "".
    let speech = cfg.media.speech_model.trim();
    if !speech.is_empty() {
        shared.set_speech(
            speech.to_string(),
            cfg.media
                .voice
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string),
        );
    }
    Some(shared)
}

/// The trust hub as media spend's caps: a tool call's row is charged to
/// its run like a model call's (`TrustSink` does the same for those).
struct HubSpend(Arc<ferrule_trust::Hub>);

impl MediaSpend for HubSpend {
    fn usd_left(&self, tree: &str) -> Result<Option<f64>, String> {
        let task = tree.strip_prefix(ferrule_trust::meter::TASK_PREFIX);
        self.0.usd_left(tree, task)
    }

    fn charge(&self, tree: &str, row: &LedgerRecord) {
        self.0.charge(tree, row);
    }
}

/// The egress client media calls go through (the credential proxy when it
/// runs), the same builder the embedder uses.
fn client(cfg: &config::Config) -> Result<reqwest::Client> {
    let broker = crate::shared_broker(cfg)?;
    let e = match broker {
        Some(b) => Some(crate::tool_egress(b)?),
        None => None,
    };
    Ok(egress::client_builder(e.as_ref())?.build()?)
}

/// The poller as a built-in task, registered like M16's learning pass:
/// `ensure` first makes tasks.db match `[media]` (added when on, removed
/// when off), then the job itself goes on the scheduler.
pub fn register_poller(
    cfg: &config::Config,
    scheduler: ferrule_gateway::Scheduler,
    shared: Option<Arc<MediaShared>>,
    ensure: bool,
) -> ferrule_gateway::Scheduler {
    let _ = cfg; // the poller is wanted exactly when `shared` exists
    use ferrule_gateway::scheduler::ensure_builtin;
    if ensure {
        let want = shared
            .as_ref()
            .map(|_| ferrule_gateway::tools::media::spec());
        match ensure_builtin(
            scheduler.store(),
            "ferrule-media",
            want.as_ref(),
            chrono::Utc::now(),
        ) {
            Ok(_) => {}
            Err(e) => {
                eprintln!("ferrule: [media] poller task: {e}; video delivery is off");
                return scheduler;
            }
        }
    }
    match shared {
        Some(shared) => scheduler.with_builtin("ferrule-media", Arc::new(MediaPoller { shared })),
        None => scheduler,
    }
}

fn warn_once(msg: &str) {
    static SAID: std::sync::Once = std::sync::Once::new();
    SAID.call_once(|| eprintln!("ferrule: {msg}"));
}
