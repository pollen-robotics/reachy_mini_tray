//! Tray icon + tooltip + dynamic menu refresh.
//!
//! The menu is rebuilt from scratch on every state change rather than
//! mutated in place. This is required because the "Hugging Face account"
//! slot needs to flip between a flat `MenuItem` ("Sign in...") and a
//! `Submenu` ("@user · remote on") - a topology change `muda` doesn't
//! support cleanly. Refreshes are gated by the `last_signature` check in
//! `hf_auth::start_status_poller` and by daemon FSM transitions, so we
//! don't actually rebuild the menu on every poll tick.

use tauri::menu::{
    CheckMenuItem, IconMenuItem, Menu, MenuItem, NativeIcon, PredefinedMenuItem, Submenu,
};
use tauri::{AppHandle, Manager, Wry};

use crate::daemon_update::{self, UpdateSnapshot};
use crate::hf_auth;
use crate::menu_icons;
use crate::state::{
    current_daemon_state, current_mode, current_serialport, current_usb_devices, AppState,
    DaemonState, IconCache, Mode,
};

pub(crate) const TRAY_ID: &str = "main";

/// Always-disabled informational row at the very top of the tray menu.
/// Carries a colored status dot (`StatusAvailable` / `StatusPartiallyAvailable`
/// / `StatusUnavailable` / `StatusNone`) plus a sentence describing the
/// daemon's FSM state. Mirrors the "Docker Desktop is running" header in
/// Docker's macOS tray menu.
///
/// The id is wired up only so we can reference the row by a stable string
/// in tests / tooling - clicks on a disabled item are no-ops.
pub(crate) const ID_STATUS: &str = "status";
pub(crate) const ID_TOGGLE: &str = "toggle";
pub(crate) const ID_TARGET_SUBMENU: &str = "target";
/// Prefix for individual USB-device menu items. The full ID is
/// `target:usb:<index>` where `<index>` is the position of the device
/// in `AppState.usb_devices` at menu build time. Indexing rather than
/// embedding the serialport keeps muda IDs free of slashes / colons.
pub(crate) const ID_TARGET_USB_PREFIX: &str = "target:usb:";
pub(crate) const ID_TARGET_SIM: &str = "target:sim";
pub(crate) const ID_TARGET_NO_USB: &str = "target:none";
/// Top-level submenu shown when the user is logged in. Its label is the
/// live account status (e.g. `@tfrere · remote on`); children are the
/// secondary actions (Reconnect, Sign out).
pub(crate) const ID_ACCOUNT_SUBMENU: &str = "account";
/// Flat top-level item shown when the user is logged out (or while OAuth
/// is in flight). Click triggers `hf_auth::start_oauth_flow`.
pub(crate) const ID_ACCOUNT_SIGNIN: &str = "account_signin";
pub(crate) const ID_ACCOUNT_SIGNOUT: &str = "account_signout";
pub(crate) const ID_ACCOUNT_REFRESH_RELAY: &str = "account_refresh_relay";
pub(crate) const ID_SHOW_LOGS: &str = "show_logs";
pub(crate) const ID_RESET_SETUP: &str = "reset_setup";
/// Shown only when a newer daemon release is available (or an upgrade is in
/// flight). Click triggers `daemon_update::start_update`.
pub(crate) const ID_UPDATE_DAEMON: &str = "update_daemon";
pub(crate) const ID_QUIT: &str = "quit";

