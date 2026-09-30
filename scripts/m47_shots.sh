#!/usr/bin/env bash
# The dashboard's screenshots for M47: every section at 390 and 1280 px, in
# the light and the dark theme, as JPEGs, from a gateway on a temp config
# and data dir with the starter suite's mock model. No key, no network.
#
#     scripts/m47_shots.sh <ferrule binary> <out dir> [--measure]
#
# It is a thin wrapper: the rig (gateway, fake Telegram, Chromium over the
# DevTools protocol) is the one CI already runs, in
# scripts/dashboard_browser_check.mjs, which keeps it honest.
set -euo pipefail
bin=${1:?usage: m47_shots.sh <ferrule binary> <out dir> [--measure]}
out=${2:?usage: m47_shots.sh <ferrule binary> <out dir> [--measure]}
shift 2
here=$(cd "$(dirname "$0")" && pwd)
export NO_PROXY="localhost,127.0.0.1,::1"
exec node "$here/dashboard_browser_check.mjs" --bin "$bin" --shots-all "$out" "$@"
