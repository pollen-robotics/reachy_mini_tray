//! `uv-trampoline` sidecar lifecycle: spawn / monitor / healthcheck / kill.
//!
//! Single source of truth for the daemon FSM transitions:
//!
//! - [`start_daemon`] / [`stop_daemon`] are the high-level entry points
//!   wired to the tray menu.
//! - [`spawn_real_daemon`] forks the trampoline and hands it off to
//!   [`monitor_daemon_output`] (async loop draining stdout/stderr/exit
//!   events).
//! - [`start_healthcheck`] polls `GET /api/daemon/status` to flip
//!   `Starting -> Running`.
//! - [`derive_bootstrap_event`] turns daemon log lines into structured
//!   `BootstrapProgress` events for the first-run window's progress bar.
//! - [`kill_daemon`] tears down the whole process group (Unix) so Python
//!   children never outlive the trampoline.

use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tauri_plugin_shell::ShellExt;

use crate::api::{local_client, DAEMON_BASE_URL};
use crate::commands::FIRST_RUN_WINDOW_LABEL;
use crate::logs;
use crate::state::{
    current_daemon_state, current_generation, current_mode, current_serialport,
    current_usb_devices, next_generation, set_daemon_state, set_serialport, AppState, DaemonState,
    Mode,
};
use crate::tray_menu::refresh_status;

/// Daemon readiness endpoint. Returns 200 once the FastAPI app, the robot
/// backend and the IO layer are all initialised. Returns 503 / connection
/// refused before that.
///
/// The daemon mounts every router under `/api/*` (see
/// `reachy_mini.daemon.app.main`); hitting `/daemon/status` without the
/// prefix returns 404 even when the daemon is live.
const HEALTHCHECK_PATH: &str = "/daemon/status";

/// Poll cadence while the daemon is in `Starting` state.
const HEALTHCHECK_INTERVAL: Duration = Duration::from_millis(500);

/// Hard timeout for reaching `Running` after a Start. Sized to cover a fresh
/// `uv-trampoline` bootstrap on a slow first-run machine (uv download ~15 s,
/// Python install ~30 s, venv + reachy-mini install ~60 s, GStreamer pre-warm
/// ~120 s, plus headroom). Subsequent starts are typically <5 s.
const HEALTHCHECK_MAX_DURATION: Duration = Duration::from_secs(300);

/// Per-request HTTP timeout used while polling `/daemon/status`.
const HEALTHCHECK_HTTP_TIMEOUT: Duration = Duration::from_secs(2);

/// Minimum delay before we trust the first healthcheck success after a
/// daemon spawn. Defends against an "eager-zombie" race: if a previous
/// daemon's Python child was orphaned (e.g. user killed the tray during a
/// crash before we could clean its pgroup), it may still be listening on
/// 8000. A fresh `Start` would otherwise see that zombie answer 200 OK
/// within milliseconds and flip the tray to "Running USB" - except the
/// real new daemon we just spawned will crash 8 s later with `address
/// already in use`. The trampoline itself takes ~1 s to fork Python, and
/// Python takes another ~7 s to boot uvicorn, so anything succeeding
/// faster than this grace period is provably not us.
const HEALTHCHECK_GRACE: Duration = Duration::from_millis(1500);

/// Tauri event emitted with `BootstrapProgress` payload while
/// `uv-trampoline` is provisioning the venv. Listened to by the first-run
/// window to drive its progress bar.
const EVENT_SETUP_PROGRESS: &str = "setup:progress";

/// Tauri event emitted once the daemon transitions to `Running`
/// (healthcheck passed). The first-run window listens for this to flip
/// into a "ready" state and auto-close itself.
const EVENT_SETUP_DONE: &str = "setup:done";

/// Sentinel `percent` value meaning "indeterminate progress, please show a
/// pulse animation". Used during long opaque steps (GStreamer plugin
/// registry scan) where we have no granular signal but want to convey that
/// the daemon is still alive.
const PROGRESS_INDETERMINATE: u8 = 255;

/// Payload emitted on `setup:progress` events. Consumed by the first-run
/// window to drive a progress bar + status label + live console.
///
/// All fields are optional so each daemon line can carry partial info:
/// - a milestone match emits `{ percent: Some(_), label: Some(_), line: Some(_) }`,
/// - a generic `[bootstrap]` line emits `{ label: Some(_), line: Some(_) }` (no
///   percent change),
/// - any other daemon line during the first-run window emits `{ line: Some(_) }`
///   only, just so the live console keeps streaming.
///
/// `percent` is otherwise monotonic on the receiving side: the HTML guards
/// against backwards moves. The special value `Some(255)` is reserved as
/// "indeterminate" (pulse animation on the bar) - used during the long
/// GStreamer plugin-registry scan where no fine-grained progress is
/// available.
#[derive(Clone, Default, Serialize)]
pub struct BootstrapProgress {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percent: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<String>,
}

