# M40 — build diet: smaller test builds, one test binary per crate

**Status:** built, 2026-09-29 (branch `m40-build-diet`). Design first,
then the as-built numbers in §4–§5.

**For contributors, in one line:** a crate's integration tests live in
`tests/it/`; to add tests, add a module to `tests/it/main.rs`, not a file
in `tests/`. Run one former file with `cargo test -p <crate> --test it <file>::`.

Max, 29.09: "the builds pollute the disk". On 28.09 the shared target dir
reached 24 GB and filled the 72 GB disk, which killed the M39 run mid-eval.
M40 cuts what one build of the workspace's tests costs on disk, and how
many executables every build writes.

## 0. What stays as it is

- **Every test stays.** The same tests run, under new names: `trust::foo`
  in the `it` binary instead of `foo` in the `trust` binary. §5 counts
  them before and after.
- **The release profile doesn't change.** Nothing here touches what users
  install.
- **The eval starter suite, its graders and the mock model don't change.**
- **Unit tests** (`#[cfg(test)]` in `src/`) stay where they are. They are
  one binary per crate already.

## 1. Baseline, before any change

Measured on this machine (4 cores, rustc 1.98.1), from an empty
`CARGO_TARGET_DIR`, `CARGO_INCREMENTAL=0`, no `CARGO_PROFILE_*` in the env.
The script is in §6.

| | before | + dev profile | + one binary per crate |
|---|---|---|---|
| `cargo build --workspace --tests`, from empty | 277 s | 202 s | 194 s |
| target dir after it | 10.66 GB | 3.65 GB | **2.62 GB** |
| executables in `debug/deps` | 102, 8.69 GB | 102, 2.57 GB | **51, 1.53 GB** |
| `cargo test --workspace`, second run (link nothing, run all) | 124 s | 121 s | 90 s |
| touch `ferrule-core/src/lib.rs`, then `cargo test --workspace` | 297 s | 259 s | 206 s |
| target dir after that | 10.67 GB | 3.65 GB | 2.62 GB |
| + `cargo clippy --workspace --all-targets` | 10.94 GB | 3.86 GB | 2.83 GB |
| + every crate's version bumped (a release), `cargo test --workspace --no-run` | **20.10 GB**, 203 exes | 6.65 GB, 203 exes | **4.59 GB**, 101 exes |
| tests passed / failed / ignored | 1628 / 0 / 28 | 1628 / 0 / 28 | 1628 / 0 / 28 |
| test binaries cargo runs (incl. unit and doc tests) | 99 | 99 | 48 |

The integration-test binaries went from 74 (one per file) to 23: one
`it` per crate that has integration tests (19), plus the four files in
§2.3. The other executables are the crates' unit-test binaries and the
`ferrule`, `ferrule-sandbox-launch` and `ferrule-fake-claude` binaries.

### What actually grows the disk

The brief assumed that every code change relinks every test binary under
a new hash. The measurement says otherwise. A test binary's hash comes
from the package id (name + **version** + source), the profile, the
features, the target and the compiler, not from the source text. An edit
relinks all the test binaries (176 s for a one-line change to
`ferrule-core` before M40) but overwrites them in place: the target dir
grew by 4 KB.

What leaves a second copy of everything behind is a change to one of
those inputs:
- **a release**, since every milestone bumps every crate's version; the
  bump alone took the dir from 10.9 GB to 20.1 GB;
- a new rustc (`rustup update`);
- a different profile or `CARGO_PROFILE_*` env, e.g. a run with
  `CARGO_PROFILE_DEV_DEBUG` set next to one without;
- `-p <crate>` builds that unify features differently from `--workspace`;
- a `Cargo.lock` change to a dependency, which re-hashes it and everything
  above it.

A shared dir that sees several milestones' releases and a toolchain
update fills up at ~9–10 GB a round. So the fix is the one the brief asked
for — **make each round small** — plus the sweep that already runs outside
the repo, which now has much less to sweep. Making the stale copies go
away is cargo's job (`cargo clean gc` is still unstable), not ours.

## 2. Decisions

### 2.1 A dev profile in `Cargo.toml`

```toml
[profile.dev]
debug = "line-tables-only"

[profile.dev.package."*"]
debug = false
```

- **Why `line-tables-only` for our crates:** a failing test's backtrace
  still names `crates/…/foo.rs:123` (checked with a deliberately failing
  test, §4), and a debugger can still step by line. What goes are the
  type and variable descriptions (`.debug_info`, most of `.debug_str`),
  which nobody on this project reads in a debugger.
