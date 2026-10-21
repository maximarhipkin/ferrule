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

M58.5 does that split. `ferrule eval` writes the line into each task
run's transcript and keeps it in `run.json` (`harness`). There it also
hashes the system prompt, with the workspace path replaced (`prompt`),
and the tool definitions (`tools`). `config` is the agent's settings and
profile there, not a config file. The "since the last run" diff names
the parts that changed (`docs/eval.md`).

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

## M53.2 — evidence discipline and honest clipping

Built 2026-10-21 on `autopilot`, from the gap analysis's M53.2.

**Why.** The reflector clipped each tool result to 1,500 characters and
read only the last 12,000 of a transcript. Harness² refuses to clip
below 10,000 characters, "in the range that fabricates defects"
([spec.py](https://github.com/google-research/harness2/blob/1cfe254e8d25b15f37048c3730134686abe60335/harness2/core/spec.py#L163-L167)):
a test log cut at 1,500 characters reads like a run that stopped short.
Its reflector rules also treat "keep the base" as a real answer and an
edit that can't name its moment as speculation
([prompts.py](https://github.com/google-research/harness2/blob/1cfe254e8d25b15f37048c3730134686abe60335/harness2/core/prompts.py#L859-L874)).

**What changed.**

- **Clipping** (`episode.rs`). `TOOL_RESULT_CHARS` is 10,000 and
  `TAIL_CHARS` 40,000, about 10k tokens, room for four results that
  size. A result over the limit keeps its head and its tail, with
  `[… N characters clipped here by the renderer, not by the run]` in
  between. The left-out-messages line and the cut of one oversized
  message say "by the renderer" too. The reflector has no tools, so
  serving the transcript as a file it reads was not an option.
- **Budget.** At most five episodes a pass is about 50k tokens of
  reflector input, inside `[learning]`'s default 300,000 tokens a pass.
  A judge call (M53) reads two tails, about 20k tokens.
- **Rules** (`reflect.rs`'s system prompt): no change is the default
  and a fine answer; every change cites the transcript moment it rests
  on; a defect seen once in one run is noise (only a mistake the
  transcript shows more than once counts); an outage, provider error or
  one-off gets nothing; truncation markers are the renderer's, not the
  agent's behaviour. The judge's prompt gets the truncation line too.
- **Evidence is required.** `add`, `edit` and `retire` carry
  `"evidence"`. Without it the answer is rejected as "not a valid
  delta: no evidence". A kept change keeps its evidence in `pass.json`,
  `log.jsonl` and the changelog line.

**Not done.** Nothing checks that the cited moment is really in the
transcript; the owner reads it next to the change. No real pass has run
with the new prompt.

## M53.3 — learned skills and scripts, validated

Built 2026-10-21 on `autopilot`, from the gap analysis's M53.3.

**Why.** In Harness²'s ablation, taking skills away cost 5.3 points,
scripts 3.7 and prompt lines 1.5, and skills and scripts were about half
of all its kept edits
([arXiv App. E](https://arxiv.org/html/2610.14224);
[tree.py's validators](https://github.com/google-research/harness2/blob/1cfe254e8d25b15f37048c3730134686abe60335/harness2/core/tree.py#L216-L390)).
The pass could only write one-line lessons. A procedure the agent
fumbled twice is better kept as a skill it loads when a task matches.

**The delta.** The reflector may answer
`{"op":"skill","name":…,"description":…,"body":…,"scripts":{"scripts/x.sh":…},"evidence":…,"reason":…}`
instead of a lesson (`reflect.rs`). It is told when to prefer one (a
multi-step procedure fumbled more than once) and which learned skills
already exist. Evidence is required, as for every change (M53.2).

**The validators** (`ferrule-learn/src/skills.rs`), in order. Any
failure rejects the skill with its reason in `pass.json` and the
changelog.
1. **Shape:** a name of lowercase words joined by hyphens, at most 40
   characters; a one-line description of 10–300 characters that passes
   the lesson screen; a body of at most 8,000 characters; at most five
   scripts at `scripts/<file>`, each at most 8,000 characters; and no
   path into ferrule's own data (`data/private`, `secrets.env`,
   `data/learn`, `playbook.md`, `skills.json`).
2. **M13's path:** the draft is written to a temp directory and read
   back with ferrule-extensions' `inspect`, the function `skill_keep`
   uses. That checks the frontmatter, the name and the description, and
   runs the injection scan over SKILL.md and its text files. The same
   scanner reads every script. A block finding rejects.
3. **Reachable:** discovery over the draft finds it, the model may
   activate it, its description reads back as written, and the catalog
   lists it by name.
4. **Every referenced script ships:** each `scripts/…` path the body
   names is a file in the draft.
5. **Scripts parse:** `sh -n`, `bash -n` or `python3 -I -m py_compile`,
   picked by extension or shebang. They run in the sandbox (the gate's),
   with the draft as their workspace, never outside it. A tool that
   isn't installed skips its scripts with a note, and so does a platform
   without `sh` (Windows). Other languages are kept unparsed with a note.

**The gate.** The same contrast gate as a lesson. With a check, the
episode's task is re-run in a scratch copy where the draft is a project
skill (`.ferrule/skills/<name>`); it must finish and pass the check
twice. Without one, the probe runs the same way and the judge contrasts
the two transcripts.

**Kept.** The draft is copied (no symlinks followed) to
`<data>/learned-skills/<name>/`, and `<data>/learn/skills.json` records
the SHA-256 of its SKILL.md and the pass. A learned skill of the same
name is moved into the pass folder first (`skills/<n>-<name>`), and the
change is a `replace`. The journal's `Change::Skill` carries the name,
description, scripts, digest, gate, evidence and any validator notes.
`ferrule learn show` lists the learned skills, `learn diff` the skill
changes, and `learn revert` removes a kept skill (and puts back the one
it replaced) when its SKILL.md is still the one the pass wrote.

**Loading** (`ferrule-skills`, `ferrule-cli`). `<data>/learned-skills`
is the last discovery root, with the new origin `Scope::Learned`
(`ferrule skills` shows `learned`). Every other skill of the same name
wins. Each agent's view of the live set (`SkillsHandle::filtered`,
`learn::skill_view`) keeps a learned skill only while its SKILL.md
matches the digest in `skills.json`, which sandboxed commands can't read
or write, and a fresh injection scan of it is clean. That is the lock of
M13's draft-check-scan-lock path. A learned skill is offered through the
catalog and `activate_skill`, never by a keyword trigger. Commands can
read the directory, so its scripts run. Under `workspace-write` and owner
mode they can't write it, unless the data dir sits inside the workspace
or the sandbox is off; then the digest lock is what holds.

**Not done.** No real pass has proposed a skill yet. An owner who edits
a learned skill by hand makes it stop loading (its digest changed); the
fix is to move it to their own skills folder. The gate re-runs the
episode with the skill offered, but nothing checks that the agent
activated it.

## M53.4 — scoped lessons and skills; add one, cut one

Built 2026-10-21 on `autopilot`, from the gap analysis's M53.4.

**Why.** Scoping was worth 2.9 and 3.7 points in Harness²'s ablation
([arXiv Table 3](https://arxiv.org/html/2610.14224)), and its domain
prompt says "Never store this matter's facts as durable memory"
([prompts.py](https://github.com/google-research/harness2/blob/1cfe254e8d25b15f37048c3730134686abe60335/harness2/core/prompts.py#L372-L408)).
Every lesson went into every agent's prompt, so a lesson from one
project or one chat could mislead another. And a full playbook refused
every new lesson instead of letting an old one go.

**Scopes** (`ferrule-learn/src/scope.rs`, `learn::scope_for`):
- `task:<id>` for a scheduled task's runs (`scheduler__<id>`);
- `chat:<channel>__<chat>` for a gateway chat;
- `workspace:<path>` for everything else (a terminal run or chat).

A sub-agent shares its root's. `build_agent_from` writes the scope as a
`learn_scope` meta line in the session's transcript. The pass reads it
for a session episode; a task episode gets the task's. An episode with
neither (a transcript from before M53.4) gets the pass's workspace.

**Lessons.** A lesson the pass adds is written `- [pb-N @scope] text`.
A prompt gets the global lessons (no scope: the owner's lines and
promoted ones) and its own scope's (`prompt_block_for`,
`learn::prompt_section`). The reflector sees the same view, and an edit
or retire must name a lesson in it. `ferrule eval`'s opt-in owner
playbook gets the global lessons only.

**Skills.** `learn/skills.json` records each learned skill's scope.
Each agent's view (`learn::skill_view`) offers a learned skill only when
it is global or of the agent's scope, and the reflector lists only
those.

**Promotion.** A lesson the reflector proposes that repeats (word-set
Jaccard ≥ 0.9) a pass lesson of another scope becomes a `promote` of
that lesson: its scope tag goes, and it is global. A skill kept under a
name another scope already has is a `promote` too: the new draft
replaces it, global. Both are gated first, like any add, with the
promoted lesson or skill in the gate run's prompt. Both are revertible:
`learn revert` puts the scope back, or the old skill.

**Add one, cut one.** Once the view reaches 90% of `max_bullets` (36 of
40), an add or a promotion must retire a lesson of the view as well.
The reflector is told and may name it (`"retire":"pb-N"`). Without a
valid pick, the pass retires the `[pb-N]` lesson no pass has kept
(added, edited or promoted) for longest; `learn/state.json`'s `touched`
holds those times. Ferrule can't see whether an agent followed a
lesson, so "unused" can only mean "not kept again". The retire is gated
with the add, as one candidate, and journaled as its own change ("made
room for add pb-N"). An add is refused only when the view is full and
holds no pass lesson to retire.

**Not done.** No real pass has run with scopes. A gateway chat and a
terminal run in the same workspace are different scopes. A gateway's
workspace has no scope of its own, so a lesson from one chat never
reaches the gateway's other chats until a second chat keeps it.
