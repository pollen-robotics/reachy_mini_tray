//! Reachy Mini tray launcher.
//!
//! Cross-platform (macOS / Windows / Linux) Tauri 2 app whose only UI is a
//! tray icon + two transient webview windows (first-run setup + log viewer).
//!
//! What this crate owns:
//!
//! - Tray icon, dynamic menu, live status indicator (idle / starting /
//!   running / crashed), connection mode toggle (USB / Simulation).
//! - Lifecycle of the `uv-trampoline` sidecar (spawn / monitor / kill with
//!   process-group cleanup) and an HTTP healthcheck on
//!   `GET http://127.0.0.1:8000/api/daemon/status` to detect readiness.
//! - First-run window driven by `setup:progress` events derived from the
//!   trampoline's stdout milestones.
//! - Hugging Face account submenu (sign in / out / reconnect remote),
//!   delegating all OAuth + token storage to the daemon.
//! - Single-instance lock so two trays can't fight over the daemon.
//!
//! Module map:
//!
//! - [`state`]: shared FSM, `AppState`, accessors, `IconCache`.
//! - [`daemon`]: sidecar spawn / monitor / healthcheck / kill.
//! - [`tray_icon`]: pre-rendered status badges composed onto the bot face.
//! - [`tray_menu`]: dynamic menu construction + `refresh_status` entry point.
//! - [`commands`]: webview window helpers and Tauri IPC commands.
//! - [`hf_auth`]: Hugging Face OAuth orchestrator + status poller.
//! - [`daemon_update`]: GitHub-release version check + in-place venv upgrade.
//! - [`api`]: daemon base URL + shared `reqwest` client factory.
//! - [`logs`]: in-memory ring-buffer logger.
//! - [`paths`]: data-dir layout (shared with `reachy_mini_desktop_app`).
//! - [`usb`]: enumerate / filter Reachy Mini USB-serial devices.
//!
//! Explicitly out of scope: tray-app auto-update (the Tauri bundle updater),
//! autostart-at-login, system sleep/wake reconciliation, Windows / Linux
//! code-signing pipelines. Note that *daemon* (Python `reachy-mini`) updates
//! ARE in scope - see [`daemon_update`].

mod api;
mod commands;
mod daemon;
mod daemon_update;
mod hf_auth;
mod logs;
mod menu_icons;
mod paths;
mod state;
mod tray_icon;
mod tray_menu;
mod usb;

use std::sync::atomic::Ordering;

use tauri::tray::TrayIconBuilder;
use tauri::{Manager, RunEvent};

