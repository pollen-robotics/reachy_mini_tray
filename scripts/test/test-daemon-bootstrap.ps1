# End-to-end smoke test (Windows port of test-daemon-bootstrap.sh).
#
# Validates that uv-trampoline can bootstrap a Python venv from scratch,
# install reachy-mini, and bring up the daemon until GET /api/daemon/status
# returns 200 OK. Tray app has no main window, so this is the equivalent
# of reachy_mini_desktop_app's GUI E2E tests, focused on the only thing
# that can actually fail headless: the bootstrap pipeline.
#
# Usage:
#   pwsh -File .\scripts\test\test-daemon-bootstrap.ps1
#   $env:REACHY_MINI_SOURCE = 'develop'; pwsh -File .\scripts\test\test-daemon-bootstrap.ps1
#   $env:SKIP_BUILD = '1'; pwsh -File .\scripts\test\test-daemon-bootstrap.ps1
#
# Env knobs:
#   SKIP_BUILD=1         - reuse src-tauri\binaries\uv-trampoline-*.exe instead of rebuilding
#   KEEP_DATA_DIR=1      - leave the temp data dir on disk after the test (debugging)
#   READY_TIMEOUT_SECS=N - max seconds to wait for the daemon (default: 600)
#   REACHY_MINI_SOURCE=  - branch/version baked into the trampoline (forwarded to build-sidecar.ps1)
#   UV_WRAPPER_DIR=      - path to the desktop app's uv-wrapper crate (forwarded to build-sidecar.ps1)

$ErrorActionPreference = 'Stop'

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Definition
$TrayRoot = (Resolve-Path (Join-Path $ScriptDir '..\..')).Path
Set-Location $TrayRoot

function Step($msg)  { Write-Host "`n== $msg ==" -ForegroundColor Cyan }
function Ok($msg)    { Write-Host "OK $msg"     -ForegroundColor Green }
function WarnMsg($m) { Write-Host "!  $m"       -ForegroundColor Yellow }
function FailMsg($m) { Write-Host "X  $m"       -ForegroundColor Red }

# ---------------------------------------------------------------------------
# 1. Build the trampoline (unless SKIP_BUILD is set)
# ---------------------------------------------------------------------------

if (-not $env:SKIP_BUILD) {
    Step 'Building uv-trampoline sidecar'
    pwsh -File (Join-Path $TrayRoot 'scripts\build-sidecar.ps1')
    if ($LASTEXITCODE -ne 0) { throw 'sidecar build failed' }
} else {
    WarnMsg 'SKIP_BUILD=1, reusing existing src-tauri\binaries\uv-trampoline-*.exe'
}

$Trampoline = Get-ChildItem -Path (Join-Path $TrayRoot 'src-tauri\binaries') `
                            -Filter 'uv-trampoline-*.exe' `
                            -ErrorAction SilentlyContinue |
              Select-Object -First 1
if (-not $Trampoline) {
    FailMsg 'uv-trampoline binary not found in src-tauri\binaries\'
    exit 1
}
Ok "Found trampoline: $($Trampoline.Name)"

# ---------------------------------------------------------------------------
# 2. Set up an isolated data dir
# ---------------------------------------------------------------------------

Step 'Preparing isolated data directory'

# uv_wrapper resolves the Windows data dir from $env:LOCALAPPDATA + 'Reachy Mini Control'.
# Override LOCALAPPDATA to point inside a fresh temp folder so the test
# always starts from a clean bootstrap and never touches the real user state.
$TestRoot = Join-Path $env:TEMP ("reachy-mini-tray-bootstrap-{0}" -f (Get-Random))
New-Item -ItemType Directory -Path $TestRoot -Force | Out-Null
$LocalAppData = Join-Path $TestRoot 'LocalAppData'
New-Item -ItemType Directory -Path $LocalAppData -Force | Out-Null
$env:LOCALAPPDATA = $LocalAppData

$DataDir = Join-Path $LocalAppData 'Reachy Mini Control'
New-Item -ItemType Directory -Path $DataDir -Force | Out-Null

$LogFile = Join-Path $TestRoot 'daemon.log'
Ok "Data dir: $DataDir"
Ok "Log file: $LogFile"

# ---------------------------------------------------------------------------
# 3. Spawn the trampoline
# ---------------------------------------------------------------------------

Step 'Launching daemon in mockup-sim, no-media mode'

$DaemonArgs = @(
    '.venv\Scripts\python.exe'
    '-m'; 'reachy_mini.daemon.app.main'
    '--desktop-app-daemon'
    '--mockup-sim'
    '--no-wake-up-on-start'
    '--no-media'
    '--localhost-only'
    '--robot-name'; 'reachy_mini_tray_smoke'
    '--log-level'; 'INFO'
)