/// Top-level entry point: re-render icon, tooltip and menu from the live
/// `AppState` + `AuthStatusStore` snapshot. Cheap; safe to spam.
pub(crate) fn refresh_status(app: &AppHandle) {
    let app_state = app.state::<AppState>();
    let state = current_daemon_state(&app_state);
    let mode = current_mode(&app_state);

    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };

    // ---- Icon ----
    let cache = app.state::<IconCache>();
    let (icon, template) = match (state, mode) {
        (DaemonState::Idle, _) => (cache.idle.clone(), true),
        (DaemonState::Starting, _) => (cache.starting.clone(), false),
        (DaemonState::Running, Mode::Usb) => (cache.running_usb.clone(), false),
        (DaemonState::Running, Mode::Simulation) => (cache.running_sim.clone(), false),
        (DaemonState::Crashed, _) => (cache.crashed.clone(), false),
    };
    if let Err(e) = tray.set_icon(Some(icon)) {
        log::warn!("set_icon failed: {}", e);
    }
    if let Err(e) = tray.set_icon_as_template(template) {
        log::warn!("set_icon_as_template failed: {}", e);
    }

    // ---- Tooltip ----
    //
    // We append the short port name when the user picked a specific USB
    // device, so hovering the tray icon already reveals which Reachy
    // Mini is targeted without expanding the menu.
    let serialport = current_serialport(&app_state);
    let target_label = compose_target_label(mode, serialport.as_deref());
    let tooltip = match state {
        DaemonState::Idle => format!("Reachy Mini - Idle ({})", target_label),
        DaemonState::Starting => format!("Reachy Mini - Starting ({})...", target_label),
        DaemonState::Running => format!("Reachy Mini - Running ({})", target_label),
        DaemonState::Crashed => format!("Reachy Mini - Crashed ({})", target_label),
    };
    if let Err(e) = tray.set_tooltip(Some(&tooltip)) {
        log::warn!("set_tooltip failed: {}", e);
    }

    // ---- Menu ----
    let snap = app
        .try_state::<hf_auth::AuthStatusStore>()
        .map(|s| s.snapshot())
        .unwrap_or_default();
    let update = app
        .try_state::<daemon_update::DaemonUpdateStore>()
        .map(|s| s.snapshot())
        .unwrap_or_default();
    let devices = current_usb_devices(&app_state);
    match build_tray_menu(
        app,
        state,
        mode,
        serialport.as_deref(),
        &devices,
        &snap,
        &update,
    ) {
        Ok(menu) => {
            if let Err(e) = tray.set_menu(Some(menu)) {
                log::warn!("set_menu failed: {}", e);
            }
        }
        Err(e) => log::warn!("build_tray_menu failed: {}", e),
    }
}

/// Compose a short user-facing target label like `USB · cu.usbserial-2120`
/// or `Simulation`, used in the tooltip and the `Robot` submenu title.
///
/// In USB mode without a selected port we report `USB (auto)` so the
/// user understands the daemon will run its own discovery.
fn compose_target_label(mode: Mode, serialport: Option<&str>) -> String {
    match mode {
        Mode::Simulation => "Simulation".to_string(),
        Mode::Usb => match serialport {
            Some(path) if !path.is_empty() => {
                let short = path.strip_prefix("/dev/").unwrap_or(path);
                format!("USB \u{00b7} {}", short)
            }
            _ => "USB (auto)".to_string(),
        },
    }
}

/// Re-renders the live tray menu (icon, tooltip, all items) on demand.
/// Called by background workers in `hf_auth` after fetching a new snapshot
/// or completing an OAuth flow. Cheap; safe to spam.
pub fn request_menu_refresh(app: &AppHandle) {
    refresh_status(app);
}

/// Possible shapes for the "Hugging Face account" slot in the tray menu.
/// Each tray refresh re-derives this from `(state, snap)` and rebuilds
/// the menu accordingly. Topology changes (flat MenuItem <-> Submenu) are
/// not expressible via `set_text` alone.
enum AccountSlot {
    /// Daemon not running: no row at all.
    Hidden,
    /// Logged out (or OAuth in flight). Flat top-level `MenuItem`.
    Flat(MenuItem<Wry>),
    /// Logged in. Submenu whose label is the live account status; its
    /// children are the secondary actions.
    Sub(Submenu<Wry>),
}

/// Build the user-visible label for the "logged in" submenu.
///
/// Format: `@{user} · {relay summary}`. The relay summary is intentionally
/// short so the submenu title fits in a typical macOS tray menu width.
fn logged_in_label(snap: &hf_auth::AuthSnapshot) -> String {
    let user = snap
        .auth
        .username
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("?");

    let relay = match snap.relay.as_ref() {
        Some(r) if r.is_connected => "remote on".to_string(),
        Some(r) => match r.state.as_str() {
            "connecting" | "reconnecting" => "reconnecting\u{2026}".to_string(),
            "unavailable" => "remote unavailable".to_string(),
            "" => "remote unknown".to_string(),
            other => format!("remote {}", other),
        },
        None => "remote unknown".to_string(),
    };

    format!("@{} \u{00b7} {}", user, relay)
}

