//! Self-update of the tray app itself, via the Tauri bundle updater.
//!
//! This is distinct from [`crate::daemon_update`], which upgrades the Python
//! `reachy-mini` package inside the venv. Here we update the *tray bundle*
//! (the `.app` / `.exe` / `.AppImage`) the same way the desktop app does:
//!
//! - `tauri.conf.json > plugins.updater` points at a signed `latest.json`
//!   hosted as a GitHub Release asset, and embeds the minisign public key.
//! - On startup (release builds only) we check the endpoint; if a newer
//!   version is published, we open a blocking overlay window
//!   (`ui/update.html`) that nags the user to install it - mirroring the
//!   desktop app's forced-update gate.
//! - The overlay calls [`install_app_update`], which downloads + verifies +
//!   installs the new bundle and relaunches the app.
//!
//! Everything is driven from Rust (the tray has no persistent main window),
//! with the overlay acting purely as a presentational front-end fed by
//! Tauri events.

use std::sync::Mutex;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_updater::UpdaterExt;

use crate::commands::show_update_window;

/// Emitted once when a newer version is found, so the overlay can populate
/// its version labels even if it opens after the check completed.
const EVENT_AVAILABLE: &str = "app-update:available";
/// Emitted on every downloaded chunk during install.
const EVENT_PROGRESS: &str = "app-update:progress";
/// Emitted when the download/install fails, so the overlay can re-enable
/// its button and surface the error.
const EVENT_ERROR: &str = "app-update:error";

/// Metadata about the pending update, cached so the overlay window can pull
/// it on load via [`get_app_update_info`] (it may open slightly after the
/// `app-update:available` event fired).
#[derive(Clone, Serialize, Default)]
pub struct AvailableInfo {
    /// Version currently running (from `tauri.conf.json`).
    pub current: String,
    /// Version offered by the update endpoint.
    pub version: String,
    /// Release notes / changelog body, if any.
    pub notes: String,
}

/// Managed state holding the last-known pending update, if any.
#[derive(Default)]
pub struct AppUpdateStore {
    info: Mutex<Option<AvailableInfo>>,
}

impl AppUpdateStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn set(&self, info: AvailableInfo) {
        if let Ok(mut guard) = self.info.lock() {
            *guard = Some(info);
        }
    }

    fn get(&self) -> Option<AvailableInfo> {
        self.info.lock().ok().and_then(|g| g.clone())
    }
}

/// Fire a one-shot background check at startup. Fail-open: any error
/// (offline, endpoint 404 before the first release, rate-limited) just
/// logs and leaves the tray running normally.
// Only called under `cfg(not(debug_assertions))` in `lib.rs`, so debug
// builds would otherwise flag it as dead code.
#[cfg_attr(debug_assertions, allow(dead_code))]
pub(crate) fn start_update_check(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        match check_impl(&app).await {
            Ok(true) => log::info!("[app-update] newer version available, overlay shown"),
            Ok(false) => log::info!("[app-update] tray is up to date"),
            Err(e) => log::warn!("[app-update] startup check failed: {}", e),
        }
    });
}

/// Manual check triggered from the tray menu. Same path as the startup
/// check, but logs an explicit "already latest" so a user click always
/// produces feedback in the logs window.
pub(crate) fn check_now(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        match check_impl(&app).await {
            Ok(true) => {}
            Ok(false) => log::info!("[app-update] manual check: already on the latest version"),
            Err(e) => log::warn!("[app-update] manual check failed: {}", e),
        }
    });
}

/// Query the updater endpoint. On a hit, cache the metadata, open the
/// overlay and emit `app-update:available`. Returns whether an update was
/// found.
async fn check_impl(app: &AppHandle) -> Result<bool, String> {
    let updater = app.updater().map_err(|e| e.to_string())?;
    let Some(update) = updater.check().await.map_err(|e| e.to_string())? else {
        return Ok(false);
    };

    let info = AvailableInfo {
        current: update.current_version.clone(),
        version: update.version.clone(),
        notes: update.body.clone().unwrap_or_default(),
    };
    log::info!(
        "[app-update] update available: {} -> {}",
        info.current,
        info.version
    );

    app.state::<AppUpdateStore>().set(info.clone());
    show_update_window(app).map_err(|e| e.to_string())?;
    // Best-effort: the overlay also pulls the info via `get_app_update_info`
    // on load, so a missed event is not fatal.
    let _ = app.emit(EVENT_AVAILABLE, info);
    Ok(true)
}

/// Overlay pulls the pending update metadata on load.
#[tauri::command]
pub fn get_app_update_info(store: tauri::State<'_, AppUpdateStore>) -> Option<AvailableInfo> {
    store.get()
}

/// Download-progress payload for the overlay's progress bar.
#[derive(Clone, Serialize)]
struct Progress {
    downloaded: u64,
    total: Option<u64>,
    percent: Option<u8>,
}

/// Download, verify (minisign) and install the pending update, then
/// relaunch. Invoked by the overlay's "Install and restart" button.
///
/// We re-run `check()` here rather than storing the `Update` object across
/// the IPC boundary: the extra request is a single small `latest.json` GET
/// and keeps the command self-contained.
#[tauri::command]
pub async fn install_app_update(app: AppHandle) -> Result<(), String> {
    let updater = app.updater().map_err(|e| e.to_string())?;
    let update = updater
        .check()
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "no update available".to_string())?;

    log::info!(
        "[app-update] installing {} (from {})",
        update.version,
        update.current_version
    );

    let mut downloaded: u64 = 0;
    let app_progress = app.clone();
    update
        .download_and_install(
            move |chunk_len, content_len| {
                downloaded += chunk_len as u64;
                let percent = content_len.map(|total| {
                    if total == 0 {
                        0
                    } else {
                        ((downloaded.saturating_mul(100)) / total).min(100) as u8
                    }
                });
                let _ = app_progress.emit(
                    EVENT_PROGRESS,
                    Progress {
                        downloaded,
                        total: content_len,
                        percent,
                    },
                );
            },
            || {},
        )
        .await
        .map_err(|e| {
            let msg = e.to_string();
            log::error!("[app-update] install failed: {}", msg);
            let _ = app.emit(EVENT_ERROR, msg.clone());
            msg
        })?;

    log::info!("[app-update] install complete, relaunching");
    // `restart()` diverges (`-> !`): it re-execs the freshly installed
    // bundle and never returns, so nothing after this runs.
    app.restart();
}