// ============================================================================
// COMMAND-LINE BUILDER
// ============================================================================

/// Build the args passed to `uv-trampoline`. Mirrors
/// `reachy_mini_desktop_app::python::build_daemon_args()` minus the bits we
/// don't need (no Avast SSL wrapper on Windows because we never ship there
/// in this MVP, no `--preload-datasets` until we want the extra startup
/// time).
///
/// The trampoline interprets `args[0]` as the Python interpreter path
/// (relative to its data dir) and execs it with `args[1..]` once bootstrap
/// is complete. Hence the leading `.venv/bin/python3`.
///
/// `serialport` is only meaningful in `Mode::Usb`. When `Some(path)`, the
/// daemon is told exactly which port to open instead of running its own
/// auto-discovery (which picks the first match arbitrarily when several
/// robots are plugged in). `None` falls back to the daemon's `auto`
/// default. The arg is omitted entirely in `Mode::Simulation` since it
/// would be ignored anyway.
pub(crate) fn build_daemon_args(mode: Mode, serialport: Option<&str>) -> Vec<String> {
    #[cfg(target_os = "windows")]
    let python = ".venv\\Scripts\\python.exe";
    #[cfg(not(target_os = "windows"))]
    let python = ".venv/bin/python3";

    let mut args = vec![
        python.to_string(),
        "-m".to_string(),
        "reachy_mini.daemon.app.main".to_string(),
        // Marker flag: lets the daemon know it was spawned by a desktop app
        // wrapper (us). Used by `apps.py` for app-discovery routing.
        "--desktop-app-daemon".to_string(),
        // Service starts in "sleeping" state - no torque, no fan. The user
        // can wake the robot from a future menu item or via the daemon API.
        "--no-wake-up-on-start".to_string(),
    ];

    match mode {
        Mode::Simulation => args.push("--mockup-sim".to_string()),
        Mode::Usb => {
            if let Some(path) = serialport {
                if !path.is_empty() {
                    args.push("--serialport".to_string());
                    args.push(path.to_string());
                }
            }
        }
    }

    args
}

// ============================================================================
// SPAWN + MONITOR
// ============================================================================

/// Spawn the `uv-trampoline` sidecar for the requested connection mode.
/// Returns the live `CommandChild` (so we can `.kill()` it later) and side
/// effects: forks an async monitor task that consumes stdout/stderr/term
/// events from the child, pushes them to the in-app logs window, parses
/// bootstrap milestones for the first-run progress bar, and reacts to
/// process termination (Crashed transitions, etc).
fn spawn_real_daemon(
    app: &AppHandle,
    mode: Mode,
    serialport: Option<&str>,
    generation: u64,
) -> Result<CommandChild, String> {
    let args = build_daemon_args(mode, serialport);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    let mut cmd = app
        .shell()
        .sidecar("uv-trampoline")
        .map_err(|e| format!("sidecar lookup failed: {}", e))?
        .args(arg_refs)
        // Force UTF-8 in Python's print/logging so we never get mojibake in
        // the in-app logs window on locales where the system default isn't
        // UTF-8 (rare on macOS, common on Windows).
        .env("PYTHONIOENCODING", "utf-8");

    // Forward REACHY_CENTRAL_URL to the daemon child so it can target a
    // fork of the central signaling Space (test / staging) without
    // patching the source. Read explicitly rather than relying on
    // platform-dependent env inheritance through tauri-plugin-shell.
    if let Ok(url) = std::env::var("REACHY_CENTRAL_URL") {
        cmd = cmd.env("REACHY_CENTRAL_URL", url);
    }

    let (rx, child) = cmd
        .spawn()
        .map_err(|e| format!("sidecar spawn failed: {}", e))?;

    let app_clone = app.clone();
    tauri::async_runtime::spawn(async move {
        monitor_daemon_output(app_clone, rx, generation).await;
    });

    Ok(child)
}

