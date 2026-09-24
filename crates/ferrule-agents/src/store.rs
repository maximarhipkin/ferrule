//! `agents.db`: who exists, who spawned whom, what each one answered, and
//! what each tree has spent. Same rusqlite + WAL pattern as `tasks.db` and
//! `memory.db`: one file, `CREATE TABLE IF NOT EXISTS` on open, every
//! "now" passed in so time-dependent rules are testable without sleeping.

use crate::error::AgentsError;
use rusqlite::{params, Connection, OptionalExtension, Row};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Running,
    Idle,
    Failed,
    Interrupted,
    Closed,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Running => "running",
            Status::Idle => "idle",
            Status::Failed => "failed",
            Status::Interrupted => "interrupted",
            Status::Closed => "closed",
        }
    }

    fn parse(s: &str) -> Status {
        match s {
            "running" => Status::Running,
            "idle" => Status::Idle,
            "failed" => Status::Failed,
            "interrupted" => Status::Interrupted,
            _ => Status::Closed,
        }
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One agent: the root of a tree (the session the owner talks to) or a
/// child some agent spawned.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentRow {
    pub id: String,
    pub tree: String,
    pub parent: Option<String>,
    pub depth: u32,
    pub name: Option<String>,
    /// `root`, `worker`, `planner` or `verifier`.
    pub role: String,
    pub task: String,
    /// Transcript id: `sessions/<session>.jsonl`.
    pub session: String,
    pub workspace: PathBuf,
    pub worktree: Option<PathBuf>,
    pub branch: Option<String>,
    pub status: Status,
    pub result: Option<String>,
    pub tokens: u64,
    pub created_at: i64,
    pub updated_at: i64,
}

pub struct AgentStore {
    conn: Mutex<Connection>,
}

const AGENT_COLUMNS: &str =
    "id, tree, parent, depth, name, role, task, session, workspace, worktree, branch, \
     status, result, tokens, created_at, updated_at";

