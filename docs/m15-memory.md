# M15: the memory update pipeline and reversible compaction (design)

Status: design, 2026-09-24. Builds on the roadmap's M15 section and on
§4.3/§4.4 of `docs/research-number-one-harness-strategy.md`. Where this
document departs from either, it says so and why.

## What changes

Today memory only grows. `remember` inserts a row, and nothing in the agent's reach can
correct or remove one. `MemoryStore::forget` exists, but no tool exposes it. The
memory block at session start is "the five newest facts plus nothing",
because `build_agent_from` calls `assemble_context(None, …)` before the
request is known. Compaction is one-way: once the history is folded into a
summary, whatever the summary left out is gone for the rest of the run, even
though every message is still on disk in the session transcript.

M15 fixes both halves:

| | before | after |
|---|---|---|
| a wrong fact | a second, contradicting fact is added | `update_memory` supersedes it; recall returns only the live version |
| a duplicate | inserted again | `remember` answers NOOP with the existing id |
| removal | CLI only (`MemoryStore::forget`, no tool) | `forget` tool (root agent only), a real delete |
| session start | 5 newest facts | facts matching the session's goal first, then the newest to fill the budget |
| compaction | summary only | summary + a `search_history` tool over the full transcript |
| an old 50 kB tool result | carried until the summary folds it | shortened to a preview + a reference the agent can fetch back |

## 1. The insert decision (ADD / UPDATE / DELETE / NOOP)

Mem0 (arXiv:2504.19413) runs one extra LLM call per extracted fact. The call
compares the fact with its top-k similar memories and picks ADD, UPDATE,
DELETE or NOOP. There are three ways ferrule could make that decision:

| who decides | cost per `remember` | quality | failure |
|---|---|---|---|
| **a cheap LLM call** (Mem0) | one provider call (~300–800 input tokens, ~50 output), plus latency, plus a provider handle inside the memory tools, plus a ledger row | good on contradictions | costs money on every write; a second model's judgement on the owner's facts; needs a working provider even for `ferrule memory add` |
| **a heuristic** | zero | good on exact and near-exact duplicates, bad on contradictions ("deploy target is fly.io" vs "… is render" share most words but *replace*; "port 5781" vs "port 5782" could be two ports) | silently merges or replaces facts it shouldn't |
| **the model, through the tools** | zero extra calls; a few hundred tokens in one tool result | the agent already has the context that says *why* the fact changed | a model that ignores the hint leaves two live facts |

**Decision: a hybrid of the heuristic and the model.**

- **NOOP is decided deterministically.** A fact whose normalized text is
  lowercased, with whitespace collapsed and trailing punctuation stripped. If
  that text equals a live memory's, or their token sets have a Jaccard
  similarity ≥ 0.9, it isn't inserted. `remember` answers `already
  remembered (#id)`.
- **ADD is the default.** The fact is inserted. If live memories are
  *similar* (Jaccard ≥ 0.3 over content words among the top BM25
  candidates, but not duplicates), the tool result lists them with their ids:
  `remembered (#7). Similar live memories: #3 … If #7 corrects one of them,
  call update_memory {"id": 3, "content": …}; if one is simply wrong now,
  forget it.`
- **UPDATE and DELETE are the model's calls**, made with `update_memory` and
  `forget`, or with `remember {"replaces": [3]}` in one step when the model
  already knows the id. The session-start memory block shows every fact's
  `#id` for exactly this.

Why not the heuristic for UPDATE: the cases where it would be wrong are the
ones where being wrong hurts, such as replacing a fact that was really a
second, separate fact. Why not the LLM: the agent that is writing the fact
is already an LLM holding the context. The offline consolidation pass in M16
is where a separate LLM judgement over the whole store belongs, run
cost-capped from the ledger. It doesn't belong on the hot path of every
write.

`update_memory {"id", "content"}` is `remember` with `replaces: [id]`: if a
live memory already holds `content` (the model `remember`ed it first and
then saw the hint), that row becomes the replacement and nothing is
inserted, so the two-step path never leaves a duplicate.

## 2. `superseded_by` semantics

Two nullable columns are added to `memories`: `superseded_by INTEGER`, the id
of the row that replaced this one, and `superseded_at INTEGER` (unix
seconds). `NULL` means the row is live.

- **Superseding** (`update_memory`, `remember` with `replaces`) inserts the new
  row (or reuses a live identical one) and sets `superseded_by`/`superseded_at` on
  each replaced row, in one transaction. The replaced row is kept: it's the
  audit trail, and M16's consolidation reads it.
- **Only live rows can be superseded.** Updating a row that was already
  replaced fails with `#3 was already replaced by #7 — update #7`. That way
  two corrections never race silently, and a chain is always a line, never a
  tree. Cycles can't happen either: a row can only be pointed at while it's
  live, and once it points at its replacement it's never live again. (The
  replacement is usually the newer row, but not always: `update_memory` with
  text an older live row already holds reuses that row.)
