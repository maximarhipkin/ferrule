//! M64: the proactive agent's own store, `<data>/proactive/proactive.db`
//! (docs/m64-proactive.md §2). The owner's only: the door, the pass, the
//! `commitments` tool in the owner's sessions and the owner's commands
//! write it, and the directory is hidden from every other chat's commands
//! and file tools (`owner_mode::guest_hidden`).

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

/// The store's directory under the data dir.
pub const DIR: &str = "proactive";

/// How long the owner's messages are kept: the patterns' window and a
/// little more.
pub const REQUESTS_KEPT_SECS: i64 = 30 * 86_400;

/// Something the owner said they'd do, or left unfinished.
#[derive(Debug, Clone, Serialize)]
pub struct Commitment {
    pub id: i64,
    /// `commitment` or `loop`.
    pub kind: String,
    pub text: String,
    /// When it's due (unix seconds), if a time or day was said.
    pub due: Option<i64>,
    /// `due` names a day, not a time of day.
    pub due_day: bool,
    /// Where it came from: `request:<id>`, `chat:<session>`, `task:<id>`.
    pub source: String,
    /// `open`, `done`, `dropped` or `snoozed`.
    pub status: String,
    pub snoozed_until: Option<i64>,
    /// The reminder task made for it.
    pub reminder: Option<String>,
    pub created: i64,
    pub updated: i64,
}

impl Commitment {
    /// Open now: open, or snoozed until a time that has come.
    pub fn is_open(&self, now: i64) -> bool {
        self.status == "open"
            || (self.status == "snoozed" && self.snoozed_until.is_some_and(|t| t <= now))
    }
}

/// What a new commitment says.
#[derive(Debug, Clone)]
pub struct NewCommitment {
    pub kind: String,
    pub text: String,
    pub due: Option<i64>,
    pub due_day: bool,
    pub source: String,
}

/// What an item may do, fixed by ferrule (never by the model).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    /// A built-in one-off whose text goes to the owner as is.
    Reminder { at: i64, text: String },
    /// One of the owner's own scheduled tasks (M56.3's `task_add`).
    Task {
        name: String,
        schedule: String,
        timezone: String,
        prompt: String,
    },
}

/// One thing the pass, the tool or a command noticed, said, suggested or
/// did, and what the owner made of it.
#[derive(Debug, Clone, Serialize)]
pub struct Item {
    pub id: String,
    pub at: i64,
    /// The suppression key: `reminder`, `loop`, `calendar`, `mail`,
    /// `note`, or `pattern:<key>`.
    pub kind: String,
    /// `noticed`, `message`, `suggest` or `done`.
    pub mode: String,
    pub text: String,
    pub action: Option<Action>,
    /// `owner`: built on the owner's own words; `outside`: on calendar or
    /// mail text.
    pub basis: String,
    pub commitment: Option<i64>,
    /// The message it went out in: `channel`, `chat`, its id.
    pub message: Option<(String, String, String)>,
    /// `done`, `not_now`, `never`, `undone`, `useful`, `up`, `down`,
    /// `failed`, `expired`.
    pub outcome: Option<String>,
    pub outcome_at: Option<i64>,
    /// The task its action made, which Undo deletes.
    pub undo: Option<String>,
}

/// What a new item says.
#[derive(Debug, Clone)]
pub struct NewItem {
    pub kind: String,
    pub mode: String,
    pub text: String,
    pub action: Option<Action>,
    pub basis: String,
    pub commitment: Option<i64>,
}