impl AgentStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, AgentsError> {
        Self::init(Connection::open(path)?)
    }

    pub fn in_memory() -> Result<Self, AgentsError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self, AgentsError> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS agents (
                 id TEXT PRIMARY KEY,
                 tree TEXT NOT NULL,
                 parent TEXT,
                 depth INTEGER NOT NULL,
                 name TEXT,
                 role TEXT NOT NULL,
                 task TEXT NOT NULL,
                 session TEXT NOT NULL,
                 workspace TEXT NOT NULL,
                 worktree TEXT,
                 branch TEXT,
                 status TEXT NOT NULL,
                 result TEXT,
                 tokens INTEGER NOT NULL DEFAULT 0,
                 created_at INTEGER NOT NULL,
                 updated_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_agents_tree ON agents(tree, status);
             CREATE INDEX IF NOT EXISTS idx_agents_parent ON agents(parent, status);
             CREATE TABLE IF NOT EXISTS spend (
                 tree TEXT NOT NULL,
                 agent TEXT NOT NULL,
                 tokens INTEGER NOT NULL,
                 at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_spend_tree ON spend(tree, at);",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn insert(&self, a: &AgentRow) -> Result<(), AgentsError> {
        self.conn.lock().unwrap().execute(
            &format!(
                "INSERT INTO agents ({AGENT_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)"
            ),
            params![
                a.id,
                a.tree,
                a.parent,
                a.depth,
                a.name,
                a.role,
                a.task,
                a.session,
                a.workspace.to_string_lossy(),
                a.worktree.as_ref().map(|p| p.to_string_lossy().into_owned()),
                a.branch,
                a.status.as_str(),
                a.result,
                a.tokens as i64,
                a.created_at,
                a.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn get(&self, id: &str) -> Result<Option<AgentRow>, AgentsError> {
        Ok(self
            .conn
            .lock()
            .unwrap()
            .query_row(
                &format!("SELECT {AGENT_COLUMNS} FROM agents WHERE id = ?1"),
                params![id],
                row_to_agent,
            )
            .optional()?)
    }

    fn query(&self, filter: &str, arg: &str) -> Result<Vec<AgentRow>, AgentsError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {AGENT_COLUMNS} FROM agents WHERE {filter} ORDER BY created_at, rowid"
        ))?;
        let rows = stmt.query_map(params![arg], row_to_agent)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn children(&self, parent: &str) -> Result<Vec<AgentRow>, AgentsError> {
        self.query("parent = ?1", parent)
    }

    pub fn tree(&self, tree: &str) -> Result<Vec<AgentRow>, AgentsError> {
        self.query("tree = ?1", tree)
    }

    pub fn all(&self) -> Result<Vec<AgentRow>, AgentsError> {
        self.query("?1 = ?1", "")
    }

    /// Children of `parent` running right now.
    pub fn running_children(&self, parent: &str) -> Result<usize, AgentsError> {
        Ok(self.conn.lock().unwrap().query_row(
            "SELECT COUNT(*) FROM agents WHERE parent = ?1 AND status = 'running'",
            params![parent],
            |r| r.get::<_, i64>(0),
        )? as usize)
    }

    /// Spawned agents in `tree` that aren't closed (the root isn't counted).
    pub fn open_in_tree(&self, tree: &str) -> Result<usize, AgentsError> {
        Ok(self.conn.lock().unwrap().query_row(
            "SELECT COUNT(*) FROM agents WHERE tree = ?1 AND parent IS NOT NULL AND status != 'closed'",
            params![tree],
            |r| r.get::<_, i64>(0),
        )? as usize)
    }

    pub fn set_status(&self, id: &str, status: Status, now: i64) -> Result<(), AgentsError> {
        self.conn.lock().unwrap().execute(
            "UPDATE agents SET status = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, status.as_str(), now],
        )?;
        Ok(())
    }

    /// Records how a run ended, unless the agent was closed meanwhile (a
    /// closed agent stays closed).
    pub fn finish(
        &self,
        id: &str,
        status: Status,
        result: &str,
        now: i64,
    ) -> Result<bool, AgentsError> {
        let n = self.conn.lock().unwrap().execute(
            "UPDATE agents SET status = ?2, result = ?3, updated_at = ?4 WHERE id = ?1 AND status != 'closed'",
            params![id, status.as_str(), result, now],
        )?;
        Ok(n > 0)
    }

    pub fn set_worktree(
        &self,
        id: &str,
        worktree: Option<&Path>,
        branch: Option<&str>,
    ) -> Result<(), AgentsError> {
        self.conn.lock().unwrap().execute(
            "UPDATE agents SET worktree = ?2, branch = ?3 WHERE id = ?1",
            params![
                id,
                worktree.map(|p| p.to_string_lossy().into_owned()),
                branch
            ],
        )?;
        Ok(())
    }

    /// After a restart nothing is running: whatever was is `interrupted`.
    /// Returns their ids.
    pub fn mark_interrupted(&self, now: i64) -> Result<Vec<String>, AgentsError> {
        let conn = self.conn.lock().unwrap();
        let ids: Vec<String> = {
            let mut stmt = conn.prepare("SELECT id FROM agents WHERE status = 'running'")?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            rows.collect::<Result<_, _>>()?
        };
        conn.execute(
            "UPDATE agents SET status = 'interrupted', updated_at = ?1 WHERE status = 'running'",
            params![now],
        )?;
        Ok(ids)
    }

    /// One provider call's tokens, charged to the agent and its tree.
    pub fn charge(
        &self,
        tree: &str,
        agent: &str,
        tokens: u64,
        now: i64,
    ) -> Result<(), AgentsError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO spend (tree, agent, tokens, at) VALUES (?1, ?2, ?3, ?4)",
            params![tree, agent, tokens as i64, now],
        )?;
        conn.execute(
            "UPDATE agents SET tokens = tokens + ?2 WHERE id = ?1",
            params![agent, tokens as i64],
        )?;
        Ok(())
    }

    /// Tokens `tree` spent at or after `since`.
    pub fn spent_since(&self, tree: &str, since: i64) -> Result<u64, AgentsError> {
        Ok(self.conn.lock().unwrap().query_row(
            "SELECT COALESCE(SUM(tokens), 0) FROM spend WHERE tree = ?1 AND at >= ?2",
            params![tree, since],
            |r| r.get::<_, i64>(0),
        )? as u64)
    }
}