- A replacement with no tags inherits the tags of the rows it replaces.
- **Recall prefers the live fact.** Recall searches every row, then maps each
  hit to the live head of its chain, dedupes the heads and keeps the best
  score. A query that only matches the *old* wording ("fly.io") therefore
  returns the correction ("render"), not the stale fact, and not nothing.
  Superseded rows are never returned as themselves. `recent()` lists only live
  rows.

## 3. What `forget` really deletes

`forget(id)` is a hard delete. It removes:

- the row, and **every older version in its chain** (the rows whose
  `superseded_by` leads to it). "Forget where I live" also means forgetting
  where I used to live. Forgetting an *old* version deletes just that row.
  The live fact stays;
- its FTS5 entry, through the existing delete trigger. After that the FTS
  index is merged (`INSERT INTO memories_fts(memories_fts) VALUES('optimize')`),
  because an FTS5 delete is only logical until its segment is merged, and the
  tokens would otherwise stay in the index b-tree;
- the bytes on disk. The store opens with `PRAGMA secure_delete = ON`, so
  freed pages are zeroed, and after a forget the WAL is checkpointed with
  `TRUNCATE`, so no copy of the old pages is left in `memory.db-wal`.

It does **not** rewrite past session transcripts (`sessions/*.jsonl`), which
may still quote the fact, or any backup of the store. The tool description
says so. Scrubbing transcripts would break their append-only, auditable
contract, and it's a separate feature (see open edges).

Who may call it: only the top-level agent (§8). The tool description steers
corrections to `update_memory`, which keeps history, and keeps `forget` for
facts that are wrong with no replacement, or that the person asked to have
forgotten.

## 4. Goal-driven recall at session start

The session's goal is the first user message of the session. That's the
task prompt for `ferrule run`, a scheduled task or a sub-agent's task, and
the first message of a chat. A resumed gateway session already has a first
user message in its replayed history, so the goal is that message plus the
new one.

Mechanism: a `SessionRecall` hook in `ferrule-core`
(`Agent::with_session_recall`). On the **first** `run()` of an agent,
before the goal is pushed, the agent asks the hook for a memory block for
the goal and appends it to the system prompt, under `[Long-term memory]`.
(M27 moved it: the block is now a user message right after the goal, so
the system prompt is the same bytes for every session and caches across
them; compaction carries it forward verbatim. See `m27-speed.md` §3.)
Every entry point gets it the same way: `run`, `chat`, the gateway, the
scheduler and sub-agents, because the goal only exists inside `run()` and
`build_agent_from` never sees it. That's why the old code passed `None`.

What goes in the block (`MemoryStore::assemble_for_goal`, 2,000 chars as before):

1. goal matches first. The goal is turned into a query: words of three or
   more characters, stopwords dropped, first 32 distinct. The query runs
   through the prefer-live recall above, top 10 by score (BM25 × the 7-day
   half-life decay);
2. then the newest live facts (up to 5) fill what's left of the budget, so
   standing preferences that share no word with the goal still show up;
3. each line is `- #id fact`, so the model can correct a fact in one call.

The system prompt is still byte-stable for the rest of the session. The
block is added once, before the first provider call, so the prompt-cache
prefix isn't disturbed. Per-turn re-recall on long sessions is left out on
purpose (open edge): it would change the prompt mid-session.

## 5. The transcript store behind `search_history`

**Where it lives.** It's the existing append-only JSONL transcript,
`<data dir>/sessions/<session id>.jsonl`. A sub-agent has its own session
file there. An eval run keeps its transcripts under the eval run dir. No new
store is added. The transcript already holds every message the model saw,
including everything compaction folded away: compaction rewrites the
in-memory history only, never the file.

**The tool.** `search_history` is read-only (`changes_files() == false`) and
lives in `ferrule-core::history`, next to the transcript format it reads. It
has two modes:

