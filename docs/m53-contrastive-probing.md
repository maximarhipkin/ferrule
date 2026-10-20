# M53 — contrastive probing: lessons when there's no check

**Status.** Built 2026-10-21. Source: `docs/research-harness2-media.md`
§4 (M53), itself from Harness² (arXiv:2610.14224).

## What changes

M16's learning pass keeps a playbook delta only when re-running the
failed task **with the lesson passes its check twice**. That is the
right gate — but most real tasks have no check command, so most
failures today teach the playbook nothing. M53 gives those episodes a
second gate, borrowed from Harness²'s one idea worth keeping: when
there is no verifier, **probe** — re-run the task once with the
candidate lesson in the prompt, then let a judge call contrast the two
transcripts and keep the delta only if the probe run is visibly better.

This is Propose–Probe–Compose scoped to ferrule's spine: K=1,
sequential, offline, inside the existing pass. No benchmark
infrastructure, no new subsystem.

## What is deliberately not in it

- **Online, mid-run harness editing** (Harness²'s task-level recursion
  proper). A harness that rewrites its own prompt mid-task breaks the
  byte-stable prompt prefix and the provider cache it exists to hit,
  and it makes "which harness produced this run" unanswerable. The
  playbook already changes *between* runs; that is the ferrule-shaped
  version of the same idea.
- **K > 1, parallel probes.** One probe per episode, per pass.
- **Rewriting the playbook.** ACE's context-collapse finding stands;
  deltas only, as M16 already enforces.
- **Judging memory merges.** Consolidation stays check-free and
  revertible through M15's UPDATE; a second judge there buys nothing.

## Parts

### 1. The probe path in the pass (`ferrule-learn`)

Today: `Plan` carries episodes with a check into the gate; episodes
without one are reflected on and dropped. M53 adds a `Probe` variant
next to the gated path. The mechanics are the ones `WorkspaceGate`
already has — `copy_workspace` into a scratch dir, re-run the task with
the candidate lesson appended to the prompt — only the verdict changes
hands: not "did the check pass" but "did the judge prefer the probe".

### 2. The judge (`judge.rs`, sibling to `reflect.rs`)

One fixed-template call, `call_kind = "learn"`, inside the pass's caps
like every other learn call:

- **Input**: the fixed rubric plus two rendered transcripts — the
  episode's own tail (`render_tail`, the same excerpt the reflector
  saw) and the probe run's tail. Never files, never memory contents:
  transcripts only.
- **Rubric, in order**: did it finish at all; did it finish with fewer
  retries and errors; is its final status truthful rather than a bare
  error; did it avoid new damage (a probe that writes outside the task
  or doubles the diff loses outright).
- **Output**: strict JSON (`json_object` parsing, the reflector's
  discipline) — `{better: bool, why: one sentence}`. **Tie, unreadable
  answer, judge error: reject.** Fail-closed, like the trust gates.
- **The judge is not the reflector's model by default**: it uses the
  pass's cheapest configured model; the episode's own model is fine as
  fallback. One call, no retries beyond the provider's own.

### 3. Kept honest, the M16 way

Everything lands in the pass folder next to the existing artifacts:
the proposal, both transcript excerpts, the judge's verdict line, and
the outcome — so `ferrule learn show <pass>` reads "kept by check" or
"kept by judge" (or "the judge said no") in words, and `learn diff` /
`learn revert` work unchanged. A kept-by-judge lesson is marked in the
playbook file with its provenance, so a later pass can prefer
check-gated lessons when trimming.

### 4. "Which harness produced this run" (transcript meta)

Harness² snapshots its eight components per run; ferrule gets the
affordable version: at session start the transcript's meta record gains
the active skill list (names + content hashes), the playbook hash, and
the config hash. Append-only, one line, no secrets (hashes, not
contents). The ledger row for the first call of a session can then
answer the question, and the M14 eval A/B can split on harness version
for free.

## Security and invariants

- The judge sees transcripts, which by invariant never contain secrets;
  the probe re-run executes in the scratch copy under the same sandbox
  and proxy as the gate's runs today.
- The byte-stable prompt prefix is untouched: probes run in their own
  scratch sessions, never inside a live one.
- Caps: a probe costs one task re-run plus one judge call, both
  `call_kind = "learn"`, both inside `[learning]`'s per-pass and per-day
  caps; an episode whose re-run would exceed the remaining budget is
  skipped with a note, not partially run.
- Sub-agents still can't write the playbook; eval still doesn't see it
  unless a suite opts in. Probing changes neither.

## Done means

A task that failed last night with no verify command — say a Telegram
briefing that errored twice — is tonight reflected on, probed once in a
scratch copy, and either teaches the playbook one line (with the
judge's sentence in the pass folder) or is honestly recorded as "no
lesson kept". `ferrule learn show` says which gate each kept lesson
passed. `make check` green with: judge prompt/parse unit tests
(including the fail-closed tie), a pass-level integration test with a
fake provider (no-check episode → probe → verdict → kept/rejected), a
caps test (probe skipped when the budget is short), and a transcript
meta test (skills + playbook hash recorded, no contents). Docs: this
file and a probing section in `docs/m16-learning-loop.md`.

## Not in scope

Restated for the next session: online refinement, K > 1, parallel
mode, judge ensembles, natural-language rubrics from the owner. If
probing proves itself on the nightly pass, the *next* milestone after
this one can consider a `/probe` chat door — the M43 pattern — that
runs one contrastive probe on demand for a task the owner names.
