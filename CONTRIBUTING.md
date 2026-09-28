# Contributing

Contributions are welcome. This file is the short version of how the repo
works; [PLAN.md](PLAN.md) is the shared working log — read it first. It
holds the current state, the open gaps, and a dated entry for every
session, and it's where your change should leave its own dated entry.

## Building

You need Rust 1.85 or newer ([rustup](https://rustup.rs)) and a C compiler
(`cc`/`clang`, or MSVC on Windows) for the bundled SQLite and `ring`. The
Linux sandbox needs kernel 5.13 or newer; on older kernels commands run
unsandboxed, or ferrule refuses to start if `[sandbox] require = true`.

```bash
cargo build --workspace
cargo install --locked --path crates/ferrule-cli   # the `ferrule` binary
```

## Testing

```bash
cargo test --workspace                     # the suite (macOS and Windows cfg out the platform-only tests)
cargo test -p ferrule-proxy -- --ignored   # + a live end-to-end run through the real network
python3 tests_e2e/setup_wizard.py          # the setup wizard in a real terminal (Linux, needs pexpect)
python3 tests_e2e/hidden_keys.py           # the agent can't reach the saved keys
```

CI runs `cargo test --workspace --locked --no-fail-fast` on Linux, macOS and
Windows, plus a live sandbox self-test, the dashboard browser check, and on
Linux the two end-to-end scripts above. Keep the suite green on the
platforms your change touches.

## Lint and format

```bash
cargo clippy --workspace --all-targets     # keep it clean
rustfmt --edition 2021 <files you touched> # the tree is rustfmt-formatted; keep it that way
```

Format the files you touch; don't reformat files your change doesn't
otherwise need to edit — it keeps diffs reviewable.

## Conventions

- **Dependencies** go in the root `[workspace.dependencies]` table and are
  referenced as `dep = { workspace = true }` from the crates, so versions
  stay in one place.
- **Docs** live in `docs/`, one file per topic or milestone, linked from the
  README's Documentation section. Every claim in a doc should be verifiable
  against the code or a cited source — that rule is the project's brand.
- **License:** the workspace is dual-licensed under MIT OR Apache-2.0
  ([LICENSE-MIT](LICENSE-MIT), [LICENSE-APACHE](LICENSE-APACHE)). New crates
  carry `license = "MIT OR Apache-2.0"` in their `Cargo.toml`. Unless you
  state otherwise, contributing a change means you license it the same way.
- **Security issues** are reported privately, not as issues or PRs — see
  [SECURITY.md](SECURITY.md).

A `v*` tag builds the release archives for every platform and publishes
them with the install scripts; releases are signed.
