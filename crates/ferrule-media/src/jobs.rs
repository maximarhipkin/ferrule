//! The `media_jobs` table (M51 §3): where an async video job lives between
//! "submitted" and "delivered", so a gateway restart loses nothing, and
//! where every media call — image, video or speech — is counted for the
//! daily cap.

use crate::MediaError;
use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Image,
    Video,
    /// M51.4: a `speak` reply.
    Speech,
}

impl JobKind {
    fn as_str(&self) -> &'static str {
        match self {
            JobKind::Image => "image",
            JobKind::Video => "video",
            JobKind::Speech => "speech",
        }
    }
}

/// One row. `state`: `pending` | `done` | `failed`.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaJob {
    pub id: String,
    pub kind: String,
    pub created_at: i64,
    pub model: String,
    pub prompt: String,
    /// The workspace path the result is saved to.
    pub out_path: String,
    /// The chat to deliver to, when the session had one.
    pub channel: Option<String>,
    pub chat_id: Option<String>,
    pub state: String,
    pub error: Option<String>,
    /// The run tree that asked for it, so a video's delivery row counts
    /// toward the right scheduled task's cap.
    pub tree: Option<String>,
    /// A video's cost as estimated at submit (dollars), which its submit
    /// row charged; the delivery row settles it to the provider's bill.
    pub estimate_usd: Option<f64>,
}

const COLUMNS: &str =
    "id, kind, created_at, model, prompt, out_path, channel, chat_id, state, error, tree, estimate_usd";

pub struct JobStore {
    conn: Mutex<Connection>,
}

