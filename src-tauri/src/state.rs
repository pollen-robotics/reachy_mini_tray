//! Shared application state and lifecycle types.
//!
//! Holds the daemon FSM (`DaemonState`), the current connection mode
//! (`Mode`), the per-restart generation counter, the pre-rendered tray icon
//! cache (`IconCache`) and the tray-only `QUIT_REQUESTED` flag.
//!
//! ## Lock ordering
//!
//! `AppState` exposes several `Mutex`es. None of the current call sites
//! holds two of them at once, but if a future change ever needs to, locks
//! MUST be acquired in this order to avoid deadlocks:
//!
//! 1. `state.daemon`
//! 2. `state.mode`
//! 3. `state.serialport`
//! 4. `state.usb_devices`
//! 5. `state.state`
//! 6. `state.crash_cause`
//! 7. `state.generation`
//!
//! Accessors below each take a single lock and release it before returning,
//! so external callers should generally not need to lock anything by hand.

use std::sync::atomic::AtomicBool;
use std::sync::Mutex;
use std::time::Instant;

use tauri::image::Image;
use tauri_plugin_shell::process::CommandChild;

use crate::usb::UsbDevice;

/// Set to `true` only when the user explicitly chooses Quit from the tray
/// menu. All other exit requests (last window closed, Cmd+Q on first-run
/// window, ...) are intercepted and cancelled to keep the tray alive.
pub(crate) static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);

/// User-selected connection mode. Drives `--mockup-sim` on the daemon
/// command line and the colour of the tray status badge.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    Usb,
    Simulation,
}

impl Mode {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Mode::Usb => "usb",
            Mode::Simulation => "simulation",
        }
    }
}

/// Daemon lifecycle as projected to the user. Maps loosely to the FSM in
/// `reachy_mini_desktop_app::daemon::DaemonStatus` (Idle / Starting /
/// Running / Stopping / Crashed) - "Stopping" is collapsed into Idle for
/// tray UX since kill is fast (<200 ms in practice).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DaemonState {
    Idle,
    Starting,
    Running,
    Crashed,
}

/// How we ended up in [`DaemonState::Crashed`]. Only consulted while the
/// FSM reads `Crashed`; drives the status-row wording ("failed to start"
/// vs "stopped unexpectedly") so the user knows whether their launch never
/// took off or a healthy daemon died under them.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CrashCause {
    /// The daemon never reached `Running`: spawn failure, exit during
    /// startup, startup-watchdog timeout, or the first-run offline abort.
    Startup,
    /// A previously `Running` daemon terminated on its own.
    Runtime,
}

/// Tauri-managed app state. Everything mutable that outlives a single
/// command call lives here.
pub struct AppState {
    /// Handle to the running `uv-trampoline` sidecar. `None` when daemon is
    /// idle / crashed / not-yet-started. We never hold this lock across a
    /// `.kill()` call: callers `take()` first then drop the guard.
    pub daemon: Mutex<Option<CommandChild>>,
    pub mode: Mutex<Mode>,
    /// User-selected USB serial port to pass to the daemon as
    /// `--serialport <path>`. `None` means "let the daemon auto-detect"
    /// (its default). Only consulted when `mode` is `Mode::Usb`.
    pub serialport: Mutex<Option<String>>,
    /// Last cached snapshot of the USB-attached Reachy Minis we found
    /// (refreshed periodically by the USB scanner thread while the
    /// daemon is `Idle` / `Crashed`). The tray menu reads this to
    /// render the per-device rows.
    pub usb_devices: Mutex<Vec<UsbDevice>>,
    pub state: Mutex<DaemonState>,
    /// Why the last transition to `Crashed` happened. Written right before
    /// `set_daemon_state(Crashed)` at every crash site; stale values are
    /// harmless because it is only read while the FSM is `Crashed`.
    pub crash_cause: Mutex<Option<CrashCause>>,
    /// Monotonically increases each time we (re)start a daemon. Used to
    /// discard late healthcheck / monitor callbacks from a previous run
    /// (e.g. user clicked Stop while bootstrap was still in progress).
    pub generation: Mutex<u64>,
    /// Startup liveness tracking for the healthcheck watchdog. Mirrors the
    /// desktop app's activity-based startup timeout: the deadline is measured
    /// from the *last daemon output line*, not from spawn time, and the
    /// budget is extended while a first-run bootstrap is streaming.
    pub startup_activity: Mutex<StartupActivity>,
}

/// Snapshot of daemon output liveness while `Starting`.
pub struct StartupActivity {
    /// Instant of the last stdout/stderr line from the trampoline/daemon
    /// (initialised to spawn time so a fully silent process still times out).
    pub last_output: Instant,
    /// `true` between the first `[bootstrap]` line and `Setup complete`.
    pub bootstrapping: bool,
}