use crate::commands::{show_first_run_window, show_logs_window};
use crate::daemon::{kill_daemon, start_daemon, stop_daemon};
use crate::logs::LogStore;
use crate::state::{
    current_daemon_state, current_mode, current_usb_devices, set_daemon_state, set_serialport,
    AppState, DaemonState, Mode, QUIT_REQUESTED,
};
use crate::tray_icon::build_icon_cache;
use crate::tray_menu::{
    build_tray_menu, refresh_status, ID_ACCOUNT_REFRESH_RELAY, ID_ACCOUNT_SIGNIN,
    ID_ACCOUNT_SIGNOUT, ID_ACCOUNT_SUBMENU, ID_QUIT, ID_RESET_SETUP, ID_SHOW_LOGS, ID_TARGET_SIM,
    ID_TARGET_USB_PREFIX, ID_TOGGLE, ID_UPDATE_DAEMON, TRAY_ID,
};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    logs::init();

    tauri::Builder::default()
        // Single-instance MUST be the first plugin registered (per the
        // plugin's documentation), otherwise rival processes can grab
        // resources before the lock is acquired. The callback fires in the
        // *first* (alive) instance whenever a second one is launched - we
        // log it and surface the existing process by opening the logs window.
        .plugin(tauri_plugin_single_instance::init(|app, args, cwd| {
            log::warn!(
                "single-instance: rejected duplicate launch (args={:?}, cwd={})",
                args,
                cwd
            );
            if let Err(e) = show_logs_window(app) {
                log::warn!("single-instance: failed to surface logs window: {}", e);
            }
        }))
        .plugin(tauri_plugin_shell::init())
        .manage(AppState::new())
        .manage(LogStore::new())
        .manage(hf_auth::AuthStatusStore::new())
        .manage(daemon_update::DaemonUpdateStore::new())
        .invoke_handler(tauri::generate_handler![
            commands::close_first_run_window,
            commands::get_logs,
            commands::clear_logs
        ])
        .setup(|app| {
            let app_handle = app.handle().clone();

            // Wire the global logger to the running app so subsequent
            // log records also feed the in-app logs window.
            logs::bind_app_handle(&app_handle);

            // Pre-flight orphan sweep.
            //
            // If the previous tray died without passing through
            // `RunEvent::ExitRequested` (SIGKILL from `cargo run` rebuild,
            // panic, `kill -9`, forced logout), its `uv-trampoline`
            // sidecar survives adoption by launchd (PPID becomes 1) and
            // keeps the Python daemon alive. That zombie stays bound to
            // `127.0.0.1:8000`, keeps holding the USB serial port, and
            // keeps re-registering itself on the central relay - which
            // surfaces as a phantom USB entry flickering in the mobile
            // app (15 s TTL sweep vs. orphan re-registration).
            //
            // The single-instance plugin above only prevents a second
            // *tray* from launching; it does nothing about surviving
            // daemons. `start_daemon` also calls `reap_orphaned_daemons`
            // right before spawning, but that only helps if the user
            // clicks Start - an orphan can live indefinitely between
            // tray relaunches otherwise. Sweeping here closes that gap.
            #[cfg(unix)]
            daemon::reap_orphaned_daemons();

            // ---- Initial tray menu ----
            //
            // The full menu is rebuilt on every `refresh_status` call, so
            // this is just the boot-time snapshot (Idle daemon + USB +
            // empty auth). Topology and labels will adjust as soon as
            // state changes.
            let initial_snap = hf_auth::AuthSnapshot::default();
            let initial_update = daemon_update::UpdateSnapshot::default();
            let menu = build_tray_menu(
                &app.handle().clone(),
                DaemonState::Idle,
                Mode::Usb,
                None,
                &[],
                &initial_snap,
                &initial_update,
            )?;

            // ---- Icon cache ----
            let base_icon = app
                .default_window_icon()
                .ok_or("missing default window icon")?
                .clone();
            let cache = build_icon_cache(&base_icon);
            let initial_icon = cache.idle.clone();
            app.manage(cache);

            // ---- Tray icon ----
            TrayIconBuilder::with_id(TRAY_ID)
                .icon(initial_icon)
                .icon_as_template(true)
                .tooltip("Reachy Mini - Idle (USB)")
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(move |app, event| match event.id.as_ref() {
                    ID_TOGGLE => {
                        // A daemon upgrade stops/starts the daemon and mutates
                        // the venv it runs from; a concurrent Start/Stop would
                        // race the in-flight `uv pip install`. Ignore until the
                        // upgrade thread clears the flag.
                        if app.state::<daemon_update::DaemonUpdateStore>().updating() {
                            log::info!("toggle ignored: daemon upgrade in progress");
                            return;
                        }
                        let app_state = app.state::<AppState>();
                        match current_daemon_state(&app_state) {
                            DaemonState::Idle | DaemonState::Crashed => start_daemon(app),
                            DaemonState::Running => stop_daemon(app),
                            // Disabled while Starting; defensive no-op.
                            DaemonState::Starting => {}
                        }
                    }
                    ID_UPDATE_DAEMON => {
                        daemon_update::start_update(app);
                    }
                    ID_TARGET_SIM => {
                        let app_state = app.state::<AppState>();
                        if matches!(
                            current_daemon_state(&app_state),
                            DaemonState::Starting | DaemonState::Running
                        ) {
                            log::info!("ignored target change: daemon busy");
                            return;
                        }
                        if let Ok(mut guard) = app_state.mode.lock() {
                            *guard = Mode::Simulation;
                        }
                        // Drop any cached serialport: in sim mode we don't
                        // want a stale path lingering in state, otherwise
                        // a future menu refresh could mis-render the
                        // selected device.
                        set_serialport(&app_state, None);
                        log::info!("target set to Simulation");
                        refresh_status(app);
                    }
                    other if other.starts_with(ID_TARGET_USB_PREFIX) => {
                        let app_state = app.state::<AppState>();
                        if matches!(
                            current_daemon_state(&app_state),
                            DaemonState::Starting | DaemonState::Running
                        ) {
                            log::info!("ignored target change: daemon busy");
                            return;
                        }
                        // Resolve `target:usb:<index>` against the cached
                        // device list. Indexes are stable for the lifetime
                        // of a single menu render: the menu is rebuilt on
                        // every refresh from the same `usb_devices` list,
                        // so a click while the menu is open always lands
                        // on the right device.
                        let idx = other[ID_TARGET_USB_PREFIX.len()..].parse::<usize>().ok();
                        let devices = current_usb_devices(&app_state);
                        let Some(dev) = idx.and_then(|i| devices.get(i)).cloned() else {
                            log::warn!("USB target id {} did not resolve to a known device", other);
                            return;
                        };
                        if let Ok(mut guard) = app_state.mode.lock() {
                            *guard = Mode::Usb;
                        }
                        set_serialport(&app_state, Some(dev.serialport.clone()));
                        log::info!("target set to USB ({})", dev.serialport);
                        refresh_status(app);
                    }
                    ID_ACCOUNT_SIGNIN => {
                        hf_auth::start_oauth_flow(app.clone());
                    }
                    ID_ACCOUNT_SIGNOUT => {
                        hf_auth::sign_out(app);
                    }
                    ID_ACCOUNT_REFRESH_RELAY => {
                        hf_auth::refresh_relay(app);
                    }
                    ID_ACCOUNT_SUBMENU => {
                        // The "@user · remote on" item is a Submenu; macOS
                        // routes its click to its children, but on some
                        // platforms / older muda builds the submenu itself
                        // can fire a hover event. Defensive no-op.
                    }
                    ID_SHOW_LOGS => {
                        if let Err(e) = show_logs_window(app) {
                            log::warn!("failed to show logs window: {}", e);
                        }
                    }
                    ID_RESET_SETUP => {
                        // Wiping the venv mid-upgrade would race the in-flight
                        // `uv pip install`; defer until it completes.
                        if app.state::<daemon_update::DaemonUpdateStore>().updating() {
                            log::info!("reset ignored: daemon upgrade in progress");
                            return;
                        }
                        // Stop the daemon first, otherwise removing the venv
                        // it depends on yields confusing errors in the logs.
                        let app_state = app.state::<AppState>();
                        kill_daemon(&app_state);
                        set_daemon_state(&app_state, DaemonState::Idle);
                        refresh_status(app);
                        match paths::reset_bootstrap() {
                            Ok(()) => log::info!("venv wiped; relaunching first-run setup"),
                            Err(e) => log::warn!("failed to wipe venv: {}", e),
                        }
                        if let Err(e) = show_first_run_window(app) {
                            log::warn!("failed to show first-run window: {}", e);
                        }
                        // Kick the bootstrap immediately, same as the
                        // first-launch path. Without this the venv stays
                        // empty and the first-run window sits at 0% forever
                        // until the user manually clicks Start daemon -
                        // surprising UX for a "Reset" action.
                        start_daemon(app);
                    }
                    ID_QUIT => {
                        log::info!("quit requested");
                        QUIT_REQUESTED.store(true, Ordering::SeqCst);
                        let app_state = app.state::<AppState>();
                        kill_daemon(&app_state);
                        app.exit(0);
                    }
                    other => log::warn!("unknown menu event: {}", other),
                })
                .build(app)?;

            // ---- HF account status poller ----
            //
            // Single long-lived blocking thread that polls the daemon's
            // `/api/hf-auth/status` and `/api/hf-auth/relay-status` while
            // it's `Running` and refreshes the account submenu labels.
            // Idles (slower cadence, no HTTP) when the daemon is down.
            hf_auth::start_status_poller(app_handle.clone());

            // ---- Daemon update poller ----
            //
            // Long-lived thread that periodically compares the installed
            // `reachy-mini` version (read from `.venv`) against the latest
            // GitHub release, surfacing an "Update daemon" row in the tray
            // menu when the install is behind. Fully fail-open (offline /
            // rate-limited -> row simply stays hidden).
            daemon_update::start_update_poller(app_handle.clone());

            // ---- USB device scanner ----
            //
            // Synchronous initial scan (so `refresh_status` further down
            // already renders the right device list) then a background
            // thread takes over to react to hot-plug / unplug.
            let initial_changed = usb::scan_and_apply(&app_handle);
            let initial_count = current_usb_devices(&app_handle.state::<AppState>()).len();
            log::info!(
                "USB scan: found {} Reachy Mini device(s) (changed={})",
                initial_count,
                initial_changed
            );
            // Boot-time mode reconciliation.
            //
            // The mode in `AppState` defaults to `Mode::Usb` (and a
            // future revision could persist the user's last choice).
            // If we land at boot with no Reachy plugged in but the
            // mode says USB, we MUST downgrade to Simulation - else
            // an auto-start (first-launch path below, or `start_daemon`
            // wired to `Toggle`) would crash the daemon with "No
            // Reachy Mini serial port found". The fallback is
            // unconditional now: previously we restricted it to
            // first-launch, but the same crash happens on subsequent
            // launches when the user unplugged the robot before
            // quitting the tray.
            //
            // Re-engaging USB requires an explicit user pick from
            // the Robot submenu (which only exposes USB rows when a
            // device is detected), so this fallback can never strand
            // a connected robot in sim mode.
            if initial_count == 0 {
                let app_state = app.state::<AppState>();
                let was_usb = matches!(current_mode(&app_state), Mode::Usb);
                if was_usb {
                    if let Ok(mut guard) = app_state.mode.lock() {
                        *guard = Mode::Simulation;
                    }
                    log::info!(
                        "no Reachy detected at boot: defaulting to Simulation \
                         (plug a robot in and pick it from the Robot submenu \
                         to switch back to USB)"
                    );
                    // The mode change above happened after `scan_and_apply`
                    // already requested a refresh (or didn't, if cache was
                    // unchanged); force an explicit refresh so the menu
                    // reflects the new mode immediately.
                    refresh_status(&app_handle);
                }
            }
            usb::start_scanner(app_handle.clone());

            // ---- First-run window + auto-bootstrap ----
            //
            // Detection rule mirrors `uv_wrapper::venv_exists()` from the
            // desktop app: if `.venv/bin/python3` exists in the shared data
            // dir, bootstrap is considered complete and we boot straight
            // into the tray (idle state, daemon not started; the user picks
            // when to launch it from the menu).
            //
            // A partial bootstrap (interrupted by quit/crash/power loss)
            // leaves no Python binary, so the next launch automatically
            // restarts setup. No sentinel file is needed.
            //
            // On first launch we auto-call `start_daemon()`: the same
            // `uv-trampoline` binary that runs the Python module is also
            // responsible for bootstrapping the venv when missing. So a
            // first-run "Start" implicitly downloads uv, installs Python,
            // creates the venv, installs reachy-mini and finally starts
            // the daemon - all observable through the progress bar in the
            // first-run window via `setup:progress` events.
            if paths::is_bootstrap_done() {
                log::info!(
                    "bootstrap detected, skipping setup window ({})",
                    paths::venv_python_path()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "<no path>".into())
                );
            } else {
                log::info!(
                    "first launch detected, opening setup window (data dir: {})",
                    paths::data_dir()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "<no path>".into())
                );
                show_first_run_window(&app_handle)?;
                // Kick off the bootstrap immediately. Default mode is USB;
                // the user can switch to Simulation later from the tray
                // menu (which will trigger a restart of the daemon).
                start_daemon(&app_handle);
            }

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| match event {
            // Block the default "exit when last window is closed" behaviour.
            // The tray must keep running until the user explicitly clicks Quit.
            RunEvent::ExitRequested { api, code, .. } => {
                if QUIT_REQUESTED.load(Ordering::SeqCst) || code.is_some() {
                    log::info!("exit confirmed - cleaning up daemon");
                    let state = app_handle.state::<AppState>();
                    kill_daemon(&state);
                } else {
                    log::debug!("exit request intercepted - tray stays alive");
                    api.prevent_exit();
                }
            }
            // macOS: when the user closes the last window via the red dot,
            // do NOT terminate the process. Tray + LSUIElement keep us
            // headless.
            #[cfg(target_os = "macos")]
            RunEvent::Reopen { .. } => {
                log::debug!("reopen event received");
            }
            _ => {}
        });
}
