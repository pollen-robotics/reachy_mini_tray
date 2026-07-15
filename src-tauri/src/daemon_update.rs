//! Runtime daemon-version update check.
//!
//! The tray bakes a *pinned* `reachy-mini` version into the `uv-trampoline`
//! sidecar at build time (see `daemon-version.txt`). That pin is a
//! deterministic **floor**: it guarantees a known-good version at first
//! install and after an app update. This module adds the complementary
//! **ceiling awareness**: it periodically asks GitHub for the latest
//! published `reachy_mini` release and, when the installed venv is behind,
//! surfaces an "Update daemon" row in the tray menu that the user can
//! click to upgrade in place.
//!
//! Why GitHub Releases (and not PyPI)? The mobile app already resolves the
//! reference this exact way (`releases/latest` -> `tag_name`), so a robot's
//! "needs an update" prompt on mobile and the desktop tray agree on what
//! "latest" means. See `reachy_mini_mobile_app/.../latestRelease.ts`.
//!
//! ## Interaction with the trampoline's spec-diff upgrade
//!
//! A user-triggered upgrade here deliberately does **not** touch the
//! trampoline's `.reachy_mini_spec` marker. That marker is compared against
//! the *baked* pin on every launch (`uv_wrapper::needs_upgrade`); leaving it
//! untouched means a runtime upgrade to a version *newer* than the pin is
//! never clobbered back down on the next launch. The pin only ever pulls the
//! venv *up* to the floor, never down from a user upgrade.
//!
//! ## Failure discipline
//!
//! Every GitHub lookup fails open: any error (offline, rate-limited,
//! unexpected shape) resolves to "unknown" and simply hides the update row.
//! We never nag on a version we can't reason about.

use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tauri::{AppHandle, Manager};

use crate::daemon::{start_daemon, stop_daemon};
use crate::logs;
use crate::paths;
use crate::state::{current_daemon_state, AppState, DaemonState};
use crate::tray_menu::request_menu_refresh;

/// GitHub Releases endpoint for the daemon package. `releases/latest`
/// excludes drafts and pre-releases by default, so it tracks the same
/// stable line the build-time pin follows.
const LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/pollen-robotics/reachy_mini/releases/latest";

/// GitHub requires a User-Agent on every API request or it answers 403.
const USER_AGENT: &str = concat!("reachy_mini_tray/", env!("CARGO_PKG_VERSION"));

const HTTP_TIMEOUT: Duration = Duration::from_secs(8);

/// The latest release changes rarely; cache it to stay well under GitHub's
/// unauthenticated 60 req/h-per-IP budget. Mirrors the mobile app's TTL.
const LATEST_TTL: Duration = Duration::from_secs(6 * 60 * 60);

/// Cadence of the background re-evaluation loop. The GitHub call itself is
/// gated by `LATEST_TTL`, so most ticks only re-read the local venv version.
const POLL_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Small delay before the first check so we don't compete with first-run
/// bootstrap / daemon boot for CPU and network.
const POLL_INITIAL_DELAY: Duration = Duration::from_secs(20);

/// `(major, minor, patch)`. Pre-release suffixes on the patch component
/// (`0rc1`) are truncated to their numeric prefix, matching both the
/// trampoline's `get_installed_version` and the mobile app's `parseSemver`.
pub type SemVer = (u32, u32, u32);

/// Render a `SemVer` back to a `major.minor.patch` string for menu labels.
pub fn fmt_version(v: SemVer) -> String {
    format!("{}.{}.{}", v.0, v.1, v.2)
}

/// Parse a `MAJOR.MINOR.PATCH[...]` string, tolerating a leading `v`
/// (GitHub tags) and any pre-release / build suffix on the patch field.
/// Returns `None` when the first three numeric components can't be read.
fn parse_semver(value: &str) -> Option<SemVer> {
    let trimmed = value.trim();
    let trimmed = trimmed
        .strip_prefix('v')
        .or_else(|| trimmed.strip_prefix('V'))
        .unwrap_or(trimmed);

    let mut parts = trimmed.split('.');
    let major = parts.next()?.parse::<u32>().ok()?;
    let minor = parts.next()?.parse::<u32>().ok()?;
    let patch_field = parts.next()?;
    let patch_digits: String = patch_field.chars().take_while(|c| c.is_ascii_digit()).collect();
    let patch = patch_digits.parse::<u32>().ok()?;
    Some((major, minor, patch))
}

