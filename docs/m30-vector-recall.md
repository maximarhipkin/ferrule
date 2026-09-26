# M30: vector recall — local embeddings merged with BM25 (design)

Status: design, 2026-09-26, branch `m30-vector-recall`. Written before the
code. Where the build departs from it: **As built** at the end.

M15 gave memory BM25 keyword recall with time decay, supersede chains and
`forget`. It misses what keyword search always misses: a paraphrase ("where
do we ship to?" for "the deploy target is render"), a synonym, a typo, and
anything asked in Hebrew about a fact written in English (or the other way
round). M30 adds a vector side and merges the two, without making any of
M15's promises weaker. User guide: [memory.md](memory.md).

M29 (a repo map next to recalled memory) is being built at the same time.
M30 keeps out of the tools crate and prompt assembly except for the
`remember`/`recall` wiring, which lives in `ferrule-cli`.

## 1. Embedders

A new crate, `ferrule-embed`, with one async trait:

```rust
trait Embedder: Send + Sync {
    fn model(&self) -> &ModelId;          // "local:potion-multilingual-128M@73908c3/256"
    async fn embed(&self, texts: &[String], purpose: Purpose) -> Result<Embedded, EmbedError>;
}
```

`Embedded` holds one L2-normalised `Vec<f32>` per text and the tokens the
backend billed (0 for local). `Purpose` is `Query` or `Document`, for
models that want different prefixes; both current backends ignore it.

Three implementations:

| backend | what | key | cost |
|---|---|---|---|
| `local` | a static (model2vec) embedding model, in process | none | none |
| `openai` | any OpenAI-compatible `POST {base_url}/embeddings` | proxy placeholder | ledger row per request |
| `FakeEmbedder` | hashed character trigrams, for tests | — | — |

### 1.1 The local model: model2vec, not a transformer

**Decision: `minishlab/potion-multilingual-128M`, a model2vec static
embedding model, run by a ~150-line pure-Rust embedder on top of the
`tokenizers` crate.**

A static model is a token → vector table distilled from a transformer
(here BAAI/bge-m3): embedding a text is tokenise, look up each token's row,
mean-pool, normalise. There is no attention and no matrix multiply, so:

- **It builds everywhere.** `tokenizers` with `default-features = false,
  features = ["fancy-regex"]` is pure Rust: no oniguruma (C) and no
  esaxx (C++), which are what make it hard to cross-compile. No ONNX
  Runtime, no BLAS, no GPU backends. The five release targets build with
  the toolchain they already use.
- **It is fast.** Microseconds per text on one core, so embedding on every
  `remember` and every recall costs nothing a user can notice.
- **It is multilingual.** bge-m3's vocabulary covers Hebrew and English, and
  the distillation keeps some cross-lingual alignment. How much is what the
  benchmark measures (§3); it is not assumed.

Rejected:

- **candle + multilingual-e5-small / bge-m3.** Better embeddings, but a real
  transformer forward pass (tens of ms per text on a laptop CPU, seconds on
  a Pi), `gemm` and friends in the build, and +5–10 MB of binary.
- **fastembed / `ort`.** ONNX Runtime is a C++ library: a prebuilt download
  at build time, or a from-source build per target. Neither fits a static
  musl binary built on stock GitHub runners.
- **The `model2vec-rs` crate.** It pulls `tokenizers` with default features
  (oniguruma, esaxx) plus `hf-hub`, `ndarray` and `half`, and it loads the
  whole matrix into RAM. The pooling it does is ~40 lines; we own them.

**The matrix is not loaded into memory.** The model's `model.safetensors` is
one F32 tensor, 500,353 × 256 (512 MB). The embedder parses the safetensors
header itself and reads just the rows a text needs (`seek` + `read`, 1 KB a
row) from the open file. A recall touches a few dozen rows; the OS page
cache does the rest. Only the tokenizer (18 MB of JSON, parsed once per
process, lazily on first use) stays in RAM.

**Weights never ship.** Not in the binary, not in the repo. `ferrule setup`
(and `ferrule memory model download`) offer the download, say its size, and
fetch it only on a yes:

- from `https://huggingface.co/<repo>/resolve/<revision>/<file>`, at a
  **pinned revision** compiled into the binary;
- each file streamed to `<data>/models/<name>/<file>.partial`, hashed with
  SHA-256 as it arrives, and renamed into place only if the hash matches
  the pinned one. A mismatch deletes the partial file and fails with the
  expected and actual hash, and nothing is enabled;
- through the same egress as the rest of ferrule's HTTP: the credential
  proxy when one runs, and the owner's corporate proxy (`HTTPS_PROXY`)
  either way.

Nothing is downloaded silently. `embedder = "local"` with the model missing
is not an error: recall is BM25, and `ferrule doctor` says the model is
missing and how to fetch it.

### 1.2 The endpoint backend

`embedder = "openai"` posts `{"model", "input": [...], "dimensions"?}` to
`{base_url}/embeddings` and reads `data[].embedding` (sorted by `index`)
and `usage.prompt_tokens`. It works with OpenAI, Azure-style gateways,
Ollama, llama.cpp's server, vLLM, LM Studio.

- **Configured like a provider:** `provider = "<name>"` borrows a
  `[providers.<name>]` entry's `base_url` and `api_key_env`; explicit
  `base_url` / `api_key_env` in `[memory]` override. A local server (Ollama)
  needs no key.
- **The key is a placeholder.** Like `[web_search]` (M28),
  `Config::finish()` binds `api_key_env` to the endpoint's host in
  `[secrets]`; the embedder holds only the proxy's placeholder, and the
  proxy swaps the real key in on the way out. The real key is never in the
  embedder's memory, a transcript or an error message.
- **Cost in the ledger.** Every request is one ledger row, `call_kind =
  "embedding"`, with its input tokens and `cost_usd =
  tokens × price_input_per_mtok / 1e6` (`[memory] price_input_per_mtok`, or
  the borrowed provider's). Failed requests are rows too, with
  `outcome = "error"` and no cost. They count toward `[trust]`'s dollar caps
  like any other row.
- **Errors are typed.** 401/403 → `Unauthorized` ("the key for {host} was
  refused"), 429 → `RateLimited` (with `Retry-After` when sent), anything
  else → `Http(status)` or `Transport`. None of them reaches the user as an
  error during a session: recall falls back to BM25.

## 2. Hybrid recall

### 2.1 Storage

M15 reserved `memories.embedding BLOB`. M30 adds `embedding_model TEXT`
next to it:

- `embedding`: the vector, little-endian f32, L2-normalised;
- `embedding_model`: the model id **with its dimension**
  (`openai:text-embedding-3-small/1536`). Two configs of the same model at
  different `dimensions` are different ids.

**`PRAGMA user_version` stays 1.** The change is one nullable column (added
idempotently at open, under `BEGIN IMMEDIATE`) and one trigger. A build
from before M30 opens the file, never selects the column, and keeps
working; rows it writes have no vector and are embedded later. A version
bump would have made every older ferrule refuse the file ("upgrade
ferrule") for a change it can safely ignore.

The trigger clears `embedding`/`embedding_model` whenever a row's `content`
changes, so a vector can never describe text the row no longer holds, even
when an older build made the change. A `forget` deletes the row, vector
included, and `secure_delete` zeroes the pages as before.

### 2.2 Never mixing models

A vector is compared only with vectors whose `embedding_model` equals the
query's. There is no "close enough" rule: a different model, a different
dimension, a different revision of the local model — all are simply not
candidates. A row with no vector, or another model's vector, is **stale**:
it takes part through BM25 only, exactly as before M30.

Stale rows are re-embedded:

- **lazily**: after a session-start recall, up to 32 stale live rows are
  embedded in the background (not on the recall's critical path);
- **on demand**: `ferrule memory reindex` embeds every stale row in id
  order, in batches, committing each batch. It stores nothing but the
  vectors, so it is resumable for free: interrupted, the next run starts
  from the rows still stale. With the endpoint backend it keeps to
  `[memory] max_requests_per_minute` and backs off on 429 (using
  `Retry-After` when sent).

Changing `embedder` or `model` is therefore safe at any time: recall
degrades to BM25 for the rows not yet re-embedded, and gets better as they
are.

### 2.3 Scoring

Both sides produce candidates at the row level:

- **BM25**: as M15, the top `5·limit + 20` FTS matches, score `-bm25`.
- **Vector**: brute-force cosine of the query vector against every row with
  the same `embedding_model`, keeping those with cosine ≥ `min_similarity`
  and at most `5·limit + 20` of them.

The floor matters. BM25 only returns rows that share a word with the query;
cosine returns *every* row with some score. Without a floor, every recall
fills its ten slots, and the session-start block grows by facts that have
nothing to do with the goal.

Two merges, both implemented, one chosen by the benchmark (§3):

- **Weighted** (ZeroClaw's): `0.7·cos + 0.3·bm25/max(bm25)` over the union,
  a missing side counting 0. `vector_weight` is configurable.
- **RRF** (reciprocal rank fusion): `Σ 1/(60 + rank)` over the two ranked
  lists. It uses ranks only, so it needs no normalisation, and BM25's
  unbounded scale can't swamp the cosine.

Then, unchanged from M15:

- **time decay**: the merged score × `0.5^(age / 7 days)`;
- **supersede**: each hit maps to the live head of its chain, and a chain
  keeps its best score. A query close to a replaced fact's wording returns
  the correction, as with BM25;
- **forget**: a forgotten row is gone from both sides;
- **the budget**: `assemble_for_goal` takes the top 10 and then the 5
  newest within 2,000 characters, as before.

**Brute force is fine up to roughly 50,000 rows at 256 dimensions** (the
local model: 50 MB of vectors read and 12.8 M multiply-adds per query,
tens of milliseconds) **or about 10,000 rows at 1,536** (the OpenAI small
model). A personal agent's memory is hundreds to low thousands of facts.
Past that, the right move is an ANN index (sqlite-vec's `vec0`, or HNSW in
Rust). sqlite-vec is a C extension. It would have to be compiled into the
bundled SQLite on five targets, and at this size it doesn't clearly win,
so it isn't used.

### 2.4 Where it's used

- **The `recall` tool** embeds the query (timeout 5 s) and runs the hybrid.
  Its description no longer says "keyword search".
- **Session-start recall** (`GoalRecall`, M15/M27) embeds the goal query the
  same way. The block is still a user message after the goal, so the
  system prompt and tool list stay the same bytes across turns (M27's
  cache-stable prefix). A test pins this with recall on.
- **`remember` / `update_memory`** embed the fact and store the vector with
  the row. If the embedder fails, the fact is stored without one (stale).
- **`ferrule memory search`** uses the hybrid too.
- Near-duplicates (optional in the brief): if the new fact's vector is
  within a cosine threshold of a live fact that Jaccard didn't flag, that
  fact joins the "similar" list in the tool's reply, which suggests
  `update_memory`. It never merges anything by itself.

`search_history` (compaction refs, M15) is out of scope and stays keyword.

### 2.5 Fallback, exactly

With `embedder = "off"` (the default), with the local model not
downloaded, or with any embedder error (network, 401, 429, timeout, bad
JSON, wrong dimension), recall is **exactly M15's** `recall()` — same
query, same rows, same scores, same order — and the user sees no error.
The failure is logged (`tracing::warn!`, once per process for the same
cause) and counted in the ledger when it was a paid request.

`ferrule eval`'s variants build memory tools without an embedder, so the
eval suite (engineered 20/20, naive 11/20, $0.98 on the stdlib mock) is
unaffected by construction. It is still run to show it.

### 2.6 `ferrule doctor`

One line under memory: the embedder (`off` / `local` / `openai`), its
model id, whether the local model is present and verified, and how many
live rows have a vector for the current model (`312/340 embedded; run
ferrule memory reindex`).

## 3. The benchmark

`crates/ferrule-memory/tests/fixtures/recall_bench.json`: 80 memories (a
personal agent's facts: infrastructure, preferences, people, places,
dates; some in Hebrew) and 48 queries, each with the id(s) of the memory
that answers it and a category:

| category | example |
|---|---|
| `keyword` | "which port does postgres use" → "Local Postgres runs on port 5781" |
| `paraphrase` | "where do we ship production" → "The deploy target is render" |
| `synonym` | "the car" → a fact about "the vehicle" |
| `cross_lingual` | Hebrew query → English fact, and the reverse |
| `typo` | "postgress prot" |
| `distractor` | a query sharing its keywords with a wrong memory |

The harness (`ferrule_memory::bench`) loads the fixture into an in-memory
store, embeds with a closure it's given, and reports recall@1, recall@5 and
MRR per category for BM25 alone and for each merge. A hermetic test runs it
with `FakeEmbedder` and checks the harness, not the model. An `#[ignore]`d
test runs it with the real local model:

```sh
FERRULE_EMBED_MODEL_DIR=/path/to/models/potion-multilingual-128M \
  cargo test -p ferrule-memory --test bench -- --ignored --nocapture
```

**The ship rule.** The merge whose numbers are better on paraphrase and
cross-lingual without losing keyword recall becomes the default. If
neither beats BM25 on those with the local model, the doc says so and
`embedder` stays `"off"` by default (it's `"off"` by default anyway: the
model is a 530 MB opt-in download).

## 4. Threat model

- **The model files are code-adjacent input.** A swapped `tokenizer.json`
  could make the tokenizer allocate or loop badly, and a swapped matrix could
  make recall return the wrong facts. Both are pinned by SHA-256 against
  hashes compiled into the binary, checked at download. At load, the
  tokenizer (18 MB) is hashed again each time; the matrix (512 MB, a
  second of hashing) is checked for its exact size, and its safetensors
  header, parsed by our own code, must match the pinned shape (dtype F32,
  [rows, dim], offsets inside the file) before any read. `ferrule doctor`
  re-hashes everything.
- **The endpoint sees memory.** With `embedder = "openai"`, every fact
  and every recall query goes to that endpoint. That is the same data the
  model provider already sees in the prompt, but it is a second party. The
  doc says so, and the local backend exists for owners who don't want it.
- **The key.** It is a placeholder bound to the endpoint's host. A
  prompt-injected tool call can't make the embedder send it elsewhere: the
  URL comes from config, not from the model. The proxy refuses to swap it
  for any other host.
- **Embeddings are a copy of the text.** Vectors can be partly inverted,
  so `forget` must delete them. It does, because they live in the row it
  deletes, and `secure_delete` plus the WAL truncate cover them as they
  cover the text.
- **No new egress for the sandbox.** Embedding happens in ferrule's
  process, not in a sandboxed command.

## 5. Failure modes

| failure | effect |
|---|---|
| model not downloaded | BM25 only; doctor says how to fetch it |
| model file corrupt / hash mismatch at load | BM25 only; doctor says re-download |
| download hash mismatch | partial deleted, clear error, nothing enabled |
| endpoint 401 | BM25 only; one warning; ledger error row; doctor shows it |
| endpoint 429 | BM25 only for that call; reindex waits and retries |
| endpoint slow | 5 s timeout, then BM25 |
| wrong dimension back | treated as an error (never stored) |
| model changed in config | old vectors ignored; rows stale until re-embedded |
| older ferrule writes rows | rows stale; content change clears the vector |
| reindex killed | next run resumes from the rows still stale |

## 6. Out of scope

- `search_history` over vectors.
- An ANN index; sqlite-vec.
- A routing kNN (research-routing §3.3). M30 doesn't block it: the
  embedder is its own crate, callable without memory, and vectors carry
  their model id.
- Re-ranking with a cross-encoder, or with the LLM.
- GPU, or transformer embedders in process.
- Changing the eval suite, graders or mock model.
