#!/usr/bin/env bash
# Build the uv-trampoline sidecar from the existing desktop app crate.
#
# We deliberately reuse `reachy_mini_desktop_app/uv-wrapper/` instead of
# vendoring the source so:
#   - any fix made on the desktop side automatically benefits the tray app;
#   - we keep one single source of truth for the bootstrap logic.
#
# Tauri's `externalBin` mechanism expects a binary named
# `<basename>-<rust_target_triple>` (e.g. `uv-trampoline-aarch64-apple-darwin`).
# It picks the right one at runtime based on the host architecture.
#
# Selecting which `reachy-mini` Python package to bake in:
#   - default            → pinned version from ../daemon-version.txt
#                          (committed, deterministic, what releases ship)
#   - REACHY_MINI_SOURCE → git branch on pollen-robotics/reachy_mini
#                          (e.g. `develop`, `integration/mobile-app-daemon`)
#   - REACHY_MINI_VERSION → pin a specific PyPI version (e.g. `1.6.4`)
#   - First positional arg `<branch-or-pypi>` is a shortcut for setting
#     REACHY_MINI_SOURCE without exporting the env var manually.
#
# Examples:
#   ./scripts/build-sidecar.sh                        # pinned daemon-version.txt
#   ./scripts/build-sidecar.sh integration/mobile-app-daemon
#   REACHY_MINI_SOURCE=develop ./scripts/build-sidecar.sh
#   REACHY_MINI_VERSION=1.6.4 ./scripts/build-sidecar.sh
#
# Run this script:
#   - once before `npm run dev` on a fresh checkout;
#   - whenever `uv-wrapper` source changes;
#   - whenever you switch the daemon branch / version;
#   - inside CI before `tauri build` for each release target.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TRAY_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
PROJECT_ROOT="$(cd "$TRAY_ROOT/.." && pwd)"

# Where to find the `uv-wrapper` Rust crate that contains the trampoline.
# Override priority:
#   1. $UV_WRAPPER_DIR  (CI / custom checkouts)
#   2. ../reachy_mini_desktop_app/uv-wrapper (sibling, default for dev)
if [[ -n "${UV_WRAPPER_DIR:-}" ]]; then
    SRC_CRATE="$UV_WRAPPER_DIR"
else
    SRC_CRATE="$PROJECT_ROOT/reachy_mini_desktop_app/uv-wrapper"
fi
DST_DIR="$TRAY_ROOT/src-tauri/binaries"
SPEC_MARKER="$DST_DIR/.reachy_mini_spec"

# Committed default pin: the single source of truth for which reachy-mini
# version a tray release ships. Read only when no explicit env / branch
# override is given (see resolution block below).
PIN_FILE="$TRAY_ROOT/daemon-version.txt"

if [[ ! -d "$SRC_CRATE" ]]; then
    echo "Error: cannot find uv-wrapper crate at $SRC_CRATE" >&2
    echo "Either set UV_WRAPPER_DIR to point to a local clone of" >&2
    echo "reachy_mini_desktop_app/uv-wrapper, or check out the repo as a" >&2
    echo "sibling of reachy_mini_tray." >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# Resolve which `reachy-mini` to install at first-run bootstrap time.
# Mirrors get_reachy_mini_spec() in uv-wrapper/src/lib.rs.
# ---------------------------------------------------------------------------

# Positional shortcut: ./build-sidecar.sh <branch>  ==  REACHY_MINI_SOURCE=<branch>
if [[ $# -ge 1 && -n "${1:-}" ]]; then
    if [[ -z "${REACHY_MINI_SOURCE:-}" ]]; then
        export REACHY_MINI_SOURCE="$1"
    fi
fi

# Read the committed pin, ignoring comments and blank lines (first bare line
# wins). Empty when the file is missing or only contains comments.
read_pin() {
    [[ -f "$PIN_FILE" ]] || return 0
    grep -vE '^[[:space:]]*(#|$)' "$PIN_FILE" | head -1 | tr -d '[:space:]'
}

# Resolution precedence (first match wins):
#   1. REACHY_MINI_VERSION env         -> reachy-mini==X.Y.Z
#   2. REACHY_MINI_SOURCE env (!=pypi) -> git+...@branch
#   3. committed daemon-version.txt    -> reachy-mini==<pin>
#   4. fallback                        -> reachy-mini (latest from PyPI)
# The pin path (3) exports REACHY_MINI_VERSION so `option_env!` in the
# trampoline's get_reachy_mini_spec() bakes the exact version in at compile
# time (build.rs already reruns when this var changes).
if [[ -n "${REACHY_MINI_VERSION:-}" ]]; then
    SPEC="reachy-mini==${REACHY_MINI_VERSION}"
elif [[ -n "${REACHY_MINI_SOURCE:-}" && "${REACHY_MINI_SOURCE}" != "pypi" ]]; then
    SPEC="git+https://github.com/pollen-robotics/reachy_mini.git@${REACHY_MINI_SOURCE}"
else
    PIN="$(read_pin)"
    if [[ -n "$PIN" ]]; then
        export REACHY_MINI_VERSION="$PIN"
        SPEC="reachy-mini==${PIN}"
    else
        SPEC="reachy-mini (latest from PyPI)"
    fi
fi

echo "📦 Bake-in spec: ${SPEC}"

# ---------------------------------------------------------------------------
# Target triplet detection
# ---------------------------------------------------------------------------

if [[ -n "${TARGET_TRIPLET:-}" ]]; then
    TRIPLET="$TARGET_TRIPLET"
    echo "Using TARGET_TRIPLET from environment: $TRIPLET"
else
    TRIPLET="$(rustc -Vv | awk '/^host:/ {print $2}')"
    echo "Detected host triplet: $TRIPLET"
fi

mkdir -p "$DST_DIR"

# ---------------------------------------------------------------------------
# Build
#
# `build.rs` already declares `cargo:rerun-if-env-changed=REACHY_MINI_SOURCE`,
# so cargo invalidates the cache automatically when the spec changes. Belt-
# and-braces: we still record the spec to a marker so a stale binary on disk
# (left over from a previous spec) gets noticed if someone runs the script
# without changing the env.
# ---------------------------------------------------------------------------

echo "🔨 Building uv-trampoline (release) from $SRC_CRATE..."
pushd "$SRC_CRATE" > /dev/null
if [[ -n "${TARGET_TRIPLET:-}" ]]; then
    cargo build --release --bin uv-trampoline --target "$TRIPLET"
    cp "target/$TRIPLET/release/uv-trampoline" "$DST_DIR/uv-trampoline-$TRIPLET"
else
    cargo build --release --bin uv-trampoline
    cp "target/release/uv-trampoline" "$DST_DIR/uv-trampoline-$TRIPLET"
fi
popd > /dev/null

chmod +x "$DST_DIR/uv-trampoline-$TRIPLET"
echo "$SPEC" > "$SPEC_MARKER"

echo "✅ Sidecar ready: $DST_DIR/uv-trampoline-$TRIPLET"
echo "   Spec:        $SPEC"
echo ""
echo "ℹ️  If a venv already exists in the user's data dir, the trampoline"
echo "   will detect the spec change on next launch and upgrade it in-place."
echo "   Use 'Reset setup…' from the tray menu to force a clean reinstall."
