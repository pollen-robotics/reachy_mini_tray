# reachy_mini_tray

A lightweight Tauri 2 menu-bar app that runs the [Reachy Mini](https://www.pollen-robotics.com/) Python daemon as a background service. It is deliberately tray-only (no main window after first-run setup) and is intended as a slimmer alternative to `reachy_mini_desktop_app` for users who only need to keep the daemon running so the robot is reachable from the [mobile app](https://github.com/pollen-robotics/reachy_mini_mobile_app) over Hugging Face's WebRTC relay.

## Features

- **Menu-bar only** on macOS (`LSUIElement = true`): no Dock icon, no Cmd-Tab entry.
- **Single Start / Stop / Restart toggle** with a live status indicator (idle / starting / running / crashed).
- **First-run bootstrap window** that streams `uv` / Python venv install progress and auto-closes once the daemon is up.
- **Hugging Face account integration**: sign in via OAuth, see `Signed in as @user · remote on/off` directly in the menu, force-refresh the central relay, sign out.
- **USB-only by design**: no Bluetooth, no LAN scanning. Mode toggle between `USB` and `Simulation`.
- **Built-in log viewer** (last 2000 lines, daemon + tray).
- **Reset setup**: wipes the data dir and re-bootstraps from scratch (only enabled while the daemon is idle/crashed).
- **Process-group lifecycle**: the trampoline is its own session leader (`setpgid`), so killing the tray cleanly takes down the entire Python subtree (no orphaned uvicorn, no stale port 8000).

## Architecture

```
┌────────────────────────────┐
│ reachy_mini_tray (Rust)    │  Tauri 2.x, no main window
│  • tray menu               │
│  • first-run window        │  WebView (UI in ./ui/)
│  • log viewer window       │
│  • HF OAuth orchestrator   │
└──────────────┬─────────────┘
               │ spawns + monitors (process group)
               ▼
┌────────────────────────────┐
│ uv-trampoline (sidecar)    │  ../reachy_mini_desktop_app/uv-wrapper
│  • bootstraps uv + venv    │
│  • installs reachy-mini    │
│  • exec's python -m daemon │
└──────────────┬─────────────┘
               │
               ▼
┌────────────────────────────┐
│ reachy_mini.daemon (Py)    │  HTTP API on 127.0.0.1:8000
│  • motors / kinematics     │
│  • WebRTC + central relay  │
│  • HF auth                 │
└────────────────────────────┘
```

The daemon is the same upstream package used by the desktop app and the wireless robot itself; this tray app just owns its lifecycle on the user's machine.

## Requirements

- **macOS** 12+ (Apple Silicon and Intel), **Windows** 10+ (x86_64), **Linux** (x86_64, glibc-based distros). All three are built and smoke-tested in CI; macOS is the daily-driver platform, Windows / Linux are best-effort.
- Rust toolchain (`rustup`), Node 18+, and the Tauri prerequisites listed at <https://tauri.app/start/prerequisites/>.
- A sibling checkout of [`reachy-mini-desktop-app`](https://github.com/pollen-robotics/reachy-mini-desktop-app) next to this repo. The `uv-trampoline` sidecar is built from `reachy_mini_desktop_app/uv-wrapper/` so both apps share a single bootstrap implementation. CI checks it out automatically; for local dev keep the two repos as siblings.

```
parent/
├── reachy_mini_desktop_app/   ← required for the sidecar build
└── reachy_mini_tray/          ← this repo
```

## Quick start

```bash
# 1. Install JS deps
npm install

# 2. Build the uv-trampoline sidecar (default: latest reachy-mini from PyPI)
npm run build:sidecar

# 3. Run in dev mode
npm run dev
```

The first launch opens a setup window, downloads `uv`, creates a Python 3.12 venv in `~/Library/Application Support/com.pollen-robotics.reachy-mini/.venv`, installs `reachy-mini`, then closes itself. From then on the app is tray-only.

### Pinning the daemon version

The `reachy-mini` Python package is baked into the sidecar at build time. The
shipped version is **pinned** in a single committed file,
[`daemon-version.txt`](./daemon-version.txt), which is the source of truth for
every default (`npm run build:sidecar`) and CI release build. Bump that file in
a dedicated commit to move the whole app to a newer daemon:

```
# daemon-version.txt
1.8.4
```

Because the pin is a concrete version string, the trampoline's spec-diff check
(`needs_upgrade` in `uv-wrapper`) fires exactly once after the app updates and
upgrades the venv in place - so each tray release ships a known-good, tested
daemon version instead of whatever happened to be latest on PyPI at first run.

For local dev and branch builds you can still override the pin without editing
the file (overrides take precedence):

```bash
# Override with a different PyPI version
REACHY_MINI_VERSION=1.6.4 npm run build:sidecar

# A development branch on pollen-robotics/reachy_mini
npm run build:sidecar:develop
npm run build:sidecar:mobile-umbrella     # integration/mobile-app-daemon
npm run build:sidecar:branch <any-branch>
```

The marker `src-tauri/binaries/.reachy_mini_spec` records the resolved spec;
building with a different spec triggers an in-place venv upgrade on the next
launch (or use `Reset setup…` from the tray for a clean reinstall). Leaving
`daemon-version.txt` empty falls back to "latest from PyPI" (non-deterministic,
not recommended for releases).

## Daemon bootstrap smoke test

The tray has no main window to drive with `tauri-driver`, so instead of a
GUI E2E suite we ship a headless test that exercises the whole bootstrap
pipeline (uv download → Python install → venv → `pip install reachy-mini`
→ daemon HTTP up). It is the exact equivalent of "did the daemon install
work?" in the desktop app's E2E.

```bash
npm run test:daemon-bootstrap
# or, in CI:
bash ./scripts/test/test-daemon-bootstrap.sh
pwsh -File .\scripts\test\test-daemon-bootstrap.ps1   # Windows
```

The script redirects the platform's data dir to a fresh temp folder, so
it always starts from a clean install and never touches the real user
state. The daemon is launched in `--mockup-sim --no-media` mode (no
hardware, no GStreamer / portaudio at runtime). Cold-cache run is
~5 minutes; subsequent runs of the same script reuse nothing (each run
is fully isolated) so plan accordingly.

This same script runs on Linux, macOS, and Windows in
`.github/workflows/daemon-smoke.yml` on every push to `main` / `develop`.

## Build a release bundle

Locally for the host platform:

```bash
npm run build:sidecar
npm run build      # invokes `tauri build`
```

`tauri build` produces, depending on the host OS:

| Platform | Artifact                                                                                  |
|----------|-------------------------------------------------------------------------------------------|
| macOS    | `src-tauri/target/release/bundle/macos/*.app` + `dmg/*.dmg`                               |
| Windows  | `src-tauri/target/release/bundle/nsis/*.exe`                                              |
| Linux    | `src-tauri/target/release/bundle/deb/*.deb` + `appimage/*.AppImage`                       |

Code-signing and notarization use the standard Tauri configuration; see `src-tauri/tauri.conf.json` and `src-tauri/Info.plist`.

### Cross-platform release via CI

`.github/workflows/release.yml` builds the matrix (macOS aarch64 + x86_64, Windows x86_64, Linux x86_64) on every `v*` tag push, attaches all installers to the GitHub Release, and supports a `workflow_dispatch` dry-run mode. `.github/workflows/ci.yml` runs `cargo fmt` / `clippy` / `check` on every push, and `.github/workflows/daemon-smoke.yml` runs the bootstrap smoke test on each OS (see below).

## Layout

```
reachy_mini_tray/
├── ui/                              # Static frontend
│   ├── index.html                   # First-run bootstrap window
│   └── logs.html                    # Log viewer window
├── scripts/
│   ├── build-sidecar.sh             # Builds uv-trampoline → src-tauri/binaries/ (Unix)
│   ├── build-sidecar.ps1            # Same, Windows port
│   └── test/
│       ├── test-daemon-bootstrap.sh   # Headless smoke test (Unix)
│       └── test-daemon-bootstrap.ps1  # Headless smoke test (Windows)
├── .github/workflows/
│   ├── ci.yml                       # cargo fmt / clippy / check (3 OS)
│   ├── release.yml                  # Tagged release: build + bundle + GH Release (3 OS)
│   └── daemon-smoke.yml             # End-to-end daemon bootstrap test (3 OS)
└── src-tauri/
    ├── Cargo.toml
    ├── tauri.conf.json
    ├── build.rs
    ├── Info.plist                   # LSUIElement = true (macOS menu-bar only)
    ├── icons/
    ├── capabilities/
    ├── python-entitlements.plist
    └── src/
        ├── main.rs                  # Thin entry point → reachy_mini_tray_lib::run
        ├── lib.rs                   # Module wiring + Tauri builder + tray event router
        ├── state.rs                 # AppState, Mode, DaemonState, IconCache + accessors
        ├── daemon.rs                # uv-trampoline lifecycle (spawn / monitor / kill / healthcheck)
        ├── tray_icon.rs             # Status-badge composition (RGBA disc + ring)
        ├── tray_menu.rs             # Dynamic menu builder + refresh_status entry point
        ├── commands.rs              # Webview window helpers + Tauri IPC commands
        ├── hf_auth.rs               # OAuth orchestrator + status poller
        ├── api.rs                   # Daemon base URL + shared reqwest client factory
        ├── logs.rs                  # In-memory ring buffer + log window IPC
        └── paths.rs                 # Per-OS data-dir helpers + bootstrap detection
```

## Roadmap

Planned for upcoming releases:

- **macOS permissions view** — a dedicated panel (likely surfaced from the
  first-run window and reachable later from the tray menu) that shows
  which macOS TCC permissions the daemon needs (USB serial / Input
  Monitoring, Microphone if audio I/O is used, etc.), their current
  granted/denied state, and one-click deep-links to the relevant pane in
  System Settings. Today users have to dig through *Settings → Privacy &
  Security* on their own when something silently fails.
- **Auto-update on macOS, Windows and Linux** — Tauri's built-in updater,
  fed by the GitHub Releases produced by `release.yml`. On macOS via the
  signed `.app`, on Windows via the NSIS installer, on Linux via the
  AppImage stream (deb users will need to keep updating through their
  package manager). Requires code-signing to be in place first (see
  below).
- **Code-signing pipelines** for macOS notarization and Windows
  Authenticode (Linux AppImage uses a detached `.sig`). Currently bundles
  ship unsigned, which is fine for internal beta but will trigger
  Gatekeeper / SmartScreen warnings for end users.

## Out of scope (for now)

Autostart-at-login and system sleep/wake handling. These have low product
demand and clear workarounds (the user can launch the tray manually after
boot/wake), so they sit behind the roadmap items above.

## License

MIT.
