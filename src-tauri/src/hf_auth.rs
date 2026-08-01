//! Hugging Face account integration.
//!
//! All real OAuth work lives in the Python daemon
//! (`reachy_mini.daemon.app.routers.hf_auth`). This module is just a thin
//! orchestrator:
//!
//! - polls the daemon for `/status` + `/relay-status` while the daemon is
//!   Running, so the tray menu always shows accurate "Signed in as @user"
//!   + "Remote access" labels;
//! - drives the OAuth dance by calling `/oauth/start`, opening the
//!   returned `auth_url` in the system browser (the daemon owns the
//!   `localhost:8000/api/hf-auth/oauth/callback` redirect), and polling
//!   `/oauth/status/{sid}` until the user finishes (or times out);
//! - exposes Sign Out (DELETE `/api/hf-auth/token`) and Reconnect to HF
//!   central (POST `/api/hf-auth/refresh-relay`) wrappers.
//!
//! No token is ever stored on the tray side. The daemon writes/reads
//! `~/.cache/huggingface/token` itself; the tray only ever sees usernames
//! and connection states.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tauri::{AppHandle, Manager};
use tauri_plugin_shell::ShellExt;

use crate::api::{local_client, DAEMON_BASE_URL};
use crate::state::{current_daemon_state, AppState, DaemonState};
use crate::tray_menu::request_menu_refresh;

/// Sub-prefix appended to [`crate::api::DAEMON_BASE_URL`] for every HF
/// route. Must match the FastAPI router prefix in
/// `reachy_mini.daemon.app.main`.
pub(crate) const HF_AUTH_PATH: &str = "/hf-auth";

const HTTP_TIMEOUT: Duration = Duration::from_secs(3);

fn hf_url(suffix: &str) -> String {
    format!("{}{}{}", DAEMON_BASE_URL, HF_AUTH_PATH, suffix)
}

/// Cadence at which we poll `/status` and `/relay-status` while the daemon
/// is `Running`.
///
/// The daemon's `/api/hf-auth/status` calls `huggingface_hub.whoami()` on
/// every request with no caching, which means each poll round-trips to
/// `huggingface.co/api/whoami-v2`. Polling too fast triggers HF's rate
/// limiter (HTTP 429); the daemon then catches the exception and returns
/// `is_logged_in: false`, which makes the tray flap back to "logged out"
/// even though the token is still perfectly valid. 15 s gives HF plenty
/// of headroom while still feeling live to the user.
const POLL_INTERVAL_RUNNING: Duration = Duration::from_secs(15);

/// Right after a successful OAuth, a sign-out or a refresh-relay we want
/// the menu to reflect the new state quickly without waiting for the next
/// 15-second tick. We override the cadence to this faster value for a
/// short burst (`POLL_BURST_DURATION`).
const POLL_INTERVAL_BURST: Duration = Duration::from_secs(2);

/// How long the post-event burst window stays open. Long enough for
/// `whoami-v2` rate-limits to clear (~10 s on a fresh start) but short
/// enough that the daemon doesn't get hammered indefinitely.
const POLL_BURST_DURATION: Duration = Duration::from_secs(20);

const POLL_INTERVAL_IDLE: Duration = Duration::from_secs(5);

/// Consecutive `is_logged_in: false` poll responses required before the
/// poller believes a logout (defense against transient whoami-v2 429s).
const LOGOUT_CONFIRM_TICKS: u32 = 2;

/// Hard cap for the OAuth poll loop. The daemon's session itself expires
/// after 600 s (`hf_auth.create_oauth_session` -> `expires_in`).
const OAUTH_POLL_INTERVAL: Duration = Duration::from_secs(1);
const OAUTH_POLL_MAX: Duration = Duration::from_secs(600);

// ============================================================================
// PUBLIC STATE
// ============================================================================

/// Daemon-reported authentication status. Cached in `AuthStatusStore` and
/// consumed by the tray menu builder (`tray_menu::refresh_status`) to render
/// the account submenu labels live.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct AuthStatus {
    #[serde(default)]
    pub is_logged_in: bool,
    #[serde(default)]
    pub username: Option<String>,
}

/// Daemon-reported central signaling relay status.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct RelayStatus {
    #[serde(default)]
    pub state: String,
    /// Human-readable detail kept around so future menu rows / log lines
    /// can surface it; intentionally not rendered yet.
    #[serde(default)]
    #[allow(dead_code)]
    pub message: Option<String>,
    #[serde(default)]
    pub is_connected: bool,
}

