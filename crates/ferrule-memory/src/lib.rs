//! Local-first agent memory in a single SQLite file.
//!
//! Design follows the 2026 consensus for agent runtimes: one file you can
//! read, back up, and `git diff`; FTS5 (BM25) keyword recall built in;
//! time-decay scoring so recent memories outrank stale ones; a token budget
//! on recall so memory never eats the context window.
//!
//! Vector recall (M30) lives in the same table: `embedding` holds a
//! normalised f32 vector and `embedding_model` the id of the model that made
//! it. The store never embeds anything itself; the caller passes vectors in
//! ([`MemoryStore::set_embedding`], [`MemoryStore::recall_hybrid`]), so this
//! crate stays free of any model or network code.

#[doc(hidden)]
pub mod bench;

use rusqlite::{params, Connection};
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The store was written by a newer ferrule; refusing to touch a schema
    /// this build does not know.
    #[error("memory store has schema version {0}, newer than this ferrule understands ({SCHEMA_VERSION}); upgrade ferrule")]
    NewerSchema(i64),
    /// A write the store refuses on purpose (unknown id, an already
    /// replaced fact). The message is written for the model to act on.
    #[error("{0}")]
    Refused(String),
}

/// The schema this build writes, kept in `PRAGMA user_version`.
/// 0: before M15. 1: M15 — `superseded_by` / `superseded_at`.
pub const SCHEMA_VERSION: i64 = 1;

/// Token-set Jaccard at or above which a new fact is the same fact (NOOP).
const DUPLICATE_JACCARD: f64 = 0.9;
/// Jaccard at or above which a live fact is shown back as "similar".
const SIMILAR_JACCARD: f64 = 0.3;

#[derive(Debug, Clone)]
pub struct Memory {
    pub id: i64,
    pub content: String,
    pub tags: Vec<String>,
    pub created_at: i64,
    pub score: f64,
    /// The row that replaced this one; `None` while the fact is live.
    pub superseded_by: Option<i64>,
}

/// What [`MemoryStore::insert`] did with a fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Inserted as a new live fact.
    Added,
    /// Already known: nothing written, `id` is the existing live fact.
    Noop,
    /// Became the live replacement of the ids in `replaced`.
    Updated,
}

#[derive(Debug, Clone)]
pub struct Inserted {
    pub id: i64,
    pub decision: Decision,
    /// Ids this write superseded (empty unless `Updated`).
    pub replaced: Vec<i64>,
    /// Live facts that look related but are not duplicates — the model
    /// decides whether the new fact corrects one of them.
    pub similar: Vec<Memory>,
}