pub struct Store {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS requests (
    id INTEGER PRIMARY KEY,
    at INTEGER NOT NULL,
    session TEXT NOT NULL,
    text TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS requests_at ON requests(at);
CREATE TABLE IF NOT EXISTS commitments (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    text TEXT NOT NULL,
    due INTEGER,
    due_day INTEGER NOT NULL DEFAULT 0,
    source TEXT NOT NULL,
    status TEXT NOT NULL,
    snoozed_until INTEGER,
    reminder TEXT,
    created INTEGER NOT NULL,
    updated INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS items (
    id TEXT PRIMARY KEY,
    at INTEGER NOT NULL,
    kind TEXT NOT NULL,
    mode TEXT NOT NULL,
    text TEXT NOT NULL,
    action TEXT,
    basis TEXT NOT NULL,
    commitment INTEGER,
    message TEXT,
    outcome TEXT,
    outcome_at INTEGER,
    undo TEXT
);
CREATE INDEX IF NOT EXISTS items_at ON items(at);
CREATE TABLE IF NOT EXISTS suppressed (
    kind TEXT PRIMARY KEY,
    at INTEGER NOT NULL,
    item TEXT
);
CREATE TABLE IF NOT EXISTS state (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

impl Store {
    /// `<data>/proactive/proactive.db`, made on first use.
    pub fn path(data: &Path) -> PathBuf {
        data.join(DIR).join("proactive.db")
    }

    pub fn open(data: &Path) -> Result<Self> {
        let path = Self::path(data);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let conn =
            Connection::open(&path).with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn db(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The data dir's store.
    pub fn open_default() -> Result<Self> {
        Self::open(&crate::config::data_dir()?)
    }

    // ---- the owner's messages ---------------------------------------------

    pub fn add_request(&self, at: i64, session: &str, text: &str) -> Result<i64> {
        let db = self.db();
        db.execute(
            "INSERT INTO requests (at, session, text) VALUES (?1, ?2, ?3)",
            params![at, session, text],
        )?;
        let id = db.last_insert_rowid();
        db.execute(
            "DELETE FROM requests WHERE at < ?1",
            params![now() - REQUESTS_KEPT_SECS],
        )?;
        Ok(id)
    }

    // ---- commitments and loops --------------------------------------------

    pub fn add_commitment(&self, c: &NewCommitment) -> Result<Commitment> {
        let t = now();
        let id = {
            let db = self.db();
            db.execute(
            "INSERT INTO commitments (kind, text, due, due_day, source, status, created, updated)
             VALUES (?1, ?2, ?3, ?4, ?5, 'open', ?6, ?6)",
            params![c.kind, c.text, c.due, c.due_day, c.source, t],
            )?;
            db.last_insert_rowid()
        };
        self.commitment(id)?
            .context("the commitment just added is gone")
    }

    pub fn commitment(&self, id: i64) -> Result<Option<Commitment>> {
        Ok(self
            .db()
            .query_row(
                &format!("SELECT {COMMITMENT} FROM commitments WHERE id = ?1"),
                params![id],
                commitment,
            )
            .optional()?)
    }

    /// Every commitment from `source` (a loop's task, a request), newest
    /// first.
    pub fn commitments_from(&self, source: &str) -> Result<Vec<Commitment>> {
        let db = self.db();
        let mut st = db.prepare(&format!(
            "SELECT {COMMITMENT} FROM commitments WHERE source = ?1 ORDER BY id DESC"
        ))?;
        let rows = st.query_map(params![source], commitment)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The open ones (snoozed ones whose time has come included), by due
    /// time then age; with `all`, every one, newest first.
    pub fn commitments(&self, all: bool) -> Result<Vec<Commitment>> {
        let db = self.db();
        let mut st = db.prepare(&format!(
            "SELECT {COMMITMENT} FROM commitments ORDER BY id DESC LIMIT 500"
        ))?;
        let rows: Vec<Commitment> = st.query_map([], commitment)?.collect::<Result<_, _>>()?;
        if all {
            return Ok(rows);
        }
        let t = now();
        let mut open: Vec<Commitment> = rows.into_iter().filter(|c| c.is_open(t)).collect();
        open.sort_by_key(|c| (c.due.unwrap_or(i64::MAX), c.id));
        Ok(open)
    }

    /// `done`, `dropped`, `open`, or `snoozed` until `until`. `false`: no
    /// such commitment.
    pub fn set_status(&self, id: i64, status: &str, until: Option<i64>) -> Result<bool> {
        let n = self.db().execute(
            "UPDATE commitments SET status = ?2, snoozed_until = ?3, updated = ?4 WHERE id = ?1",
            params![id, status, until, now()],
        )?;
        Ok(n > 0)
    }

    pub fn set_reminder(&self, id: i64, task: Option<&str>) -> Result<()> {
        self.db().execute(
            "UPDATE commitments SET reminder = ?2, updated = ?3 WHERE id = ?1",
            params![id, task, now()],
        )?;
        Ok(())
    }

    // ---- items --------------------------------------------------------------

    pub fn add_item(&self, n: &NewItem) -> Result<Item> {
        let id = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
        let action = n.action.as_ref().map(serde_json::to_string).transpose()?;
        self.db().execute(
            "INSERT INTO items (id, at, kind, mode, text, action, basis, commitment)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                id,
                now(),
                n.kind,
                n.mode,
                n.text,
                action,
                n.basis,
                n.commitment
            ],
        )?;
        self.item(&id)?.context("the item just added is gone")
    }

    pub fn item(&self, id: &str) -> Result<Option<Item>> {
        Ok(self
            .db()
            .query_row(
                &format!("SELECT {ITEM} FROM items WHERE id = ?1"),
                params![id],
                item,
            )
            .optional()?)
    }

    pub fn set_outcome(&self, id: &str, outcome: &str) -> Result<bool> {
        let n = self.db().execute(
            "UPDATE items SET outcome = ?2, outcome_at = ?3 WHERE id = ?1",
            params![id, outcome, now()],
        )?;
        Ok(n > 0)
    }

    pub fn set_undo(&self, id: &str, task: Option<&str>) -> Result<()> {
        self.db().execute(
            "UPDATE items SET undo = ?2 WHERE id = ?1",
            params![id, task],
        )?;
        Ok(())
    }

    /// Where an item went out: `channel`, `chat`, the message's id.
    pub fn set_message(&self, id: &str, channel: &str, chat: &str, message: &str) -> Result<()> {
        self.db().execute(
            "UPDATE items SET message = ?2 WHERE id = ?1",
            params![id, format!("{channel}\t{chat}\t{message}")],
        )?;
        Ok(())
    }

    // ---- the Never list -----------------------------------------------------

    pub fn suppress(&self, kind: &str, item: Option<&str>) -> Result<()> {
        self.db().execute(
            "INSERT INTO suppressed (kind, at, item) VALUES (?1, ?2, ?3)
             ON CONFLICT(kind) DO UPDATE SET at = excluded.at, item = excluded.item",
            params![kind, now(), item],
        )?;
        Ok(())
    }

    pub fn is_suppressed(&self, kind: &str) -> Result<bool> {
        Ok(self
            .db()
            .query_row(
                "SELECT 1 FROM suppressed WHERE kind = ?1",
                params![kind],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    // ---- state ----------------------------------------------------------------
}

const COMMITMENT: &str =
    "id, kind, text, due, due_day, source, status, snoozed_until, reminder, created, updated";

fn commitment(r: &Row<'_>) -> rusqlite::Result<Commitment> {
    Ok(Commitment {
        id: r.get(0)?,
        kind: r.get(1)?,
        text: r.get(2)?,
        due: r.get(3)?,
        due_day: r.get(4)?,
        source: r.get(5)?,
        status: r.get(6)?,
        snoozed_until: r.get(7)?,
        reminder: r.get(8)?,
        created: r.get(9)?,
        updated: r.get(10)?,
    })
}

const ITEM: &str =
    "id, at, kind, mode, text, action, basis, commitment, message, outcome, outcome_at, undo";

fn item(r: &Row<'_>) -> rusqlite::Result<Item> {
    let action: Option<String> = r.get(5)?;
    let message: Option<String> = r.get(8)?;
    Ok(Item {
        id: r.get(0)?,
        at: r.get(1)?,
        kind: r.get(2)?,
        mode: r.get(3)?,
        text: r.get(4)?,
        action: action.and_then(|a| serde_json::from_str(&a).ok()),
        basis: r.get(6)?,
        commitment: r.get(7)?,
        message: message.and_then(|m| {
            let mut parts = m.splitn(3, '\t').map(str::to_string);
            Some((parts.next()?, parts.next()?, parts.next()?))
        }),
        outcome: r.get(9)?,
        outcome_at: r.get(10)?,
        undo: r.get(11)?,
    })
}
