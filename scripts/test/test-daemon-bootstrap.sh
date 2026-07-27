#!/usr/bin/env bash
#
# End-to-end smoke test: validates that `uv-trampoline` can bootstrap the
# Python venv from scratch, install reachy-mini, and bring up the daemon
# until `GET /api/daemon/status` returns 200 OK.
#
# This is the headless equivalent of reachy_mini_desktop_app's E2E
# WebDriver tests for the tray app. The tray app has no main window, so
# instead of automating UI clicks we exercise the lifecycle layer that
# matters most: "does the daemon install + start work on a clean machine?"
#
# Strategy:
#
#   - Build the trampoline (or reuse an existing one in src-tauri/binaries).
#   - Redirect the platform's data dir to a fresh temp folder so the test
#     never touches the real user state and always starts from a clean
#     bootstrap.
#   - Spawn the trampoline with --mockup-sim --no-media --no-wake-up-on-start
#     so it neither needs hardware nor GStreamer / portaudio at runtime.
#   - Poll http://127.0.0.1:8000/api/daemon/status until ready or timeout.
#   - Tear down (SIGTERM the trampoline + its process group, then SIGKILL).
#
# Usage:
#   bash ./scripts/test/test-daemon-bootstrap.sh
#   REACHY_MINI_SOURCE=develop bash ./scripts/test/test-daemon-bootstrap.sh
#   SKIP_BUILD=1 bash ./scripts/test/test-daemon-bootstrap.sh
#   KEEP_DATA_DIR=1 bash ./scripts/test/test-daemon-bootstrap.sh   # keep tmp dir for debugging
#
# Env knobs:
#   SKIP_BUILD=1         - don't rebuild the sidecar, just use what's there
#   KEEP_DATA_DIR=1      - leave the temp data dir on disk after the test
#   READY_TIMEOUT_SECS=N - max seconds to wait for the daemon (default: 600)
#   REACHY_MINI_SOURCE=  - branch/version to bake into the trampoline (forwarded to build-sidecar.sh)
#   UV_WRAPPER_DIR=      - path to the desktop app's uv-wrapper crate (forwarded to build-sidecar.sh)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TRAY_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$TRAY_ROOT"

# Colors (only when stdout is a TTY)
if [[ -t 1 ]]; then
    GREEN='\033[0;32m'; RED='\033[0;31m'; YELLOW='\033[1;33m'; BLUE='\033[0;34m'; NC='\033[0m'
else
    GREEN=''; RED=''; YELLOW=''; BLUE=''; NC=''
fi

step()  { printf "\n${BLUE}== %s ==${NC}\n" "$1"; }
ok()    { printf "${GREEN}✓ %s${NC}\n" "$1"; }
warn()  { printf "${YELLOW}⚠ %s${NC}\n" "$1"; }
fail()  { printf "${RED}✗ %s${NC}\n" "$1" >&2; }

# ---------------------------------------------------------------------------
# 1. Build the trampoline (unless SKIP_BUILD is set)
# ---------------------------------------------------------------------------

if [[ -z "${SKIP_BUILD:-}" ]]; then
    step "Building uv-trampoline sidecar"
    bash "$TRAY_ROOT/scripts/build-sidecar.sh"
else
    warn "SKIP_BUILD=1, reusing existing src-tauri/binaries/uv-trampoline-*"
fi

TRAMPOLINE=$(ls "$TRAY_ROOT"/src-tauri/binaries/uv-trampoline-* 2>/dev/null \
    | grep -v '\.spec' \
    | head -n 1 || true)
if [[ -z "$TRAMPOLINE" || ! -x "$TRAMPOLINE" ]]; then
    fail "uv-trampoline binary not found in src-tauri/binaries/"
    exit 1
fi
ok "Found trampoline: $(basename "$TRAMPOLINE")"

# ---------------------------------------------------------------------------
# 2. Set up an isolated data dir
# ---------------------------------------------------------------------------

step "Preparing isolated data directory"

UNAME_S=$(uname -s)
# `-t` is interpreted differently on GNU vs BSD mktemp. Pass an explicit
# template so the XXXXXX placeholder is always expanded portably.
TEST_HOME=$(mktemp -d "${TMPDIR:-/tmp}/reachy-mini-tray-bootstrap.XXXXXX")
LOG_FILE="$TEST_HOME/daemon.log"

case "$UNAME_S" in
    Darwin)
        # uv_wrapper resolves data dir from $HOME/Library/Application Support/...
        export HOME="$TEST_HOME"
        EXPECTED_DATA_DIR="$HOME/Library/Application Support/com.pollen-robotics.reachy-mini"
        ;;
    Linux)
        # uv_wrapper honors $XDG_DATA_HOME first, falling back to $HOME/.local/share/...
        export XDG_DATA_HOME="$TEST_HOME/share"
        # Keep HOME real so uv can still read ~/.cache/... if it wants; the
        # data dir override above is enough to isolate the venv.
        EXPECTED_DATA_DIR="$XDG_DATA_HOME/reachy-mini-control"
        ;;
    *)
        fail "Unsupported OS: $UNAME_S (use test-daemon-bootstrap.ps1 on Windows)"
        exit 2
        ;;
esac

mkdir -p "$EXPECTED_DATA_DIR"
ok "Data dir: $EXPECTED_DATA_DIR"
ok "Log file: $LOG_FILE"