pub struct MemoryStore {
    conn: Connection,
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl MemoryStore {
    /// Open (or create) a memory database, migrating an older schema in place.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, MemoryError> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn in_memory() -> Result<Self, MemoryError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self, MemoryError> {
        // The gateway and a `ferrule memory` command may write at once: wait
        // instead of failing with "database is locked". secure_delete zeroes
        // the pages a `forget` frees.
        conn.busy_timeout(std::time::Duration::from_millis(5_000))?;
        conn.execute_batch("PRAGMA secure_delete = ON;")?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(MemoryError::NewerSchema(version));
        }
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
        if version < SCHEMA_VERSION {
            migrate(&conn)?;
        }
        ensure_vector_column(&conn)?;
        Ok(Self { conn })
    }

    /// Plain insert, no decision: the pre-M15 behaviour, kept for callers
    /// that want every call to be a new row.
    pub fn remember(&self, content: &str, tags: &[&str]) -> Result<i64, MemoryError> {
        self.conn.execute(
            "INSERT INTO memories (content, tags, created_at) VALUES (?1, ?2, ?3)",
            params![content, tags.join(","), now_secs()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// The M15 write path. With `replaces` empty: NOOP when a live fact
    /// already says the same thing, otherwise ADD (and report similar live
    /// facts). With `replaces`: the new fact becomes the live replacement of
    /// those ids (UPDATE); a live identical row is reused rather than
    /// duplicated. Replaced rows are kept, marked `superseded_by`.
    pub fn insert(
        &self,
        content: &str,
        tags: &[&str],
        replaces: &[i64],
    ) -> Result<Inserted, MemoryError> {
        let content = content.trim();
        if content.is_empty() {
            return Err(MemoryError::Refused(
                "refusing to store an empty fact".into(),
            ));
        }
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let mut replaces: Vec<i64> = replaces.to_vec();
        replaces.sort_unstable();
        replaces.dedup();
        let mut inherited: Vec<String> = Vec::new();
        for &id in &replaces {
            match self.get(id)? {
                None => return Err(MemoryError::Refused(format!("there is no memory #{id}"))),
                Some(m) => {
                    if m.superseded_by.is_some() {
                        let head = self.head(id)?.unwrap_or(id);
                        return Err(MemoryError::Refused(format!(
                            "#{id} was already replaced by #{head} — update #{head} instead"
                        )));
                    }
                    for t in m.tags {
                        if !inherited.contains(&t) {
                            inherited.push(t);
                        }
                    }
                }
            }
        }

        let candidates = self.live_candidates(content)?;
        let norm = normalize(content);
        let words = word_set(content);
        let duplicate = candidates.iter().find(|m| {
            normalize(&m.content) == norm
                || (!words.is_empty()
                    && jaccard(&words, &word_set(&m.content)) >= DUPLICATE_JACCARD)
        });
        let similar: Vec<Memory> = candidates
            .iter()
            .filter(|m| {
                Some(m.id) != duplicate.map(|d| d.id)
                    && !replaces.contains(&m.id)
                    && !words.is_empty()
                    && jaccard(&words, &word_set(&m.content)) >= SIMILAR_JACCARD
            })
            .take(5)
            .cloned()
            .collect();

        if let Some(dup) = duplicate {
            let id = dup.id;
            let rest: Vec<i64> = replaces.iter().copied().filter(|&r| r != id).collect();
            if rest.is_empty() {
                tx.commit()?;
                return Ok(Inserted {
                    id,
                    decision: Decision::Noop,
                    replaced: vec![],
                    similar,
                });
            }
            self.mark_superseded(&rest, id)?;
            tx.commit()?;
            return Ok(Inserted {
                id,
                decision: Decision::Updated,
                replaced: rest,
                similar,
            });
        }

        let tags: Vec<String> = if tags.is_empty() && !replaces.is_empty() {
            inherited
        } else {
            tags.iter().map(|t| t.to_string()).collect()
        };
        self.conn.execute(
            "INSERT INTO memories (content, tags, created_at) VALUES (?1, ?2, ?3)",
            params![content, tags.join(","), now_secs()],
        )?;
        let id = self.conn.last_insert_rowid();
        self.mark_superseded(&replaces, id)?;
        tx.commit()?;
        let decision = if replaces.is_empty() {
            Decision::Added
        } else {
            Decision::Updated
        };
        Ok(Inserted {
            id,
            decision,
            replaced: replaces,
            similar,
        })
    }

    /// The live fact that already says `content`: the NOOP check `insert`
    /// makes, without writing anything.
    pub fn known(&self, content: &str) -> Result<Option<i64>, MemoryError> {
        Ok(self
            .live_candidates(content.trim())?
            .into_iter()
            .find(|m| same_fact(&m.content, content))
            .map(|m| m.id))
    }

    /// Live facts carrying every tag in `tags`, oldest first.
    pub fn live_tagged(&self, tags: &[&str]) -> Result<Vec<Memory>, MemoryError> {
        let Some(first) = tags.first() else {
            return Ok(Vec::new());
        };
        let mut stmt = self.conn.prepare(
            "SELECT id, content, tags, created_at, superseded_by FROM memories
             WHERE superseded_by IS NULL AND instr(tags, ?1) > 0 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![first], row_to_memory)?;
        let mut out = Vec::new();
        for m in rows {
            let m = m?;
            if tags.iter().all(|t| m.tags.iter().any(|have| have == t)) {
                out.push(m);
            }
        }
        Ok(out)
    }

    /// `insert(content, tags, [id])`: replace one live fact.
    pub fn supersede(
        &self,
        id: i64,
        content: &str,
        tags: &[&str],
    ) -> Result<Inserted, MemoryError> {
        self.insert(content, tags, &[id])
    }

    fn mark_superseded(&self, ids: &[i64], by: i64) -> Result<(), MemoryError> {
        let now = now_secs();
        for &old in ids {
            self.conn.execute(
                "UPDATE memories SET superseded_by = ?1, superseded_at = ?2 WHERE id = ?3",
                params![by, now, old],
            )?;
        }
        Ok(())
    }

    /// One row by id, live or not.
    pub fn get(&self, id: i64) -> Result<Option<Memory>, MemoryError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, content, tags, created_at, superseded_by FROM memories WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(params![id], row_to_memory)?;
        Ok(rows.next().transpose()?)
    }

    /// The live end of `id`'s chain (`id` itself when it is live).
    /// `None` when `id` does not exist.
    pub fn head(&self, id: i64) -> Result<Option<i64>, MemoryError> {
        let mut cur = match self.get(id)? {
            None => return Ok(None),
            Some(m) => m,
        };
        // A chain only ever points at larger ids, so this terminates; the
        // bound guards a hand-edited store.
        for _ in 0..10_000 {
            match cur.superseded_by {
                None => break,
                Some(next) => match self.get(next)? {
                    Some(m) => cur = m,
                    // Dangling pointer (a hand edit): treat as live.
                    None => break,
                },
            }
        }
        Ok(Some(cur.id))
    }

    /// Live facts sharing words with `content`: the candidate pool for the
    /// NOOP and "similar" checks.
    fn live_candidates(&self, content: &str) -> Result<Vec<Memory>, MemoryError> {
        let query = fts_escape(&capped_words(content, 32).join(" "));
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT m.id, m.content, m.tags, m.created_at, m.superseded_by
             FROM memories_fts JOIN memories m ON m.id = memories_fts.rowid
             WHERE memories_fts MATCH ?1 AND m.superseded_by IS NULL
             ORDER BY bm25(memories_fts) LIMIT 20",
        )?;
        let rows = stmt.query_map(params![query], row_to_memory)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// BM25 keyword search with time-decay scoring: score = bm25 * decay,
    /// where decay halves every 7 days (ZeroClaw's proven half-life).
    /// Every row is searched, but each hit is reported as the live head of
    /// its chain: a query matching only a replaced fact's wording returns
    /// the correction, never the stale fact.
    pub fn recall(&self, query: &str, limit: usize) -> Result<Vec<Memory>, MemoryError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let scored = self
            .bm25_candidates(query, limit * 5 + 20)?
            .into_iter()
            .map(|c| (c.id, c.created_at, c.superseded_by, -c.rank))
            .collect();
        self.rank_heads(scored, limit)
    }

    /// The top `n` FTS matches of `query`, best first (`rank` is bm25:
    /// negative, more negative is better).
    fn bm25_candidates(&self, query: &str, n: usize) -> Result<Vec<Candidate>, MemoryError> {
        let escaped = fts_escape(query);
        if escaped.is_empty() || n == 0 {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT m.id, m.created_at, m.superseded_by, bm25(memories_fts) AS rank
             FROM memories_fts JOIN memories m ON m.id = memories_fts.rowid
             WHERE memories_fts MATCH ?1
             ORDER BY rank LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![escaped, n as i64], |row| {
            Ok(Candidate {
                id: row.get(0)?,
                created_at: row.get(1)?,
                superseded_by: row.get(2)?,
                rank: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Time decay (the score halves every 7 days), then each row reported
    /// as the live head of its chain, a chain keeping its best score, then
    /// the top `limit`. `scored` is `(id, created_at, superseded_by,
    /// score before decay)`.
    fn rank_heads(
        &self,
        scored: Vec<(i64, i64, Option<i64>, f64)>,
        limit: usize,
    ) -> Result<Vec<Memory>, MemoryError> {
        let now = now_secs() as f64;
        let half_life_secs = 7.0 * 24.0 * 3600.0;
        let mut best: Vec<(i64, f64)> = Vec::new();
        for (id, created_at, superseded_by, raw) in scored {
            let age_secs = (now - created_at as f64).max(0.0);
            let decay = 0.5f64.powf(age_secs / half_life_secs);
            let score = raw * decay;
            let head = match superseded_by {
                None => id,
                Some(_) => match self.head(id)? {
                    Some(h) => h,
                    None => continue,
                },
            };
            match best.iter_mut().find(|(h, _)| *h == head) {
                Some(entry) => entry.1 = entry.1.max(score),
                None => best.push((head, score)),
            }
        }
        best.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        best.truncate(limit);

        let mut out = Vec::with_capacity(best.len());
        for (id, score) in best {
            if let Some(mut m) = self.get(id)? {
                m.score = score;
                out.push(m);
            }
        }
        Ok(out)
    }

    /// Most recent live memories, for session-start context assembly.
    pub fn recent(&self, limit: usize) -> Result<Vec<Memory>, MemoryError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, content, tags, created_at, superseded_by FROM memories
             WHERE superseded_by IS NULL ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], row_to_memory)?;
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

    /// The session-start memory block for a goal: facts matching the goal
    /// first (top 10), then the newest live facts (up to 5) to fill the
    /// budget. Each line is `- #id fact`, so the model can correct a fact
    /// by id in one call.
    pub fn assemble_for_goal(&self, goal: &str, char_budget: usize) -> Result<String, MemoryError> {
        self.assemble_for_goal_hybrid(goal, None, char_budget, &Hybrid::default())
    }

    /// [`MemoryStore::assemble_for_goal`] with the goal's matches found by
    /// [`MemoryStore::recall_hybrid`]. `vector` is the goal embedded by the
    /// caller; `None` is exactly `assemble_for_goal`.
    pub fn assemble_for_goal_hybrid(
        &self,
        goal: &str,
        vector: Option<QueryVector<'_>>,
        char_budget: usize,
        hybrid: &Hybrid,
    ) -> Result<String, MemoryError> {
        let mut seen = std::collections::HashSet::new();
        let mut used = 0usize;
        let mut parts = Vec::new();
        let mut push = |m: &Memory| {
            let line = format!("- #{} {}", m.id, m.content);
            if seen.insert(m.id) && used + line.len() <= char_budget {
                used += line.len() + 1;
                parts.push(line);
            }
        };
        let query = goal_query(goal);
        if !query.is_empty() || vector.is_some() {
            for m in self.recall_hybrid(&query, vector, 10, hybrid)? {
                push(&m);
            }
        }
        for m in self.recent(5)? {
            push(&m);
        }
        Ok(parts.join("\n"))
    }

    /// Hard-delete a fact. Forgetting a live fact deletes it and every
    /// older version in its chain; forgetting an old version deletes just
    /// that row (the chain is re-linked around it). The FTS index is merged
    /// and the WAL truncated so no copy of the text stays in the files.
    /// Returns the deleted ids — empty when `id` does not exist.
    pub fn forget(&self, id: i64) -> Result<Vec<i64>, MemoryError> {
        let row = match self.get(id)? {
            None => return Ok(Vec::new()),
            Some(m) => m,
        };
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let deleted: Vec<i64> = if row.superseded_by.is_some() {
            self.conn.execute(
                "UPDATE memories SET superseded_by = ?1 WHERE superseded_by = ?2",
                params![row.superseded_by, id],
            )?;
            self.conn
                .execute("DELETE FROM memories WHERE id = ?1", params![id])?;
            vec![id]
        } else {
            let mut stmt = self.conn.prepare(
                "WITH RECURSIVE chain(id) AS (
                     SELECT ?1
                     UNION SELECT m.id FROM memories m JOIN chain c ON m.superseded_by = c.id
                 ) SELECT id FROM chain ORDER BY id",
            )?;
            let ids = stmt
                .query_map(params![id], |r| r.get::<_, i64>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            for &d in &ids {
                self.conn
                    .execute("DELETE FROM memories WHERE id = ?1", params![d])?;
            }
            ids
        };
        tx.commit()?;
        // An FTS5 delete is logical until the segments merge; merge now so the
        // tokens leave the index b-tree, then drop the WAL's copies.
        self.conn.execute(
            "INSERT INTO memories_fts(memories_fts) VALUES('optimize')",
            [],
        )?;
        self.conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        Ok(deleted)
    }
}

impl MemoryStore {
    /// The largest id ever written (0 for an empty store). A write whose
    /// id is above the value read before it created a row.
    pub fn max_id(&self) -> Result<i64, MemoryError> {
        Ok(self
            .conn
            .query_row("SELECT COALESCE(MAX(id), 0) FROM memories", [], |r| {
                r.get(0)
            })?)
    }

    /// Groups of near-duplicate live facts: among the newest `max_rows`
    /// live facts, two are linked when their content-word sets have a
    /// Jaccard similarity of at least `threshold`, and each connected group
    /// of two or more is returned (largest first, then newest first; each
    /// group's facts in id order). No model involved — the M16 learning
    /// pass asks one about each group.
    pub fn similar_clusters(
        &self,
        threshold: f64,
        max_rows: usize,
    ) -> Result<Vec<Vec<Memory>>, MemoryError> {
        let rows = self.recent(max_rows)?;
        let sets: Vec<_> = rows.iter().map(|m| word_set(&m.content)).collect();
        let mut parent: Vec<usize> = (0..rows.len()).collect();
        fn find(parent: &mut [usize], mut i: usize) -> usize {
            while parent[i] != i {
                parent[i] = parent[parent[i]];
                i = parent[i];
            }
            i
        }
        for i in 0..rows.len() {
            if sets[i].is_empty() {
                continue;
            }
            for j in i + 1..rows.len() {
                if !sets[j].is_empty() && jaccard(&sets[i], &sets[j]) >= threshold {
                    let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                    if a != b {
                        parent[a] = b;
                    }
                }
            }
        }
        let mut groups: std::collections::HashMap<usize, Vec<Memory>> =
            std::collections::HashMap::new();
        for (i, row) in rows.iter().enumerate() {
            let root = find(&mut parent, i);
            groups.entry(root).or_default().push(row.clone());
        }
        let mut out: Vec<Vec<Memory>> = groups
            .into_values()
            .filter(|g| g.len() >= 2)
            .map(|mut g| {
                g.sort_by_key(|m| m.id);
                g
            })
            .collect();
        out.sort_by(|a, b| {
            b.len()
                .cmp(&a.len())
                .then(b.last().map(|m| m.id).cmp(&a.last().map(|m| m.id)))
        });
        Ok(out)
    }

    /// Undo one UPDATE made by [`MemoryStore::insert`]: the `replaced` rows
    /// that still point at `new_id` become live again, and `new_id` is
    /// deleted when the write `created` it. Refused when `new_id` is gone
    /// or has itself been replaced since — undoing would lose that later
    /// correction. Returns the ids made live again.
    pub fn undo_update(
        &self,
        new_id: i64,
        replaced: &[i64],
        created: bool,
    ) -> Result<Vec<i64>, MemoryError> {
        let row = self
            .get(new_id)?
            .ok_or_else(|| MemoryError::Refused(format!("there is no memory #{new_id}")))?;
        if let Some(by) = row.superseded_by {
            let head = self.head(new_id)?.unwrap_or(by);
            return Err(MemoryError::Refused(format!(
                "#{new_id} has since been replaced by #{head}; not undoing"
            )));
        }
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let mut restored = Vec::new();
        for &old in replaced {
            let n = self.conn.execute(
                "UPDATE memories SET superseded_by = NULL, superseded_at = NULL
                 WHERE id = ?1 AND superseded_by = ?2",
                params![old, new_id],
            )?;
            if n > 0 {
                restored.push(old);
            }
        }
        if created {
            // Anything else that points at it (a later write replacing an
            // unrelated fact with this one) goes back to live too.
            self.conn.execute(
                "UPDATE memories SET superseded_by = NULL, superseded_at = NULL
                 WHERE superseded_by = ?1",
                params![new_id],
            )?;
            self.conn
                .execute("DELETE FROM memories WHERE id = ?1", params![new_id])?;
        }
        tx.commit()?;
        Ok(restored)
    }
}

/// How the two sides of a hybrid recall are combined.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Merge {
    /// `w·cos + (1−w)·bm25/max(bm25)` over the union of candidates, a
    /// missing side counting 0.
    Weighted { vector_weight: f64 },
    /// Reciprocal rank fusion: `Σ 1/(k + rank)` over the two ranked lists.
    Rrf { k: f64 },
}

/// Hybrid recall settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hybrid {
    pub merge: Merge,
    /// Rows whose cosine with the query is below this are not vector
    /// candidates (they can still match by keyword).
    pub min_similarity: f32,
}

impl Default for Hybrid {
    fn default() -> Self {
        Self {
            merge: Merge::Weighted { vector_weight: 0.7 },
            min_similarity: 0.3,
        }
    }
}

/// A query embedded by the caller: the model's id and the (normalised)
/// vector. Only rows embedded by the same model id are compared with it.
#[derive(Debug, Clone, Copy)]
pub struct QueryVector<'a> {
    pub model: &'a str,
    pub vector: &'a [f32],
}