impl AppState {
    pub(crate) fn new() -> Self {
        Self {
            daemon: Mutex::new(None),
            mode: Mutex::new(Mode::Usb),
            serialport: Mutex::new(None),
            usb_devices: Mutex::new(Vec::new()),
            state: Mutex::new(DaemonState::Idle),
            crash_cause: Mutex::new(None),
            generation: Mutex::new(0),
            startup_activity: Mutex::new(StartupActivity {
                last_output: Instant::now(),
                bootstrapping: false,
            }),
        }
    }
}

/// Pre-rendered tray icons. Generated once in `setup()` from the bundled
/// default icon. `Image<'static>` is cheap to clone (just a slice +
/// dimensions); the underlying RGBA bytes are leaked one-shot at startup.
pub struct IconCache {
    pub idle: Image<'static>,
    pub starting: Image<'static>,
    pub running_usb: Image<'static>,
    pub running_sim: Image<'static>,
    pub crashed: Image<'static>,
}

// ----- Accessors -----
//
// All accessors take a single lock and release it before returning. They
// favour a sane fallback over panicking on poisoning, except for
// `next_generation` / `current_generation` where a wrong reading would
// silently break the stale-event guard - panicking there surfaces a real
// bug instead of letting it corrupt user state.

pub(crate) fn current_mode(state: &AppState) -> Mode {
    state.mode.lock().map(|g| *g).unwrap_or(Mode::Usb)
}

pub(crate) fn current_serialport(state: &AppState) -> Option<String> {
    state.serialport.lock().ok().and_then(|g| g.clone())
}

/// Replace the user-selected USB serialport. `None` reverts to the
/// daemon's `auto` mode.
pub(crate) fn set_serialport(state: &AppState, path: Option<String>) {
    if let Ok(mut g) = state.serialport.lock() {
        *g = path;
    }
}

pub(crate) fn current_usb_devices(state: &AppState) -> Vec<UsbDevice> {
    state
        .usb_devices
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default()
}

/// Replace the cached USB-device list. Returns `true` if the new list
/// differs from the previous one, so callers can short-circuit menu
/// rebuilds when nothing changed.
pub(crate) fn set_usb_devices(state: &AppState, devices: Vec<UsbDevice>) -> bool {
    let mut guard = match state.usb_devices.lock() {
        Ok(g) => g,
        Err(_) => return false,
    };
    if *guard == devices {
        return false;
    }
    *guard = devices;
    true
}

pub(crate) fn current_daemon_state(state: &AppState) -> DaemonState {
    state.state.lock().map(|g| *g).unwrap_or(DaemonState::Idle)
}

pub(crate) fn set_daemon_state(state: &AppState, new_state: DaemonState) {
    if let Ok(mut g) = state.state.lock() {
        *g = new_state;
    }
}

/// Record why the daemon is about to be (or was just) marked `Crashed`.
pub(crate) fn set_crash_cause(state: &AppState, cause: CrashCause) {
    if let Ok(mut g) = state.crash_cause.lock() {
        *g = Some(cause);
    }
}

pub(crate) fn current_crash_cause(state: &AppState) -> Option<CrashCause> {
    state.crash_cause.lock().ok().and_then(|g| *g)
}

/// Record fresh daemon output (any stdout/stderr line). Called on the
/// monitor thread; the healthcheck thread reads it via `startup_snapshot`.
pub(crate) fn note_daemon_output(state: &AppState) {
    if let Ok(mut g) = state.startup_activity.lock() {
        g.last_output = Instant::now();
    }
}

pub(crate) fn set_bootstrapping(state: &AppState, bootstrapping: bool) {
    if let Ok(mut g) = state.startup_activity.lock() {
        g.bootstrapping = bootstrapping;
    }
}

/// Reset activity tracking at spawn time (fresh deadline, no bootstrap).
pub(crate) fn reset_startup_activity(state: &AppState) {
    if let Ok(mut g) = state.startup_activity.lock() {
        g.last_output = Instant::now();
        g.bootstrapping = false;
    }
}

/// `(last_output, bootstrapping)` snapshot for the healthcheck watchdog.
pub(crate) fn startup_snapshot(state: &AppState) -> (Instant, bool) {
    state
        .startup_activity
        .lock()
        .map(|g| (g.last_output, g.bootstrapping))
        .unwrap_or((Instant::now(), false))
}

pub(crate) fn next_generation(state: &AppState) -> u64 {
    let mut guard = state.generation.lock().expect("generation mutex");
    *guard = guard.wrapping_add(1);
    *guard
}

pub(crate) fn current_generation(state: &AppState) -> u64 {
    *state.generation.lock().expect("generation mutex")
}
