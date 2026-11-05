<#
.SYNOPSIS
  Builds, packages, signs and (after confirmation) publishes a Cerberus Usage release.

.DESCRIPTION
  Runs on the maintainer's PC. Steps: clean tree, version check, tests, release build, installer
  (Inno Setup), signature (cargo run --bin sign-release), summary, confirmation, gh release create.
  It never pushes commits and never prints secrets. The signing key stays in the Windows
  credential store (service cerberus-usage-release).

.PARAMETER DryRun
  Runs everything up to and including the installer and prints the summary, but does not sign
  and does not upload.

.PARAMETER Repo
  GitHub repository for the release lookup and upload.
#>
param(
    [switch]$DryRun,
    [string]$Repo = 'Cerberus-Lab-de/cerberus-usage'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$root = $PSScriptRoot
Set-Location $root

function Step([string]$text) { Write-Host "`n== $text" -ForegroundColor Cyan }

# Runs a native command and stops when it fails.
function Invoke-Native([string]$exe, [string[]]$arguments) {
    & $exe @arguments
    if ($LASTEXITCODE -ne 0) {
        throw "Befehl fehlgeschlagen (Exit $LASTEXITCODE): $(Split-Path -Leaf $exe) $($arguments -join ' ')"
    }
}

function Find-Tool([string]$name, [string[]]$candidates) {
    foreach ($c in $candidates) { if ($c -and (Test-Path -LiteralPath $c)) { return $c } }
    $cmd = Get-Command $name -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    return $null
}

# Latest release version, or $null when there is none (HTTP 404). Any other failure (auth,
# network, rate limit) throws. Native stderr is redirected to a file with the error preference
# relaxed, because Windows PowerShell 5.1 turns merged stderr into a terminating error.
function Get-LatestReleaseVersion([string]$ghExe, [string]$repo) {
    $errFile = [System.IO.Path]::GetTempFileName()
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $out = & $ghExe api "repos/$repo/releases/latest" --jq .tag_name 2>$errFile
        $code = $LASTEXITCODE
        $err = (Get-Content -LiteralPath $errFile -Raw -ErrorAction SilentlyContinue)
    } finally {
        $ErrorActionPreference = $previous
        Remove-Item -LiteralPath $errFile -Force -ErrorAction SilentlyContinue
    }
    if ($code -eq 0) {
        $tag = ("$out").Trim() -replace '^v', ''
        if ($tag -notmatch '^\d+\.\d+\.\d+$') { throw "Unerwarteter Release-Tag '$tag'." }
        return [version]$tag
    }
    if ("$err" -match 'HTTP 404') { return $null }
    throw "Release-Abfrage fehlgeschlagen: $("$err".Trim())"
}

$cargo = Find-Tool 'cargo' @("$env:USERPROFILE\.cargo\bin\cargo.exe")
if (-not $cargo) { throw 'cargo nicht gefunden.' }
$iscc = Find-Tool 'ISCC' @(
    "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe",
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
    "$env:ProgramFiles\Inno Setup 6\ISCC.exe")
if (-not $iscc) { throw 'Inno Setup (ISCC.exe) nicht gefunden. Installation: winget install JRSoftware.InnoSetup' }
$git = (Get-Command git -ErrorAction Stop).Source
$gh = Get-Command gh -ErrorAction SilentlyContinue

