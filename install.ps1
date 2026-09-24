# Install ferrule on Windows, then set it up. In PowerShell:
#
#   irm https://raw.githubusercontent.com/maximarhipkin/ferrule/main/install.ps1 | iex
#
# It downloads the release zip, checks its sha256, puts ferrule.exe in
# %LOCALAPPDATA%\Programs\ferrule, adds that to your PATH and starts
# `ferrule setup` — the wizard that asks for the provider and key, Telegram
# and the rest. Running it again upgrades in place and keeps your settings.
#
# Native Windows has no OS sandbox in ferrule yet: shell commands the agent
# runs have your own permissions. For full isolation, run the Linux build
# under WSL2 (curl -fsSL .../install.sh | sh inside it).
#
# Optional environment:
#   $env:FERRULE_VERSION      a release tag such as v0.2.0 (default: the latest)
#   $env:FERRULE_INSTALL_DIR  where ferrule.exe goes
#   $env:FERRULE_NO_SETUP=1   install only, don't start the wizard
#   $env:GITHUB_TOKEN         optional: download through the API (a private fork)
#
# Everything runs inside a function, so `iex` leaves your session's
# preferences alone and a failure never closes the window.

function Install-Ferrule {
    $ErrorActionPreference = 'Stop'
    # Windows PowerShell's progress bar slows downloads to a crawl.
    $ProgressPreference = 'SilentlyContinue'
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $repo = 'maximarhipkin/ferrule'
    $version = if ($env:FERRULE_VERSION) { $env:FERRULE_VERSION } else { 'latest' }
    $dir = if ($env:FERRULE_INSTALL_DIR) { $env:FERRULE_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\ferrule' }

    # A 32-bit PowerShell on 64-bit Windows reports x86 here and the real
    # one in PROCESSOR_ARCHITEW6432.
    $arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
    switch ($arch) {
        'AMD64' { }
        'ARM64' { Write-Host 'ARM64 Windows: installing the x86_64 build, which runs under emulation.' }
        default { throw "no prebuilt binary for $arch Windows; build from source: https://github.com/$repo#install" }
    }
    $asset = 'ferrule-x86_64-pc-windows-msvc.zip'

    $tmp = Join-Path ([IO.Path]::GetTempPath()) ("ferrule-install-" + [Guid]::NewGuid())
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        Write-Host "Downloading ferrule ($version, x86_64-pc-windows-msvc)..."
        if ($env:GITHUB_TOKEN) {
            # A private repository: assets come through the API. Both
            # PowerShells drop the token on the redirect to the storage host.
            $auth = @{ Authorization = "Bearer $env:GITHUB_TOKEN" }
            $release = if ($version -eq 'latest') { 'latest' } else { "tags/$version" }
            $assets = (Invoke-RestMethod -UseBasicParsing -Headers $auth "https://api.github.com/repos/$repo/releases/$release").assets
            foreach ($name in $asset, "$asset.sha256") {
                $found = $assets | Where-Object { $_.name -eq $name } | Select-Object -First 1
                if (-not $found) { throw "no release asset $name ($version)" }
                $headers = $auth + @{ Accept = 'application/octet-stream' }
                Invoke-WebRequest -UseBasicParsing -Headers $headers -OutFile (Join-Path $tmp $name) $found.url
            }
        } else {
            $base = if ($version -eq 'latest') { "https://github.com/$repo/releases/latest/download" } else { "https://github.com/$repo/releases/download/$version" }
            foreach ($name in $asset, "$asset.sha256") {
                Invoke-WebRequest -UseBasicParsing -OutFile (Join-Path $tmp $name) "$base/$name"
            }
        }

        $zip = Join-Path $tmp $asset
        $want = ((Get-Content (Join-Path $tmp "$asset.sha256") -Raw).Trim() -split '\s+')[0].ToLower()
        $got = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
        if ($want -ne $got) { throw "checksum mismatch for $asset (expected $want, got $got)" }

        Expand-Archive -Path $zip -DestinationPath (Join-Path $tmp 'x') -Force
        New-Item -ItemType Directory -Path $dir -Force | Out-Null
        $exe = Join-Path $dir 'ferrule.exe'
        # A running ferrule.exe can't be overwritten, but it can be renamed;
        # a gateway left open keeps the old file until it restarts, and a
        # later install deletes it.
        Get-ChildItem $dir -Filter 'ferrule.exe.*.old' | Remove-Item -Force -ErrorAction SilentlyContinue
        $running = $false
        if (Test-Path $exe) {
            $running = [bool](Get-Process ferrule -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe })
            Move-Item $exe (Join-Path $dir ("ferrule.exe." + [Guid]::NewGuid().ToString('N').Substring(0, 8) + ".old"))
        }
        Copy-Item (Join-Path $tmp 'x\ferrule.exe') $exe
        Write-Host "Installed $(& $exe --version) -> $exe"
        if ($running) { Write-Host 'A ferrule gateway is still running the old version; restart it to upgrade it too.' }
    } finally {
        Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }

    # PATH: the raw registry value, so entries like %USERPROFILE%\bin stay
    # unexpanded, written back as an expandable string.
    $key = Get-Item 'HKCU:\Environment'
    $userPath = $key.GetValue('Path', '', 'DoNotExpandEnvironmentNames')
    $entries = @($userPath -split ';' | Where-Object { $_ })
    if ($entries -notcontains $dir) {
        Set-ItemProperty -Path 'HKCU:\Environment' -Name Path -Value (($entries + $dir) -join ';') -Type ExpandString
        # Setting any user variable through .NET broadcasts the change, so
        # terminals opened from now on see the new PATH.
        [Environment]::SetEnvironmentVariable('FERRULE_PATH_REFRESH', '1', 'User')
        [Environment]::SetEnvironmentVariable('FERRULE_PATH_REFRESH', $null, 'User')
        Write-Host "Added $dir to your PATH."
    }
    if (($env:Path -split ';') -notcontains $dir) { $env:Path = "$dir;$env:Path" }

    if ((& $exe config path) -match '^config\s+none yet') {
        if ($env:FERRULE_NO_SETUP -eq '1') {
            Write-Host 'Next: `ferrule setup` walks you through the settings.'
        } else {
            Write-Host ''
            & $exe setup
        }
    } else {
        Write-Host 'Your settings are kept. `ferrule setup` changes them, `ferrule doctor` checks them.'
    }
}

try {
    Install-Ferrule
} catch {
    Write-Host "ferrule install: $($_.Exception.Message)" -ForegroundColor Red
}
