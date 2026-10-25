# Builds the GameFrame installer: third-party notices (cargo about) + release exe -> Inno Setup -> target\installer\GameFrame-Setup-<version>.exe and GameFrame-Third-Party-Notices.txt
#
# Requires cargo-about (version pinned in .github/workflows/build.yml; locally:
# cargo install cargo-about --locked --version 0.9.2) and Inno Setup 7
# (winget install JRSoftware.InnoSetup.7). Only Inno Setup 7 install dirs are
# searched -- it installs side by side with 6, and an ISCC.exe on PATH could be
# either.

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot

$iscc = @(
    "$env:LOCALAPPDATA\Programs\Inno Setup 7\ISCC.exe"
    "$env:ProgramFiles\Inno Setup 7\ISCC.exe"
    "${env:ProgramFiles(x86)}\Inno Setup 7\ISCC.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) {
    throw 'ISCC.exe not found -- install Inno Setup 7: winget install JRSoftware.InnoSetup.7'
}

cargo about --version | Out-Null
if ($LASTEXITCODE -ne 0) {
    throw 'cargo-about not found -- install it: cargo install cargo-about --locked --version 0.9.2'
}

Push-Location $repo
try {
    cargo build --release --locked
    if ($LASTEXITCODE -ne 0) { throw 'cargo build --release --locked failed' }

    # Third-party notices (crate licenses + egui's fonts), installed next to the exe.
    # --threshold 0.9 matches deny.toml's confidence-threshold (cargo-about defaults to 0.8).
    # --fail: a license that can't be resolved or isn't in about.toml's accepted list fails the build.
    $notices = Join-Path $repo 'target\installer\GameFrame-Third-Party-Notices.txt'
    New-Item -ItemType Directory -Force (Split-Path $notices) | Out-Null
    Remove-Item $notices -ErrorAction SilentlyContinue
    cargo about generate --locked --fail --threshold 0.9 -o $notices (Join-Path $PSScriptRoot 'third-party-notices.hbs')
    if ($LASTEXITCODE -ne 0) { throw 'cargo about generate failed' }
    if (-not (Test-Path $notices) -or (Get-Item $notices).Length -eq 0) { throw 'cargo about produced no notices file' }

    $meta = cargo metadata --format-version 1 --no-deps
    if ($LASTEXITCODE -ne 0) { throw 'cargo metadata failed' }
    $version = ($meta | ConvertFrom-Json).packages |
        Where-Object name -eq 'gameframe' | Select-Object -ExpandProperty version

    & $iscc "/DAppVersion=$version" (Join-Path $PSScriptRoot 'gameframe.iss')
    if ($LASTEXITCODE -ne 0) { throw 'ISCC failed' }

    Write-Host "Installer: $(Join-Path $repo "target\installer\GameFrame-Setup-$version.exe")"
}
finally {
    Pop-Location
}
