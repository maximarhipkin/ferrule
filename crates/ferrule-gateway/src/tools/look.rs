//! M65 §0: the vision helper (`docs/m65-video.md`). For a chat whose model
//! can't see images (the Claude plan through Claude Code), a cheap vision
//! model on the `[media]` backend looks at each photo and tool image and
//! answers in words, and what it says takes the image's place
//! ([`ferrule_core::vision::Looker`]). Each call is a `media` ledger row
//! charged to the run, under the trust hub's dollar caps, like the other
//! media calls. Answers are kept by the image's path, its modification
//! time and the task, so the same file isn't paid for twice.

use super::media::{MediaShared, Row};
use ferrule_media::LookReq;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// How many answers the cache keeps before it starts over.
const KEPT: usize = 256;

pub struct VisionHelper {
    shared: Arc<MediaShared>,
    model: String,
    session_id: String,
    /// The run tree its spend is charged to.
    tree: String,
}

impl VisionHelper {
    pub fn new(shared: Arc<MediaShared>, model: String, session_id: &str, tree: &str) -> Self {
        Self {
            shared,
            model,
            session_id: session_id.to_string(),
            tree: tree.to_string(),
        }
    }
}

/// An image's path and modification time, for the cache; `None` when the
/// file can't be read.
fn stamp(path: &Path) -> Option<String> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let nanos = modified
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Some(format!("{}@{nanos}", path.display()))
}

/// `path` as an image the helper may be shown: a real PNG, JPEG, GIF or
/// WebP under the size cap on an image sent to a model.
fn checked(path: &Path) -> Result<PathBuf, String> {
    match ferrule_core::vision::image_file(path) {
        Some(Ok(_)) => Ok(path.to_path_buf()),
        Some(Err(why)) => Err(why),
        None => Err(format!(
            "{} is gone or isn't a PNG, JPEG, GIF or WebP image",
            path.file_name().map_or_else(
                || path.display().to_string(),
                |n| n.to_string_lossy().into_owned()
            )
        )),
    }
}

#[async_trait::async_trait]
impl ferrule_core::vision::Looker for VisionHelper {
    fn model(&self) -> String {
        self.model.clone()
    }

    async fn look(
        &self,
        image: &Path,
        task: &str,
        reference: Option<&Path>,
    ) -> Result<String, String> {
        let key = match (stamp(image), reference.map(stamp)) {
            (Some(i), None) => Some(format!("{i}\n{task}")),
            (Some(i), Some(Some(r))) => Some(format!("{i}\n{r}\n{task}")),
            _ => None,
        };
        if let Some(said) = key
            .as_ref()
            .and_then(|k| self.shared.looks.lock().unwrap().get(k).cloned())
        {
            return Ok(said);
        }
        let mut images = vec![checked(image)?];
        if let Some(r) = reference {
            images.push(checked(r)?);
        }
        self.shared.check_budget(&self.tree)?;
        let started = std::time::Instant::now();
        let out = self
            .shared
            .backend
            .look(LookReq {
                model: self.model.clone(),
                task: task.to_string(),
                images,
            })
            .await;
        let err = out.as_ref().err().map(|e| e.to_string());
        self.shared.ledger(
            Row {
                session_id: &self.session_id,
                tree: Some(&self.tree),
                model: &self.model,
                latency_ms: started.elapsed().as_millis() as u64,
                err: err.as_deref(),
                cost_usd: out.as_ref().ok().and_then(|o| o.cost_usd),
            },
            Some(&self.tree),
        );
        let said = out.map_err(|e| e.to_string())?.text.trim().to_string();
        if let Some(key) = key {
            let mut looks = self.shared.looks.lock().unwrap();
            if looks.len() >= KEPT {
                looks.clear();
            }
            looks.insert(key, said.clone());
        }
        Ok(said)
    }
}