/// Async loop that drains the sidecar's event stream until process
/// termination. Owns the `Receiver<CommandEvent>` produced by
/// `Command::spawn()` and is the *only* place that reacts to
/// `CommandEvent::Terminated`.
///
/// Generation guard: a stale Terminated event from a previous daemon run
/// (e.g. after a Stop -> Start sequence) must NOT crash the new one. We
/// compare the captured generation to `AppState.generation` and bail out
/// silently if they diverge.
async fn monitor_daemon_output(
    app: AppHandle,
    mut rx: tauri::async_runtime::Receiver<CommandEvent>,
    generation: u64,
) {
    while let Some(event) = rx.recv().await {
        match event {
            CommandEvent::Stdout(bytes) => {
                let line = String::from_utf8_lossy(&bytes).to_string();
                handle_daemon_line(&app, &line, "INFO");
            }
            CommandEvent::Stderr(bytes) => {
                let line = String::from_utf8_lossy(&bytes).to_string();
                // Default to WARN: most stderr noise is uvicorn / Python
                // warnings. `parse_line_level` upgrades to ERROR when an
                // explicit marker is present.
                handle_daemon_line(&app, &line, "WARN");
            }
            CommandEvent::Terminated(payload) => {
                log::info!(
                    "daemon terminated (gen={}, code={:?}, signal={:?})",
                    generation,
                    payload.code,
                    payload.signal
                );

                let app_state = app.state::<AppState>();
                if current_generation(&app_state) != generation {
                    log::debug!(
                        "ignoring stale Terminated event (captured_gen={}, current_gen={})",
                        generation,
                        current_generation(&app_state)
                    );
                    continue;
                }

                if let Ok(mut guard) = app_state.daemon.lock() {
                    guard.take();
                }

                // Only crash the FSM if we were actively running/starting.
                // If the user already clicked Stop, `kill_daemon` set state
                // to Idle and we should respect that.
                let cur = current_daemon_state(&app_state);
                if matches!(cur, DaemonState::Starting | DaemonState::Running) {
                    set_daemon_state(&app_state, DaemonState::Crashed);
                    refresh_status(&app);
                }
            }
            _ => {}
        }
    }
}

/// Single ingestion point for any line emitted by the trampoline / Python
/// daemon: pushes it to the in-app log window and (when applicable) emits
/// a `setup:progress` event for the first-run window.
///
/// While the first-run window is open, EVERY daemon line is forwarded as a
/// `setup:progress` event with at least a `line` field, so the window's
/// live mini-console keeps streaming even between known milestones (e.g.
/// during the 4-minute GStreamer plugin-registry scan, where the only
/// signal is the throttled `still working... 60s` heartbeat).
fn handle_daemon_line(app: &AppHandle, raw_line: &str, default_level: &str) {
    let trimmed = raw_line.trim_end();
    if trimmed.is_empty() {
        return;
    }
    let level = logs::parse_line_level(trimmed, default_level);
    logs::push_external(app, "daemon", &level, trimmed.to_string());

    let first_run_open = app.get_webview_window(FIRST_RUN_WINDOW_LABEL).is_some();
    if !first_run_open {
        return;
    }

    let mut progress = derive_bootstrap_event(trimmed);
    progress.line = Some(trimmed.to_string());
    if let Err(e) = app.emit(EVENT_SETUP_PROGRESS, &progress) {
        log::warn!("failed to emit setup:progress: {}", e);
    }
}