- **Why no debug info for dependencies:** a backtrace through tokio or
  hyper frames shows the function names from the symbol table anyway; the
  file:line inside a dependency is rarely what a test failure needs.
- **Tests inherit it.** `[profile.test]` inherits from `dev`, so both
  `cargo build` and `cargo test` use it. CI, the owner's machine and the
  dev container all get it without an env var.
- **`split-debuginfo` and `strip`:** see §4 for what was measured.
  - **Not `strip = "debuginfo"` on dev.** It would take the line tables
    out, and backtraces would lose file:line.
  - **Not `strip = "symbols"`.** Backtraces would lose the function names.
  - **`split-debuginfo` is left at each OS's default.** macOS already
    defaults to `unpacked`, which leaves the DWARF in the `.o` files and
    doesn't link it into the executable. On Linux, `unpacked` moves the
    DWARF into `.dwo` files next to the objects, so the target dir keeps
    it anyway. Windows keeps its `.pdb` either way.

### 2.2 One test binary per crate

Each crate's `tests/*.rs` becomes `tests/it/<name>.rs`, and
`tests/it/main.rs` declares one `mod` per former file. The shared helpers
(`tests/common/`, the gateway's `tests/support/`) become `mod common` /
`mod support` in the same binary, compiled once instead of once per file.

- **Why:** each test binary links the whole dependency tree of its crate,
  50–200 MB each, and there were 74 of them.
  One binary per crate links it once. The link step also runs once per
  crate instead of once per file, and linking is most of an incremental
  test build.
- **Every crate moves, even the ones with a single file**, so the rule
  has no exceptions: `--test it` everywhere.
- **Fixtures stay put** (`tests/fixtures/`, `tests/golden/`). A test that
  used `include_str!("fixtures/…")` now says `"../fixtures/…"`; paths
  built from `CARGO_MANIFEST_DIR` don't change.
- **A test that re-runs its own binary** as a helper
  (`--exact child_refresh`) now names the helper by its module path
  (`chatgpt::child_refresh`).

### 2.3 What stays its own binary, and why

Merging files puts their tests in one process, running in parallel. A
file stays separate if it changes something process-wide that the other
files' tests read. The search: `set_var`, `remove_var`,
`set_current_dir`, `static` singletons, `OnceLock`/`thread_local`,
`serial` attributes, `ctor`, signal handlers, panic hooks, global
subscribers, fixed ports and fixed temp paths.

| File | Why it stays separate |
|---|---|
| `ferrule-plans/tests/engine_env.rs` | sets `ANTHROPIC_API_KEY` and friends in the process env; the other engine tests would inherit them |
| `ferrule-sandbox/tests/windows.rs` | sets `FERRULE_SHELL=powershell`, and the sandbox picks its shell once per process |
| `ferrule-sandbox/tests/windows_git_bash.rs` | sets `FERRULE_SHELL=bash`, same reason |
| `ferrule-mcp/tests/sandbox.rs` | on Windows sets `FERRULE_SHELL=powershell`; `browser.rs` starts a sandbox too, and whichever runs first would pick the shell for both |

The two Windows files are `#![cfg(windows)]`, so on Linux and macOS they
build to a tiny empty binary.

What was found and judged safe to merge:
- `ferrule-sandbox/tests/enforcement.rs` and `ferrule-mcp/tests/mcp_client.rs`
  set `FERRULE_TEST_*` variables that only they read.
- `ferrule-proxy/tests/egress.rs` sets `NO_PROXY=localhost,127.0.0.1,::1`.
  The other proxy tests set their proxy explicitly, which `NO_PROXY`
  doesn't touch, and a loopback bypass is what every hermetic test wants.
- The statics (`TOOLS_OFFERED` in the agents helpers, the gateway mocks'
  shared runtime, `claude_plan.rs`'s fake-binary path) were already shared
  by every test in their binary; the entries are keyed per test.
- No test changes the CWD, installs a signal handler, a panic hook or a
  global tracing subscriber, or uses `serial`/`ctor`.

## 3. Failure modes

- **Two former files now run at the same time.** Before, cargo ran test
  binaries one after another, so a test in `trust.rs` never overlapped
  one in `models.rs`. Now they share a thread pool. A hidden shared
  resource (a fixed port, a fixed temp path, an env var) would show up as
  a flake. The mitigation is the audit in §2.3 and three green CI runs on
  three OSes; the fix for a new one is to split the file back out with the
  reason, not to add sleeps.
- **More tests in parallel per binary.** Slow real-binary tests
  (ferrule-cli's) now overlap with each other more. Their timeouts were
  already sized for a loaded CI runner.
- **Without `--no-fail-fast`, cargo stops at the first failing binary.**
  That binary is now a crate's whole integration suite, not one file, so
  a local `cargo test` without the flag reports fewer results after a
  failure. CI already passes the flag.
- **Backtraces.** If `line-tables-only` ever loses a file:line we need,
  `CARGO_PROFILE_DEV_DEBUG=true` brings full debug info back for one run.

## 4. After the profile

The target dir went from 10.66 GB to 3.65 GB, and the test executables
from 8.69 GB to 2.57 GB: most of the debug info was the dependencies'.
The biggest executable left, `ferrule-cli`'s unit tests, is 191 MB: 55 MB
of code, 17 MB `.debug_str` and 15 MB `.debug_line` (mostly std's own,
which ships prebuilt), and the symbol table.

**The backtrace check.** A deliberately failing integration test in
`ferrule-core` (an out-of-bounds index in a helper), run with
`RUST_BACKTRACE=1`, printed:

```
thread 'm40_backtrace_probe' panicked at crates/ferrule-core/tests/m40_backtrace_probe.rs:7:13:
   3: m40_backtrace_probe::helper
             at ./tests/m40_backtrace_probe.rs:7:13
   4: m40_backtrace_probe::m40_backtrace_probe
             at ./tests/m40_backtrace_probe.rs:4:5
```

So file:line survives for our frames. The test was then removed.

**`split-debuginfo` and `strip` were not added** (§2.1): with only line
tables left, splitting moves little off the executables on Linux and
none of it out of the target dir, and either `strip` level would cost
the file:line or the function names above.

## 5. After one binary per crate

Another 1 GB off a fresh build (3.65 → 2.62 GB), half the executables,
and a quarter less time for a test run after an edit (259 → 206 s),
because the link step runs 19 times instead of 74. A release now adds
~2 GB to a shared dir instead of ~9.4 GB before M40.

**The test count** is the same before and after: 1628 passed and 28
ignored, and the set of test names is identical once the module prefix
(`trust::`) is taken off the `it` binaries' names (1656 names, compared
line by line).

**Two files re-run their own test binary** as a helper process
(`chatgpt::child_refresh`, `enforcement::net_probe_helper` and
`unix_probe_helper`); their `--exact` names now carry the module. A
missed rename would have matched no test and exited 0, which the network
probe would read as "the network works", so each name was checked with
`--list --exact` against the built binary.

**`ferrule-cli`'s dashboard code** includes the connections tests' mocks
through `#[path]` (for its own unit tests); that path moved to
`tests/it/common/mod.rs`, as did `channels.rs`'s path to the gateway's
`tests/it/support/`.

**CI guards the layout.** A step on Linux fails if `crates/*/tests/`
holds a `.rs` file other than the four in §2.3, and says where it goes.

## 6. How it was measured

From the worktree, with the toolchain env,
`CARGO_TARGET_DIR` pointing at an empty dir and `CARGO_INCREMENTAL=0`:

```bash
rm -rf "$CARGO_TARGET_DIR"
time cargo build --workspace --tests
du -sb "$CARGO_TARGET_DIR"
find "$CARGO_TARGET_DIR/debug/deps" -maxdepth 1 -type f -executable ! -name '*.so' | xargs du -cb | tail -1
cargo test --workspace --no-fail-fast            # first run
time cargo test --workspace --no-fail-fast       # the measured run
touch crates/ferrule-core/src/lib.rs
time cargo test --workspace --no-fail-fast
du -sb "$CARGO_TARGET_DIR"
cargo clippy --workspace --all-targets -- -D warnings
sed -i 's/^version = "0.10.0"/version = "0.10.1"/' crates/*/Cargo.toml
time cargo test --workspace --no-run             # a release, then revert
du -sb "$CARGO_TARGET_DIR"
```

## 7. Out of scope

- **Deleting stale artifacts.** `cargo clean gc` is unstable; the
  repo-external sweep stays.
- **A faster linker** (`mold`, `lld`): a per-machine choice, not something
  the repo can require on three OSes.
- **Unit-test binaries.** They are one per crate already.
- **`opt-level` for dependencies in dev.** It would make tests run faster
  but compile slower and bigger; a separate measurement.
- **`cargo nextest`.** It would run each test in its own process and bring
  back the isolation the merge gives up, but it is an extra tool for every
  contributor and CI.
