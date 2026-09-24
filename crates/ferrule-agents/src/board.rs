//! The tree's shared board and task list in `agents.db`. The board carries
//! findings (and direct messages); the task list carries work, with
//! dependency edges, claimed atomically.

use crate::error::AgentsError;
use crate::store::AgentStore;
use rusqlite::{params, OptionalExtension, Row, TransactionBehavior};

/// Longest post, in chars. Anything longer belongs in a file whose path is
/// posted.
pub const MAX_POST_CHARS: usize = 4_000;

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub id: i64,
    pub tree: String,
    pub author: String,
    /// Set for a direct message.
    pub recipient: Option<String>,
    pub topic: Option<String>,
    pub body: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Open,
    Claimed,
    Done,
    Failed,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Open => "open",
            TaskStatus::Claimed => "claimed",
            TaskStatus::Done => "done",
            TaskStatus::Failed => "failed",
        }
    }

    fn parse(s: &str) -> TaskStatus {
        match s {
            "claimed" => TaskStatus::Claimed,
            "done" => TaskStatus::Done,
            "failed" => TaskStatus::Failed,
            _ => TaskStatus::Open,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Task {
    pub id: i64,
    pub tree: String,
    pub author: String,
    pub title: String,
    pub detail: Option<String>,
    pub status: TaskStatus,
    pub claimed_by: Option<String>,
    pub result: Option<String>,
    /// Tasks that must be done before this one can be claimed.
    pub after: Vec<i64>,
}

/// Why a claim of a specific task was refused.
#[derive(Debug, Clone, PartialEq)]
pub enum ClaimRefused {
    NotFound,
    NotOpen(TaskStatus, Option<String>),
    Blocked(Vec<i64>),
    NotYours(String),
}

const TASK_COLUMNS: &str = "id, tree, author, title, detail, status, claimed_by, result";

fn row_to_task(r: &Row) -> rusqlite::Result<Task> {
    Ok(Task {
        id: r.get(0)?,
        tree: r.get(1)?,
        author: r.get(2)?,
        title: r.get(3)?,
        detail: r.get(4)?,
        status: TaskStatus::parse(&r.get::<_, String>(5)?),
        claimed_by: r.get(6)?,
        result: r.get(7)?,
        after: Vec::new(),
    })
}

fn row_to_entry(r: &Row) -> rusqlite::Result<Entry> {
    Ok(Entry {
        id: r.get(0)?,
        tree: r.get(1)?,
        author: r.get(2)?,
        recipient: r.get(3)?,
        topic: r.get(4)?,
        body: r.get(5)?,
        created_at: r.get(6)?,
    })
}

fn deps(conn: &rusqlite::Connection, task: i64) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT after FROM work_deps WHERE task = ?1 ORDER BY after")?;
    let rows = stmt.query_map(params![task], |r| r.get(0))?;
    rows.collect()
}

/// Dependencies of `task` that aren't done.
fn unmet(conn: &rusqlite::Connection, task: i64) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT d.after FROM work_deps d JOIN work w ON w.id = d.after \
         WHERE d.task = ?1 AND w.status != 'done' ORDER BY d.after",
    )?;
    let rows = stmt.query_map(params![task], |r| r.get(0))?;
    rows.collect()
}