/// Heuristic mapping from `uv-trampoline` and daemon log lines to a
/// progress percentage + a humanized step label.
///
/// Design choices:
/// - The percentages are **anchors**, not a true linear progress bar. They
///   exist purely to tell the user "we're not stuck" (the hard part of
///   first-run UX is the 4 min GStreamer scan, where no useful percentage
///   exists at all).
/// - Each `[prewarm:*]` sub-step gets its own label so the user always
///   sees something change at least once per minute.
/// - Lines with no recognized marker return `BootstrapProgress::default()`
///   (no percent, no label) - the caller will still attach `line` so the
///   live console shows the raw output.
///
/// Order matters: the most specific patterns must be checked first.
pub(crate) fn derive_bootstrap_event(line: &str) -> BootstrapProgress {
    let lower = line.to_ascii_lowercase();
    let mk = |percent: u8, label: &str| BootstrapProgress {
        percent: Some(percent),
        label: Some(label.to_string()),
        line: None,
    };
    let label_only = |label: &str| BootstrapProgress {
        percent: None,
        label: Some(label.to_string()),
        line: None,
    };

    // ---------- Bootstrap milestones (uv-trampoline) ----------
    if lower.contains("setup complete!") {
        return mk(98, "Setup complete - starting daemon\u{2026}");
    }
    if lower.contains("pre-warming complete") {
        return mk(95, "Pre-warm complete");
    }
    // Pre-warm sub-steps emitted by `[prewarm:.venv]` / `[prewarm:apps_venv]`.
    // Tracked individually because the registry scan alone takes ~4 min on
    // a fresh install and a single static label would feel frozen.
    if lower.contains("reachy_mini imported") {
        return mk(94, "Reachy Mini SDK loaded");
    }
    if lower.contains("importing reachy_mini") {
        return mk(92, "Loading Reachy Mini SDK\u{2026}");
    }
    if lower.contains("gstreamer ready") {
        return mk(90, "GStreamer ready");
    }
    if lower.contains("scanning plugin registry") || lower.contains("initializing gstreamer") {
        return mk(
            PROGRESS_INDETERMINATE,
            "Scanning GStreamer plugins (first run, ~3 min)\u{2026}",
        );
    }
    if lower.contains("[prewarm") && lower.contains("importing gi") {
        return mk(82, "Pre-warming Python bindings\u{2026}");
    }
    // The trampoline's `still working...` heartbeat (every 5s during pre-warm).
    // Don't move the bar, just keep the label informative without re-overwriting
    // it with anything more specific that the renderer might already show.
    if lower.contains("pre-warming python imports (still working") {
        return BootstrapProgress::default();
    }
    if lower.contains("pre-warming gstreamer") {
        return mk(80, "Pre-warming GStreamer\u{2026}");
    }
    if lower.contains("[bootstrap] pre-warming") {
        return mk(78, "Pre-warming runtime\u{2026}");
    }
    if lower.contains("[bootstrap] signing") {
        return mk(72, "Signing Python binaries\u{2026}");
    }
    if lower.contains("packages installed successfully") {
        return mk(65, "Packages installed");
    }
    if lower.contains("creating apps_venv") {
        return mk(55, "Creating apps environment\u{2026}");
    }
    if lower.contains("installing reachy-mini") || lower.contains("installing reachy_mini") {
        return mk(40, "Installing reachy-mini and dependencies\u{2026}");
    }
    if lower.contains("creating .venv") {
        return mk(30, "Creating virtual environment\u{2026}");
    }
    if lower.contains("installing python") {
        return mk(20, "Installing Python 3.12\u{2026}");
    }
    if lower.contains("downloading uv") {
        return mk(12, "Downloading package manager (uv)\u{2026}");
    }
    if lower.contains("uv downloaded successfully") {
        return mk(15, "Package manager ready");
    }
    if lower.contains("first run detected") {
        return mk(8, "First run detected, preparing environment\u{2026}");
    }

    // ---------- Daemon-runtime hints (after `Setup complete!`) ----------
    // These don't move the bar (we're at 98 %) but give the user a sense
    // that "starting daemon..." is not stuck either.
    if lower.contains("starting reachy mini daemon") {
        return label_only("Starting daemon\u{2026}");
    }
    if lower.contains("found reachy mini serial port") {
        return label_only("Reachy Mini detected on USB");
    }
    if lower.contains("creating robotbackend") {
        return label_only("Initializing robot backend\u{2026}");
    }
    if lower.contains("uvicorn") && lower.contains("started server process") {
        return label_only("HTTP server started");
    }

    BootstrapProgress::default()
}

// ============================================================================
// HEALTHCHECK
// ============================================================================