- `{"query": "port", "limit": 8}` does a case-insensitive search for every
  query word over the session's user, assistant and tool messages. Hits are
  ranked by how many query words they contain, newest first on a tie. It
  returns up to `limit` hits (max 20). Each hit is shown as `[message N ·
  role (tool name)] …snippet…`: 400 characters around the first match, and
  the result's `ref` when it's a tool result;
- `{"ref": "r…", "offset": 0}` returns one tool result in full, paged at the
  tool context's `max_output_chars`. When there's more, the page ends with
  the next offset.

**Size limits.** The file is streamed line by line, never loaded whole, and a
single line over 4 MiB is skipped. Output is capped as described above. A
search costs one sequential read of the session file, which is fine up to
tens of MB. A chat lane that lives for months is the case to watch (open
edge).

**Scope and redaction.**

- It only ever reads *its own* session's file. There's no path or session
  parameter, so a child can't read its parent's session and a chat can't
  read another chat.
- Everything it can return is something the model already saw in this
  session. The system prompt isn't in the transcript (it's rebuilt at
  startup), reasoning is excluded, and secrets are kept out of messages by
  the credential gateway's placeholders.
- It never returns its own earlier results. The results of earlier
  `search_history` calls are skipped, so repeated searches don't echo and
  grow.

**Retention** is the transcripts' retention. Today that means kept until the
owner deletes them, and M15 doesn't change it (open edge).

## 6. Shortening old large tool results: the reference format

When the context passes the compaction trigger, `ContextOverflow::Compact`
now runs three stages. After stage 2 it re-measures and stops if that
stage shortened anything and the context now fits (dedupe alone never skips
the summary, as before M15):

1. dedupe identical tool results (existing, free);
2. **shorten old large tool results** (new, free). A tool result is
   shortened when it sits outside the verbatim tail (`compaction_keep_last`)
   and is longer than `AgentConfig.shorten_tool_results_over` (default 4,000
   chars). It's replaced *in memory* by

   ```
   [ferrule: an older tool result (read_file, 48213 chars) was shortened to save context. It began:
   <first 600 chars>
   …
   The full text is still in this session's history: search_history {"ref": "r9f3a1c2e4b5d6a7b"}]
   ```

   The ref is `r` + 16 hex digits of the FNV-1a-64 hash of the original
   text. It's content-addressed, so it doesn't depend on message positions,
   provider-generated tool-call ids (some providers reuse `"1"`), or how a
   resumed session renumbers its history. `search_history` resolves it by
   hashing the tool results in the transcript. Results holding a skill block
   are never shortened, since compaction carries those verbatim;
3. the structured LLM summary of the head (existing). It's now skipped when
   stage 2 was enough, which saves a provider call and keeps more of the
   history verbatim. When it does run, the summary message also lists the
   refs of the shortened results it folded (up to 20), so the pointer
   survives the fold.

Shortening only happens when the agent can fetch the result back: when it
has a transcript and a `search_history` tool. An ephemeral agent (no
transcript) keeps the old behaviour. Stage 2 emits a new
`AgentEvent::ToolResultsShortened`.