/// Pick the right account slot for the current `(state, auth snapshot)`.
fn account_slot(
    app: &AppHandle,
    state: DaemonState,
    snap: &hf_auth::AuthSnapshot,
) -> tauri::Result<AccountSlot> {
    if !matches!(state, DaemonState::Running) {
        return Ok(AccountSlot::Hidden);
    }

    if snap.oauth_in_flight {
        // Disabled flat row that doubles as a status indicator while the
        // OAuth round-trip is in flight in the user's browser.
        let item = MenuItem::with_id(
            app,
            ID_ACCOUNT_SIGNIN,
            "Signing in\u{2026} (check your browser)",
            false,
            None::<&str>,
        )?;
        return Ok(AccountSlot::Flat(item));
    }

    if !snap.auth.is_logged_in {
        let item = MenuItem::with_id(
            app,
            ID_ACCOUNT_SIGNIN,
            "Sign in with Hugging Face\u{2026}",
            true,
            None::<&str>,
        )?;
        return Ok(AccountSlot::Flat(item));
    }

    let refresh_relay = MenuItem::with_id(
        app,
        ID_ACCOUNT_REFRESH_RELAY,
        "Reconnect remote access",
        true,
        None::<&str>,
    )?;
    let signout = MenuItem::with_id(app, ID_ACCOUNT_SIGNOUT, "Sign out", true, None::<&str>)?;
    let label = logged_in_label(snap);
    let sub = Submenu::with_id_and_items(
        app,
        ID_ACCOUNT_SUBMENU,
        &label,
        true,
        &[&refresh_relay, &signout],
    )?;
    Ok(AccountSlot::Sub(sub))
}