fn row_to_agent(r: &Row) -> rusqlite::Result<AgentRow> {
    Ok(AgentRow {
        id: r.get(0)?,
        tree: r.get(1)?,
        parent: r.get(2)?,
        depth: r.get(3)?,
        name: r.get(4)?,
        role: r.get(5)?,
        task: r.get(6)?,
        session: r.get(7)?,
        workspace: PathBuf::from(r.get::<_, String>(8)?),
        worktree: r.get::<_, Option<String>>(9)?.map(PathBuf::from),
        branch: r.get(10)?,
        status: Status::parse(&r.get::<_, String>(11)?),
        result: r.get(12)?,
        tokens: r.get::<_, i64>(13)? as u64,
        created_at: r.get(14)?,
        updated_at: r.get(15)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, parent: Option<&str>, status: Status) -> AgentRow {
        AgentRow {
            id: id.into(),
            tree: "t".into(),
            parent: parent.map(Into::into),
            depth: u32::from(parent.is_some()),
            name: None,
            role: if parent.is_some() { "worker" } else { "root" }.into(),
            task: "x".into(),
            session: id.into(),
            workspace: PathBuf::from("/w"),
            worktree: None,
            branch: None,
            status,
            result: None,
            tokens: 0,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn counts_running_children_and_open_agents() {
        let s = AgentStore::in_memory().unwrap();
        s.insert(&row("t", None, Status::Idle)).unwrap();
        s.insert(&row("a", Some("t"), Status::Running)).unwrap();
        s.insert(&row("b", Some("t"), Status::Idle)).unwrap();
        s.insert(&row("c", Some("t"), Status::Closed)).unwrap();
        assert_eq!(s.running_children("t").unwrap(), 1);
        assert_eq!(s.open_in_tree("t").unwrap(), 2);
        assert_eq!(s.children("t").unwrap().len(), 3);
        assert_eq!(
            s.get("a").unwrap().unwrap(),
            row("a", Some("t"), Status::Running)
        );
    }

    #[test]
    fn finish_leaves_a_closed_agent_closed() {
        let s = AgentStore::in_memory().unwrap();
        s.insert(&row("a", Some("t"), Status::Running)).unwrap();
        s.set_status("a", Status::Closed, 1).unwrap();
        assert!(!s.finish("a", Status::Idle, "late", 2).unwrap());
        assert_eq!(s.get("a").unwrap().unwrap().status, Status::Closed);
    }

    #[test]
    fn restart_marks_running_interrupted() {
        let path = tempfile::tempdir().unwrap();
        let db = path.path().join("agents.db");
        {
            let s = AgentStore::open(&db).unwrap();
            s.insert(&row("a", Some("t"), Status::Running)).unwrap();
            s.insert(&row("b", Some("t"), Status::Idle)).unwrap();
        }
        let s = AgentStore::open(&db).unwrap();
        assert_eq!(s.mark_interrupted(5).unwrap(), vec!["a".to_string()]);
        assert_eq!(s.get("a").unwrap().unwrap().status, Status::Interrupted);
        assert_eq!(s.get("b").unwrap().unwrap().status, Status::Idle);
    }

    #[test]
    fn spend_is_windowed_per_tree() {
        let s = AgentStore::in_memory().unwrap();
        s.insert(&row("a", Some("t"), Status::Running)).unwrap();
        s.charge("t", "a", 100, 10).unwrap();
        s.charge("t", "a", 50, 20).unwrap();
        s.charge("other", "z", 999, 20).unwrap();
        assert_eq!(s.spent_since("t", 0).unwrap(), 150);
        assert_eq!(s.spent_since("t", 15).unwrap(), 50);
        assert_eq!(s.get("a").unwrap().unwrap().tokens, 150);
    }
}