/// Spawn a dedicated thread that polls `/daemon/status` until the daemon
/// becomes ready, fails to come up within `HEALTHCHECK_MAX_DURATION`, or its
/// generation moves on (Stop / Restart). Transitions Starting -> Running on
/// the first 200 OK and Starting -> Crashed on hard timeout.
///
/// Plain blocking `reqwest`: HTTP is loopback, latency is sub-ms, and we
/// don't want to share a tokio runtime with the sidecar event loop.
fn start_healthcheck(app: AppHandle, generation: u64) {
    std::thread::spawn(move || {
        let client = match local_client(HEALTHCHECK_HTTP_TIMEOUT) {
            Ok(c) => c,
            Err(e) => {
                log::error!("failed to build healthcheck client: {}", e);
                return;
            }
        };
        let url = format!("{}{}", DAEMON_BASE_URL, HEALTHCHECK_PATH);
        let started = Instant::now();

        loop {
            let app_state = app.state::<AppState>();

            // Generation moved on - Stop / Restart was issued. Bail out.
            if current_generation(&app_state) != generation {
                log::debug!("healthcheck thread exiting (gen mismatch)");
                return;
            }

            // FSM moved away from Starting - we're done one way or another.
            let cur = current_daemon_state(&app_state);
            if !matches!(cur, DaemonState::Starting) {
                log::debug!("healthcheck thread exiting (state={:?})", cur);
                return;
            }

            if started.elapsed() > HEALTHCHECK_MAX_DURATION {
                log::error!("healthcheck timed out after {:?}", HEALTHCHECK_MAX_DURATION);
                set_daemon_state(&app_state, DaemonState::Crashed);
                refresh_status(&app);
                return;
            }

            match client.get(&url).send() {
                Ok(resp) if resp.status().is_success() => {
                    let elapsed = started.elapsed();
                    if elapsed < HEALTHCHECK_GRACE {
                        // Almost certainly a leftover daemon from a previous
                        // run still bound to 8000. Don't trust it: stay in
                        // Starting and keep polling. Our just-spawned Python
                        // hasn't even finished booting uvicorn yet.
                        log::warn!(
                            "healthcheck answered too fast ({:?} < {:?}) - probable zombie daemon, ignoring",
                            elapsed,
                            HEALTHCHECK_GRACE
                        );
                        std::thread::sleep(HEALTHCHECK_INTERVAL);
                        continue;
                    }
                    log::info!(
                        "daemon healthcheck OK (gen={}, latency={:?})",
                        generation,
                        elapsed
                    );
                    set_daemon_state(&app_state, DaemonState::Running);
                    refresh_status(&app);
                    // Tell the first-run window (if still open) it can flip
                    // into the "ready" state and auto-close itself. Safe to
                    // emit unconditionally; the event is a no-op when the
                    // window doesn't exist.
                    if let Err(e) = app.emit(EVENT_SETUP_DONE, ()) {
                        log::debug!("failed to emit {}: {}", EVENT_SETUP_DONE, e);
                    }
                    return;
                }
                Ok(resp) => {
                    log::debug!("healthcheck non-2xx: {}", resp.status());
                }
                Err(_) => {
                    // Connection refused / not-yet-listening / DNS - silent.
                    // The daemon's own logs will surface real failures.
                }
            }

            std::thread::sleep(HEALTHCHECK_INTERVAL);
        }
    });
}

// ============================================================================
// KILL + ZOMBIE REAP
// ============================================================================

/// Kill the running daemon (if any), best-effort. Idempotent.
///
/// On Unix, the trampoline `setpgid(0, 0)`s itself at startup, so its pid
/// equals its process group id. Killing the **group** (not just the
/// trampoline) is essential to also bring down the Python child it spawned;
/// otherwise Python would survive, keep `127.0.0.1:8000` bound and the USB
/// serial port held, breaking every subsequent restart.
///
/// We send SIGTERM first to give Python ~250 ms to flush logs and unbind
/// sockets cleanly, then SIGKILL the group as a hard fallback.
pub(crate) fn kill_daemon(state: &AppState) {
    let child = match state.daemon.lock() {
        Ok(mut g) => g.take(),
        Err(e) => {
            log::error!("daemon mutex poisoned: {}", e);
            return;
        }
    };
    let Some(child) = child else { return };
    let pid = child.pid();
    log::info!("killing daemon pid={}", pid);

    #[cfg(unix)]
    {
        // Phase 1: graceful SIGTERM to the whole subtree.
        // killpg() takes the *process group id*, which - thanks to setpgid
        // in the trampoline - is the trampoline pid itself.
        kill_process_group(pid as i32, libc::SIGTERM);
        std::thread::sleep(Duration::from_millis(250));
    }

    // Phase 2: hard kill the trampoline (also delivers SIGKILL via the
    // shell-plugin's owned `tokio::process::Child` so it can reap properly
    // and we don't leak FDs from the piped stdout/stderr streams).
    if let Err(e) = child.kill() {
        log::warn!("kill failed (pid={}): {}", pid, e);
    }

    #[cfg(unix)]
    {
        // Phase 3: belt-and-braces SIGKILL on the group. Catches any
        // grandchild that survived because Python forked workers (rare,
        // but uvicorn's `--reload` mode does it).
        kill_process_group(pid as i32, libc::SIGKILL);

        // Phase 4: last-resort sweep by listening port. If the trampoline
        // was built without `setpgid` (old binary) the group kill above is a
        // no-op and Python would otherwise survive as an orphan on
        // `:8000` / `:8443`. Reaping by port catches exactly that case.
        reap_orphaned_daemons();
    }
}

