# Builds the GameFrame installer: release exe -> Inno Setup -> target\installer\GameFrame-Setup-<version>.exe
#
# Requires Inno Setup 7 (winget install JRSoftware.InnoSetup.7). Only Inno
# Setup 7 install dirs are searched -- it installs side by side with 6, and an
# ISCC.exe on PATH could be either.

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

Push-Location $repo
try {
    cargo build --release --locked
    if ($LASTEXITCODE -ne 0) { throw 'cargo build --release --locked failed' }

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