impl JobStore {
    pub fn open(path: &Path) -> Result<Self, MediaError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| MediaError::Store(format!("{}: {e}", parent.display())))?;
        }
        let conn = Connection::open(path)
            .map_err(|e| MediaError::Store(format!("{}: {e}", path.display())))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS media_jobs (
                id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                model TEXT NOT NULL,
                prompt TEXT NOT NULL,
                out_path TEXT NOT NULL,
                channel TEXT,
                chat_id TEXT,
                state TEXT NOT NULL,
                error TEXT
            );",
        )
        .map_err(|e| MediaError::Store(e.to_string()))?;
        // Columns added after M51: a table from before gets them empty.
        for (column, ty) in [("tree", "TEXT"), ("estimate_usd", "REAL")] {
            let has = conn
                .prepare("SELECT name FROM pragma_table_info('media_jobs') WHERE name = ?1")
                .and_then(|mut st| st.exists(params![column]))
                .map_err(|e| MediaError::Store(e.to_string()))?;
            if !has {
                conn.execute_batch(&format!("ALTER TABLE media_jobs ADD COLUMN {column} {ty};"))
                    .map_err(|e| MediaError::Store(e.to_string()))?;
            }
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Images are recorded too (state `done` at once): the daily cap counts
    /// every generation.
    pub fn record(&self, job: &MediaJob) -> Result<(), MediaError> {
        self.conn
            .lock()
            .unwrap()
            .execute(
                &format!(
                    "INSERT INTO media_jobs ({COLUMNS})
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)"
                ),
                params![
                    job.id,
                    job.kind,
                    job.created_at,
                    job.model,
                    job.prompt,
                    job.out_path,
                    job.channel,
                    job.chat_id,
                    job.state,
                    job.error,
                    job.tree,
                    job.estimate_usd
                ],
            )
            .map_err(|e| MediaError::Store(e.to_string()))?;
        Ok(())
    }

    /// Generations since `since_unix` — the daily cap's count. An
    /// unreadable table is an error, not a zero: fail-closed.
    pub fn count_since(&self, since_unix: i64) -> Result<u64, MediaError> {
        self.conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM media_jobs WHERE created_at >= ?1",
                params![since_unix],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n as u64)
            .map_err(|e| MediaError::Store(e.to_string()))
    }

    /// `kind`'s generations since `since_unix` (the daily video cap).
    /// Fail-closed like [`Self::count_since`].
    pub fn count_kind_since(&self, kind: JobKind, since_unix: i64) -> Result<u64, MediaError> {
        self.conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM media_jobs WHERE kind = ?1 AND created_at >= ?2",
                params![kind.as_str(), since_unix],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n as u64)
            .map_err(|e| MediaError::Store(e.to_string()))
    }

    pub fn pending_videos(&self) -> Result<Vec<MediaJob>, MediaError> {
        let conn = self.conn.lock().unwrap();
        let mut st = conn
            .prepare(&format!(
                "SELECT {COLUMNS}
                 FROM media_jobs WHERE kind = 'video' AND state = 'pending' ORDER BY created_at"
            ))
            .map_err(|e| MediaError::Store(e.to_string()))?;
        let rows = st
            .query_map([], |r| {
                Ok(MediaJob {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    created_at: r.get(2)?,
                    model: r.get(3)?,
                    prompt: r.get(4)?,
                    out_path: r.get(5)?,
                    channel: r.get(6)?,
                    chat_id: r.get(7)?,
                    state: r.get(8)?,
                    error: r.get(9)?,
                    tree: r.get(10)?,
                    estimate_usd: r.get(11)?,
                })
            })
            .map_err(|e| MediaError::Store(e.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| MediaError::Store(e.to_string()))
    }

    pub fn mark(&self, id: &str, state: &str, error: Option<&str>) -> Result<(), MediaError> {
        self.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE media_jobs SET state = ?2, error = ?3 WHERE id = ?1",
                params![id, state, error],
            )
            .map_err(|e| MediaError::Store(e.to_string()))?;
        Ok(())
    }

    pub fn video(id: &str, model: &str, prompt: &str, out_path: &str) -> MediaJob {
        MediaJob {
            id: id.into(),
            kind: JobKind::Video.as_str().into(),
            created_at: now_unix(),
            model: model.into(),
            prompt: prompt.into(),
            out_path: out_path.into(),
            channel: None,
            chat_id: None,
            state: "pending".into(),
            error: None,
            tree: None,
            estimate_usd: None,
        }
    }

    pub fn image(id: &str, model: &str, prompt: &str, out_path: &str) -> MediaJob {
        MediaJob {
            state: "done".into(),
            ..Self::video(id, model, prompt, out_path)
        }
        .with_kind(JobKind::Image)
    }

    /// M51.4: a spoken reply, done at once like an image.
    pub fn speech(id: &str, model: &str, text: &str, out_path: &str) -> MediaJob {
        MediaJob {
            state: "done".into(),
            ..Self::video(id, model, text, out_path)
        }
        .with_kind(JobKind::Speech)
    }
}

impl MediaJob {
    fn with_kind(mut self, kind: JobKind) -> Self {
        self.kind = kind.as_str().into();
        self
    }
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jobs_are_recorded_counted_polled_and_marked() {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::open(&dir.path().join("media.db")).unwrap();
        let mut v = JobStore::video("v1", "veo", "a clip", "media/v1.mp4");
        v.channel = Some("telegram".into());
        v.chat_id = Some("42".into());
        store.record(&v).unwrap();
        store
            .record(&JobStore::image("i1", "flux", "a logo", "media/i1.png"))
            .unwrap();
        assert_eq!(store.count_since(0).unwrap(), 2);
        assert_eq!(store.count_since(now_unix() + 60).unwrap(), 0);

        let pending = store.pending_videos().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "v1");
        assert_eq!(pending[0].chat_id.as_deref(), Some("42"));

        store.mark("v1", "failed", Some("out of GPUs")).unwrap();
        assert!(store.pending_videos().unwrap().is_empty());
        // Reopening sees the same rows: a restart loses nothing.
        let again = JobStore::open(&dir.path().join("media.db")).unwrap();
        assert_eq!(again.count_since(0).unwrap(), 2);
    }
}