/// Snapshot consumed by the tray menu. We deliberately keep the daemon-side
/// types (`AuthStatus`, `RelayStatus`) flat and `Default` so a missing /
/// failed call lands as a sane "no info" state (`is_logged_in: false`,
/// empty relay state) rather than crashing the menu render.
#[derive(Clone, Debug, Default)]
pub struct AuthSnapshot {
    pub auth: AuthStatus,
    pub relay: Option<RelayStatus>,
    /// `true` while an OAuth flow is in flight. Drives the "Sign in\u{2026}"
    /// button into a disabled "Signing in\u{2026}" state.
    pub oauth_in_flight: bool,
}

pub struct AuthStatusStore {
    inner: Mutex<AuthSnapshot>,
    oauth_running: AtomicBool,
    /// Monotonic deadline (`Instant`) up to which the poller should run at
    /// `POLL_INTERVAL_BURST`. Stored as nanoseconds since the poller's
    /// own start instant via a `Mutex<Option<Instant>>` keeps things
    /// straightforward.
    burst_until: Mutex<Option<Instant>>,
}

impl AuthStatusStore {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(AuthSnapshot::default()),
            oauth_running: AtomicBool::new(false),
            burst_until: Mutex::new(None),
        }
    }

    pub fn snapshot(&self) -> AuthSnapshot {
        let mut snap = self.inner.lock().map(|g| g.clone()).unwrap_or_default();
        snap.oauth_in_flight = self.oauth_running.load(Ordering::SeqCst);
        snap
    }

    fn set_auth(&self, auth: AuthStatus) {
        if let Ok(mut g) = self.inner.lock() {
            g.auth = auth;
        }
    }

    fn set_relay(&self, relay: Option<RelayStatus>) {
        if let Ok(mut g) = self.inner.lock() {
            g.relay = relay;
        }
    }

    fn try_acquire_oauth(&self) -> bool {
        self.oauth_running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    fn release_oauth(&self) {
        self.oauth_running.store(false, Ordering::SeqCst);
    }

    /// Open a burst-polling window so the next `POLL_BURST_DURATION` worth
    /// of poller iterations run at the faster `POLL_INTERVAL_BURST`. Used
    /// after OAuth / sign-out / refresh-relay to make the tray menu
    /// reflect the new state in seconds, not on the next 15 s tick.
    fn trigger_burst(&self) {
        if let Ok(mut g) = self.burst_until.lock() {
            *g = Some(Instant::now() + POLL_BURST_DURATION);
        }
    }

    fn in_burst_window(&self) -> bool {
        match self.burst_until.lock() {
            Ok(g) => g.map(|t| Instant::now() < t).unwrap_or(false),
            Err(_) => false,
        }
    }
}

// ============================================================================
// HTTP CLIENT
// ============================================================================

fn http_client() -> Result<reqwest::blocking::Client, String> {
    local_client(HTTP_TIMEOUT)
}

#[derive(Deserialize)]
struct OauthStartResponse {
    #[serde(default)]
    status: String,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    auth_url: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Deserialize)]