// ============================================================================
// STATE
// ============================================================================

/// Version + provenance of the `reachy-mini` currently in `.venv`.
#[derive(Clone, Copy, Debug, Default)]
struct InstalledInfo {
    version: Option<SemVer>,
    /// `true` when the package was installed from a VCS (git) source rather
    /// than a released wheel. Detected from PEP 610 `direct_url.json`.
    from_git: bool,
}

/// Immutable view of the update state, consumed by the tray menu builder.
#[derive(Clone, Copy, Debug, Default)]
pub struct UpdateSnapshot {
    pub installed: Option<SemVer>,
    /// The venv is pinned to a git branch/commit (dev build). We never
    /// advertise a PyPI update on top of it - clicking the row would swap
    /// the branch out for a released wheel, which is almost never intended.
    pub installed_from_git: bool,
    pub latest: Option<SemVer>,
    pub updating: bool,
}

impl UpdateSnapshot {
    /// True only when both versions are known AND the installed one is
    /// strictly behind the latest. Suppressed for git installs and unknown
    /// on either side -> false (fail-open: no row, no nag).
    pub fn available(&self) -> bool {
        if self.installed_from_git {
            return false;
        }
        matches!((self.installed, self.latest), (Some(i), Some(l)) if l > i)
    }

    /// A stable string used by the poller to detect when a menu rebuild is
    /// actually warranted (avoids spamming `refresh_status`).
    fn signature(&self) -> String {
        format!(
            "{:?}|{}|{:?}|{}|{}",
            self.installed,
            self.installed_from_git,
            self.latest,
            self.updating,
            self.available()
        )
    }
}

struct CachedLatest {
    value: Option<SemVer>,
    fetched_at: Option<Instant>,
}

/// Tauri-managed store for the daemon update state.
pub struct DaemonUpdateStore {
    installed: Mutex<InstalledInfo>,
    latest: Mutex<CachedLatest>,
    updating: AtomicBool,
}

impl DaemonUpdateStore {
    pub fn new() -> Self {
        Self {
            installed: Mutex::new(InstalledInfo::default()),
            latest: Mutex::new(CachedLatest {
                value: None,
                fetched_at: None,
            }),
            updating: AtomicBool::new(false),
        }
    }

    pub fn snapshot(&self) -> UpdateSnapshot {
        let installed = self.installed.lock().map(|g| *g).unwrap_or_default();
        UpdateSnapshot {
            installed: installed.version,
            installed_from_git: installed.from_git,
            latest: self.latest.lock().ok().and_then(|g| g.value),
            updating: self.updating.load(Ordering::SeqCst),
        }
    }

    fn set_installed(&self, info: InstalledInfo) {
        if let Ok(mut g) = self.installed.lock() {
            *g = info;
        }
    }

    pub fn updating(&self) -> bool {
        self.updating.load(Ordering::SeqCst)
    }