# --- 1. Clean tree --------------------------------------------------------------------------
Step 'Arbeitsverzeichnis prüfen'
$dirty = & $git status --porcelain
if ($LASTEXITCODE -ne 0) { throw 'git status fehlgeschlagen.' }
if ($dirty) { throw "Das Arbeitsverzeichnis ist nicht sauber. Erst committen:`n$($dirty -join "`n")" }

# --- 2. Version ------------------------------------------------------------------------------
Step 'Version prüfen'
$cargoToml = Get-Content -LiteralPath (Join-Path $root 'Cargo.toml') -Raw -Encoding UTF8
$m = [regex]::Match($cargoToml, '(?ms)^\[package\].*?^version\s*=\s*"(?<v>[^"]+)"')
if (-not $m.Success) { throw 'Keine Version in Cargo.toml gefunden.' }
$version = $m.Groups['v'].Value
if ($version -notmatch '^\d+\.\d+\.\d+$') { throw "Version '$version' ist nicht von der Form x.y.z." }
$newVersion = [version]$version

if (-not $gh) {
    if ($DryRun) { Write-Warning 'gh nicht gefunden: Versionsvergleich mit dem letzten Release übersprungen (Probelauf).' }
    else { throw 'gh (GitHub CLI) nicht gefunden.' }
} else {
    $latest = Get-LatestReleaseVersion $gh.Source $Repo
    if ($null -eq $latest) {
        Write-Host "Noch kein Release vorhanden: $version wäre das erste."
    } else {
        if ($newVersion -le $latest) {
            throw "Version $version ist nicht neuer als das letzte Release ($latest). Version in Cargo.toml erhöhen."
        }
        Write-Host "Letztes Release: $latest, neu: $version"
    }
}

# --- 3. Release notes --------------------------------------------------------------------------
Step 'Änderungsnotizen lesen'
$changelog = Get-Content -LiteralPath (Join-Path $root 'CHANGELOG.md') -Raw -Encoding UTF8
$pattern = '(?ms)^##\s+\[?' + [regex]::Escape($version) + '(?![\d.])\]?[^\r\n]*\r?\n(?<body>.*?)(?=^##\s|\z)'
$nm = [regex]::Match($changelog, $pattern)
if (-not $nm.Success -or [string]::IsNullOrWhiteSpace($nm.Groups['body'].Value)) {
    throw "In CHANGELOG.md fehlt ein Abschnitt für Version $version."
}
$notes = $nm.Groups['body'].Value.Trim()

# --- 4. Tests and build -----------------------------------------------------------------------
Step 'Tests'
Invoke-Native $cargo @('test', '--locked')
Step 'Release-Build'
Invoke-Native $cargo @('build', '--release', '--locked')
$exe = Join-Path $root 'target\release\cerberus-usage.exe'
if (-not (Test-Path -LiteralPath $exe)) { throw "Build-Ergebnis fehlt: $exe" }

# --- 5. Installer --------------------------------------------------------------------------------
Step 'Installer bauen'
# Inno Setup reads a BOM-less script as ANSI and garbles umlauts in dialogs.
$issPath = Join-Path $root 'installer\cerberus-usage.iss'
$head3 = [System.IO.File]::ReadAllBytes($issPath)[0..2]
if ($head3[0] -ne 0xEF -or $head3[1] -ne 0xBB -or $head3[2] -ne 0xBF) {
    throw 'installer\cerberus-usage.iss ist nicht als UTF-8 mit BOM gespeichert (Umlaute würden falsch angezeigt).'
}
$dist = Join-Path $root 'dist'
New-Item -ItemType Directory -Force -Path $dist | Out-Null
$installer = Join-Path $dist "CerberusUsage-Setup-$version.exe"
$manifest = "$installer.manifest.json"
if (Test-Path -LiteralPath $installer) { Remove-Item -LiteralPath $installer -Force }
if (Test-Path -LiteralPath $manifest) { Remove-Item -LiteralPath $manifest -Force }
Invoke-Native $iscc @("/DAppVersion=$version", $issPath)
if (-not (Test-Path -LiteralPath $installer)) { throw "Installer fehlt: $installer" }

# --- 6. Sign ----------------------------------------------------------------------------------------
if (-not $DryRun) {
    Step 'Manifest signieren'
    Invoke-Native $cargo @('run', '--release', '--locked', '--bin', 'sign-release', '--', 'sign', $installer, $version)
    if (-not (Test-Path -LiteralPath $manifest)) { throw "Manifest fehlt: $manifest" }
}

# --- 7. Summary ----------------------------------------------------------------------------------------
Step 'Zusammenfassung'
$hash = (Get-FileHash -LiteralPath $installer -Algorithm SHA256).Hash.ToLower()
$size = (Get-Item -LiteralPath $installer).Length
Write-Host "Version:   $version"
Write-Host "Installer: $installer"
Write-Host "SHA-256:   $hash"
Write-Host ("Größe:     {0:N0} Bytes ({1:N2} MB)" -f $size, ($size / 1MB))
if ($DryRun) {
    Write-Host 'Manifest:  (Probelauf, nicht signiert)'
    Write-Host "`nProbelauf beendet. Nichts wurde signiert oder hochgeladen." -ForegroundColor Yellow
    return
}
Write-Host "Manifest:  $manifest"

# --- 8. Publish (after confirmation) ----------------------------------------------------------------
$head = (& $git rev-parse HEAD).Trim()
$remotes = & $git branch -r --contains $head
if (-not $remotes) {
    throw 'Der aktuelle Commit ist noch nicht gepusht. Das Skript pusht nie selbst: erst git push, dann erneut starten.'
}
$answer = Read-Host "Release v$version auf GitHub veröffentlichen? (j/n)"
if ($answer -ne 'j') {
    Write-Host 'Abgebrochen. Nichts wurde hochgeladen.'
    return
}
$notesFile = Join-Path $dist "release-notes-$version.md"
Set-Content -LiteralPath $notesFile -Value $notes -Encoding UTF8
Invoke-Native $gh.Source @('release', 'create', "v$version", $installer, $manifest,
    '--repo', $Repo, '--target', $head, '--title', "Cerberus Usage $version", '--notes-file', $notesFile)
Write-Host "Release v$version veröffentlicht." -ForegroundColor Green