/// How many rows have a vector from a given model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EmbeddingCounts {
    /// Live (not superseded) facts.
    pub live: usize,
    /// Live facts with a vector from this model.
    pub live_embedded: usize,
    /// Rows of any kind (live or replaced) without a vector from this
    /// model: what `ferrule memory reindex` has left to do.
    pub stale: usize,
}

struct Candidate {
    id: i64,
    created_at: i64,
    superseded_by: Option<i64>,
    /// bm25 for a keyword match, cosine for a vector match.
    rank: f64,
}

impl MemoryStore {
    /// Stores `vector` as `id`'s embedding by `model`, if the row still
    /// holds `content` (the text that was embedded). Returns whether it was
    /// stored.
    pub fn set_embedding(
        &self,
        id: i64,
        content: &str,
        model: &str,
        vector: &[f32],
    ) -> Result<bool, MemoryError> {
        let n = self.conn.execute(
            "UPDATE memories SET embedding = ?1, embedding_model = ?2 WHERE id = ?3 AND content = ?4",
            params![vec_to_bytes(vector), model, id, content],
        )?;
        Ok(n > 0)
    }

    /// Up to `limit` rows (live or replaced) with no vector from `model`,
    /// in id order, after `after_id`: the reindex queue.
    pub fn stale_rows(
        &self,
        model: &str,
        after_id: i64,
        limit: usize,
    ) -> Result<Vec<(i64, String)>, MemoryError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, content FROM memories
             WHERE id > ?1 AND embedding_model IS NOT ?2
             ORDER BY id LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![after_id, model, limit as i64], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Up to `limit` live facts with no vector from `model`, newest first:
    /// what the lazy re-embed after a session-start recall picks up.
    pub fn stale_live(&self, model: &str, limit: usize) -> Result<Vec<(i64, String)>, MemoryError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, content FROM memories
             WHERE superseded_by IS NULL AND embedding_model IS NOT ?1
             ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![model, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn embedding_counts(&self, model: &str) -> Result<EmbeddingCounts, MemoryError> {
        Ok(self.conn.query_row(
            "SELECT
                 COALESCE(SUM(superseded_by IS NULL), 0),
                 COALESCE(SUM(superseded_by IS NULL AND embedding_model IS ?1), 0),
                 COALESCE(SUM(embedding_model IS NOT ?1), 0)
             FROM memories",
            params![model],
            |r| {
                Ok(EmbeddingCounts {
                    live: r.get::<_, i64>(0)? as usize,
                    live_embedded: r.get::<_, i64>(1)? as usize,
                    stale: r.get::<_, i64>(2)? as usize,
                })
            },
        )?)
    }

    /// Every row embedded by `query.model` with cosine ≥ `floor`, best
    /// first, at most `n`. Brute force: one pass over the vectors.
    fn vector_candidates(
        &self,
        query: QueryVector<'_>,
        floor: f32,
        n: usize,
    ) -> Result<Vec<Candidate>, MemoryError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, created_at, superseded_by, embedding FROM memories
             WHERE embedding_model = ?1 AND embedding IS NOT NULL",
        )?;
        let mut rows = stmt.query(params![query.model])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let blob = row.get_ref(3)?.as_blob().unwrap_or_default();
            let Some(cos) = cosine_bytes(query.vector, blob) else {
                continue;
            };
            if cos >= floor {
                out.push(Candidate {
                    id: row.get(0)?,
                    created_at: row.get(1)?,
                    superseded_by: row.get(2)?,
                    rank: f64::from(cos),
                });
            }
        }
        out.sort_by(|a, b| {
            b.rank
                .partial_cmp(&a.rank)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.id.cmp(&a.id))
        });
        out.truncate(n);
        Ok(out)
    }

    /// Keyword and vector recall merged. With `vector` `None` this is
    /// exactly [`MemoryStore::recall`]. Otherwise the top `5·limit + 20`
    /// keyword matches and the top `5·limit + 20` rows by cosine (at or
    /// above `min_similarity`, same model only) are merged per
    /// `hybrid.merge`, then decayed and mapped to chain heads as `recall`
    /// does. Rows without a vector from this model take part by keyword
    /// only.
    pub fn recall_hybrid(
        &self,
        query: &str,
        vector: Option<QueryVector<'_>>,
        limit: usize,
        hybrid: &Hybrid,
    ) -> Result<Vec<Memory>, MemoryError> {
        let Some(qv) = vector else {
            return self.recall(query, limit);
        };
        if limit == 0 {
            return Ok(Vec::new());
        }
        let n = limit * 5 + 20;
        let lexical = self.bm25_candidates(query, n)?;
        let semantic = self.vector_candidates(qv, hybrid.min_similarity, n)?;

        // id → (created_at, superseded_by, keyword part, vector part)
        let mut merged: Vec<(i64, i64, Option<i64>, f64)> = Vec::new();
        let mut add = |c: &Candidate, part: f64| match merged.iter_mut().find(|m| m.0 == c.id) {
            Some(m) => m.3 += part,
            None => merged.push((c.id, c.created_at, c.superseded_by, part)),
        };
        match hybrid.merge {
            Merge::Weighted { vector_weight } => {
                let w = vector_weight.clamp(0.0, 1.0);
                let max = lexical.iter().map(|c| -c.rank).fold(0.0f64, f64::max);
                for c in &lexical {
                    let norm = if max > 0.0 {
                        (-c.rank).max(0.0) / max
                    } else {
                        0.0
                    };
                    add(c, (1.0 - w) * norm);
                }
                for c in &semantic {
                    add(c, w * c.rank);
                }
            }
            Merge::Rrf { k } => {
                for (i, c) in lexical.iter().enumerate() {
                    add(c, 1.0 / (k + (i + 1) as f64));
                }
                for (i, c) in semantic.iter().enumerate() {
                    add(c, 1.0 / (k + (i + 1) as f64));
                }
            }
        }
        self.rank_heads(merged, limit)
    }

    /// Live facts whose vector by `query.model` has cosine ≥ `threshold`
    /// with `query.vector`, best first, skipping `exclude`: near-duplicates
    /// the keyword check can miss (a paraphrase, another language).
    pub fn similar_by_vector(
        &self,
        query: QueryVector<'_>,
        threshold: f32,
        exclude: &[i64],
        limit: usize,
    ) -> Result<Vec<Memory>, MemoryError> {
        let mut out = Vec::new();
        for c in self.vector_candidates(query, threshold, usize::MAX)? {
            if out.len() == limit {
                break;
            }
            if c.superseded_by.is_some() || exclude.contains(&c.id) {
                continue;
            }
            if let Some(mut m) = self.get(c.id)? {
                m.score = c.rank;
                out.push(m);
            }
        }
        Ok(out)
    }

    /// A plain insert with a given creation time, for benchmarks and tests
    /// of time decay.
    #[doc(hidden)]
    pub fn remember_at(
        &self,
        content: &str,
        tags: &[&str],
        created_at: i64,
    ) -> Result<i64, MemoryError> {
        self.conn.execute(
            "INSERT INTO memories (content, tags, created_at) VALUES (?1, ?2, ?3)",
            params![content, tags.join(","), created_at],
        )?;
        Ok(self.conn.last_insert_rowid())
    }
}