# ---------------------------------------------------------------------------
# 3. Spawn the trampoline + cleanup trap
# ---------------------------------------------------------------------------

step "Launching daemon in mockup-sim, no-media mode"

# Mirrors `build_daemon_args(Mode::Simulation)` from src-tauri/src/lib.rs,
# plus `--no-media` (no camera / audio / WebRTC, hence no GStreamer or
# portaudio runtime dep) and a unique robot name to avoid colliding with
# any real Reachy Mini running on the same network during local testing.
DAEMON_ARGS=(
    .venv/bin/python3
    -m reachy_mini.daemon.app.main
    --desktop-app-daemon
    --mockup-sim
    --no-wake-up-on-start
    --no-media
    --robot-name reachy_mini_tray_smoke
    --log-level INFO
)

# Important: the trampoline `setpgid(0, 0)`s itself at startup so the
# Python child it execs joins its process group. Killing the whole group
# is the only reliable way to bring everything down, so we do that on
# trap with `kill -- -PGID`.
"$TRAMPOLINE" "${DAEMON_ARGS[@]}" > "$LOG_FILE" 2>&1 &
DAEMON_PID=$!

cleanup() {
    local exit_code=$?
    if kill -0 "$DAEMON_PID" 2>/dev/null; then
        # Resolve the process group id (== pid since the trampoline created its own group)
        local pgid
        pgid=$(ps -o pgid= -p "$DAEMON_PID" 2>/dev/null | tr -d ' ' || true)
        if [[ -n "$pgid" ]]; then
            kill -TERM "-$pgid" 2>/dev/null || true
            sleep 1
            kill -KILL "-$pgid" 2>/dev/null || true
        else
            kill -TERM "$DAEMON_PID" 2>/dev/null || true
            sleep 1
            kill -KILL "$DAEMON_PID" 2>/dev/null || true
        fi
    fi

    # Dump the log BEFORE removing the temp dir: LOG_FILE lives inside
    # TEST_HOME, so wiping it first (as we used to) left `tail` with nothing
    # to read - which is why every CI failure so far was undiagnosable.
    if [[ $exit_code -ne 0 ]]; then
        fail "Smoke test failed (exit=$exit_code). Last 60 lines of daemon log:"
        tail -n 60 "$LOG_FILE" 2>/dev/null | sed 's/^/    /' >&2 || true
    fi

    if [[ -z "${KEEP_DATA_DIR:-}" ]]; then
        rm -rf "$TEST_HOME"
    else
        warn "KEEP_DATA_DIR=1, leaving $TEST_HOME on disk"
    fi
    exit $exit_code
}
trap cleanup EXIT INT TERM

ok "Trampoline spawned (pid=$DAEMON_PID)"

# ---------------------------------------------------------------------------
# 4. Poll the daemon's HTTP status endpoint
# ---------------------------------------------------------------------------

step "Waiting for /api/daemon/status to return 200"

READY_TIMEOUT_SECS="${READY_TIMEOUT_SECS:-600}" # 10 min covers a cold uv+venv+pip install
DEADLINE=$(( $(date +%s) + READY_TIMEOUT_SECS ))
LAST_PHASE=""

while (( $(date +%s) < DEADLINE )); do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        fail "Daemon process exited before becoming ready (see log above)"
        exit 1
    fi

    if response=$(curl -fsS --max-time 3 http://127.0.0.1:8000/api/daemon/status 2>/dev/null); then
        printf "\n"
        ok "Daemon is up: $response"
        break
    fi

    # Surface the bootstrap phase so a 5-min wait doesn't look frozen.
    # We grep the trampoline log for the one-liner milestones it prints.
    if [[ -f "$LOG_FILE" ]]; then
        phase=$(tail -n 50 "$LOG_FILE" \
            | grep -oE '(downloading uv|installing python|creating venv|installing reachy-mini|still working|Application startup complete)' \
            | tail -n 1 || true)
        if [[ -n "$phase" && "$phase" != "$LAST_PHASE" ]]; then
            printf "\n  current phase: %s" "$phase"
            LAST_PHASE="$phase"
        fi
    fi

    printf "."
    sleep 2
done

if ! curl -fsS --max-time 3 http://127.0.0.1:8000/api/daemon/status > /dev/null 2>&1; then
    fail "Daemon did not become ready within ${READY_TIMEOUT_SECS}s"
    exit 1
fi

# ---------------------------------------------------------------------------
# 5. Sanity-check the response and the bootstrap artifacts
# ---------------------------------------------------------------------------

step "Verifying installation artifacts"

if [[ -x "$EXPECTED_DATA_DIR/.venv/bin/python3" ]]; then
    ok ".venv/bin/python3 exists"
else
    fail "expected .venv/bin/python3 inside $EXPECTED_DATA_DIR"
    exit 1
fi

INSTALLED_VERSION=$( \
    "$EXPECTED_DATA_DIR/uv" pip list --python "$EXPECTED_DATA_DIR/.venv/bin/python3" 2>/dev/null \
        | awk '$1 == "reachy-mini" { print $2 }' \
        || true \
)
if [[ -n "$INSTALLED_VERSION" ]]; then
    ok "reachy-mini installed: $INSTALLED_VERSION"
else
    warn "could not determine installed reachy-mini version (non-fatal)"
fi

step "Smoke test passed 🎉"
exit 0
