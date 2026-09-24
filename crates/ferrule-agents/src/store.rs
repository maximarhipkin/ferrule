//! `agents.db`: who exists, who spawned whom, what each one answered, and
//! what each tree has spent. Same rusqlite + WAL pattern as `tasks.db` and
//! `memory.db`: one file, `CREATE TABLE IF NOT EXISTS` on open, every
//! "now" passed in so time-dependent rules are testable without sleeping.

use crate::error::AgentsError;
use rusqlite::{params, Connection, OptionalExtension, Row};
use std::fs::{File, TryLockError};
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
    /// The commit the worktree started at.
    pub base: Option<String>,
    pub status: Status,
    pub result: Option<String>,
    pub tokens: u64,
    pub created_at: i64,
    pub updated_at: i64,
}

pub struct AgentStore {
    pub(crate) conn: Mutex<Connection>,
    /// Lock files, one per process with a supervisor: `agents.owners/`
    /// next to the db. None in memory.
    owners_dir: Option<PathBuf>,
    /// This process's id and its held lock, once `claim_process` ran.
    owner: Mutex<Option<(String, File)>>,
}

const AGENT_COLUMNS: &str =
    "id, tree, parent, depth, name, role, task, session, workspace, worktree, branch, \
     base, status, result, tokens, created_at, updated_at";

