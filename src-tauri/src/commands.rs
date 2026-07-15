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
