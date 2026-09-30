# One command per repo operation, for people and agents alike.
# `make check` is the consistent-state predicate: the repo is green when
# it exits 0 (AGENTS.md).

.PHONY: setup check fmt clippy test dev

# Toolchain pieces the checks need (Rust 1.85+ via rustup, plus a C
# compiler for the bundled SQLite and ring).
setup:
	rustup component add rustfmt clippy

# The full pipeline: formatting, lint, tests. Must exit 0 before a commit.
check: fmt clippy test

fmt:
	cargo fmt --all -- --check

clippy:
	cargo clippy --workspace --all-targets

test:
	cargo test --workspace --locked --no-fail-fast

# The gateway daemon from the working tree (Ctrl-C to stop).
dev:
	cargo run -p ferrule-cli -- gateway