/// Ports the daemon listens on. `:8000` is the HTTP API, `:8443` the
/// GStreamer WebRTC signaling server. A zombie daemon holds *both*, so we
/// sweep both to be sure we don't leave a half-dead process around.
#[cfg(unix)]
const DAEMON_LISTEN_PORTS: [&str; 2] = ["8000", "8443"];

/// Find any process currently bound to one of [`DAEMON_LISTEN_PORTS`] and
/// kill it with SIGKILL. Called in several places:
///
/// 1. At tray boot (from `lib.rs::setup`), to clean up orphans left over
///    from a previous tray that died without running its shutdown hook
///    (SIGKILL from `cargo run` rebuild, panic, `kill -9`, logout). Such
///    a trampoline gets reparented to launchd (PPID 1) and keeps its
///    Python child alive, which stays bound to `:8000` / `:8443` and keeps
///    (de)registering itself on the central relay - visible as a phantom
///    USB entry flickering in the mobile app.
/// 2. As a pre-flight in `start_daemon`, belt-and-braces in case an
///    orphan appeared between boot and the first Start click.
/// 3. As a post-kill sweep in [`kill_daemon`], in case the explicit
///    process-group kill missed the Python child (e.g. an old trampoline
///    binary built without `setpgid`, or a slow async shutdown).
///
/// All paths clean up the same class of zombies (the kind that surface
/// as `[Errno 48] address already in use` on the next daemon spawn).
///
/// We shell out to `lsof` because it's the only universally-available way
/// on macOS (no `/proc/net/tcp`) to map a TCP port to its owner pid without
/// adding a 2 MB networking crate. `lsof -nP -iTCP:8000 -iTCP:8443
/// -sTCP:LISTEN -t` prints one pid per line and exits 1 if nothing matches
/// - both are fine.
///
/// Belt-and-braces: also nukes the killed pid's process group, so if the
/// zombie was itself a parent (e.g. an old trampoline), its Python child
/// goes down too.
#[cfg(unix)]
pub(crate) fn reap_orphaned_daemons() {
    let mut lsof_args: Vec<String> = vec!["-nP".to_string()];
    for port in DAEMON_LISTEN_PORTS {
        lsof_args.push(format!("-iTCP:{}", port));
    }
    lsof_args.push("-sTCP:LISTEN".to_string());
    lsof_args.push("-t".to_string());

    let output = match std::process::Command::new("lsof")
        .args(&lsof_args)
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            log::debug!("lsof not available, skipping zombie sweep: {}", e);
            return;
        }
    };

    // A process listening on both ports shows up once per port, so dedup
    // while preserving order.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut pids: Vec<i32> = Vec::new();
    for pid in stdout.lines().filter_map(|s| s.trim().parse::<i32>().ok()) {
        if !pids.contains(&pid) {
            pids.push(pid);
        }
    }

    if pids.is_empty() {
        return;
    }

    log::warn!(
        "found {} orphan process(es) on TCP/{:?}, killing: {:?}",
        pids.len(),
        DAEMON_LISTEN_PORTS,
        pids
    );

    // Our own process group. In dev (`yarn dev`) the tray binary, the shell
    // job and a daemon child that failed to `setpgid` into its own group can
    // all share this single group. We must never `killpg` it, or we SIGKILL
    // the tray (and `yarn dev`) itself - observed as exit 137 + the app
    // closing whenever the daemon is restarted with a leftover orphan around.
    // SAFETY: getpgrp(2) is a libc syscall.
    let own_pgid = unsafe { libc::getpgrp() };

    for pid in pids {
        // SAFETY: kill(2)/killpg(2)/getpgid(2) are libc syscalls.
        unsafe {
            // First the whole group (catches forked workers) - but only if
            // it isn't our own group, otherwise we'd take the tray down too.
            let pgid = libc::getpgid(pid);
            if pgid > 1 && pgid != own_pgid {
                libc::killpg(pgid, libc::SIGKILL);
            } else if pgid == own_pgid {
                log::warn!(
                    "orphan pid={} shares the tray's process group ({}); \
                     killing the pid directly instead of the group",
                    pid,
                    pgid
                );
            }
            // Then the pid itself: always safe (it's the orphan, never us),
            // and the only thing we can do when the orphan shares our group.
            libc::kill(pid, libc::SIGKILL);
        }
    }

    // Give the OS ~200 ms to actually release the listening socket before
    // the caller tries to bind to it. Without this, the next `uvicorn`
    // start can still race-lose the port.
    std::thread::sleep(Duration::from_millis(200));
}

