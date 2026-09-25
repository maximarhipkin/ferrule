# The dashboard's smoke test, about 2 minutes (docs/dashboard.md, "Smoke
# test"). Builds ferrule (or uses $env:FERRULE_BIN), then runs
# scripts/dashboard_smoke.py: a mock model, a fake Telegram, a gateway on a
# temp config, /dashboard, login, the tunnel if cloudflared is installed,
# the live OpenRouter catalog once, /dashboard off. PASS/FAIL/SKIP per step;
# exits non-zero on any FAIL. Never touches your own config or data.
$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")

$py = Get-Command python3 -ErrorAction SilentlyContinue
if (-not $py) { $py = Get-Command python -ErrorAction SilentlyContinue }
if (-not $py) { Write-Output "FAIL  python — the mocks need it"; exit 1 }

if ($env:FERRULE_BIN) {
    $bin = $env:FERRULE_BIN
    Write-Output "PASS  build — using `$env:FERRULE_BIN ($bin)"
} else {
    cargo build --release -p ferrule-cli --bin ferrule *> $null
    if ($LASTEXITCODE -ne 0) { Write-Output "FAIL  build — cargo build --release -p ferrule-cli failed"; exit 1 }
    $target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { "target" }
    $bin = Join-Path $target "release/ferrule.exe"
    Write-Output "PASS  build — $bin"
}

& $py.Source scripts/dashboard_smoke.py --bin $bin @args
exit $LASTEXITCODE