fn vec_to_bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// The dot product of `q` with a stored vector (both normalised, so the
/// cosine). `None` when the blob isn't a vector of `q`'s length.
fn cosine_bytes(q: &[f32], blob: &[u8]) -> Option<f32> {
    let (words, rest) = blob.as_chunks::<4>();
    if !rest.is_empty() || words.len() != q.len() || q.is_empty() {
        return None;
    }
    Some(
        words
            .iter()
            .zip(q)
            .map(|(w, x)| f32::from_le_bytes(*w) * x)
            .sum(),
    )
}

/// M30: the `embedding_model` column and a trigger that clears a row's
/// vector when its text changes. Not a schema version: a build from before
/// M30 never reads the column and keeps working on the file. Idempotent,
/// and serialized across processes by `BEGIN IMMEDIATE`.
fn ensure_vector_column(conn: &Connection) -> Result<(), MemoryError> {
    let has = |conn: &Connection| -> Result<bool, MemoryError> {
        let mut stmt = conn.prepare("PRAGMA table_info(memories)")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
        for c in rows {
            if c? == "embedding_model" {
                return Ok(true);
            }
        }
        Ok(false)
    };
    let trigger = |conn: &Connection| -> Result<bool, MemoryError> {
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'trigger' AND name = 'memories_embedding_clear'",
            [],
            |r| r.get::<_, i64>(0),
        )? > 0)
    };
    if has(conn)? && trigger(conn)? {
        return Ok(());
    }
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    if !has(conn)? {
        conn.execute_batch("ALTER TABLE memories ADD COLUMN embedding_model TEXT;")?;
    }
    conn.execute_batch(
        "CREATE TRIGGER IF NOT EXISTS memories_embedding_clear
         AFTER UPDATE OF content ON memories WHEN old.content IS NOT new.content BEGIN
             UPDATE memories SET embedding = NULL, embedding_model = NULL WHERE id = new.id;
         END;",
    )?;
    tx.commit()?;
    Ok(())
}