/// `killpg(pgid, sig)` wrapper that defends against the most embarrassing
/// failure mode: pgid == 0 ("our own process group"), which would kill the
/// tray app itself. Returns silently on any error.
#[cfg(unix)]
fn kill_process_group(pgid: i32, sig: i32) {
    if pgid <= 1 {
        log::warn!("refusing killpg with pgid={}", pgid);
        return;
    }
    // Refuse to nuke our own process group. In dev (`yarn dev`) a trampoline
    // whose `setpgid(0, 0)` didn't take effect stays in the tray's group;
    // killpg() on it would SIGKILL the tray itself. The caller has already
    // `.kill()`ed the trampoline child directly, so skipping the group here
    // only loses the (rare) forked-worker reap, not the main kill.
    // SAFETY: getpgrp(2) is a libc syscall.
    let own_pgid = unsafe { libc::getpgrp() };
    if pgid == own_pgid {
        log::warn!(
            "refusing killpg on the tray's own process group ({})",
            pgid
        );
        return;
    }
    // SAFETY: killpg(2) is a libc syscall, no Rust invariants to uphold.
    let rc = unsafe { libc::killpg(pgid, sig) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        // ESRCH "no such process group" is the happy path on the second
        // call (everything is already dead). Don't spam the log for it.
        if err.raw_os_error() != Some(libc::ESRCH) {
            log::debug!("killpg(pgid={}, sig={}) failed: {}", pgid, sig, err);
        }
    }
}

// ============================================================================
// HIGH-LEVEL START / STOP
// ============================================================================

pub(crate) fn start_daemon(app: &AppHandle) {
    let app_state = app.state::<AppState>();
    if matches!(
        current_daemon_state(&app_state),
        DaemonState::Starting | DaemonState::Running
    ) {
        log::info!("daemon already busy, ignoring Start");
        return;
    }

    // Safety net: refuse to launch in USB mode when no Reachy is
    // physically connected. The daemon would just crash with
    // `"No Reachy Mini serial port found"` and the central relay
    // would never register, leaving the user with a confusing
    // "daemon crashed" state and a robot invisible from the mobile
    // app. We auto-downgrade to Simulation instead so the daemon
    // stays usable, and we log the override loudly so the user
    // understands why the target switched. The menu is already
    // structured to prevent the user from picking USB explicitly
    // when no device is detected (see `build_target_submenu`); this
    // guard catches the edge case where a stale `Mode::Usb` was
    // persisted from a previous run with a robot plugged in.
    {
        let devices = current_usb_devices(&app_state);
        let mode_now = current_mode(&app_state);
        if matches!(mode_now, Mode::Usb) && devices.is_empty() {
            log::warn!(
                "USB target requested but no Reachy detected; \
                 auto-falling back to Simulation"
            );
            if let Ok(mut guard) = app_state.mode.lock() {
                *guard = Mode::Simulation;
            }
            // Drop the cached serialport too: it would be a stale
            // path that no longer matches anything plugged in, and
            // we don't want a future hot-plug to silently re-engage
            // USB on the next start without an explicit user pick.
            set_serialport(&app_state, None);
        }
    }

    let mode = current_mode(&app_state);
    let serialport = current_serialport(&app_state);

    // Pre-flight: kill any pre-existing daemon left over from a crash or a
    // previous version of the tray that didn't use process-group cleanup.
    // Without this, our just-spawned Python would die with `address already
    // in use` on port 8000 ~8 s after the user clicks Start.
    #[cfg(unix)]
    reap_orphaned_daemons();

    set_daemon_state(&app_state, DaemonState::Starting);
    let gen = next_generation(&app_state);
    refresh_status(app);

    match spawn_real_daemon(app, mode, serialport.as_deref(), gen) {
        Ok(child) => {
            let pid = child.pid();
            log::info!(
                "daemon spawned pid={} mode={} serialport={} gen={}",
                pid,
                mode.as_str(),
                serialport.as_deref().unwrap_or("auto"),
                gen
            );
            if let Ok(mut guard) = app_state.daemon.lock() {
                *guard = Some(child);
            }
        }
        Err(e) => {
            log::error!("failed to spawn daemon: {}", e);
            set_daemon_state(&app_state, DaemonState::Crashed);
            refresh_status(app);
            return;
        }
    }

    // Real readiness probe: poll `GET /daemon/status` every
    // HEALTHCHECK_INTERVAL until 200 OK, max HEALTHCHECK_MAX_DURATION.
    // Generation guard inside the thread filters out stale signals after
    // Stop / Restart sequences.
    start_healthcheck(app.clone(), gen);
}