struct OauthStatusResponse {
    #[serde(default)]
    status: String,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

fn fetch_auth_status(client: &reqwest::blocking::Client) -> Result<AuthStatus, String> {
    let url = hf_url("/status");
    let resp = client
        .get(&url)
        .send()
        .map_err(|e| format!("GET {}: {}", url, e))?;
    if !resp.status().is_success() {
        return Err(format!("GET {} -> {}", url, resp.status()));
    }
    resp.json::<AuthStatus>()
        .map_err(|e| format!("decode auth status: {}", e))
}

fn fetch_relay_status(client: &reqwest::blocking::Client) -> Result<RelayStatus, String> {
    let url = hf_url("/relay-status");
    let resp = client
        .get(&url)
        .send()
        .map_err(|e| format!("GET {}: {}", url, e))?;
    if !resp.status().is_success() {
        return Err(format!("GET {} -> {}", url, resp.status()));
    }
    resp.json::<RelayStatus>()
        .map_err(|e| format!("decode relay status: {}", e))
}

/// Subset of `/api/hf-auth/central-robot-status` that we care about. The
/// daemon proxies `tfrere-reachy-mini-central.hf.space/api/robot-status`
/// using the stored HF token and returns the list of robots currently
/// registered as producers under the user's account.
///
/// We use this as a fallback when `/api/hf-auth/relay-status` returns
/// `state: "unavailable"`. That endpoint guards on `daemon.wireless_version`
/// and refuses to expose the real relay state when the daemon is launched
/// in non-wireless mode (which is exactly our case: `--desktop-app-daemon`
/// without `--wireless-version`). The relay actually runs and registers
/// the robot just fine; only the API response lies. `central-robot-status`
/// asks central directly, so it returns the truth - and matches what the
/// mobile app sees.
#[derive(Debug, Default, Deserialize)]
struct CentralRobotStatus {
    #[serde(default)]
    available: bool,
    #[serde(default)]
    robots: Vec<serde_json::Value>,
}

fn fetch_central_robot_status(
    client: &reqwest::blocking::Client,
) -> Result<CentralRobotStatus, String> {
    let url = hf_url("/central-robot-status");
    let resp = client
        .get(&url)
        .send()
        .map_err(|e| format!("GET {}: {}", url, e))?;
    if !resp.status().is_success() {
        return Err(format!("GET {} -> {}", url, resp.status()));
    }
    resp.json::<CentralRobotStatus>()
        .map_err(|e| format!("decode central robot status: {}", e))
}

// ============================================================================
// OAUTH FLOW
// ============================================================================

/// Kick off the daemon-driven OAuth flow. No-op if the daemon isn't
/// currently `Running` (the OAuth callback URL `localhost:8000/...`
/// requires the daemon's HTTP server to be listening) or if another flow
/// is already in flight.
///
/// Spawns a dedicated blocking thread; the caller's path is non-blocking.
pub fn start_oauth_flow(app: AppHandle) {
    let store = app.state::<AuthStatusStore>();
    let app_state = app.state::<AppState>();

    if !matches!(current_daemon_state(&app_state), DaemonState::Running) {
        log::warn!("OAuth aborted: daemon is not running yet");
        return;
    }

    if !store.try_acquire_oauth() {
        log::info!("OAuth aborted: another flow is already in flight");
        return;
    }

    request_menu_refresh(&app);

    std::thread::spawn(move || {
        let result = oauth_flow_blocking(&app);
        match &result {
            Ok(username) => log::info!("OAuth flow completed: signed in as @{}", username),
            Err(e) => log::warn!("OAuth flow failed: {}", e),
        }

        // Release the in-flight flag and force one immediate refresh of
        // both the cached snapshot and the menu, regardless of outcome.
        let store = app.state::<AuthStatusStore>();
        store.release_oauth();

        // After a successful OAuth, prime the snapshot with what the
        // OAuth handshake itself returned (`Ok(username)`). The daemon's
        // `/status` endpoint may briefly answer `is_logged_in: false` due
        // to whoami-v2 rate-limiting, so don't rely on it for the very
        // first menu render.
        if let Ok(username) = &result {
            store.set_auth(AuthStatus {
                is_logged_in: true,
                username: Some(username.clone()),
            });
        }

        if let Ok(client) = http_client() {
            if let Ok(s) = fetch_auth_status(&client) {
                // Only override the optimistic snapshot if `/status`
                // actively confirms login - never downgrade based on a
                // 429-flavoured "false".
                if s.is_logged_in {
                    store.set_auth(s);
                }
            }
            if let Ok(r) = fetch_relay_status(&client) {
                store.set_relay(Some(r));
            }
        }

        // Open a burst-poll window so the next ~20 seconds run at 2 s
        // cadence, picking up the relay's "connected" transition fast.
        store.trigger_burst();
        request_menu_refresh(&app);
    });
}

fn oauth_flow_blocking(app: &AppHandle) -> Result<String, String> {
    let client = http_client()?;

    // Step 1: ask the daemon to mint a fresh OAuth session. We always
    // pass `use_localhost=true`: the daemon's redirect URI is
    // `http://localhost:8000/api/hf-auth/oauth/callback`, which is what
    // we open in the user's browser - no proxy required.
    let url = hf_url("/oauth/start?use_localhost=true");
    let resp = client
        .get(&url)
        .send()
        .map_err(|e| format!("oauth/start: {}", e))?;
    if !resp.status().is_success() {
        return Err(format!("oauth/start -> {}", resp.status()));
    }
    let payload = resp
        .json::<OauthStartResponse>()
        .map_err(|e| format!("oauth/start decode: {}", e))?;
    if payload.status != "success" {
        return Err(format!(
            "oauth/start status={} message={}",
            payload.status,
            payload.message.unwrap_or_default()
        ));
    }
    let session_id = payload
        .session_id
        .ok_or_else(|| "oauth/start: missing session_id".to_string())?;
    let auth_url = payload
        .auth_url
        .ok_or_else(|| "oauth/start: missing auth_url".to_string())?;

    // Step 2: open the HF authorize URL in the system browser. The daemon
    // handles the callback itself.
    //
    // `Shell::open` is technically deprecated in favour of the dedicated
    // `tauri-plugin-opener` plugin, but it works identically and we
    // already have the shell plugin in scope for the sidecar. Migrating
    // is a separate housekeeping concern.
    #[allow(deprecated)]
    {
        log::info!("opening HF OAuth URL: {}", &auth_url);
        if let Err(e) = app.shell().open(&auth_url, None) {
            return Err(format!("failed to open browser: {}", e));
        }
    }

    // Step 3: poll until success / error / expiration. We also bail out
    // early if the daemon leaves `Running` (Stop / Quit / crash); in that
    // case the callback can never land anyway.
    let started = Instant::now();
    let status_url = hf_url(&format!("/oauth/status/{}", session_id));
    loop {
        let app_state = app.state::<AppState>();
        if !matches!(current_daemon_state(&app_state), DaemonState::Running) {
            let _ = client
                .delete(hf_url(&format!("/oauth/session/{}", session_id)))
                .send();
            return Err("daemon stopped during OAuth flow".to_string());
        }

        if started.elapsed() > OAUTH_POLL_MAX {
            // Best-effort cleanup so the daemon's session table doesn't
            // accumulate dead entries.
            let _ = client
                .delete(hf_url(&format!("/oauth/session/{}", session_id)))
                .send();
            return Err("OAuth poll timed out (10 min)".to_string());
        }

        match client.get(&status_url).send() {
            Ok(resp) => {
                if let Ok(s) = resp.json::<OauthStatusResponse>() {
                    match s.status.as_str() {
                        "authorized" => {
                            return Ok(s.username.unwrap_or_else(|| "?".into()));
                        }
                        "error" | "expired" => {
                            return Err(format!(
                                "OAuth {}: {}",
                                s.status,
                                s.message.unwrap_or_default()
                            ));
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                log::debug!("oauth poll transient error: {}", e);
            }
        }
        std::thread::sleep(OAUTH_POLL_INTERVAL);
    }
}

// ============================================================================
// SIGN OUT / REFRESH RELAY
// ============================================================================

/// Logout: ask the daemon to wipe its stored token and reconnect the relay
/// in `WAITING_FOR_TOKEN` state.
pub fn sign_out(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        let client = match http_client() {
            Ok(c) => c,
            Err(e) => {
                log::warn!("sign_out: http client: {}", e);
                return;
            }
        };
        let url = hf_url("/token");
        match client.delete(&url).send() {
            Ok(resp) if resp.status().is_success() => {
                log::info!("sign-out OK");
            }
            Ok(resp) => {
                log::warn!("sign-out unexpected status: {}", resp.status());
            }
            Err(e) => {
                log::warn!("sign-out failed: {}", e);
            }
        }
        // Force an immediate snapshot refresh so the menu reflects the
        // logged-out state without waiting for the next poll tick.
        let store = app.state::<AuthStatusStore>();
        store.set_auth(AuthStatus::default());
        store.set_relay(None);
        store.trigger_burst();
        request_menu_refresh(&app);
    });
}

/// POST /api/hf-auth/refresh-relay - heals a "zombie relay" state where
/// the daemon thinks it's connected but central no longer lists it as a
/// producer. Cf. PR #1047.
pub fn refresh_relay(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        let client = match http_client() {
            Ok(c) => c,
            Err(e) => {
                log::warn!("refresh_relay: http client: {}", e);
                return;
            }
        };
        let url = hf_url("/refresh-relay");
        match client.post(&url).send() {
            Ok(resp) if resp.status().is_success() => {
                log::info!("refresh-relay requested");
            }
            Ok(resp) => {
                log::warn!("refresh-relay unexpected status: {}", resp.status());
            }
            Err(e) => {
                log::warn!("refresh-relay failed: {}", e);
            }
        }
        let store = app.state::<AuthStatusStore>();
        store.trigger_burst();
        request_menu_refresh(&app);
    });
}