impl AgentStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, AgentsError> {
        let path = path.as_ref();
        let owners = path
            .parent()
            .unwrap_or(Path::new("."))
            .join("agents.owners");
        Self::init(Connection::open(path)?, Some(owners))
    }

    pub fn in_memory() -> Result<Self, AgentsError> {
        Self::init(Connection::open_in_memory()?, None)
    }

    fn init(conn: Connection, owners_dir: Option<PathBuf>) -> Result<Self, AgentsError> {
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
                 base TEXT,
                 status TEXT NOT NULL,
                 result TEXT,
                 tokens INTEGER NOT NULL DEFAULT 0,
                 created_at INTEGER NOT NULL,
                 updated_at INTEGER NOT NULL,
                 owner TEXT
             );
             CREATE INDEX IF NOT EXISTS idx_agents_tree ON agents(tree, status);
             CREATE INDEX IF NOT EXISTS idx_agents_parent ON agents(parent, status);
             CREATE TABLE IF NOT EXISTS spend (
                 tree TEXT NOT NULL,
                 agent TEXT NOT NULL,
                 tokens INTEGER NOT NULL,
                 at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_spend_tree ON spend(tree, at);
             CREATE TABLE IF NOT EXISTS board (
                 id INTEGER PRIMARY KEY,
                 tree TEXT NOT NULL,
                 author TEXT NOT NULL,
                 recipient TEXT,
                 topic TEXT,
                 body TEXT NOT NULL,
                 created_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_board_tree ON board(tree, id);
             CREATE TABLE IF NOT EXISTS work (
                 id INTEGER PRIMARY KEY,
                 tree TEXT NOT NULL,
                 author TEXT NOT NULL,
                 title TEXT NOT NULL,
                 detail TEXT,
                 status TEXT NOT NULL,
                 claimed_by TEXT,
                 result TEXT,
                 created_at INTEGER NOT NULL,
                 updated_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_work_tree ON work(tree, status);
             CREATE TABLE IF NOT EXISTS work_deps (
                 task INTEGER NOT NULL,
                 after INTEGER NOT NULL,
                 PRIMARY KEY (task, after)
             );",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
            owners_dir,
            owner: Mutex::new(None),
        })
    }

    /// Makes this process the owner of the agents it runs from now on: a
    /// lock file it holds until it exits, so another process (a `ferrule
    /// run` next to the gateway) can tell its running agents are alive.
    pub fn claim_process(&self) -> Result<(), AgentsError> {
        let mut owner = self.owner.lock().unwrap();
        if owner.is_some() {
            return Ok(());
        }
        let id = uuid::Uuid::new_v4().simple().to_string();
        let Some(dir) = &self.owners_dir else {
            *owner = Some((id, tempfile_lock()?));
            return Ok(());
        };
        std::fs::create_dir_all(dir)?;
        let file = File::create(dir.join(format!("{id}.lock")))?;
        file.try_lock().map_err(|e| match e {
            TryLockError::Error(e) => AgentsError::Io(e),
            TryLockError::WouldBlock => AgentsError::Invalid("owner lock is taken".into()),
        })?;
        *owner = Some((id, file));
        Ok(())
    }

    fn owner_id(&self) -> Option<String> {
        self.owner
            .lock()
            .unwrap()
            .as_ref()
            .map(|(id, _)| id.clone())
    }

    /// Whether `owner` is another process that is still alive.
    fn alive_elsewhere(&self, owner: &str) -> bool {
        if self.owner_id().as_deref() == Some(owner) {
            return false;
        }
        let Some(dir) = &self.owners_dir else {
            return false;
        };
        let Ok(file) = File::open(dir.join(format!("{owner}.lock"))) else {
            return false;
        };
        matches!(file.try_lock(), Err(TryLockError::WouldBlock))
    }

    /// Whether `id` is running in another ferrule process, which alone can
    /// stop it.
    pub fn running_elsewhere(&self, id: &str) -> Result<bool, AgentsError> {
        let owner: Option<Option<String>> = self
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT owner FROM agents WHERE id = ?1 AND status = 'running'",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(owner.flatten().is_some_and(|o| self.alive_elsewhere(&o)))
    }

    pub fn insert(&self, a: &AgentRow) -> Result<(), AgentsError> {
        self.conn.lock().unwrap().execute(
            &format!(
                "INSERT INTO agents ({AGENT_COLUMNS}, owner) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)"
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
                a.base,
                a.status.as_str(),
                a.result,
                a.tokens as i64,
                a.created_at,
                a.updated_at,
                self.owner_id(),
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
            "UPDATE agents SET status = ?2, updated_at = ?3, owner = ?4 WHERE id = ?1",
            params![id, status.as_str(), now, self.owner_id()],
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

    /// Records the copy of the repo an agent works in.
    pub fn set_worktree(
        &self,
        id: &str,
        workspace: &Path,
        worktree: &Path,
        branch: Option<&str>,
        base: &str,
    ) -> Result<(), AgentsError> {
        self.conn.lock().unwrap().execute(
            "UPDATE agents SET workspace = ?2, worktree = ?3, branch = ?4, base = ?5 WHERE id = ?1",
            params![
                id,
                workspace.to_string_lossy(),
                worktree.to_string_lossy(),
                branch,
                base
            ],
        )?;
        Ok(())
    }

    /// Back to working in `workspace`, with no copy of the repo.
    pub fn clear_worktree(&self, id: &str, workspace: &Path) -> Result<(), AgentsError> {
        self.conn.lock().unwrap().execute(
            "UPDATE agents SET workspace = ?2, worktree = NULL, branch = NULL, base = NULL WHERE id = ?1",
            params![id, workspace.to_string_lossy()],
        )?;
        Ok(())
    }

    /// After a restart nothing of ours is running: whatever was running in
    /// a process that is gone is `interrupted`. Agents another live process
    /// runs are left alone. Returns the ids marked.
    pub fn mark_interrupted(&self, now: i64) -> Result<Vec<String>, AgentsError> {
        let running: Vec<(String, Option<String>)> = {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn.prepare("SELECT id, owner FROM agents WHERE status = 'running'")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<Result<_, _>>()?
        };
        let mine = self.owner_id();
        let gone: Vec<String> = running
            .into_iter()
            .filter(|(_, owner)| match owner {
                Some(o) => mine.as_deref() != Some(o.as_str()) && !self.alive_elsewhere(o),
                None => true,
            })
            .map(|(id, _)| id)
            .collect();
        let conn = self.conn.lock().unwrap();
        for id in &gone {
            conn.execute(
                "UPDATE agents SET status = 'interrupted', updated_at = ?2 WHERE id = ?1 AND status = 'running'",
                params![id, now],
            )?;
        }
        drop(conn);
        // Lock files left by processes that are gone.
        if let Some(dir) = &self.owners_dir {
            let files = std::fs::read_dir(dir).into_iter().flatten().flatten();
            let stale = files.filter_map(|f| {
                let name = f.file_name().to_string_lossy().into_owned();
                let owner = name.strip_suffix(".lock")?.to_string();
                (self.owner_id().as_deref() != Some(owner.as_str())
                    && !self.alive_elsewhere(&owner))
                .then_some(f.path())
            });
            for path in stale.collect::<Vec<_>>() {
                let _ = std::fs::remove_file(path);
            }
        }
        Ok(gone)
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

/// An in-memory store's stand-in for a lock file: an anonymous temp file.
fn tempfile_lock() -> Result<File, AgentsError> {
    let path = std::env::temp_dir().join(format!(
        "ferrule-agents-{}.lock",
        uuid::Uuid::new_v4().simple()
    ));
    let file = File::create(&path)?;
    let _ = std::fs::remove_file(&path);
    Ok(file)
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
        base: r.get(11)?,
        status: Status::parse(&r.get::<_, String>(12)?),
        result: r.get(13)?,
        tokens: r.get::<_, i64>(14)? as u64,
        created_at: r.get(15)?,
        updated_at: r.get(16)?,
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
            base: None,
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
    fn a_live_process_keeps_its_running_agents() {
        let path = tempfile::tempdir().unwrap();
        let db = path.path().join("agents.db");
        let owners = || {
            std::fs::read_dir(path.path().join("agents.owners"))
                .unwrap()
                .count()
        };
        // The gateway: claims, runs "a".
        let gateway = AgentStore::open(&db).unwrap();
        gateway.claim_process().unwrap();
        gateway
            .insert(&row("a", Some("t"), Status::Running))
            .unwrap();
        // A process that died with "b" running.
        {
            let dead = AgentStore::open(&db).unwrap();
            dead.claim_process().unwrap();
            dead.insert(&row("b", Some("t"), Status::Running)).unwrap();
        }
        assert_eq!(owners(), 2);
        // A `ferrule run` starting next to the gateway.
        let run = AgentStore::open(&db).unwrap();
        run.claim_process().unwrap();
        assert_eq!(run.mark_interrupted(5).unwrap(), vec!["b".to_string()]);
        assert_eq!(run.get("a").unwrap().unwrap().status, Status::Running);
        assert!(run.running_elsewhere("a").unwrap());
        assert!(!run.running_elsewhere("b").unwrap());
        assert!(!gateway.running_elsewhere("a").unwrap());
        // The dead process's lock file is swept; the two live ones stay.
        assert_eq!(owners(), 2);
        // Once the gateway is gone its agent counts as interrupted too.
        drop(gateway);
        assert!(!run.running_elsewhere("a").unwrap());
        assert_eq!(run.mark_interrupted(6).unwrap(), vec!["a".to_string()]);
        assert_eq!(owners(), 1);
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
