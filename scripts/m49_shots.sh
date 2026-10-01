#!/usr/bin/env bash
# The dashboard's screenshots for M49: every section at 390 and 1280 px, in
# the light and the dark theme, plus Home and Settings in Hebrew, as WebP
# (phones at 1.5x, desktops at 1x, quality 45), from a gateway on a temp
# config and data dir with the starter suite's mock model. No key, no network.
#
#     scripts/m49_shots.sh <ferrule binary> <out dir> [--measure]
#
# Like m47_shots.sh it is a thin wrapper over the rig in
# scripts/dashboard_browser_check.mjs.
set -euo pipefail
bin=${1:?usage: m49_shots.sh <ferrule binary> <out dir> [--measure]}
out=${2:?usage: m49_shots.sh <ferrule binary> <out dir> [--measure]}
shift 2
here=$(cd "$(dirname "$0")" && pwd)
export NO_PROXY="localhost,127.0.0.1,::1"
exec node "$here/dashboard_browser_check.mjs" --bin "$bin" --shots-all "$out" --shots-format webp --shots-quality 45 "$@"
