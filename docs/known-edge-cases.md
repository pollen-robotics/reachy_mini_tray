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

**Status:** Not handled - highest user-facing impact.

The daemon readiness probe uses a fixed 300 s ceiling from spawn:

```rust
// src-tauri/src/daemon.rs
const HEALTHCHECK_MAX_DURATION: Duration = Duration::from_secs(300);
```

It is never reset on activity. On a first launch (especially Windows), several
slow steps stack up:

- Windows Defender real-time scan of every file in the freshly created venv
  (thousands of files),
- the GStreamer plugin-registry scan (~2-4 min on a cold machine),
- uv download + Python install + `reachy-mini` install.

On a slow disk / HDD this can exceed 5 minutes, so the tray flips to `Crashed`
while the bootstrap is in fact still progressing. The `uv-trampoline` already
emits `[bootstrap] ... (still working...)` heartbeats every 5 s.

The desktop app avoids this by using an **activity-based** reset (~15 s without
output) instead of a hard cap.

**Minimal fix (~10 lines):** we already parse every daemon line in
`handle_daemon_line`. While the first-run window is open, re-arm the healthcheck
deadline whenever a bootstrap line arrives (shared `Instant`/timestamp the
healthcheck thread reads), so the timeout measures *inactivity*, not total time.

---

## 2. `wmic` is being removed on Windows 11 (24H2+)

**Status:** Not handled - low impact, graceful degradation.

The Windows orphan sweep has three steps (see `reap_orphaned_daemons` in
`src-tauri/src/daemon.rs`):

1. `netstat -ano` maps daemon listening ports to PIDs,
2. `taskkill /F /T /PID <pid>` kills the process tree,
3. **belt-and-braces:** `wmic process where "CommandLine like '%...%'"` kills any
   leftover Python daemon by module name.

Step 3 relies on `wmic`, which recent Windows 11 builds no longer install by
default. When absent, `Command::new("wmic")` fails and step 3 silently no-ops.

**Impact is low:** steps 1-2 already catch any *alive* daemon (uvicorn keeps
`:8000` bound for the whole process lifetime). Step 3 only matters for the rare
case of a process holding the serial (COM) port without an HTTP listener.

**Minimal fix (~5 lines):** replace the `wmic` call with a PowerShell CIM
one-liner that works on all supported Windows versions:

```
powershell -NoProfile -Command "Get-CimInstance Win32_Process |
  Where-Object { $_.CommandLine -like '*reachy_mini.daemon.app.main*' } |
  ForEach-Object { $_.ProcessId }"
```

Then `taskkill /F /T /PID` each returned PID. Alternatively, accept the current
graceful degradation.

---

## 3. A third-party process on `:8000` / `:8443` gets killed

**Status:** Accepted.

The orphan sweep runs at boot, as a start pre-flight, and after every kill. It
kills **any** owner of the daemon ports without verifying it is actually our
daemon. This affects macOS, Linux, and Windows alike (it mirrors the desktop
app's behavior).

If an unrelated application happens to listen on `:8000` or `:8443`, it will be
terminated.

**Why accepted:** verifying process identity (name/path/parentage) before
killing is a larger change, and the probability of a legitimate collision on
these specific ports is low. Documented rather than coded around.

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
- **USB mode with no device** auto-falls back to Simulation instead of crashing.
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
