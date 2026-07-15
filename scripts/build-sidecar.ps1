# Build the uv-trampoline sidecar from the existing desktop app crate
# (Windows port of build-sidecar.sh). Mirrors the same conventions:
#
#   - Source crate lives in $env:UV_WRAPPER_DIR if set, otherwise falls back
#     to ..\reachy_mini_desktop_app\uv-wrapper (sibling of reachy_mini_tray).
#   - Destination is src-tauri\binaries\uv-trampoline-<triplet>.exe.
#   - Selecting which `reachy-mini` Python package to bake in:
#       default                 -> pinned version from ..\daemon-version.txt
#       $env:REACHY_MINI_SOURCE -> git branch on pollen-robotics/reachy_mini
#       $env:REACHY_MINI_VERSION-> pin a specific PyPI version
#       First positional arg    -> shortcut for REACHY_MINI_SOURCE.
#   - Triplet detection respects $env:TARGET_TRIPLET, otherwise rustc -Vv.
#
# Usage:
#   pwsh -File .\scripts\build-sidecar.ps1
#   pwsh -File .\scripts\build-sidecar.ps1 integration\mobile-app-daemon
#   $env:REACHY_MINI_VERSION = '1.6.4'; pwsh -File .\scripts\build-sidecar.ps1

param(
    [Parameter(Position = 0)]
    [string]$BranchOrPypi
)

$ErrorActionPreference = 'Stop'

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Definition
$TrayRoot = (Resolve-Path (Join-Path $ScriptDir '..')).Path
$ProjectRoot = (Resolve-Path (Join-Path $TrayRoot '..')).Path

if ($env:UV_WRAPPER_DIR) {
    $SrcCrate = $env:UV_WRAPPER_DIR
} else {
    $SrcCrate = Join-Path $ProjectRoot 'reachy_mini_desktop_app\uv-wrapper'
}

$DstDir = Join-Path $TrayRoot 'src-tauri\binaries'
$SpecMarker = Join-Path $DstDir '.reachy_mini_spec'

# Committed default pin: single source of truth for the shipped reachy-mini
# version. Read only when no explicit env / branch override is given.
$PinFile = Join-Path $TrayRoot 'daemon-version.txt'

# Return the first bare (non-comment, non-blank) line of the pin file, or ''.
function Resolve-Pin($path) {
    if (-not (Test-Path $path)) { return '' }
    foreach ($line in Get-Content $path) {
        $t = $line.Trim()
        if ($t -and -not $t.StartsWith('#')) { return $t }
    }
    return ''
}

if (-not (Test-Path $SrcCrate)) {
    Write-Error "Cannot find uv-wrapper crate at $SrcCrate. Either set `$env:UV_WRAPPER_DIR or check out reachy_mini_desktop_app as a sibling of reachy_mini_tray."
    exit 1
}

# ---------------------------------------------------------------------------
# Resolve which reachy-mini to install at first-run bootstrap time.
# Mirrors get_reachy_mini_spec() in uv-wrapper/src/lib.rs.
# ---------------------------------------------------------------------------

if ($BranchOrPypi -and -not $env:REACHY_MINI_SOURCE) {
    $env:REACHY_MINI_SOURCE = $BranchOrPypi
}

# Resolution precedence (first match wins):
#   1. $env:REACHY_MINI_VERSION         -> reachy-mini==X.Y.Z
#   2. $env:REACHY_MINI_SOURCE (!=pypi) -> git+...@branch
#   3. committed daemon-version.txt     -> reachy-mini==<pin>
#   4. fallback                         -> reachy-mini (latest from PyPI)
# The pin path (3) sets $env:REACHY_MINI_VERSION so `option_env!` in the
# trampoline bakes the exact version in at compile time (build.rs reruns when
# this var changes).
if ($env:REACHY_MINI_VERSION) {
    $Spec = "reachy-mini==$($env:REACHY_MINI_VERSION)"
} elseif ($env:REACHY_MINI_SOURCE -and $env:REACHY_MINI_SOURCE -ne 'pypi') {
    $Spec = "git+https://github.com/pollen-robotics/reachy_mini.git@$($env:REACHY_MINI_SOURCE)"
} else {
    $Pin = Resolve-Pin $PinFile
    if ($Pin) {
        $env:REACHY_MINI_VERSION = $Pin
        $Spec = "reachy-mini==$Pin"
    } else {
        $Spec = 'reachy-mini (latest from PyPI)'
    }
}

Write-Host "📦 Bake-in spec: $Spec"

# ---------------------------------------------------------------------------
# Target triplet detection
# ---------------------------------------------------------------------------

if ($env:TARGET_TRIPLET) {
    $Triplet = $env:TARGET_TRIPLET
    Write-Host "Using TARGET_TRIPLET from environment: $Triplet"
} else {
    $Triplet = (rustc -Vv | Select-String '^host:' | ForEach-Object { ($_ -split '\s+')[1] })
    Write-Host "Detected host triplet: $Triplet"
}

if (-not (Test-Path $DstDir)) {
    New-Item -ItemType Directory -Path $DstDir -Force | Out-Null
}

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------

Write-Host "🔨 Building uv-trampoline (release) from $SrcCrate..."
Push-Location $SrcCrate
try {
    if ($env:TARGET_TRIPLET) {
        cargo build --release --bin uv-trampoline --target $Triplet
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
        $SrcExe = Join-Path "target\$Triplet\release" 'uv-trampoline.exe'
    } else {
        cargo build --release --bin uv-trampoline
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
        $SrcExe = 'target\release\uv-trampoline.exe'
    }
    $DstExe = Join-Path $DstDir "uv-trampoline-$Triplet.exe"
    Copy-Item $SrcExe $DstExe -Force
} finally {
    Pop-Location
}

Set-Content -Path $SpecMarker -Value $Spec -Encoding ASCII

Write-Host "✅ Sidecar ready: $DstExe"
Write-Host "   Spec:        $Spec"
Write-Host ''
Write-Host 'ℹ️  If a venv already exists in the user data dir, the trampoline'
Write-Host '   will detect the spec change on next launch and upgrade in-place.'
Write-Host '   Use ''Reset setup…'' from the tray menu to force a clean reinstall.'
