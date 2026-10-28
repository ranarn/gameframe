# Builds the GameFrame installer: third-party notices (cargo about) + release exe -> Inno Setup -> target\installer\GameFrame-Setup-<version>.exe and GameFrame-Third-Party-Notices.txt
#
# -NoticesOnly: only generate the notices file (no Inno Setup, no release build); the PR job uses this.
# -NoticesPath: with -NoticesOnly, where to write the notices file (relative paths resolve against the caller's
# current directory; default target\installer\GameFrame-Third-Party-Notices.txt, which gameframe.iss packages).
# This script is the single place for the `cargo about generate` invocation, the cargo-about version
# check and the about.toml / deny.toml license-list parity check.
#
# Requires cargo-about at the version pinned as CARGO_ABOUT_VERSION in
# .github/actions/install-cargo-about/action.yml (the failure message below gives the install
# command for it) and, unless -NoticesOnly, Inno Setup 7
# (winget install JRSoftware.InnoSetup.7). Only Inno Setup 7 install dirs are
# searched -- it installs side by side with 6, and an ISCC.exe on PATH could be
# either.

param(
    [switch]$NoticesOnly,
    [string]$NoticesPath
)

$ErrorActionPreference = 'Stop'
if ($NoticesPath -and -not $NoticesOnly) { throw '-NoticesPath requires -NoticesOnly (the installer always packages the default notices file)' }
if ($NoticesPath) { $NoticesPath = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($NoticesPath) }
$repo = Split-Path -Parent $PSScriptRoot

# Quoted strings inside the first `[ ... ]` array assigned to `allow`/`accepted` in a TOML snippet.
function Get-TomlLicenseList([string]$Text, [string]$Key) {
    $noComments = ($Text -split "`r?`n" | ForEach-Object { $_ -replace '#.*$', '' }) -join "`n"
    if ($noComments -notmatch "(?ms)^\s*$Key\s*=\s*\[(.*?)\]") { throw "no '$Key' list found" }
    [regex]::Matches($Matches[1], '"([^"]*)"') | ForEach-Object { $_.Groups[1].Value.Trim() }
}

function Assert-LicenseListsMatch([string]$AboutToml, [string]$DenyToml) {
    $accepted = @(Get-TomlLicenseList (Get-Content $AboutToml -Raw) 'accepted')
    $denyText = Get-Content $DenyToml -Raw
    if ($denyText -notmatch '(?ms)^\[licenses\]\s*$(.*?)(?=^\[|\z)') { throw "no [licenses] section in $DenyToml" }
    $allow = @(Get-TomlLicenseList $Matches[1] 'allow')
    $onlyAccepted = @($accepted | Where-Object { $_ -cnotin $allow } | Sort-Object -Unique)
    $onlyAllow = @($allow | Where-Object { $_ -cnotin $accepted } | Sort-Object -Unique)
    if ($onlyAccepted -or $onlyAllow) {
        $parts = @()
        if ($onlyAccepted) { $parts += "Only in about.toml: $($onlyAccepted -join ', ')." }
        if ($onlyAllow) { $parts += "Only in deny.toml: $($onlyAllow -join ', ')." }
        throw ("License lists differ between about.toml 'accepted' and deny.toml [licenses].allow. " + ($parts -join ' '))
    }
}

function Get-PinnedCargoAboutVersion([string]$ActionFile) {
    if ((Get-Content $ActionFile -Raw) -notmatch '(?m)^\s*CARGO_ABOUT_VERSION:\s*[\x22\x27]?([^\x22\x27\s]+)[\x22\x27]?\s*$') {
        throw "CARGO_ABOUT_VERSION not found in $ActionFile"
    }
    $Matches[1]
}

Assert-LicenseListsMatch (Join-Path $repo 'about.toml') (Join-Path $repo 'deny.toml')

$pinned = Get-PinnedCargoAboutVersion (Join-Path $repo '.github\actions\install-cargo-about\action.yml')
$installHint = "install it: cargo install cargo-about --locked --features cli --version $pinned"
$prevEap = $ErrorActionPreference
$ErrorActionPreference = 'Continue'
$aboutVersion = cargo about --version 2>$null
$aboutExit = $LASTEXITCODE
$ErrorActionPreference = $prevEap
if ($aboutExit -ne 0) { throw "cargo-about not found -- $installHint" }
if (($aboutVersion | Out-String).Trim() -notmatch "(^|\s)$([regex]::Escape($pinned))$") {
    throw "cargo-about version mismatch: found '$(($aboutVersion | Out-String).Trim())', pinned $pinned -- $installHint"
}

if (-not $NoticesOnly) {
    $iscc = @(
        "$env:LOCALAPPDATA\Programs\Inno Setup 7\ISCC.exe"
        "$env:ProgramFiles\Inno Setup 7\ISCC.exe"
        "${env:ProgramFiles(x86)}\Inno Setup 7\ISCC.exe"
    ) | Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $iscc) {
        throw 'ISCC.exe not found -- install Inno Setup 7: winget install JRSoftware.InnoSetup.7'
    }
}

Push-Location $repo
try {
    if (-not $NoticesOnly) {
        cargo build --release --locked
        if ($LASTEXITCODE -ne 0) { throw 'cargo build --release --locked failed' }
    }

    # Third-party notices (crate licenses + egui's fonts), installed next to the exe.
    # --threshold 0.9 matches deny.toml's confidence-threshold (cargo-about defaults to 0.8).
    # --fail: a license that can't be resolved or isn't in about.toml's accepted list fails the build.
    $notices = if ($NoticesPath) { $NoticesPath } else { Join-Path $repo 'target\installer\GameFrame-Third-Party-Notices.txt' }
    New-Item -ItemType Directory -Force (Split-Path $notices) | Out-Null
    Remove-Item $notices -ErrorAction SilentlyContinue
    cargo about generate --locked --fail --threshold 0.9 -o $notices (Join-Path $PSScriptRoot 'third-party-notices.hbs')
    if ($LASTEXITCODE -ne 0) { throw 'cargo about generate failed' }
    if (-not (Test-Path $notices) -or (Get-Item $notices).Length -eq 0) { throw 'cargo about produced no notices file' }

    if ($NoticesOnly) { Write-Host "Notices: $notices"; return }

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
