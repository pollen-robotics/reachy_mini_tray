//! Tiny persisted tray settings.
//!
//! Currently a single field: the user's last *explicit* connection-mode
//! pick (USB / Simulation), so it survives tray restarts. Stored as a small
//! JSON file in the shared data dir, next to the daemon's `.venv`.
//!
//! Deliberately NOT persisted:
//!
//! - the selected serialport (device paths churn across replugs; the USB
//!   scanner's auto-select already handles the single-robot case);
//! - the boot-time USB -> Simulation reconciliation in `lib.rs` (that's a
//!   runtime downgrade, not a user choice - persisting it would strand a
//!   user in sim mode after one robot-unplugged boot).
//!
//! Everything is fail-open: a missing / corrupt file just means defaults.

use serde::{Deserialize, Serialize};

use crate::paths;
use crate::state::Mode;

const SETTINGS_FILE: &str = "tray_settings.json";

#[derive(Debug, Default, Serialize, Deserialize)]
struct Settings {
    /// `"usb"` or `"simulation"` (see [`Mode::as_str`]). `None` / unknown
    /// values fall back to the built-in default (USB).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mode: Option<String>,
}

fn settings_path() -> Option<std::path::PathBuf> {
    paths::data_dir().map(|d| d.join(SETTINGS_FILE))
}

/// Parse a persisted mode string back into a [`Mode`]. Inverse of
/// [`Mode::as_str`]; unknown strings yield `None` (treat as default).
fn mode_from_str(s: &str) -> Option<Mode> {
    match s {
        "usb" => Some(Mode::Usb),
        "simulation" => Some(Mode::Simulation),
        _ => None,
    }
}

/// Load the persisted connection mode, if any.
pub(crate) fn load_mode() -> Option<Mode> {
    let path = settings_path()?;
    let raw = std::fs::read_to_string(&path).ok()?;
    let settings: Settings = match serde_json::from_str(&raw) {
        Ok(s) => s,
        Err(e) => {
            log::warn!("ignoring corrupt {}: {}", SETTINGS_FILE, e);
            return None;
        }
    };
    settings.mode.as_deref().and_then(mode_from_str)
}

/// Persist the user's explicit connection-mode pick. Best-effort: a write
/// failure is logged and forgotten (the in-memory mode still applies for
/// this session).
pub(crate) fn save_mode(mode: Mode) {
    let Some(path) = settings_path() else {
        log::warn!("cannot persist mode: data dir unavailable");
        return;
    };
    // The data dir may not exist yet on a very first launch (the trampoline
    // normally creates it during bootstrap).
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let settings = Settings {
        mode: Some(mode.as_str().to_string()),
    };
    match serde_json::to_string_pretty(&settings) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                log::warn!("failed to persist mode to {}: {}", path.display(), e);
            }
        }
        Err(e) => log::warn!("failed to serialize settings: {}", e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_roundtrips_through_its_string_form() {
        for mode in [Mode::Usb, Mode::Simulation] {
            assert_eq!(mode_from_str(mode.as_str()), Some(mode));
        }
    }

    #[test]
    fn unknown_mode_string_yields_none() {
        assert_eq!(mode_from_str("wifi"), None);
        assert_eq!(mode_from_str(""), None);
        assert_eq!(mode_from_str("USB"), None); // case-sensitive on purpose
    }

    #[test]
    fn corrupt_json_is_rejected_gracefully() {
        let parsed: Result<Settings, _> = serde_json::from_str("{not json");
        assert!(parsed.is_err());
    }

    #[test]
    fn missing_mode_field_deserializes_to_none() {
        let settings: Settings = serde_json::from_str("{}").unwrap();
        assert!(settings.mode.is_none());
    }
}
