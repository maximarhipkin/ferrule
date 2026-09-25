#!/usr/bin/env bash
# The dashboard's smoke test, about 2 minutes (docs/dashboard.md, "Smoke
# test"). Builds ferrule (or uses $FERRULE_BIN), then runs
# scripts/dashboard_smoke.py: a mock model, a fake Telegram, a gateway on a
# temp config, /dashboard, login, the tunnel if cloudflared is installed,
# the live OpenRouter catalog once, /dashboard off. PASS/FAIL/SKIP per step;
# exits non-zero on any FAIL. Never touches your own config or data.
set -euo pipefail
cd "$(dirname "$0")/.."

py=$(command -v python3 || command -v python || true)
if [ -z "$py" ]; then
  echo "FAIL  python3 — the mocks need it"; exit 1
fi

if [ -n "${FERRULE_BIN:-}" ]; then
  bin=$FERRULE_BIN
  echo "PASS  build — using \$FERRULE_BIN ($bin)"
else
  if ! cargo build --release -p ferrule-cli --bin ferrule >/dev/null 2>&1; then
    echo "FAIL  build — cargo build --release -p ferrule-cli failed"; exit 1
  fi
  bin="${CARGO_TARGET_DIR:-target}/release/ferrule"
  echo "PASS  build — $bin"
fi

exec "$py" scripts/dashboard_smoke.py --bin "$bin" "$@"