// ============================================================================
// STATUS POLLER
// ============================================================================

/// Pure "are we logged in?" reconciliation for one poll tick.
///
/// The daemon's `/status` endpoint hits HuggingFace's `whoami-v2` on every
/// call with no caching, so HF rate-limits us to 429 and we get back
/// `is_logged_in: false` even when the token is perfectly valid. We defend
/// against that with three signals:
///
/// 1. trust positive responses immediately (and refresh the sticky
///    username / reset the negative-streak counter);
/// 2. require [`LOGOUT_CONFIRM_TICKS`] consecutive negatives before
///    believing a logout (defense against transient 429);
/// 3. if the central relay is connected, the user MUST be logged in (the
///    relay can't register without a valid token), so keep the login
///    regardless of what `/status` said.
///
/// `sticky_username` / `consecutive_logged_out` are the poller's cross-tick
/// memory; this function owns all their mutations so the whole policy is
/// testable without HTTP.
fn reconcile_auth(
    raw: &AuthStatus,
    relay_connected: bool,
    sticky_username: &mut Option<String>,
    consecutive_logged_out: &mut u32,
) -> AuthStatus {
    let mut effective = raw.clone();
    if raw.is_logged_in {
        *consecutive_logged_out = 0;
        if let Some(name) = raw.username.as_ref() {
            if !name.is_empty() {
                *sticky_username = Some(name.clone());
            }
        }
    } else {
        *consecutive_logged_out = consecutive_logged_out.saturating_add(1);
        let still_trust_login = relay_connected || *consecutive_logged_out < LOGOUT_CONFIRM_TICKS;
        if still_trust_login && sticky_username.is_some() {
            effective.is_logged_in = true;
            effective.username = sticky_username.clone();
            log::debug!(
                "auth poller: keeping sticky login (raw whoami=false, likely 429; relay_connected={}, ticks={}/{})",
                relay_connected,
                consecutive_logged_out,
                LOGOUT_CONFIRM_TICKS
            );
        } else if !still_trust_login {
            // Confirmed logout: clear sticky cache.
            *sticky_username = None;
        }
    }
    effective
}