/// 0 → 1: the `superseded_by` / `superseded_at` columns, their index and an
/// update trigger for FTS. Idempotent, and serialized across processes by
/// `BEGIN IMMEDIATE`.
fn migrate(conn: &Connection) -> Result<(), MemoryError> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(MemoryError::NewerSchema(version));
    }
    if version < 1 {
        let mut cols = Vec::new();
        {
            let mut stmt = conn.prepare("PRAGMA table_info(memories)")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
            for c in rows {
                cols.push(c?);
            }
        }
        for col in ["superseded_by", "superseded_at"] {
            if !cols.iter().any(|c| c == col) {
                conn.execute_batch(&format!("ALTER TABLE memories ADD COLUMN {col} INTEGER;"))?;
            }
        }
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS memories_superseded ON memories(superseded_by);
             CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE OF content, tags ON memories BEGIN
                 INSERT INTO memories_fts(memories_fts, rowid, content, tags) VALUES('delete', old.id, old.content, old.tags);
                 INSERT INTO memories_fts(rowid, content, tags) VALUES (new.id, new.content, new.tags);
             END;
             PRAGMA user_version = 1;",
        )?;
    }
    tx.commit()?;
    Ok(())
}

fn row_to_memory(row: &rusqlite::Row<'_>) -> rusqlite::Result<Memory> {
    Ok(Memory {
        id: row.get(0)?,
        content: row.get(1)?,
        tags: split_tags(&row.get::<_, String>(2)?),
        created_at: row.get(3)?,
        score: 0.0,
        superseded_by: row.get(4)?,
    })
}