    fn try_begin_update(&self) -> bool {
        self.updating
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    fn end_update(&self) {
        self.updating.store(false, Ordering::SeqCst);
    }

    /// Return the cached latest version, refreshing from GitHub when the
    /// cache is empty or older than `LATEST_TTL`. On a failed refresh we
    /// keep whatever we had (never downgrade a good value to `None`) but
    /// still stamp the fetch time so we respect the TTL and don't hammer
    /// the API.
    fn latest_cached_or_fetch(&self) -> Option<SemVer> {
        {
            let guard = self.latest.lock().ok()?;
            let fresh = guard
                .fetched_at
                .map(|t| t.elapsed() < LATEST_TTL)
                .unwrap_or(false);
            if fresh {
                return guard.value;
            }
        }

        let fetched = fetch_latest_from_github();
        if let Ok(mut guard) = self.latest.lock() {
            if let Some(v) = fetched {
                guard.value = Some(v);
            }
            // else: keep the previous value (fail-open, serve stale).
            guard.fetched_at = Some(Instant::now());
            return guard.value;
        }
        fetched
    }
}

impl Default for DaemonUpdateStore {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// VERSION SOURCES
// ============================================================================

/// Locate the `reachy_mini-*.dist-info` directory inside `.venv`. Mirrors
/// the site-packages resolution in `uv_wrapper::get_installed_version`.
fn dist_info_dir() -> Option<std::path::PathBuf> {
    let py = paths::venv_python_for(".venv")?;
    // Both layouts put the interpreter two levels below the venv root:
    // `.venv/bin/python3` (Unix) and `.venv\Scripts\python.exe` (Windows).
    let venv_root = py.parent()?.parent()?;

    let site_packages = if cfg!(target_os = "windows") {
        venv_root.join("Lib").join("site-packages")
    } else {
        let lib = venv_root.join("lib");
        let python_dir = std::fs::read_dir(&lib)
            .ok()?
            .filter_map(|e| e.ok())
            .find(|e| e.file_name().to_string_lossy().starts_with("python3"))?;
        python_dir.path().join("site-packages")
    };

    std::fs::read_dir(&site_packages)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| {
            let name = p.file_name().unwrap_or_default().to_string_lossy();
            name.starts_with("reachy_mini-") && name.ends_with(".dist-info")
        })
}

/// Read the installed `reachy-mini` version *and* provenance straight from
/// the `.venv` dist-info. Self-contained (no dependency on the daemon
/// running or on the `uv-wrapper` crate) so the menu is accurate even while
/// the daemon is idle.
///
/// Provenance comes from PEP 610's `direct_url.json`: a `vcs_info` key means
/// the package was installed from a git branch/commit (a dev build), so we
/// must not offer a PyPI "update" that would silently replace it.
fn read_installed() -> InstalledInfo {
    let Some(dir) = dist_info_dir() else {
        return InstalledInfo::default();
    };

    let version = std::fs::read_to_string(dir.join("METADATA"))
        .ok()
        .and_then(|meta| {
            meta.lines()
                .find(|l| l.starts_with("Version: "))
                .and_then(|l| l.strip_prefix("Version: "))
                .and_then(parse_semver)
        });

    let from_git = std::fs::read_to_string(dir.join("direct_url.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .map(|v| v.get("vcs_info").is_some())
        .unwrap_or(false);

    InstalledInfo { version, from_git }
}

#[derive(Deserialize)]
struct GithubRelease {
    #[serde(default)]
    tag_name: Option<String>,
}

/// Resolve the latest published daemon version from GitHub Releases, or
/// `None` on any failure (offline, 403 rate-limit, unexpected shape).
fn fetch_latest_from_github() -> Option<SemVer> {
    let client = reqwest::blocking::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .ok()?;
    let resp = client
        .get(LATEST_RELEASE_URL)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", USER_AGENT)
        .send()
        .ok()?;
    if !resp.status().is_success() {
        log::debug!("github releases/latest -> {}", resp.status());
        return None;
    }
    let rel = resp.json::<GithubRelease>().ok()?;
    let tag = rel.tag_name?;
    parse_semver(&tag)
}

// ============================================================================
// POLLER
// ============================================================================

/// Spawn the single long-lived background thread that keeps the update
/// snapshot current and refreshes the tray menu when it changes.
pub fn start_update_poller(app: AppHandle) {
    std::thread::spawn(move || {
        std::thread::sleep(POLL_INITIAL_DELAY);
        let mut last_signature: Option<String> = None;
        loop {
            refresh_once(&app, &mut last_signature);
            std::thread::sleep(POLL_INTERVAL);
        }
    });
}

fn refresh_once(app: &AppHandle, last_signature: &mut Option<String>) {
    let store = app.state::<DaemonUpdateStore>();
    // Never poke state while an upgrade is running: the venv is mid-mutation
    // and the "Updating daemon…" row must stay stable until it completes.
    if store.updating() {
        return;
    }

    store.set_installed(read_installed());
    let _ = store.latest_cached_or_fetch();

    let snap = store.snapshot();
    let sig = snap.signature();
    if last_signature.as_deref() != Some(&sig) {
        *last_signature = Some(sig);
        if snap.available() {
            if let (Some(i), Some(l)) = (snap.installed, snap.latest) {
                log::info!(
                    "daemon update available: {} -> {}",
                    fmt_version(i),
                    fmt_version(l)
                );
            }
        }
        request_menu_refresh(app);
    }
}

// ============================================================================
// UPGRADE
// ============================================================================

/// Trigger a user-requested daemon upgrade to the latest known version.
/// No-op when already updating, or when no update is available. Spawns a
/// worker thread so the tray event loop is never blocked.
pub fn start_update(app: &AppHandle) {
    let store = app.state::<DaemonUpdateStore>();
    let snap = store.snapshot();

    if snap.updating {
        log::info!("daemon update ignored: one already in progress");
        return;
    }
    let Some(target) = snap.latest.filter(|_| snap.available()) else {
        log::info!("daemon update ignored: nothing newer than installed");
        return;
    };
    if !store.try_begin_update() {
        return;
    }
    request_menu_refresh(app);

    let app = app.clone();
    std::thread::spawn(move || {
        let result = run_upgrade(&app, target);

        let store = app.state::<DaemonUpdateStore>();
        // Re-read the venv so the snapshot reflects reality whether the
        // upgrade succeeded, partially applied, or failed.
        store.set_installed(read_installed());
        store.end_update();

        match result {
            Ok(()) => logs::push_external(
                &app,
                "tray",
                "INFO",
                format!("Daemon upgraded to {}", fmt_version(target)),
            ),
            Err(e) => logs::push_external(
                &app,
                "tray",
                "ERROR",
                format!("Daemon upgrade failed: {}", e),
            ),
        }
        request_menu_refresh(&app);
    });
}

/// Stop the daemon (if running), `uv pip install -U` the target version into
/// both venvs, then restart the daemon iff it was running before. Returns a
/// human-readable error on the first failing step.
fn run_upgrade(app: &AppHandle, target: SemVer) -> Result<(), String> {
    let data_dir = paths::data_dir().ok_or("data dir unavailable")?;
    let spec = format!("reachy-mini=={}", fmt_version(target));

    let was_running = matches!(
        current_daemon_state(&app.state::<AppState>()),
        DaemonState::Running | DaemonState::Starting
    );

    // Stop first: on Windows the venv DLLs are locked while Python runs, and
    // even on Unix we don't want the daemon executing half-swapped files.
    if was_running {
        logs::push_external(app, "tray", "INFO", "Stopping daemon for upgrade…".into());
        stop_daemon(app);
        // Give the OS a beat to release the venv files / listening sockets.
        std::thread::sleep(Duration::from_millis(500));
    }

    logs::push_external(app, "tray", "INFO", format!("Upgrading {} in .venv…", spec));
    upgrade_venv(app, &data_dir, ".venv", &spec)?;

    // apps_venv only exists on installs that provisioned the shared app SDK
    // environment (reachy-mini >= 1.6). Upgrade it in lockstep when present.
    if paths::venv_python_for("apps_venv")
        .map(|p| p.exists())
        .unwrap_or(false)
    {
        logs::push_external(
            app,
            "tray",
            "INFO",
            format!("Upgrading {} in apps_venv…", spec),
        );
        upgrade_venv(app, &data_dir, "apps_venv", &spec)?;
    }

    // Deliberately DO NOT write the `.reachy_mini_spec` marker here: keeping
    // it pointing at the baked pin lets the pin act as a floor without
    // clobbering this (newer) user upgrade on the next launch.

    if was_running {
        logs::push_external(app, "tray", "INFO", "Restarting daemon…".into());
        start_daemon(app);
    }
    Ok(())
}

/// Run `uv pip install -U --python <venv-python> <spec>` for one venv,
/// forwarding captured output to the in-app log window. Mirrors
/// `uv_wrapper::upgrade_venvs` (minus the marker write).
fn upgrade_venv(app: &AppHandle, data_dir: &Path, venv: &str, spec: &str) -> Result<(), String> {
    let uv = paths::uv_exe_path().ok_or("uv executable path unavailable")?;
    if !uv.exists() {
        return Err(format!("uv not found at {} (run setup first)", uv.display()));
    }
    let python = paths::venv_python_for(venv).ok_or("venv python path unavailable")?;
    let python_str = python.to_str().ok_or("venv python path is not valid UTF-8")?;

    let output = Command::new(&uv)
        .current_dir(data_dir)
        .env("UV_PYTHON_INSTALL_DIR", data_dir)
        .env("UV_WORKING_DIR", data_dir)
        .args(["pip", "install", "-U", "--python", python_str, spec])
        .output()
        .map_err(|e| format!("failed to launch uv: {}", e))?;

    for line in String::from_utf8_lossy(&output.stdout)
        .lines()
        .chain(String::from_utf8_lossy(&output.stderr).lines())
    {
        let trimmed = line.trim_end();
        if !trimmed.is_empty() {
            logs::push_external(app, "tray", "INFO", format!("[uv:{}] {}", venv, trimmed));
        }
    }

    if !output.status.success() {
        return Err(format!(
            "uv pip install for {} exited with {:?}",
            venv,
            output.status.code()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_semver_plain() {
        assert_eq!(parse_semver("1.8.4"), Some((1, 8, 4)));
    }

    #[test]
    fn parse_semver_strips_leading_v() {
        assert_eq!(parse_semver("v1.8.4"), Some((1, 8, 4)));
        assert_eq!(parse_semver("V2.0.0"), Some((2, 0, 0)));
    }

    #[test]
    fn parse_semver_truncates_prerelease_patch() {
        assert_eq!(parse_semver("1.9.0rc1"), Some((1, 9, 0)));
        assert_eq!(parse_semver("v1.9.0-rc.2"), Some((1, 9, 0)));
    }

    #[test]
    fn parse_semver_rejects_garbage() {
        assert_eq!(parse_semver("not-a-version"), None);
        assert_eq!(parse_semver("1.8"), None);
        assert_eq!(parse_semver(""), None);
    }

    #[test]
    fn snapshot_available_only_when_strictly_newer() {
        let base = |i, l| UpdateSnapshot {
            installed: i,
            installed_from_git: false,
            latest: l,
            updating: false,
        };
        assert!(base(Some((1, 8, 4)), Some((1, 8, 5))).available());
        assert!(base(Some((1, 8, 4)), Some((1, 9, 0))).available());
        assert!(!base(Some((1, 8, 4)), Some((1, 8, 4))).available());
        assert!(!base(Some((1, 8, 5)), Some((1, 8, 4))).available());
        // Unknown on either side never advertises an update.
        assert!(!base(None, Some((1, 8, 5))).available());
        assert!(!base(Some((1, 8, 4)), None).available());
    }

    #[test]
    fn snapshot_suppresses_update_for_git_installs() {
        // A dev build (git branch, e.g. 1.8.3) must never advertise the
        // latest PyPI release (1.8.4) as an update - clicking it would
        // replace the branch with a released wheel.
        let git = UpdateSnapshot {
            installed: Some((1, 8, 3)),
            installed_from_git: true,
            latest: Some((1, 8, 4)),
            updating: false,
        };
        assert!(!git.available());
    }

    #[test]
    fn fmt_version_roundtrips() {
        assert_eq!(fmt_version((1, 8, 4)), "1.8.4");
    }

    #[test]
    fn store_update_flag_is_exclusive() {
        let store = DaemonUpdateStore::new();
        assert!(!store.updating());
        assert!(store.try_begin_update());
        assert!(!store.try_begin_update(), "second begin must fail");
        store.end_update();
        assert!(store.try_begin_update(), "end should re-allow begin");
    }
}