/// Spawn a single long-lived blocking thread that polls `/status` and
/// `/relay-status` whenever the daemon is `Running`. Updates the cached
/// `AuthStatusStore` and triggers a menu refresh on every change.
///
/// While the daemon is `Idle`, `Starting` or `Crashed`, the poller falls
/// back to a slower idle cadence and skips HTTP altogether (the daemon
/// HTTP server isn't listening, so any call would just time out and
/// burn CPU). When the daemon flips back to `Running`, the next tick
/// resumes full polling.
pub fn start_status_poller(app: AppHandle) {
    std::thread::spawn(move || {
        let mut last_signature: Option<String> = None;
        // Last username we got from `/status` while `is_logged_in: true`.
        // Persists across 429-induced false negatives so the menu doesn't
        // briefly flap to "Sign in..." between successful round-trips.
        let mut sticky_username: Option<String> = None;
        // Count of consecutive `is_logged_in: false` responses seen since
        // the last truthy one. We only believe a logout once we've seen
        // multiple in a row OR the relay has clearly dropped, otherwise
        // a single 429 would log the user out in the UI.
        let mut consecutive_logged_out: u32 = 0;

        loop {
            let app_state = app.state::<AppState>();
            let daemon_state = current_daemon_state(&app_state);

            if matches!(daemon_state, DaemonState::Running) {
                let client = match http_client() {
                    Ok(c) => c,
                    Err(e) => {
                        log::warn!("auth poller http client: {}", e);
                        std::thread::sleep(POLL_INTERVAL_RUNNING);
                        continue;
                    }
                };

                let store = app.state::<AuthStatusStore>();

                let raw_auth = fetch_auth_status(&client).unwrap_or_default();
                let relay_result = fetch_relay_status(&client);
                match &relay_result {
                    Ok(r) => log::debug!(
                        "poller tick: auth.logged_in={} user={:?} | relay.state={} relay.connected={}",
                        raw_auth.is_logged_in,
                        raw_auth.username,
                        r.state,
                        r.is_connected,
                    ),
                    Err(e) => log::warn!(
                        "poller tick: auth.logged_in={} user={:?} | relay-status FETCH FAILED: {}",
                        raw_auth.is_logged_in,
                        raw_auth.username,
                        e,
                    ),
                }
                let mut relay = relay_result.ok();

                // Fallback for the "Lite version" daemon bug: when launched
                // without `--wireless-version` (our default for desktop
                // mode), `/api/hf-auth/relay-status` always returns
                // `state: "unavailable"` even though the central relay is
                // fully operational. Ask `/api/hf-auth/central-robot-status`
                // (which proxies HF central using the stored token) for the
                // ground truth: if our token has at least one robot
                // registered as producer, the relay IS connected.
                let relay_says_unavailable = relay
                    .as_ref()
                    .map(|r| r.state == "unavailable")
                    .unwrap_or(false);
                if relay_says_unavailable && raw_auth.is_logged_in {
                    match fetch_central_robot_status(&client) {
                        Ok(c) if c.available && !c.robots.is_empty() => {
                            log::info!(
                                "relay-status reports 'unavailable' (daemon-side bug) but central lists {} robot(s); inferring connected",
                                c.robots.len()
                            );
                            relay = Some(RelayStatus {
                                state: "connected".to_string(),
                                message: Some("inferred from central-robot-status".to_string()),
                                is_connected: true,
                            });
                        }
                        Ok(c) => {
                            log::debug!(
                                "central-robot-status fallback: available={}, robots={}",
                                c.available,
                                c.robots.len()
                            );
                        }
                        Err(e) => {
                            log::debug!("central-robot-status fallback failed: {}", e);
                        }
                    }
                }

                let relay_connected = relay.as_ref().map(|r| r.is_connected).unwrap_or(false);

                // "Are we logged in?" reconciliation - full policy and
                // rationale documented on `reconcile_auth`.
                let effective_auth = reconcile_auth(
                    &raw_auth,
                    relay_connected,
                    &mut sticky_username,
                    &mut consecutive_logged_out,
                );

                let signature = format!(
                    "{}|{}|{}|{}",
                    effective_auth.is_logged_in,
                    effective_auth.username.as_deref().unwrap_or(""),
                    relay.as_ref().map(|r| r.state.as_str()).unwrap_or(""),
                    relay_connected
                );

                store.set_auth(effective_auth);
                store.set_relay(relay);

                if last_signature.as_deref() != Some(&signature) {
                    last_signature = Some(signature);
                    request_menu_refresh(&app);
                }

                let interval = if store.in_burst_window() {
                    POLL_INTERVAL_BURST
                } else {
                    POLL_INTERVAL_RUNNING
                };
                std::thread::sleep(interval);
            } else {
                if last_signature.is_some() {
                    last_signature = None;
                    let store = app.state::<AuthStatusStore>();
                    store.set_relay(None);
                    request_menu_refresh(&app);
                }
                sticky_username = None;
                consecutive_logged_out = 0;
                std::thread::sleep(POLL_INTERVAL_IDLE);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hf_url_concatenates_base_prefix_and_suffix() {
        assert_eq!(
            hf_url("/status"),
            "http://127.0.0.1:8000/api/hf-auth/status"
        );
        assert_eq!(
            hf_url("/oauth/start?use_localhost=true"),
            "http://127.0.0.1:8000/api/hf-auth/oauth/start?use_localhost=true"
        );
    }

    #[test]
    fn auth_status_decodes_full_payload() {
        let raw = r#"{"is_logged_in": true, "username": "tfrere"}"#;
        let s: AuthStatus = serde_json::from_str(raw).expect("decode");
        assert!(s.is_logged_in);
        assert_eq!(s.username.as_deref(), Some("tfrere"));
    }

    #[test]
    fn auth_status_decodes_empty_payload_as_logged_out() {
        let s: AuthStatus = serde_json::from_str("{}").expect("decode");
        assert!(!s.is_logged_in);
        assert!(s.username.is_none());
    }

    #[test]
    fn relay_status_decodes_connecting() {
        let raw = r#"{"state": "connecting", "is_connected": false, "message": "in progress"}"#;
        let r: RelayStatus = serde_json::from_str(raw).expect("decode");
        assert_eq!(r.state, "connecting");
        assert!(!r.is_connected);
        assert_eq!(r.message.as_deref(), Some("in progress"));
    }

    #[test]
    fn relay_status_decodes_minimal_payload() {
        let r: RelayStatus = serde_json::from_str("{}").expect("decode");
        assert!(r.state.is_empty());
        assert!(!r.is_connected);
        assert!(r.message.is_none());
    }

    #[test]
    fn auth_snapshot_default_is_logged_out_no_oauth() {
        let snap = AuthSnapshot::default();
        assert!(!snap.auth.is_logged_in);
        assert!(snap.relay.is_none());
        assert!(!snap.oauth_in_flight);
    }

    #[test]
    fn auth_status_store_oauth_acquire_is_exclusive() {
        let store = AuthStatusStore::new();
        assert!(store.try_acquire_oauth());
        assert!(
            !store.try_acquire_oauth(),
            "second acquire must fail while one is in flight"
        );
        store.release_oauth();
        assert!(store.try_acquire_oauth(), "release should re-allow acquire");
    }

    #[test]
    fn auth_status_store_burst_window_starts_closed() {
        let store = AuthStatusStore::new();
        assert!(!store.in_burst_window());
        store.trigger_burst();
        assert!(store.in_burst_window());
    }

    // ---- reconcile_auth (429-resilience policy) ----

    fn logged_in(user: &str) -> AuthStatus {
        AuthStatus {
            is_logged_in: true,
            username: Some(user.to_string()),
        }
    }

    fn logged_out() -> AuthStatus {
        AuthStatus::default()
    }

    #[test]
    fn positive_response_sets_sticky_and_resets_counter() {
        let mut sticky = None;
        let mut ticks = 5;
        let eff = reconcile_auth(&logged_in("tfrere"), false, &mut sticky, &mut ticks);
        assert!(eff.is_logged_in);
        assert_eq!(sticky.as_deref(), Some("tfrere"));
        assert_eq!(ticks, 0);
    }

    #[test]
    fn single_negative_keeps_sticky_login() {
        // One `false` after a valid login is most likely a whoami-v2 429:
        // the UI must NOT flap to "Sign in...".
        let mut sticky = Some("tfrere".to_string());
        let mut ticks = 0;
        let eff = reconcile_auth(&logged_out(), false, &mut sticky, &mut ticks);
        assert!(eff.is_logged_in);
        assert_eq!(eff.username.as_deref(), Some("tfrere"));
        assert_eq!(ticks, 1);
    }

    #[test]
    fn confirmed_negatives_log_out_and_clear_sticky() {
        let mut sticky = Some("tfrere".to_string());
        let mut ticks = 0;
        // First negative: still trusted.
        let eff = reconcile_auth(&logged_out(), false, &mut sticky, &mut ticks);
        assert!(eff.is_logged_in);
        // Second negative reaches LOGOUT_CONFIRM_TICKS: believed.
        let eff = reconcile_auth(&logged_out(), false, &mut sticky, &mut ticks);
        assert!(!eff.is_logged_in);
        assert!(sticky.is_none(), "sticky username must be cleared");
    }

    #[test]
    fn connected_relay_overrides_any_negative_streak() {
        // The relay can't register without a valid token, so as long as
        // it's connected the login is kept no matter how many negatives.
        let mut sticky = Some("tfrere".to_string());
        let mut ticks = 0;
        for _ in 0..10 {
            let eff = reconcile_auth(&logged_out(), true, &mut sticky, &mut ticks);
            assert!(eff.is_logged_in);
            assert_eq!(eff.username.as_deref(), Some("tfrere"));
        }
        assert!(sticky.is_some());
    }

    #[test]
    fn negative_without_sticky_is_logged_out_immediately() {
        // Never-logged-in user: nothing to keep sticky, report logged out.
        let mut sticky = None;
        let mut ticks = 0;
        let eff = reconcile_auth(&logged_out(), false, &mut sticky, &mut ticks);
        assert!(!eff.is_logged_in);
        assert!(eff.username.is_none());
    }

    #[test]
    fn empty_username_does_not_poison_sticky() {
        let mut sticky = Some("tfrere".to_string());
        let mut ticks = 0;
        let eff = reconcile_auth(
            &AuthStatus {
                is_logged_in: true,
                username: Some(String::new()),
            },
            false,
            &mut sticky,
            &mut ticks,
        );
        assert!(eff.is_logged_in);
        // Sticky keeps the last non-empty name.
        assert_eq!(sticky.as_deref(), Some("tfrere"));
    }
}
