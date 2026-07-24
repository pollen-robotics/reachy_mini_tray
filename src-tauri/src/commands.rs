//! Webview window helpers and Tauri IPC commands.
//!
//! The tray app only ever opens two webview windows:
//!
//! - `first-run` (`index.html`): shown on first launch and after `Reset
//!   setup…`. Drives the bootstrap progress bar via `setup:progress` /
//!   `setup:done` events.
//! - `logs` (`logs.html`): on-demand log viewer that tails the in-memory
//!   ring buffer maintained by [`crate::logs`].
//!
//! IPC commands here are exclusively trivial getters / dismissals invoked
//! from those two windows.

use tauri::{AppHandle, Manager, State, WebviewUrl, WebviewWindowBuilder};

use crate::logs::{LogEntry, LogStore};

pub(crate) const FIRST_RUN_WINDOW_LABEL: &str = "first-run";
pub(crate) const LOGS_WINDOW_LABEL: &str = "logs";
pub(crate) const UPDATE_WINDOW_LABEL: &str = "update";

pub(crate) fn show_first_run_window(app: &AppHandle) -> tauri::Result<()> {
    if let Some(existing) = app.get_webview_window(FIRST_RUN_WINDOW_LABEL) {
        existing.show()?;
        existing.set_focus()?;
        return Ok(());
    }

    WebviewWindowBuilder::new(
        app,
        FIRST_RUN_WINDOW_LABEL,
        WebviewUrl::App("index.html".into()),
    )
    .title("Reachy Mini - First-time setup")
    .inner_size(520.0, 460.0)
    .min_inner_size(440.0, 380.0)
    .resizable(true)
    .center()
    .visible(true)
    .build()?;
    Ok(())
}

pub(crate) fn show_logs_window(app: &AppHandle) -> tauri::Result<()> {
    if let Some(existing) = app.get_webview_window(LOGS_WINDOW_LABEL) {
        existing.show()?;
        existing.set_focus()?;
        return Ok(());
    }

    let builder =
        WebviewWindowBuilder::new(app, LOGS_WINDOW_LABEL, WebviewUrl::App("logs.html".into()))
            .title("Reachy mini tray logs")
            // The window hosts both the 3D viewer (left) and the logs pane (right);
            // a wider default + larger minimum keeps both panes legible at boot.
            .inner_size(1200.0, 640.0)
            .min_inner_size(600.0, 320.0)
            .resizable(true)
            .center()
            .visible(true);

    // macOS: let the webview extend under the traffic lights and drop the
    // native title text so the app can draw its own integrated top bar.
    #[cfg(target_os = "macos")]
    let builder = builder
        .title_bar_style(tauri::TitleBarStyle::Overlay)
        .hidden_title(true);

    builder.build()?;
    Ok(())
}

/// Open (or focus) the blocking self-update overlay.
///
/// The overlay is a small, centered, always-on-top window that nags the
/// user to install a newly published tray release. It is opened from the
/// updater's async check task, so window creation is dispatched onto the
/// main thread (Tauri requires webview windows to be built there on some
/// platforms).
pub(crate) fn show_update_window(app: &AppHandle) -> tauri::Result<()> {
    if let Some(existing) = app.get_webview_window(UPDATE_WINDOW_LABEL) {
        existing.show()?;
        existing.set_focus()?;
        return Ok(());
    }

    let app = app.clone();
    let app_build = app.clone();
    app.run_on_main_thread(move || {
        // Re-check after hopping threads: a concurrent check could have
        // created the window in the meantime.
        if app_build.get_webview_window(UPDATE_WINDOW_LABEL).is_some() {
            return;
        }

        let builder = WebviewWindowBuilder::new(
            &app_build,
            UPDATE_WINDOW_LABEL,
            WebviewUrl::App("update.html".into()),
        )
        .title("Reachy Mini - Update")
        .inner_size(460.0, 360.0)
        .min_inner_size(460.0, 360.0)
        .resizable(false)
        .center()
        .always_on_top(true)
        .visible(true);

        // macOS: draw under the traffic lights and hide the native title so
        // the overlay reads as a single flat card, matching the other windows.
        #[cfg(target_os = "macos")]
        let builder = builder
            .title_bar_style(tauri::TitleBarStyle::Overlay)
            .hidden_title(true);

        if let Err(e) = builder.build() {
            log::warn!("failed to build update window: {}", e);
        }
    })?;

    Ok(())
}

#[tauri::command]
pub fn close_first_run_window(app: AppHandle) {
    // Click on Done is just a UI dismissal. The "bootstrap is done" signal
    // is the presence of `.venv/bin/python3` on disk (see
    // `paths::is_bootstrap_done`), written by `uv-trampoline` once the venv
    // is fully provisioned. Closing the window early without a complete
    // venv simply means the next launch reopens it.
    if let Some(win) = app.get_webview_window(FIRST_RUN_WINDOW_LABEL) {
        let _ = win.close();
    }
}

#[tauri::command]
pub fn get_logs(store: State<'_, LogStore>) -> Vec<LogEntry> {
    store.snapshot()
}

#[tauri::command]
pub fn clear_logs(store: State<'_, LogStore>) {
    store.clear();
}
