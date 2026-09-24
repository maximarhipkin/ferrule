//! Local-first agent memory in a single SQLite file.
//!
//! Design follows the 2026 consensus for agent runtimes: one file you can
//! read, back up, and `git diff`; FTS5 (BM25) keyword recall built in;
//! time-decay scoring so recent memories outrank stale ones; a token budget
//! on recall so memory never eats the context window. Vector/semantic recall
//! plugs into the same tables later (embedding column is already reserved).

use rusqlite::{params, Connection};
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

#[derive(Debug, Clone)]
pub struct Memory {
    pub id: i64,
    pub content: String,
    pub tags: Vec<String>,
    pub created_at: i64,
    pub score: f64,
}

pub struct MemoryStore {
    conn: Connection,
}

impl MemoryStore {
    /// Open (or create) a memory database. Pass ":memory:" for tests.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, MemoryError> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn in_memory() -> Result<Self, MemoryError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self, MemoryError> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS memories (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 content TEXT NOT NULL,
                 tags TEXT NOT NULL DEFAULT '',
                 created_at INTEGER NOT NULL,
                 embedding BLOB
             );
             CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
                 content, tags, content='memories', content_rowid='id'
             );
             CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
                 INSERT INTO memories_fts(rowid, content, tags) VALUES (new.id, new.content, new.tags);
             END;
             CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
                 INSERT INTO memories_fts(memories_fts, rowid, content, tags) VALUES('delete', old.id, old.content, old.tags);
             END;",
        )?;
        Ok(Self { conn })
    }

    pub fn remember(&self, content: &str, tags: &[&str]) -> Result<i64, MemoryError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        self.conn.execute(
            "INSERT INTO memories (content, tags, created_at) VALUES (?1, ?2, ?3)",
            params![content, tags.join(","), now],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// BM25 keyword search with time-decay scoring: score = bm25 * decay,
    /// where decay halves every 7 days (ZeroClaw's proven half-life).
    pub fn recall(&self, query: &str, limit: usize) -> Result<Vec<Memory>, MemoryError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as f64)
            .unwrap_or(0.0);
        let half_life_secs = 7.0 * 24.0 * 3600.0;

        let mut stmt = self.conn.prepare(
            "SELECT m.id, m.content, m.tags, m.created_at, bm25(memories_fts) AS rank
             FROM memories_fts JOIN memories m ON m.id = memories_fts.rowid
             WHERE memories_fts MATCH ?1
             ORDER BY rank LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![fts_escape(query), limit as i64], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, f64>(4)?,
            ))
        })?;

        let mut out = Vec::new();
        for row in rows {
            let (id, content, tags, created_at, rank) = row?;
            let age_secs = (now - created_at as f64).max(0.0);
            let decay = 0.5f64.powf(age_secs / half_life_secs);
            // bm25 returns negative values; more negative = better match.
            let score = (-rank) * decay;
            out.push(Memory {
                id,
                content,
                tags: split_tags(&tags),
                created_at,
                score,
            });
        }
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(out)
    }

    /// Most recent memories, for session-start context assembly.
    pub fn recent(&self, limit: usize) -> Result<Vec<Memory>, MemoryError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, content, tags, created_at FROM memories ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok(Memory {
                id: row.get(0)?,
                content: row.get(1)?,
                tags: split_tags(&row.get::<_, String>(2)?),
                created_at: row.get(3)?,
                score: 0.0,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Assemble recall within a character budget — pinned/recent first, then
    /// query matches — so memory never floods the context window.
    pub fn assemble_context(
        &self,
        query: Option<&str>,
        char_budget: usize,
    ) -> Result<String, MemoryError> {
        let mut seen = std::collections::HashSet::new();
        let mut used = 0usize;
        let mut parts = Vec::new();

        let mut push = |m: &Memory| {
            if seen.insert(m.id) && used + m.content.len() <= char_budget {
                used += m.content.len();
                parts.push(format!("- {}", m.content));
            }
        };

        for m in self.recent(5)? {
            push(&m);
        }
        if let Some(q) = query {
            for m in self.recall(q, 10)? {
                push(&m);
            }
        }
        Ok(parts.join("\n"))
    }

    pub fn forget(&self, id: i64) -> Result<(), MemoryError> {
        self.conn
            .execute("DELETE FROM memories WHERE id = ?1", params![id])?;
        Ok(())
    }
}

fn split_tags(tags: &str) -> Vec<String> {
    tags.split(',')
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect()
}

/// FTS5 query strings are a mini-language; quote terms to keep user/model
/// queries from blowing up the parser.
fn fts_escape(query: &str) -> String {
    query
        .split_whitespace()
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{}\"", t.replace('"', "")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remember_and_recall_by_keyword() {
        let store = MemoryStore::in_memory().unwrap();
        store
            .remember("User prefers Rust and hates garbage collectors", &["pref"])
            .unwrap();
        store
            .remember("Deploy target is a Raspberry Pi Zero", &["infra"])
            .unwrap();

        let hits = store.recall("Rust", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].content.contains("Rust"));
    }

    #[test]
    fn recent_returns_newest_first() {
        let store = MemoryStore::in_memory().unwrap();
        store.remember("first", &[]).unwrap();
        store.remember("second", &[]).unwrap();
        let r = store.recent(2).unwrap();
        assert_eq!(r[0].content, "second");
    }

    #[test]
    fn assemble_context_respects_budget() {
        let store = MemoryStore::in_memory().unwrap();
        for i in 0..10 {
            store
                .remember(&format!("fact number {i} {}", "x".repeat(100)), &[])
                .unwrap();
        }
        let ctx = store.assemble_context(None, 300).unwrap();
        assert!(ctx.len() < 320);
        assert!(ctx.contains("fact number 9"));
    }

    #[test]
    fn weird_queries_do_not_crash_fts() {
        let store = MemoryStore::in_memory().unwrap();
        store.remember("something safe", &[]).unwrap();
        let _ = store.recall("what's OR (broken \"syntax\"", 10).unwrap();
    }
}
