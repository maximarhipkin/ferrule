# AGENTS.md — working on the ferrule repo

A directory page, not an encyclopedia: hard constraints and the commands
that verify them live here; everything else is pointed to, not copied.
Read [PLAN.md](PLAN.md) before your first change — it is the shared
working log (current state, open gaps, a dated entry per session), and
your session must leave its own dated entry there. Details:
[CONTRIBUTING.md](CONTRIBUTING.md).

## Verify (run before every commit)

```bash
make check        # = cargo fmt --check + clippy --workspace --all-targets + cargo test --workspace --locked
make test         # tests only
cargo test -p <crate> --test it <file>::   # one integration-test module
```

The repo is consistent when `make check` exits 0. Only a passing run
counts as done — confidence is not evidence. CI runs the same suite on
Linux, macOS and Windows.

## Hard rules (invariants — do not break these)

- **Compaction never cuts between a tool call and its results.** (M34;
  covered by tests in `ferrule-core`.)
- **Secrets never reach the model or its commands.** API keys live in
  `private/secrets.env`; commands see same-shaped placeholders, swapped
  only by the credential proxy for bound hosts. Never log a token.
- **The sandbox stays on by default.** New command-executing surfaces
  (tools, MCP, hooks) go through `ferrule-sandbox`, not around it.
- **Every run ends in a truthful status** — what's done, what's left,
  what blocks it. Never a bare error, never silent.
- **The prompt prefix stays byte-stable** so provider prompt caches hit;
  append, don't rewrite.
- Format what you touch with `rustfmt --edition 2021`; keep clippy
  clean. Don't reformat files your change doesn't need.

## Conventions

- **Tests:** integration tests are one binary per crate in
  `crates/<crate>/tests/it/` — add a module to `tests/it/main.rs`, not a
  file in `tests/`. Exceptions (process-wide env/CWD) are listed in CI;
  see [docs/m40-build-diet.md](docs/m40-build-diet.md).
- **Dependencies** go in the root `[workspace.dependencies]` table and
  are referenced as `dep = { workspace = true }`.
- **Docs:** one file per topic/milestone in `docs/`, linked from the
  README's Documentation section. Every claim verifiable against code or
  a cited source — that rule is the project's brand.
- **One logical change per commit**, message explains *why*. Keep
  `make check` green at every commit so the next session can resume.
- If context runs low: stop, update PLAN.md, commit a clean checkpoint —
  don't rush a finish.

## Map

- `crates/` — the workspace; per-crate responsibilities in the README's
  "Project layout" table. The agent loop is `ferrule-core`, the binary
  and all wiring is `ferrule-cli`, the daemon is `ferrule-gateway`.
- [docs/roadmap.md](docs/roadmap.md) — every milestone's design and
  status; per-milestone notes are `docs/mNN-*.md`.
- [docs/research-report.md](docs/research-report.md) — why the harness
  is built the way it is.
- `evals/` — the harness benchmark (`ferrule eval run evals/starter`);
  [docs/eval.md](docs/eval.md).
- `tests_e2e/` — Python end-to-end scripts (Linux).
