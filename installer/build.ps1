# Builds the GameFrame installer: release exe -> Inno Setup -> target\installer\GameFrame-Setup-<version>.exe
#
# Requires Inno Setup 6 (winget install JRSoftware.InnoSetup).

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot

$iscc = @(
    (Get-Command ISCC.exe -ErrorAction SilentlyContinue)?.Source
    "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe"
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe"
    "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
) | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
if (-not $iscc) {
    throw 'ISCC.exe not found -- install Inno Setup 6: winget install JRSoftware.InnoSetup'
}

Push-Location $repo
try {
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw 'cargo build --release failed' }

    $version = (cargo metadata --format-version 1 --no-deps | ConvertFrom-Json).packages |
        Where-Object name -eq 'gameframe' | Select-Object -ExpandProperty version

    & $iscc "/DAppVersion=$version" (Join-Path $PSScriptRoot 'gameframe.iss')
    if ($LASTEXITCODE -ne 0) { throw 'ISCC failed' }

    Write-Host "Installer: $(Join-Path $repo "target\installer\GameFrame-Setup-$version.exe")"
}
finally {
    Pop-Location
}
