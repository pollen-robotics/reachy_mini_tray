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

/// macOS: bring the app + `window` to the foreground when opening a real,
/// user-facing window (logs / first-run).
///
/// The tray runs as an `Accessory` agent (no Dock icon, never the active
/// app), so a freshly shown window silently opens *behind* whatever app is
/// focused - the user never sees it. Promoting to `Regular` lets the app
/// take focus like any normal app (a Dock icon appears while a window is
/// open, which is exactly what users expect); we drop back to `Accessory`
/// once the last such window closes via [`demote_if_no_user_windows`].
#[cfg(target_os = "macos")]
fn present_window(app: &AppHandle, window: &tauri::WebviewWindow) {
    let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
    let _ = window.show();
    let _ = window.set_focus();
}

/// macOS: revert to menu-bar-agent mode (no Dock icon) once no user-facing
/// window remains open. The update overlay is intentionally excluded: it is
/// `always_on_top` and shows fine under `Accessory`, so it must not keep the
/// Dock icon alive.
#[cfg(target_os = "macos")]
fn demote_if_no_user_windows(app: &AppHandle) {
    let has_user_window = app.get_webview_window(LOGS_WINDOW_LABEL).is_some()
        || app.get_webview_window(FIRST_RUN_WINDOW_LABEL).is_some();
    if !has_user_window {
        let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);
    }
}

/// macOS: wire a `Destroyed` handler that demotes the app back to
/// `Accessory` when this window closes. No-op on other platforms.
#[cfg(target_os = "macos")]
fn wire_demote_on_close(app: &AppHandle, window: &tauri::WebviewWindow) {
    let app = app.clone();
    window.on_window_event(move |event| {
        if matches!(event, tauri::WindowEvent::Destroyed) {
            demote_if_no_user_windows(&app);
        }
    });
}

pub(crate) fn show_first_run_window(app: &AppHandle) -> tauri::Result<()> {
    if let Some(existing) = app.get_webview_window(FIRST_RUN_WINDOW_LABEL) {
        #[cfg(target_os = "macos")]
        present_window(app, &existing);
        #[cfg(not(target_os = "macos"))]
        {
            existing.show()?;
            existing.set_focus()?;
        }
        return Ok(());
    }

    let window = WebviewWindowBuilder::new(
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

    #[cfg(target_os = "macos")]
    {
        present_window(app, &window);
        wire_demote_on_close(app, &window);
    }
    Ok(())
}

pub(crate) fn show_logs_window(app: &AppHandle) -> tauri::Result<()> {
    if let Some(existing) = app.get_webview_window(LOGS_WINDOW_LABEL) {
        #[cfg(target_os = "macos")]
        present_window(app, &existing);
        #[cfg(not(target_os = "macos"))]
        {
            existing.show()?;
            existing.set_focus()?;
        }
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

    let window = builder.build()?;

    #[cfg(target_os = "macos")]
    {
        present_window(app, &window);
        wire_demote_on_close(app, &window);
    }
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
    let app = app.clone();
    // Everything - the existence check, the show/focus of an already-open
    // overlay, and the build of a new one - runs inside this single
    // main-thread closure. This is deliberate:
    //   1. `show_update_window` is called from the updater's async check
    //      task, and AppKit window ops (`show`/`set_focus`/`build`) must run
    //      on the main thread on macOS.
    //   2. Doing the `get_webview_window` check *inside* the closure (rather
    //      than at call time) serialises concurrent callers - a startup
    //      check and a manual "Check for updates" firing together can't race
    //      into building two overlays, because the second closure sees the
    //      window the first one created.
    app.clone().run_on_main_thread(move || {
        if let Some(existing) = app.get_webview_window(UPDATE_WINDOW_LABEL) {
            let _ = existing.show();
            let _ = existing.set_focus();
            return;
        }

        let builder = WebviewWindowBuilder::new(
            &app,
            UPDATE_WINDOW_LABEL,
            WebviewUrl::App("update.html".into()),
        )
        .title("Reachy Mini Tray - Update")
        .inner_size(460.0, 468.0)
        .min_inner_size(460.0, 468.0)
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