Why only at the trigger and not continuously: every rewrite of an old
message invalidates the provider's prompt-cache prefix from that point on.
Shortening when the loop was about to rewrite the history anyway costs no
extra cache misses. (The strategy doc's "dedupe at push time" has the same
cache cost, so it isn't done here either.)

`ContextOverflow::Truncate`, the naive eval baseline, is untouched. It still
drops the oldest messages and emits `Truncated`, with no shortening, no
references and no `search_history` tool.

## 7. The migration

The schema version is `PRAGMA user_version`: 0 before M15, 1 from M15 on.
`MemoryStore::open` runs the migration in place, inside `BEGIN IMMEDIATE`.
Two processes opening the store at once (the gateway and a `ferrule memory`
command) are serialized, and the second one finds nothing to do.

- `ALTER TABLE memories ADD COLUMN superseded_by INTEGER` and `… superseded_at
  INTEGER`, each only when `pragma table_info` says the column is missing. An
  interrupted or hand-made store is therefore still fine.
- `CREATE INDEX IF NOT EXISTS memories_superseded ON memories(superseded_by)`.
- An `AFTER UPDATE OF content, tags` FTS trigger, for completeness. M15 never
  edits content in place, but a hand edit shouldn't desync the index.
- `PRAGMA user_version = 1`.

Every existing row becomes live (both columns `NULL`), so recall on a
migrated store returns exactly what it returned before. A store with
`user_version` above 1 (written by a newer ferrule) is refused with a clear
error rather than written by code that doesn't know its schema. An older
ferrule that opens a migrated store keeps working: the new columns are
nullable and its inserts leave them `NULL`. It just shows superseded facts
too.

The store now also sets `busy_timeout = 5000`, so the gateway and a CLI
command writing at the same moment wait for each other instead of failing
with `database is locked`.

Test: `crates/ferrule-memory/tests/fixtures/pre-m15-memory.db` was created by
the `ferrule` binary built from main at `2f1045f` (`ferrule memory add` ×3).
The test copies it, opens it with the new store, checks the columns and the
version, recalls the old facts, supersedes one, and recalls again.

## 8. The sub-agent policy (M12)

| tool | root | writing child (worker, planner) | read-only child (verifier without a worktree, any `read_only` spec) |
|---|---|---|---|
| `recall` | ✓ | ✓ | ✓ |
| `search_history` (own session) | ✓ | ✓ | ✓ |
| `remember` | ✓ (with `replaces`) | ✓ (ADD/NOOP only: `replaces` is not in its schema and is refused) | ✗ |
| `update_memory` | ✓ | ✗ | ✗ |
| `forget` | ✓ | ✗ | ✗ |
| session-start recall | ✓ | ✓ (its task is its goal) | ✓ |

Children never rewrite or delete long-term memory. The store is shared by the
whole tree and outlives it, and a child sees only its slice of the work, so
it's the wrong place to decide that a fact is wrong. A writing child can
still add, as today (it could before M15). A read-only child can't add
either, which is unchanged. This is enforced in code: `memory_tools::tools`
takes a `MemoryAccess` (`Full` / `Append` / `Read`), `build_agent_from`
derives it from the `ChildSpec`, and the `remember` tool itself refuses
`replaces` under `Append`, even if a model sends it anyway.

## 9. Failure modes

- **The store can't be opened** (locked for more than 5 s, corrupt, or a
  newer schema). Session-start recall is skipped and the session starts
  without the block, as before. Memory tools return a tool error the model
  can read.
- **The model ignores the "similar memories" hint.** Two live facts
  contradict each other. Both show up with ids, so the next session can fix
  it, and M16's consolidation is the backstop.
- **The heuristic calls a genuinely new fact a duplicate.** That takes a
  Jaccard ≥ 0.9, so the texts are nearly identical. The tool says which id
  it matched, so the model can see it.
- **A bad `update_memory`** (wrong id, a superseded id, a missing id). It's
  refused with the reason and the live head's id. Nothing is written.
- **A bad `forget`.** It's irreversible by design; only the root agent has
  it, and its description asks for `update_memory` when there's a
  replacement.
- **A ref that isn't found** (the transcript was deleted or truncated, or
  the agent has no transcript). The result says so and suggests a `query`
  search. The in-memory preview is still in context.
- **A transcript write fails** (disk full). The loop already only warns. A
  result shortened afterwards might not be fetchable. Rare, and the preview
  plus the summary remain.
- **The transcript is huge.** Search time grows linearly. The tool is
  streaming and capped, so it's slow but bounded.
- **Two processes migrate at once.** `BEGIN IMMEDIATE` plus the column
  checks make the migration idempotent.

## 10. How it's tested (hermetic: mock providers, temp dirs)

- `ferrule-memory`: the decision table (NOOP, similar, ADD), supersede
  chains and their refusals, prefer-live recall on old wording, forget
  deleting the chain and leaving no trace in FTS or the file bytes, the
  migration from the committed pre-M15 fixture, and a refused newer
  schema.
- `ferrule-core`: `search_history` over a real transcript, where a session
  is compacted, then answers from the dropped part by calling
  `search_history`. An old large result is shortened to a ref and fetched
  back in full. Shortening is skipped with no transcript, and the naive
  `Truncate` path is unchanged.
- `ferrule-cli` binary tests (`tests/memory.rs`, a scripted model server):
  a fact is stored in one `ferrule run`, corrected through `update_memory`
  in a second, and the third session's system prompt holds the correction,
  not the old fact. A read-only child's tool list has `recall` and
  `search_history` but no `remember`, `update_memory` or `forget`.
- `ferrule eval run evals/starter --variant ab` on the mock model: the
  engineered variant stays at 20/20.
