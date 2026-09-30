#!/usr/bin/env bash
# M44's memory and size measurements (docs/m44-managed-mode.md §6, §9).
# Builds ferrule in release (or uses $FERRULE_BIN), then runs
# scripts/m44_measure.py: a fake model that makes one shell tool call, a
# fake Telegram, a gateway on a temp config and data dir; prints idle RSS,
# the peak over one turn, and the binary size as a Markdown table.
# Linux only (it reads /proc). Never touches your own config or data.
set -euo pipefail
cd "$(dirname "$0")/.."

py=$(command -v python3 || command -v python || true)
if [ -z "$py" ]; then
  echo "FAIL  python3 — the mocks need it"; exit 1
fi
if [ "$(uname -s)" != "Linux" ]; then
  echo "FAIL  this reads /proc, so it runs on Linux only"; exit 1
fi

if [ -n "${FERRULE_BIN:-}" ]; then
  bin=$FERRULE_BIN
  echo "using \$FERRULE_BIN ($bin)" >&2
else
  cargo build --release -p ferrule-cli --bin ferrule >&2
  bin="${CARGO_TARGET_DIR:-target}/release/ferrule"
fi

exec "$py" scripts/m44_measure.py --bin "$bin" "$@"