pub(crate) fn stop_daemon(app: &AppHandle) {
    let app_state = app.state::<AppState>();
    next_generation(&app_state);
    kill_daemon(&app_state);
    set_daemon_state(&app_state, DaemonState::Idle);
    refresh_status(app);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_daemon_args_usb_has_no_mockup() {
        let args = build_daemon_args(Mode::Usb, None);
        assert!(args.iter().any(|a| a == "--desktop-app-daemon"));
        assert!(args.iter().any(|a| a == "--no-wake-up-on-start"));
        assert!(!args.iter().any(|a| a == "--mockup-sim"));
        assert!(!args.iter().any(|a| a == "--serialport"));
        // The trampoline interprets args[0] as the python interpreter path.
        let py = &args[0];
        if cfg!(target_os = "windows") {
            assert!(py.ends_with("python.exe"));
        } else {
            assert!(py.ends_with("python3"));
        }
        assert_eq!(args[1], "-m");
        assert_eq!(args[2], "reachy_mini.daemon.app.main");
    }

    #[test]
    fn build_daemon_args_simulation_adds_mockup() {
        let args = build_daemon_args(Mode::Simulation, None);
        assert!(args.iter().any(|a| a == "--mockup-sim"));
    }

    #[test]
    fn build_daemon_args_usb_injects_explicit_serialport() {
        let args = build_daemon_args(Mode::Usb, Some("/dev/cu.usbserial-2120"));
        let idx = args
            .iter()
            .position(|a| a == "--serialport")
            .expect("--serialport flag missing");
        assert_eq!(
            args.get(idx + 1).map(String::as_str),
            Some("/dev/cu.usbserial-2120")
        );
    }

    #[test]
    fn build_daemon_args_simulation_ignores_serialport() {
        // In sim mode we never want `--serialport` even if a user-selected
        // port is still cached from a previous USB session: the daemon
        // would happily try to open it during `--mockup-sim` startup.
        let args = build_daemon_args(Mode::Simulation, Some("/dev/cu.usbserial-2120"));
        assert!(args.iter().any(|a| a == "--mockup-sim"));
        assert!(!args.iter().any(|a| a == "--serialport"));
    }

    #[test]
    fn build_daemon_args_usb_skips_empty_serialport() {
        // Defensive: if a UI ever passes an empty string we treat it the
        // same as `None` (don't pass `--serialport ""` to the daemon).
        let args = build_daemon_args(Mode::Usb, Some(""));
        assert!(!args.iter().any(|a| a == "--serialport"));
    }

    #[test]
    fn derive_bootstrap_event_anchors_milestones() {
        let p = derive_bootstrap_event("[bootstrap] Downloading uv 0.4.0");
        assert_eq!(p.percent, Some(12));
        assert!(p.label.is_some());

        let p = derive_bootstrap_event("Setup complete!");
        assert_eq!(p.percent, Some(98));

        let p = derive_bootstrap_event("Pre-warming complete (apps_venv)");
        assert_eq!(p.percent, Some(95));
    }

    #[test]
    fn derive_bootstrap_event_indeterminate_for_gstreamer_scan() {
        let p = derive_bootstrap_event("Scanning plugin registry, please wait...");
        assert_eq!(p.percent, Some(PROGRESS_INDETERMINATE));
    }

    #[test]
    fn derive_bootstrap_event_unknown_line_yields_default() {
        let p = derive_bootstrap_event("totally generic uvicorn output");
        assert!(p.percent.is_none());
        assert!(p.label.is_none());
        assert!(p.line.is_none());
    }

    #[test]
    fn derive_bootstrap_event_runtime_hints_have_label_no_percent() {
        let p = derive_bootstrap_event("INFO:reachy_mini.daemon: Starting Reachy Mini daemon");
        assert!(p.percent.is_none());
        assert!(p.label.is_some());
    }

    #[test]
    fn derive_bootstrap_event_specificity_order() {
        // "Setup complete!" must beat the bare "starting daemon" runtime
        // hint when both literally appear (defensive against a future
        // log-format change that could put them on the same line).
        let p = derive_bootstrap_event("Setup complete! Starting Reachy Mini daemon now");
        assert_eq!(p.percent, Some(98));
    }
}