/// Build a fresh tray menu reflecting the given
/// `(state, mode, serialport, usb_devices, snap)`.
pub(crate) fn build_tray_menu(
    app: &AppHandle,
    state: DaemonState,
    mode: Mode,
    serialport: Option<&str>,
    usb_devices: &[crate::usb::UsbDevice],
    snap: &hf_auth::AuthSnapshot,
    update: &UpdateSnapshot,
) -> tauri::Result<Menu<Wry>> {
    // ---- Status row (always disabled, top of menu) ----
    //
    // Mirrors Docker Desktop's "Docker Desktop is running" header: a
    // permanent informational row at the very top of the menu, with a
    // colored native dot icon that matches the tray icon's tint. Lets
    // the user read the daemon's FSM state without having to map the
    // tray-icon tint to a meaning.
    let status_row = build_status_row(app, state)?;

    // ---- Toggle (Start / Stop / Restart) ----
    //
    // Icon sourcing:
    //   - Idle     -> custom play glyph   (▶, filled triangle)
    //   - Starting -> NativeIcon::RefreshFreestanding (busy / spinner)
    //   - Running  -> custom stop glyph   (■, filled rounded square)
    //   - Crashed  -> NativeIcon::Refresh (↻ retry)
    //
    // Play / Stop are baked procedurally in `menu_icons` because the
    // native `RightFacingTriangle` reads as a thin chevron at the menu
    // -item icon size, which gives a "next page" affordance rather than
    // the "press to start" play-button cue users expect from Docker
    // Desktop, Spotify, etc. The custom glyphs match SF Symbols'
    // `play.fill` / `stop.fill` weight and read clearly at 22 pt.
    let toggle = match state {
        DaemonState::Idle => IconMenuItem::with_id(
            app,
            ID_TOGGLE,
            "Start daemon",
            true,
            Some(menu_icons::play_icon()),
            None::<&str>,
        )?,
        DaemonState::Running => IconMenuItem::with_id(
            app,
            ID_TOGGLE,
            "Stop daemon",
            true,
            Some(menu_icons::stop_icon()),
            None::<&str>,
        )?,
        DaemonState::Starting => IconMenuItem::with_id_and_native_icon(
            app,
            ID_TOGGLE,
            "Starting\u{2026}",
            false,
            Some(NativeIcon::RefreshFreestanding),
            None::<&str>,
        )?,
        DaemonState::Crashed => IconMenuItem::with_id_and_native_icon(
            app,
            ID_TOGGLE,
            "Restart daemon",
            true,
            Some(NativeIcon::Refresh),
            None::<&str>,
        )?,
    };

    // ---- Robot submenu ----
    let busy = matches!(state, DaemonState::Starting | DaemonState::Running);
    let target_submenu = build_target_submenu(app, mode, serialport, usb_devices, !busy)?;

    // ---- Account slot ----
    let account = account_slot(app, state, snap)?;

    // ---- Footer ----
    let show_logs = MenuItem::with_id(app, ID_SHOW_LOGS, "Show logs\u{2026}", true, None::<&str>)?;
    // Reset setup wipes the daemon's data dir; doing it while the daemon
    // is `Starting` or `Running` would race against open file handles
    // (venv binaries currently exec'd, sqlite locks, serial port, etc.).
    // Only allow it from `Idle` / `Crashed`.
    let reset_setup = MenuItem::with_id(
        app,
        ID_RESET_SETUP,
        "Reset setup\u{2026}",
        !busy,
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(app, ID_QUIT, "Quit", true, None::<&str>)?;

    // ---- Daemon update row ----
    //
    // Rendered only when the version poller (`daemon_update`) found a newer
    // daemon release than the one installed in `.venv`, or while a
    // user-triggered upgrade is running. Absent otherwise so the menu stays
    // clean on an up-to-date install.
    let update_row: Option<IconMenuItem<Wry>> = if update.updating {
        Some(IconMenuItem::with_id_and_native_icon(
            app,
            ID_UPDATE_DAEMON,
            "Updating daemon\u{2026}",
            false,
            Some(NativeIcon::RefreshFreestanding),
            None::<&str>,
        )?)
    } else if update.available() {
        let label = match (update.installed, update.latest) {
            (Some(i), Some(l)) => format!(
                "Update daemon ({} \u{2192} {})",
                daemon_update::fmt_version(i),
                daemon_update::fmt_version(l)
            ),
            _ => "Update daemon".to_string(),
        };
        Some(IconMenuItem::with_id_and_native_icon(
            app,
            ID_UPDATE_DAEMON,
            &label,
            true,
            Some(NativeIcon::Refresh),
            None::<&str>,
        )?)
    } else {
        None
    };

    // Predefined separators are cheap and `muda` requires distinct
    // instances per insertion site. Build all we might need up-front.
    let sep_status = PredefinedMenuItem::separator(app)?;
    let sep_top = PredefinedMenuItem::separator(app)?;
    let sep_account = PredefinedMenuItem::separator(app)?;
    let sep_update = PredefinedMenuItem::separator(app)?;
    let sep_footer = PredefinedMenuItem::separator(app)?;
    let sep_quit = PredefinedMenuItem::separator(app)?;

    let mut items: Vec<&dyn tauri::menu::IsMenuItem<Wry>> =
        vec![&status_row, &sep_status, &toggle, &sep_top, &target_submenu];

    match &account {
        AccountSlot::Flat(item) => {
            items.push(&sep_account);
            items.push(item);
        }
        AccountSlot::Sub(sub) => {
            items.push(&sep_account);
            items.push(sub);
        }
        AccountSlot::Hidden => {}
    }

    // Update row sits in its own section just above the footer so it reads
    // as an action on the daemon, not part of the account block.
    if let Some(row) = update_row.as_ref() {
        items.push(&sep_update);
        items.push(row);
    }

    items.push(&sep_footer);
    items.push(&show_logs);
    items.push(&reset_setup);
    items.push(&sep_quit);
    items.push(&quit);

    Menu::with_items(app, &items)
}

/// Build the always-disabled status row at the top of the tray menu.
///
/// The native icon is one of the four `Status*` colored dots, which on
/// macOS render as small circles tinted by the system. Combined with a
/// short sentence ("Daemon is running" / etc.), the row gives the user
/// a one-glance read on the daemon's FSM state, doubling the signal
/// already carried by the tray icon's tint - on a multi-tray menu bar
/// that signal is otherwise easy to lose.
fn build_status_row(app: &AppHandle, state: DaemonState) -> tauri::Result<IconMenuItem<Wry>> {
    let (text, icon) = match state {
        DaemonState::Idle => ("Daemon is idle", NativeIcon::StatusNone),
        DaemonState::Starting => (
            "Daemon is starting\u{2026}",
            NativeIcon::StatusPartiallyAvailable,
        ),
        DaemonState::Running => ("Daemon is running", NativeIcon::StatusAvailable),
        DaemonState::Crashed => ("Daemon has crashed", NativeIcon::StatusUnavailable),
    };
    IconMenuItem::with_id_and_native_icon(
        app,
        ID_STATUS,
        text,
        // Disabled: purely informational. Clicks are a no-op so we
        // don't need a router entry for `ID_STATUS`.
        false,
        Some(icon),
        None::<&str>,
    )
}

/// Build the `Robot` submenu, dynamically populated with the detected
/// USB devices and a Simulation fallback.
///
/// Layout:
///
/// - When at least one USB Reachy is plugged in: one CheckMenuItem per
///   device (selected when both `Mode::Usb` is active **and** the
///   selected serialport matches), then a separator, then a
///   `Simulation` CheckMenuItem.
/// - When no USB device is detected: a single disabled "No Reachy Mini
///   detected (plug USB)" row, a separator, and the `Simulation` row.
///   The user can still launch in sim mode.
///
/// The submenu's title carries a short summary of the current target
/// (e.g. `Robot: USB · cu.usbserial-2120`) so the user can see the
/// selection without expanding the submenu.
fn build_target_submenu(
    app: &AppHandle,
    mode: Mode,
    serialport: Option<&str>,
    usb_devices: &[crate::usb::UsbDevice],
    enabled: bool,
) -> tauri::Result<Submenu<Wry>> {
    // Build all leaf items first so they outlive the borrow we need
    // when collecting them into a slice for `Submenu::with_id_and_items`.
    let mut usb_items: Vec<CheckMenuItem<Wry>> = Vec::with_capacity(usb_devices.len());
    for (idx, dev) in usb_devices.iter().enumerate() {
        let id = format!("{}{}", ID_TARGET_USB_PREFIX, idx);
        let checked = matches!(mode, Mode::Usb) && serialport == Some(dev.serialport.as_str());
        let item = CheckMenuItem::with_id(app, id, &dev.label, true, checked, None::<&str>)?;
        usb_items.push(item);
    }

    let no_device_row = if usb_devices.is_empty() {
        Some(MenuItem::with_id(
            app,
            ID_TARGET_NO_USB,
            "No Reachy Mini detected (plug USB)",
            // Disabled - purely informational. Clicking it is a no-op.
            false,
            None::<&str>,
        )?)
    } else {
        None
    };

    let sim_item = CheckMenuItem::with_id(
        app,
        ID_TARGET_SIM,
        "Simulation",
        true,
        matches!(mode, Mode::Simulation),
        None::<&str>,
    )?;
    let sep = PredefinedMenuItem::separator(app)?;

    let title = format!("Robot: {}", compose_target_label(mode, serialport));

    let mut children: Vec<&dyn tauri::menu::IsMenuItem<Wry>> = Vec::new();
    if let Some(row) = no_device_row.as_ref() {
        children.push(row);
    }
    for item in usb_items.iter() {
        children.push(item);
    }
    children.push(&sep);
    children.push(&sim_item);

    Submenu::with_id_and_items(app, ID_TARGET_SUBMENU, &title, enabled, &children)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_label_simulation_is_constant() {
        assert_eq!(compose_target_label(Mode::Simulation, None), "Simulation");
        // Even when a stale serialport is still in state, sim mode wins.
        assert_eq!(
            compose_target_label(Mode::Simulation, Some("/dev/cu.usbserial-2120")),
            "Simulation"
        );
    }

    #[test]
    fn target_label_usb_with_port_strips_dev_prefix() {
        assert_eq!(
            compose_target_label(Mode::Usb, Some("/dev/cu.usbserial-2120")),
            "USB \u{00b7} cu.usbserial-2120"
        );
    }

    #[test]
    fn target_label_usb_without_port_says_auto() {
        assert_eq!(compose_target_label(Mode::Usb, None), "USB (auto)");
        assert_eq!(compose_target_label(Mode::Usb, Some("")), "USB (auto)");
    }

    #[test]
    fn target_label_usb_with_windows_port() {
        // Windows ports like COM3 don't have a /dev/ prefix; the label
        // should pass them through untouched.
        assert_eq!(
            compose_target_label(Mode::Usb, Some("COM3")),
            "USB \u{00b7} COM3"
        );
    }
}