fn split_tags(tags: &str) -> Vec<String> {
    tags.split(',')
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect()
}

/// Whether two facts say the same thing, as `insert` decides NOOP.
pub fn same_fact(a: &str, b: &str) -> bool {
    let words = word_set(a);
    normalize(a) == normalize(b)
        || (!words.is_empty() && jaccard(&words, &word_set(b)) >= DUPLICATE_JACCARD)
}

/// Whether `b` looks related to `a` without saying the same thing: what
/// `insert` reports back as `similar`.
pub fn resembles(a: &str, b: &str) -> bool {
    let words = word_set(a);
    !words.is_empty() && jaccard(&words, &word_set(b)) >= SIMILAR_JACCARD
}

/// Lowercase, collapse whitespace, strip trailing punctuation: two facts
/// equal after this are the same fact.
fn normalize(s: &str) -> String {
    let lower = s.to_lowercase();
    let collapsed = lower.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
        .trim_end_matches(|c: char| c.is_ascii_punctuation() || c.is_whitespace())
        .to_string()
}

const STOPWORDS: &[&str] = &[
    "the", "and", "for", "are", "was", "were", "with", "that", "this", "from", "into", "you",
    "your", "our", "has", "have", "had", "not", "but", "can", "will", "would", "should", "could",
    "all", "any", "its", "his", "her", "their", "they", "them", "what", "which", "who", "when",
    "where", "how", "why", "about", "then", "than", "there", "here", "also", "just", "some",
    "please", "does", "did", "done", "been", "being", "use", "using",
];

/// Lowercased alphanumeric runs (Unicode-aware, so Hebrew counts).
fn words(s: &str) -> impl Iterator<Item = String> + '_ {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase())
}

/// Content words for the similarity checks: stopwords and one-letter words
/// dropped, but numbers always kept ("port 5781" and "port 5782" differ).
fn word_set(s: &str) -> std::collections::HashSet<String> {
    words(s)
        .filter(|w| {
            w.chars().any(|c| c.is_numeric())
                || (w.chars().count() >= 2 && !STOPWORDS.contains(&w.as_str()))
        })
        .collect()
}

fn jaccard(a: &std::collections::HashSet<String>, b: &std::collections::HashSet<String>) -> f64 {
    let union = a.union(b).count();
    if union == 0 {
        return 0.0;
    }
    a.intersection(b).count() as f64 / union as f64
}

/// The first `max` distinct words of `s`, in order.
fn capped_words(s: &str, max: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for w in words(s) {
        if !out.contains(&w) {
            out.push(w);
            if out.len() == max {
                break;
            }
        }
    }
    out
}

/// A goal turned into a recall query: words of three or more characters,
/// stopwords dropped, the first 32 distinct.
pub fn goal_query(goal: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for w in words(goal) {
        if w.chars().count() >= 3 && !STOPWORDS.contains(&w.as_str()) && !out.contains(&w) {
            out.push(w);
            if out.len() == 32 {
                break;
            }
        }
    }
    out.join(" ")
}

