# Known edge cases

Living document tracking edge cases in the tray app, their current handling,
impact, and the minimal change needed to address them. Update it when a case
is fixed or a new one is found.

Legend:

- **Not handled** - the case can occur and produces a bad outcome today.
- **Accepted** - known, deliberately left as-is (fix cost outweighs benefit).
- **Handled** - covered, listed here so we don't re-investigate it.

---

## 1. First-run readiness timeout is a hard cap, not activity-based

**Status:** Handled - the startup watchdog is now activity-based.

The old probe used a fixed 300 s ceiling from spawn, never reset on activity.
On a slow first launch (Windows Defender venv scan, GStreamer registry scan,
uv + Python + `reachy-mini` downloads) that cap could fire while the bootstrap
was in fact still progressing.

The watchdog now mirrors the desktop app's production semantics
(`DAEMON_CONFIG.STARTUP` in `src/config/daemon.ts`):

- every stdout/stderr line from the trampoline/daemon re-arms the deadline
  (`handle_daemon_line` -> `state::note_daemon_output`);
- the budget is **90 s of silence** normally (`STARTUP_INACTIVITY_BUDGET`),
  extended to **600 s** between the first `[bootstrap]` line and
  `[bootstrap] Setup complete!` (`STARTUP_INACTIVITY_BUDGET_BOOTSTRAP`);
- the trampoline's 5 s `still working...` heartbeats keep re-arming the
  deadline during the long opaque steps, so a live bootstrap can never time
  out, while a genuinely wedged silent process is caught within 90 s.

---

## 2. `wmic` is being removed on Windows 11 (24H2+)

**Status:** Handled - replaced by PowerShell CIM.

The Windows orphan sweep's belt-and-braces step (kill leftover Python daemons
by module name) used `wmic`, which recent Windows 11 builds no longer install
by default. It now shells out to
`Get-CimInstance Win32_Process -Filter "CommandLine like '%...%'"` via
`powershell -NoProfile`, available on every supported Windows version. The
per-PID command-line lookup used by the identity check (case 3 below) goes
through the same CIM path.

---

## 3. A third-party process on `:8000` / `:8443` gets killed

**Status:** Handled - identity check before every port-based kill.

The orphan sweep (boot, start pre-flight, post-kill) now reads each candidate
PID's command line (`ps -o command=` on Unix, PowerShell CIM on Windows) and
only kills processes matching the daemon module
(`reachy_mini.daemon.app.main`) or the `uv-trampoline` sidecar. An unrelated
server parked on `:8000` is left alone with a loud log line; the subsequent
daemon start will then fail with `address already in use`, which is the
correct outcome (the user must free the port or the collision is theirs to
arbitrate).

Residual risk (accepted): when the command line is *unreadable* (process
already exiting, permission edge), we fail open and kill anyway - matching
the historical behaviour, and killing an already-dead PID is harmless. The
sweep is also two-phase now (graceful TERM, 500 ms grace, force-kill
survivors), mirroring the desktop app's `cleanup_system_daemons`.

---

## 4. `taskkill` fails when the daemon runs under a different user / elevation

**Status:** Accepted - rare.

`taskkill /F /T /PID` needs sufficient rights over the target process. If the
orphaned daemon ended up running elevated or under another user, the call fails
and we ignore the error, leaving the orphan alive.

**Why accepted:** the tray always spawns the daemon as the same, non-elevated
user, so the owner matches in practice.

---

## Correctly handled (recorded to avoid re-investigation)

- **Orphaned daemon cleanup is wired on all four paths** on both Unix and
  Windows: boot sweep, start pre-flight, explicit kill, and app exit.
- **`taskkill /T` cannot take the tray down:** we exclude our own PID, and the
  orphaned Python child is reparented away from the tray, so it is never our
  ancestor.
- **USB / serial:** cross-platform via the `serialport` crate; `COM<n>` handled
  on Windows, `cu.*`/`tty.*` twins de-duplicated on macOS, single-device
  auto-select, ghost-selection cleared on unplug.
- **USB mode with no device** auto-falls back to Simulation instead of
  crashing, and surfaces the downgrade via a native OS notification when it
  happens on an explicit Start (boot-time reconciliation stays silent: the
  menu already shows the mode before the user acts).
- **Connection mode persists across launches** (`tray_settings.json` in the
  shared data dir); only explicit user picks are persisted, never the
  boot-time USB -> Simulation downgrade.
- **Port match is robust:** `:8000` does not match `:18000` nor a PID column
  that happens to equal `8000`.
- **Zombie "answers too fast" race:** a 1.5 s healthcheck grace period ignores a
  leftover daemon still bound to `:8000` right after a restart.

---

## OS code-signing (release hygiene)

The bundle updater's minisign signature is independent of OS code-signing and
always works. OS signing only affects the **first manual install** experience
and the ability to relaunch a swapped bundle. Current state:

| Platform | Now (repo under `tfrere`) | Target (repo under `pollen`) |
|----------|---------------------------|------------------------------|
| macOS    | ad-hoc (`signingIdentity: "-"`), injected in CI when no `APPLE_SIGNING_IDENTITY` secret | Developer ID + notarization via the `APPLE_*` secrets |
| Windows  | unsigned (SmartScreen warning on first install) | Authenticode (e.g. Azure Trusted Signing) |
| Linux    | AppImage, no OS signing needed | unchanged |

The pipeline is **already wired for full signing**: `release.yml` passes all the
`APPLE_*` secrets to `tauri-action`. The day they are populated (planned at the
pollen migration), macOS builds become fully notarized with **no workflow
change** - the ad-hoc fallback step self-skips as soon as `APPLE_SIGNING_IDENTITY`
is present.

### Interim macOS ad-hoc caveat

- Ad-hoc signed bundles are **not notarized**, so the first manual install still
  triggers Gatekeeper (right-click > Open once). This does **not** affect
  auto-update: once installed, the updater can verify (minisign), swap and
  relaunch the ad-hoc bundle without a Gatekeeper prompt.
- Ad-hoc identities are machine-agnostic, so a build signed on the runner opens
  on any Mac (unlike a certificate tied to a keychain).

## Self-update (Tauri bundle updater)

The tray self-updates via `tauri-plugin-updater` (see `src/app_update.rs`):
a startup check (release builds only) against
`releases/latest/download/latest.json` opens a blocking overlay when a newer
version is published. The CI builds, signs and publishes everything through
`tauri-apps/tauri-action` (see `release.yml`): it signs the updater artifacts
with `TAURI_SIGNING_PRIVATE_KEY` and assembles + uploads the merged `latest.json`
in a single step (`uploadUpdaterJson: true`).

Known edge cases:

- **First updater-enabled release can't reach older installs:** builds shipped
  before this feature have no updater, so users on those versions must
  reinstall once from the GitHub Release. Every version from here on
  self-updates. (Expected, one-time.)
- **Signing key loss = broken updates:** if `TAURI_SIGNING_PRIVATE_KEY` (repo
  secret) and its local backup (`~/.tauri/reachy_mini_tray.key`) are both
  lost, the embedded `pubkey` no longer matches and updates fail to verify.
  Recovery requires shipping a new pubkey via a manually-installed build.
- **`releases/latest` skips pre-releases:** alpha/beta tags are marked
  prerelease, so `releases/latest/download/latest.json` always resolves to the
  newest *stable* release. Pre-release testers won't be offered pre-releases
  through the updater (acceptable: they install those manually).
