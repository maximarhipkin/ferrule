# Memory

The agent keeps long-term facts in one SQLite file (`<data>/memory.db`).
It stores them with `remember`, corrects them with `update_memory` (the old
version stays as history and is never recalled), deletes them with `forget`,
and looks them up with `recall`. At the start of every session the facts
closest to the goal are recalled into the prompt, within about 2,000
characters, with recent facts ranked higher. Design: [m15-memory.md](m15-memory.md),
[m30-vector-recall.md](m30-vector-recall.md).
Memories from OpenClaw or Hermes Agent come over with `ferrule import`
([migrate.md](migrate.md)).

## Keyword recall and recall by meaning

Out of the box, recall is keyword search (SQLite FTS5, BM25). It finds a
fact when the question shares a word with it. It misses a paraphrase
("where do we ship production?" for "The deploy target is render"), a
synonym, a typo, and a Hebrew question about a fact written in English.

Turn on an **embedder** and recall also searches by meaning. Both searches
run and their results are merged, so a question that keyword search
already answers still gets the same answer.

| `[memory] embedder` | what | key | cost |
|---|---|---|---|
| `"off"` (default) | keyword search only | — | — |
| `"local"` | a 531 MB multilingual model ([potion-multilingual-128M](https://huggingface.co/minishlab/potion-multilingual-128M)), run in ferrule's process | none | none; nothing leaves the machine |
| `"openai"` | any OpenAI-compatible `/v1/embeddings` endpoint: OpenAI, Ollama, llama.cpp, vLLM, LM Studio | the endpoint's, if it has one | a ledger row per request |

The local model is the recommended one. It handles Hebrew and English, and
some of the time it matches a question in one to a fact in the other. On
the benchmark below it triples first-place recall on paraphrases and
misspellings, and it costs microseconds per fact.

## Turning it on

Run `ferrule setup` → **Memory recall**, and pick *local model* or one of
your providers. For the local model, setup states the size and downloads
only after you confirm. Or:

```sh
ferrule memory model download     # asks first; --yes to skip the question
```

and then in the config:

```toml
[memory]
embedder = "local"
```

The model goes to `<data>/models/potion-multilingual-128M@73908c3/`. It is
fetched from Hugging Face at a pinned revision, through ferrule's proxy
settings, and each file is checked against a SHA-256 built into ferrule.
A file that doesn't match is deleted and the download fails with both
hashes. Nothing is ever downloaded unless you ask for it.

For an endpoint:

```toml
[memory]
embedder = "openai"
provider = "openai"                 # borrow [providers.openai]'s base_url and key
model = "text-embedding-3-small"
dimensions = 512                    # optional for OpenAI's models, required for others
price_input_per_mtok = 0.02         # the cost in the ledger; unpriced when unset
max_requests_per_minute = 60        # the pace of `ferrule memory reindex`
```

A local server needs no key: set `base_url` (e.g.
`"http://localhost:11434/v1"` for Ollama) and leave out `provider` and
`api_key_env`. As with model providers, the key stays in the credential
proxy and ferrule itself only holds a placeholder. **Every fact you store and
every recall query is sent to that endpoint.** If you don't want that, use
the local model.

Tuning, rarely needed:

```toml
merge = "weighted"      # or "rrf"
vector_weight = 0.7     # the similarity's share of a weighted merge
min_similarity = 0.3    # below this, a fact can only match by keyword
```

## Existing memories: `ferrule memory reindex`

Each fact's vector is stored with the model that made it, and vectors of
different models are never compared. Facts with no vector for the current
model (everything stored before you turned the embedder on, or before you
changed the model) are still found by keyword. They catch up by meaning:

- **by themselves**: after each session-start recall, up to 32 of them are
  embedded in the background;
- **all at once**: `ferrule memory reindex` (`--batch N`, default 32). It
  is safe to interrupt: the next run picks up where it stopped. Against an
  endpoint it keeps to `max_requests_per_minute`. On a 429 it waits (using
  `Retry-After` when the endpoint sends one) and retries.

`ferrule doctor` shows the embedder, its model and how many live facts are
embedded (`hybrid recall · local:potion-multilingual-128M@73908c3/256 ·
312/340 live memories embedded`). If the local model is missing,
incomplete or corrupt, doctor says what to run.

## When the embedder fails

The embedder can fail: the model isn't downloaded, the endpoint is down or
refuses the key, rate limits, times out after 5 s, or answers with the
wrong dimension. Then recall is exactly keyword recall, and the session
shows no error. The cause is logged once per process (visible with
`RUST_LOG=warn`, and in the gateway's journal). With an endpoint, a failed
request is still a ledger row, with no cost. A fact stored while the
embedder is failing is saved without a vector and embedded later.

## Near-duplicates

When the agent stores a fact whose meaning is very close to one it already
has (cosine ≥ 0.8), the `remember` reply lists the existing one, so the
agent can use `update_memory` instead. Nothing is merged automatically.

## Limits

- Search by meaning compares the query with every embedded fact (brute
  force). That is tens of milliseconds up to roughly 50,000 facts with the
  local model, or about 10,000 with a 1,536-dimension endpoint model. A
  personal agent has hundreds to a few thousand.
- `search_history` (compacted tool results) stays keyword-only.
- Cross-lingual recall works some of the time, not always (2 in 10
  Hebrew↔English questions answered first on the benchmark).
- A build without the `local-embed` feature
  (`cargo build --no-default-features`) has no local model. There,
  `embedder = "local"` falls back to keyword recall, and doctor says why.

## The benchmark

`crates/ferrule-memory/tests/fixtures/recall_bench.json` holds 80 facts
(some in Hebrew) and 48 questions. With the local model:

| questions | keyword r@1 / r@5 / MRR | weighted (default) | RRF |
|---|---|---|---|
| all (48) | .333 / .375 / .358 | **.625 / .750 / .684** | .542 / .771 / .644 |
| paraphrase (10) | .20 / .20 / .217 | **.60 / .80 / .683** | .40 / .80 / .583 |
| Hebrew↔English (10) | 0 / 0 / 0 | .20 / .30 / .250 | .20 / .40 / .258 |
| synonym (8) | .25 / .25 / .250 | **.75 / .875 / .812** | .625 / .875 / .750 |
| typo (6) | .167 / .167 / .167 | .50 / .667 / .583 | .50 / .667 / .583 |
| misleading keywords (6) | .50 / .833 / .667 | **.833 / 1 / .917** | .667 / 1 / .833 |
| keyword (8) | 1 / 1 / 1 | 1 / 1 / 1 | 1 / 1 / 1 |

To run it yourself, after `ferrule memory model download`:

```sh
FERRULE_EMBED_MODEL_DIR=<data>/models/potion-multilingual-128M@73908c3 \
  cargo test -p ferrule-memory --test bench -- --ignored --nocapture
```