/// FTS5 query strings are a mini-language; quote terms to keep user/model
/// queries from blowing up the parser.
fn fts_escape(query: &str) -> String {
    query
        .split_whitespace()
        .map(|t| t.replace('"', ""))
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{t}\""))
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
    fn insert_decides_noop_add_and_reports_similar() {
        let store = MemoryStore::in_memory().unwrap();
        let a = store
            .insert("The deploy target is fly.io, region ams", &["infra"], &[])
            .unwrap();
        assert_eq!(a.decision, Decision::Added);

        // Same fact, different case/whitespace/trailing punctuation: NOOP.
        let b = store
            .insert("the deploy  target is fly.io, region ams.", &[], &[])
            .unwrap();
        assert_eq!(b.decision, Decision::Noop);
        assert_eq!(b.id, a.id);

        // Related but different: ADD, and the old one is shown as similar.
        let c = store
            .insert("The deploy target is render, region frankfurt", &[], &[])
            .unwrap();
        assert_eq!(c.decision, Decision::Added);
        assert_ne!(c.id, a.id);
        assert_eq!(
            c.similar.iter().map(|m| m.id).collect::<Vec<_>>(),
            vec![a.id]
        );

        // Unrelated: plain ADD, nothing similar.
        let d = store
            .insert("Max drinks his coffee black", &[], &[])
            .unwrap();
        assert_eq!(d.decision, Decision::Added);
        assert!(d.similar.is_empty());
        assert_eq!(store.recent(10).unwrap().len(), 3);
    }

    #[test]
    fn supersede_prefers_the_live_fact_on_old_wording() {
        let store = MemoryStore::in_memory().unwrap();
        let old = store
            .insert("The deploy target is fly.io", &["infra"], &[])
            .unwrap()
            .id;
        let new = store
            .supersede(old, "The deploy target is render.com", &[])
            .unwrap();
        assert_eq!(new.decision, Decision::Updated);
        assert_eq!(new.replaced, vec![old]);

        // A query that only matches the old wording returns the correction.
        let hits = store.recall("fly.io", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, new.id);
        assert!(hits[0].content.contains("render.com"));
        // Tags were inherited.
        assert_eq!(hits[0].tags, vec!["infra".to_string()]);
        // A query matching both returns the live head once.
        let hits = store.recall("deploy target", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, new.id);
        // recent() lists live rows only; the old row is kept for audit.
        assert_eq!(store.recent(10).unwrap().len(), 1);
        assert_eq!(store.get(old).unwrap().unwrap().superseded_by, Some(new.id));
        assert_eq!(store.head(old).unwrap(), Some(new.id));
    }

    #[test]
    fn superseding_twice_or_unknown_ids_is_refused() {
        let store = MemoryStore::in_memory().unwrap();
        let a = store.insert("port is 5781", &[], &[]).unwrap().id;
        let b = store.supersede(a, "port is 5782", &[]).unwrap().id;
        let err = store
            .supersede(a, "port is 5783", &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains(&format!("already replaced by #{b}")), "{err}");
        let err = store.supersede(999, "x", &[]).unwrap_err().to_string();
        assert!(err.contains("no memory #999"), "{err}");
        // Nothing was written by the refused calls.
        assert_eq!(store.recent(10).unwrap().len(), 1);
        assert!(store.recall("5783", 5).unwrap().is_empty());
    }

    #[test]
    fn update_to_an_existing_live_fact_reuses_it() {
        let store = MemoryStore::in_memory().unwrap();
        let old = store
            .insert("staging runs postgres 16", &[], &[])
            .unwrap()
            .id;
        let noted = store
            .insert("staging runs postgres 18 now", &[], &[])
            .unwrap()
            .id;
        let r = store
            .supersede(old, "Staging runs Postgres 18 now.", &[])
            .unwrap();
        assert_eq!(r.decision, Decision::Updated);
        assert_eq!(r.id, noted, "no duplicate row for the correction");
        assert_eq!(store.recent(10).unwrap().len(), 1);
        // Updating a fact to its own text is a NOOP.
        let r = store
            .supersede(noted, "staging runs postgres 18 now", &[])
            .unwrap();
        assert_eq!(r.decision, Decision::Noop);
    }

    #[test]
    fn forget_deletes_the_chain_and_leaves_no_trace_in_fts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.db");
        let store = MemoryStore::open(&path).unwrap();
        let a = store
            .insert("home address zanzibarstreet 12", &[], &[])
            .unwrap()
            .id;
        let b = store
            .supersede(a, "home address quokkalane 7", &[])
            .unwrap()
            .id;
        let keep = store.insert("unrelated fact stays", &[], &[]).unwrap().id;
        let deleted = store.forget(b).unwrap();
        assert_eq!(deleted, vec![a, b]);
        assert!(store.get(a).unwrap().is_none());
        assert!(store.recall("zanzibarstreet", 5).unwrap().is_empty());
        assert!(store.recall("quokkalane", 5).unwrap().is_empty());
        assert!(store.get(keep).unwrap().is_some());
        assert!(store.forget(12345).unwrap().is_empty());
        drop(store);
        // No copy of the text left in the database or WAL bytes.
        for f in ["memory.db", "memory.db-wal"] {
            if let Ok(bytes) = std::fs::read(dir.path().join(f)) {
                for needle in ["zanzibarstreet", "quokkalane"] {
                    assert!(
                        !bytes.windows(needle.len()).any(|w| w == needle.as_bytes()),
                        "{needle} still in {f}"
                    );
                }
            }
        }
    }

    #[test]
    fn forgetting_an_old_version_keeps_the_live_fact_linked() {
        let store = MemoryStore::in_memory().unwrap();
        let a = store.insert("editor is vim", &[], &[]).unwrap().id;
        let b = store.supersede(a, "editor is helix", &[]).unwrap().id;
        let c = store.supersede(b, "editor is zed", &[]).unwrap().id;
        assert_eq!(store.forget(b).unwrap(), vec![b]);
        assert_eq!(store.get(a).unwrap().unwrap().superseded_by, Some(c));
        assert_eq!(store.recall("vim", 5).unwrap()[0].id, c);
    }

    #[test]
    fn assemble_for_goal_puts_goal_matches_first_with_ids() {
        let store = MemoryStore::in_memory().unwrap();
        let port = store
            .insert("The staging database listens on port 5781", &[], &[])
            .unwrap()
            .id;
        for i in 0..8 {
            store.insert(&format!("filler fact {i}"), &[], &[]).unwrap();
        }
        let block = store
            .assemble_for_goal("Which port does the staging database use?", 2_000)
            .unwrap();
        let first = block.lines().next().unwrap();
        assert_eq!(
            first,
            format!("- #{port} The staging database listens on port 5781")
        );
        // The newest facts fill the rest.
        assert!(block.contains("filler fact 7"), "{block}");
        assert!(!block.contains("filler fact 2"));
        let tiny = store
            .assemble_for_goal("staging database port", 60)
            .unwrap();
        assert!(tiny.len() <= 60);
    }

    #[test]
    fn a_newer_schema_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.db");
        drop(MemoryStore::open(&path).unwrap());
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA user_version = 2;").unwrap();
        drop(conn);
        let err = MemoryStore::open(&path).err().unwrap();
        assert!(matches!(err, MemoryError::NewerSchema(2)), "{err}");
    }

    #[test]
    fn weird_queries_do_not_crash_fts() {
        let store = MemoryStore::in_memory().unwrap();
        store.remember("something safe", &[]).unwrap();
        let _ = store.recall("what's OR (broken \"syntax\"", 10).unwrap();
    }

    #[test]
    fn similar_clusters_group_near_duplicates_only() {
        let store = MemoryStore::in_memory().unwrap();
        let a = store
            .remember("deploys go to render via the cli", &[])
            .unwrap();
        let b = store.remember("deploys go to render via cli", &[]).unwrap();
        let c = store
            .remember("deploys go to render with the render cli", &[])
            .unwrap();
        store
            .remember("postgres listens on port 5781", &[])
            .unwrap();
        store.remember("redis listens on port 6379", &[]).unwrap();
        let groups = store.similar_clusters(0.5, 100).unwrap();
        assert_eq!(groups.len(), 1, "{groups:?}");
        let ids: Vec<i64> = groups[0].iter().map(|m| m.id).collect();
        assert_eq!(ids, vec![a, b, c]);
        // Replaced facts are never clustered.
        store
            .insert("deploys go to render", &[], &[a, b, c])
            .unwrap();
        assert!(store.similar_clusters(0.5, 100).unwrap().is_empty());
    }

    #[test]
    fn undo_update_restores_the_replaced_facts() {
        let store = MemoryStore::in_memory().unwrap();
        let a = store.remember("the api runs on port 8080", &[]).unwrap();
        let b = store.remember("api runs on port 8080 in dev", &[]).unwrap();
        let before = store.max_id().unwrap();
        let r = store
            .insert("The API runs on port 8080 (dev and prod)", &[], &[a, b])
            .unwrap();
        assert!(r.id > before);
        assert_eq!(store.recent(10).unwrap().len(), 1);
        let restored = store.undo_update(r.id, &r.replaced, true).unwrap();
        assert_eq!(restored, vec![a, b]);
        assert!(store.get(r.id).unwrap().is_none());
        let live: Vec<i64> = store.recent(10).unwrap().iter().map(|m| m.id).collect();
        assert_eq!(live, vec![b, a]);
        assert_eq!(store.recall("8080", 5).unwrap().len(), 2);
    }

    #[test]
    fn undo_update_of_a_reused_row_keeps_it_and_refuses_after_a_correction() {
        let store = MemoryStore::in_memory().unwrap();
        let a = store.remember("ci runs on github actions", &[]).unwrap();
        let b = store.remember("CI runs on GitHub Actions.", &[]).unwrap();
        let before = store.max_id().unwrap();
        let r = store
            .insert("ci runs on github actions", &[], &[a, b])
            .unwrap();
        assert_eq!(r.decision, Decision::Updated);
        assert!(r.id <= before, "reused an existing row");
        let restored = store.undo_update(r.id, &r.replaced, false).unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(store.recent(10).unwrap().len(), 2);

        // Merge again, then correct the merged fact: undo is refused.
        let r = store
            .insert("CI runs on GitHub Actions and Buildkite", &[], &[a, b])
            .unwrap();
        store
            .supersede(r.id, "CI runs on Buildkite only", &[])
            .unwrap();
        assert!(matches!(
            store.undo_update(r.id, &r.replaced, true),
            Err(MemoryError::Refused(_))
        ));
    }

    #[test]
    fn known_and_live_tagged_look_without_writing() {
        let s = MemoryStore::in_memory().unwrap();
        let a = s
            .insert(
                "Dana deploys the shop on Vercel",
                &["import:hermes", "from:MEMORY.md"],
                &[],
            )
            .unwrap();
        s.insert(
            "The staging port is 5781",
            &["import:hermes", "from:USER.md"],
            &[],
        )
        .unwrap();
        assert_eq!(
            s.known("dana deploys  the shop on vercel.").unwrap(),
            Some(a.id)
        );
        assert_eq!(s.known("Dana deploys on Netlify").unwrap(), None);
        let tagged = s.live_tagged(&["import:hermes", "from:MEMORY.md"]).unwrap();
        assert_eq!(tagged.len(), 1);
        assert_eq!(tagged[0].id, a.id);
        assert!(s.live_tagged(&["import:openclaw"]).unwrap().is_empty());
        s.supersede(a.id, "Dana deploys the shop on Netlify", &[])
            .unwrap();
        let tagged = s.live_tagged(&["import:hermes", "from:MEMORY.md"]).unwrap();
        assert_eq!(tagged.len(), 1, "only the live head");
        assert_ne!(tagged[0].id, a.id);
        assert!(resembles(
            "Dana deploys the shop on Vercel",
            "Dana deploys the shop on Netlify"
        ));
        assert!(!same_fact(
            "Dana deploys the shop on Vercel",
            "Dana deploys the shop on Netlify"
        ));
    }
}