$proc = Start-Process -FilePath $Trampoline.FullName `
                      -ArgumentList $DaemonArgs `
                      -RedirectStandardOutput $LogFile `
                      -RedirectStandardError "$LogFile.err" `
                      -PassThru -NoNewWindow

# Cleanup: register a script block that runs on script exit (including Ctrl-C).
$cleanup = {
    param($trampolineProc, $rootDir, $logFile, $keepDataDir)
    if ($trampolineProc -and -not $trampolineProc.HasExited) {
        # taskkill /T also brings down the whole tree (Python child, etc.)
        try { taskkill /F /T /PID $trampolineProc.Id 2>$null | Out-Null } catch {}
    }
    if (-not $keepDataDir) {
        Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $rootDir
    } else {
        Write-Host "!  KEEP_DATA_DIR=1, leaving $rootDir on disk" -ForegroundColor Yellow
    }
}

Ok "Trampoline spawned (pid=$($proc.Id))"

try {
    # ---------------------------------------------------------------------------
    # 4. Poll the daemon's HTTP status endpoint
    # ---------------------------------------------------------------------------

    Step 'Waiting for /api/daemon/status to return 200'

    $timeout = if ($env:READY_TIMEOUT_SECS) { [int]$env:READY_TIMEOUT_SECS } else { 600 }
    $deadline = (Get-Date).AddSeconds($timeout)
    $ready = $false
    $lastPhase = ''

    while ((Get-Date) -lt $deadline) {
        if ($proc.HasExited) {
            FailMsg 'Daemon process exited before becoming ready (see log below)'
            throw 'daemon exited'
        }

        try {
            $resp = Invoke-WebRequest -UseBasicParsing -TimeoutSec 3 `
                                      -Uri 'http://127.0.0.1:8000/api/daemon/status'
            if ($resp.StatusCode -eq 200) {
                Write-Host ''
                Ok "Daemon is up: $($resp.Content)"
                $ready = $true
                break
            }
        } catch {
            # Not ready yet, surface the bootstrap phase from the log.
            if (Test-Path $LogFile) {
                $tail = Get-Content -Tail 50 $LogFile -ErrorAction SilentlyContinue
                $phase = $tail | Select-String -Pattern '(downloading uv|installing python|creating venv|installing reachy-mini|still working|Application startup complete)' `
                                | Select-Object -Last 1
                if ($phase -and $phase.Matches[0].Value -ne $lastPhase) {
                    Write-Host "`n  current phase: $($phase.Matches[0].Value)" -NoNewline
                    $lastPhase = $phase.Matches[0].Value
                }
            }
            Write-Host '.' -NoNewline
            Start-Sleep -Seconds 2
        }
    }

    if (-not $ready) {
        FailMsg "Daemon did not become ready within ${timeout}s"
        throw 'timeout'
    }

    # ---------------------------------------------------------------------------
    # 5. Sanity-check the bootstrap artifacts
    # ---------------------------------------------------------------------------

    Step 'Verifying installation artifacts'

    $venvPython = Join-Path $DataDir '.venv\Scripts\python.exe'
    if (Test-Path $venvPython) {
        Ok '.venv\Scripts\python.exe exists'
    } else {
        FailMsg "expected .venv\Scripts\python.exe inside $DataDir"
        throw 'missing venv'
    }

    $uvExe = Join-Path $DataDir 'uv.exe'
    if (Test-Path $uvExe) {
        $pipList = & $uvExe pip list --python $venvPython 2>$null
        $reachyLine = $pipList | Where-Object { $_ -match '^reachy-mini\s' } | Select-Object -First 1
        if ($reachyLine) {
            $version = ($reachyLine -split '\s+')[1]
            Ok "reachy-mini installed: $version"
        } else {
            WarnMsg 'could not find reachy-mini in pip list (non-fatal)'
        }
    }

    Step 'Smoke test passed'
} finally {
    # Dump the logs BEFORE cleanup removes the temp dir: $LogFile (and its
    # .err sibling) live inside $TestRoot, so removing it first left
    # Get-Content with nothing to read - which is why CI failures here were
    # undiagnosable. stderr is redirected to "$LogFile.err", so surface both.
    if (-not $ready) {
        FailMsg 'Last 60 lines of daemon stdout:'
        Get-Content -Tail 60 $LogFile -ErrorAction SilentlyContinue | ForEach-Object { Write-Host "    $_" }
        if (Test-Path "$LogFile.err") {
            FailMsg 'Last 60 lines of daemon stderr:'
            Get-Content -Tail 60 "$LogFile.err" -ErrorAction SilentlyContinue | ForEach-Object { Write-Host "    $_" }
        }
    }
    & $cleanup $proc $TestRoot $LogFile $env:KEEP_DATA_DIR
}

if (-not $ready) { exit 1 }
exit 0
