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

/// M64.5: Not now and 👎 on a kind are counted over this window (§7).
pub const BACK_OFF_WINDOW_SECS: i64 = 30 * 86_400;
/// The longest a kind waits after them, in days.
pub const BACK_OFF_MAX_DAYS: i64 = 16;

/// One message the owner sent in their own chat.
#[derive(Debug, Clone, Serialize)]
pub struct Request {
    pub id: i64,
    pub at: i64,
    pub session: String,
    pub text: String,
}

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
    /// The owner's last 👍 or 👎 on its message (M64.5): `up` or `down`.
    pub thumb: Option<String>,
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
CREATE TABLE IF NOT EXISTS feedback (
    id INTEGER PRIMARY KEY,
    item TEXT NOT NULL,
    kind TEXT NOT NULL,
    at INTEGER NOT NULL,
    verdict TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS feedback_kind ON feedback(kind, at);
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

    /// The owner's messages since `since` (unix seconds), oldest first.
    pub fn requests_since(&self, since: i64) -> Result<Vec<Request>> {
        let db = self.db();
        let mut st = db
            .prepare("SELECT id, at, session, text FROM requests WHERE at >= ?1 ORDER BY at, id")?;
        let rows = st.query_map(params![since], request)?;
        Ok(rows.collect::<Result<_, _>>()?)
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

    /// Items since `since`, oldest first.
    pub fn items_since(&self, since: i64) -> Result<Vec<Item>> {
        let db = self.db();
        let mut st = db.prepare(&format!(
            "SELECT {ITEM} FROM items WHERE at >= ?1 ORDER BY at, rowid"
        ))?;
        let rows = st.query_map(params![since], item)?;
        Ok(rows.collect::<Result<_, _>>()?)
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

    /// The item that went out in this message, if one did.
    pub fn item_by_message(
        &self,
        channel: &str,
        chat: &str,
        message: &str,
    ) -> Result<Option<Item>> {
        Ok(self
            .db()
            .query_row(
                &format!("SELECT {ITEM} FROM items WHERE message = ?1 ORDER BY at DESC LIMIT 1"),
                params![format!("{channel}\t{chat}\t{message}")],
                item,
            )
            .optional()?)
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

    /// The list, oldest first: each kind and when.
    pub fn suppressed(&self) -> Result<Vec<(String, i64)>> {
        let db = self.db();
        let mut st = db.prepare("SELECT kind, at FROM suppressed ORDER BY at")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Takes `kind` off the list; `false` when it wasn't on it.
    pub fn unsuppress(&self, kind: &str) -> Result<bool> {
        let n = self
            .db()
            .execute("DELETE FROM suppressed WHERE kind = ?1", params![kind])?;
        Ok(n > 0)
    }

    /// Empties the list; how many kinds were on it.
    pub fn clear_suppressed(&self) -> Result<usize> {
        Ok(self.db().execute("DELETE FROM suppressed", [])?)
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

    // ---- feedback (M64.5) ---------------------------------------------------

    /// The owner's word on an item, kept by its kind for the back-off:
    /// `not_now`, `down` (slow the kind down), `useful`, `up` (back to
    /// normal).
    pub fn add_feedback(&self, item: &Item, verdict: &str) -> Result<()> {
        self.db().execute(
            "INSERT INTO feedback (item, kind, at, verdict) VALUES (?1, ?2, ?3, ?4)",
            params![item.id, item.kind, now(), verdict],
        )?;
        Ok(())
    }

    /// Until when `kind` waits after Not now or 👎 (§7), if it does now.
    pub fn resting(&self, kind: &str, now: i64) -> Result<Option<i64>> {
        let db = self.db();
        let mut st = db.prepare(
            "SELECT at, verdict FROM feedback WHERE kind = ?1 AND at >= ?2 ORDER BY at, id",
        )?;
        let rows: Vec<(i64, String)> = st
            .query_map(params![kind, now - BACK_OFF_WINDOW_SECS], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<Result<_, _>>()?;
        Ok(back_off(&rows, now))
    }

    /// Every kind waiting now, and until when.
    pub fn resting_kinds(&self, now: i64) -> Result<Vec<(String, i64)>> {
        let kinds: Vec<String> = {
            let db = self.db();
            let mut st = db.prepare("SELECT DISTINCT kind FROM feedback WHERE at >= ?1")?;
            let rows = st.query_map(params![now - BACK_OFF_WINDOW_SECS], |r| r.get(0))?;
            rows.collect::<Result<_, _>>()?
        };
        let mut out = Vec::new();
        for kind in kinds {
            if let Some(until) = self.resting(&kind, now)? {
                out.push((kind, until));
            }
        }
        out.sort_by_key(|(_, until)| *until);
        Ok(out)
    }

    // ---- state --------------------------------------------------------------

    pub fn state(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .db()
            .query_row(
                "SELECT value FROM state WHERE key = ?1",
                params![key],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn set_state(&self, key: &str, value: &str) -> Result<()> {
        self.db().execute(
            "INSERT INTO state (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }
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

/// The back-off of §7 over one kind's feedback (oldest first, the
/// window's only): the n-th Not now or 👎 since the last 👍 or Useful
/// makes the kind wait 2^(n-1) days (at most 16) from it. Until when, if
/// that's still ahead.
pub fn back_off(rows: &[(i64, String)], now: i64) -> Option<i64> {
    let reset = rows
        .iter()
        .rposition(|(_, v)| v == "up" || v == "useful")
        .map_or(0, |i| i + 1);
    let waved: Vec<i64> = rows[reset..]
        .iter()
        .filter(|(_, v)| v == "not_now" || v == "down")
        .map(|(at, _)| *at)
        .collect();
    let last = *waved.last()?;
    let n = u32::try_from(waved.len()).unwrap_or(u32::MAX);
    let days = 2i64.pow(n.saturating_sub(1).min(4)).min(BACK_OFF_MAX_DAYS);
    let until = last + days * 86_400;
    (until > now).then_some(until)
}

const ITEM: &str =
    "id, at, kind, mode, text, action, basis, commitment, message, outcome, outcome_at, undo,
    (SELECT f.verdict FROM feedback f WHERE f.item = items.id AND f.verdict IN ('up', 'down')
     ORDER BY f.at DESC, f.id DESC LIMIT 1)";

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
        thumb: r.get(12)?,
    })
}

fn request(r: &Row<'_>) -> rusqlite::Result<Request> {
    Ok(Request {
        id: r.get(0)?,
        at: r.get(1)?,
        session: r.get(2)?,
        text: r.get(3)?,
    })
}