impl AgentStore {
    pub fn post(
        &self,
        tree: &str,
        author: &str,
        recipient: Option<&str>,
        topic: Option<&str>,
        body: &str,
        now: i64,
    ) -> Result<i64, AgentsError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO board (tree, author, recipient, topic, body, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![tree, author, recipient, topic, body, now],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Entries of `tree` after `since` that `reader` may see (every post,
    /// and direct messages to or from it), oldest first, at most `limit`.
    pub fn read_board(
        &self,
        tree: &str,
        reader: &str,
        since: i64,
        topic: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Entry>, AgentsError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, tree, author, recipient, topic, body, created_at FROM board \
             WHERE tree = ?1 AND id > ?2 AND (recipient IS NULL OR recipient = ?3 OR author = ?3) \
             AND (?4 IS NULL OR topic = ?4) ORDER BY id LIMIT ?5",
        )?;
        let rows = stmt.query_map(
            params![tree, since, reader, topic, limit as i64],
            row_to_entry,
        )?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Adds a task; every id in `after` must be a task of the same tree.
    pub fn add_task(
        &self,
        tree: &str,
        author: &str,
        title: &str,
        detail: Option<&str>,
        after: &[i64],
        now: i64,
    ) -> Result<i64, AgentsError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        for dep in after {
            let found: Option<String> = tx
                .query_row("SELECT tree FROM work WHERE id = ?1", params![dep], |r| {
                    r.get(0)
                })
                .optional()?;
            if found.as_deref() != Some(tree) {
                return Err(AgentsError::Invalid(format!(
                    "there is no task {dep} in this tree"
                )));
            }
        }
        tx.execute(
            "INSERT INTO work (tree, author, title, detail, status, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, 'open', ?5, ?5)",
            params![tree, author, title, detail, now],
        )?;
        let id = tx.last_insert_rowid();
        for dep in after {
            tx.execute(
                "INSERT OR IGNORE INTO work_deps (task, after) VALUES (?1, ?2)",
                params![id, dep],
            )?;
        }
        tx.commit()?;
        Ok(id)
    }

    pub fn task(&self, id: i64) -> Result<Option<Task>, AgentsError> {
        let conn = self.conn.lock().unwrap();
        let task = conn
            .query_row(
                &format!("SELECT {TASK_COLUMNS} FROM work WHERE id = ?1"),
                params![id],
                row_to_task,
            )
            .optional()?;
        Ok(match task {
            Some(mut t) => {
                t.after = deps(&conn, t.id)?;
                Some(t)
            }
            None => None,
        })
    }

    pub fn tasks(&self, tree: &str) -> Result<Vec<Task>, AgentsError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {TASK_COLUMNS} FROM work WHERE tree = ?1 ORDER BY id"
        ))?;
        let mut tasks: Vec<Task> = stmt
            .query_map(params![tree], row_to_task)?
            .collect::<Result<_, _>>()?;
        for t in &mut tasks {
            t.after = deps(&conn, t.id)?;
        }
        Ok(tasks)
    }

    /// Claims task `id`, or the oldest open task of `tree` whose
    /// dependencies are done, for `claimer`. Only tasks written by one of
    /// `authors` can be claimed. One immediate transaction: two claimers
    /// never get the same task.
    pub fn claim(
        &self,
        tree: &str,
        claimer: &str,
        authors: &[String],
        id: Option<i64>,
        now: i64,
    ) -> Result<Result<Task, Option<ClaimRefused>>, AgentsError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let chosen = match id {
            Some(id) => {
                let task = tx
                    .query_row(
                        &format!("SELECT {TASK_COLUMNS} FROM work WHERE id = ?1 AND tree = ?2"),
                        params![id, tree],
                        row_to_task,
                    )
                    .optional()?;
                let Some(task) = task else {
                    return Ok(Err(Some(ClaimRefused::NotFound)));
                };
                if task.status != TaskStatus::Open {
                    return Ok(Err(Some(ClaimRefused::NotOpen(
                        task.status,
                        task.claimed_by,
                    ))));
                }
                if !authors.contains(&task.author) {
                    return Ok(Err(Some(ClaimRefused::NotYours(task.author))));
                }
                let blocked = unmet(&tx, id)?;
                if !blocked.is_empty() {
                    return Ok(Err(Some(ClaimRefused::Blocked(blocked))));
                }
                id
            }
            None => {
                let mut stmt = tx.prepare(
                    "SELECT id, author FROM work w WHERE tree = ?1 AND status = 'open' \
                     AND NOT EXISTS (SELECT 1 FROM work_deps d JOIN work x ON x.id = d.after \
                                     WHERE d.task = w.id AND x.status != 'done') \
                     ORDER BY id",
                )?;
                let open: Vec<(i64, String)> = stmt
                    .query_map(params![tree], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<Result<_, _>>()?;
                drop(stmt);
                match open.into_iter().find(|(_, a)| authors.contains(a)) {
                    Some((id, _)) => id,
                    None => return Ok(Err(None)),
                }
            }
        };
        tx.execute(
            "UPDATE work SET status = 'claimed', claimed_by = ?2, updated_at = ?3 \
             WHERE id = ?1 AND status = 'open'",
            params![chosen, claimer, now],
        )?;
        let mut task = tx.query_row(
            &format!("SELECT {TASK_COLUMNS} FROM work WHERE id = ?1"),
            params![chosen],
            row_to_task,
        )?;
        task.after = deps(&tx, chosen)?;
        tx.commit()?;
        Ok(Ok(task))
    }

    /// Finishes a task `claimer` holds. False if it doesn't hold it.
    pub fn finish_task(
        &self,
        id: i64,
        claimer: &str,
        result: &str,
        failed: bool,
        now: i64,
    ) -> Result<bool, AgentsError> {
        let status = if failed { "failed" } else { "done" };
        let n = self.conn.lock().unwrap().execute(
            "UPDATE work SET status = ?3, result = ?4, updated_at = ?5 \
             WHERE id = ?1 AND claimed_by = ?2 AND status = 'claimed'",
            params![id, claimer, status, result, now],
        )?;
        Ok(n > 0)
    }

    /// Puts the tasks `agent` holds back to open; how many.
    pub fn release_claims(&self, agent: &str, now: i64) -> Result<usize, AgentsError> {
        Ok(self.conn.lock().unwrap().execute(
            "UPDATE work SET status = 'open', claimed_by = NULL, updated_at = ?2 \
             WHERE claimed_by = ?1 AND status = 'claimed'",
            params![agent, now],
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn authors(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn direct_messages_are_seen_only_by_the_two_ends() {
        let s = AgentStore::in_memory().unwrap();
        s.post("t", "a", None, Some("ci"), "public", 1).unwrap();
        s.post("t", "a", Some("b"), None, "to b", 2).unwrap();
        s.post("other", "x", None, None, "elsewhere", 3).unwrap();
        let bodies = |reader: &str| -> Vec<String> {
            s.read_board("t", reader, 0, None, 50)
                .unwrap()
                .into_iter()
                .map(|e| e.body)
                .collect()
        };
        assert_eq!(bodies("a"), ["public", "to b"]);
        assert_eq!(bodies("b"), ["public", "to b"]);
        assert_eq!(bodies("c"), ["public"]);
        let ci = s.read_board("t", "c", 0, Some("ci"), 50).unwrap();
        assert_eq!(ci.len(), 1);
        assert!(s
            .read_board("t", "c", ci[0].id, None, 50)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn claims_follow_dependencies_and_authors() {
        let s = AgentStore::in_memory().unwrap();
        let root = authors(&["r"]);
        let first = s.add_task("t", "r", "first", None, &[], 1).unwrap();
        let second = s.add_task("t", "r", "second", None, &[first], 1).unwrap();
        let theirs = s.add_task("t", "x", "not yours", None, &[], 1).unwrap();
        assert!(matches!(
            s.add_task("t", "r", "bad", None, &[999], 1),
            Err(AgentsError::Invalid(_))
        ));

        assert_eq!(
            s.claim("t", "w", &root, Some(second), 2).unwrap(),
            Err(Some(ClaimRefused::Blocked(vec![first])))
        );
        assert_eq!(
            s.claim("t", "w", &root, Some(theirs), 2).unwrap(),
            Err(Some(ClaimRefused::NotYours("x".into())))
        );
        let got = s.claim("t", "w1", &root, None, 2).unwrap().unwrap();
        assert_eq!(got.id, first);
        // The next is blocked until the first is done, and "not yours" is
        // never offered.
        assert_eq!(s.claim("t", "w2", &root, None, 2).unwrap(), Err(None));
        assert!(!s.finish_task(first, "w2", "not mine", false, 3).unwrap());
        assert!(s.finish_task(first, "w1", "ok", false, 3).unwrap());
        let got = s.claim("t", "w2", &root, None, 4).unwrap().unwrap();
        assert_eq!((got.id, got.after.clone()), (second, vec![first]));

        assert_eq!(s.release_claims("w2", 5).unwrap(), 1);
        let t = s.task(second).unwrap().unwrap();
        assert_eq!((t.status, t.claimed_by), (TaskStatus::Open, None));
    }
}
