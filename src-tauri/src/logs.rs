//! In-memory ring-buffer logger and external-line ingestion helpers.
//!
//! - Implements `log::Log` so every `log::info!` / `warn!` / `error!` call
//!   from our crate is captured.
//! - Keeps the last `MAX_LOG_LINES` entries in a process-wide ring buffer
//!   exposed via `LogStore` (state-managed by Tauri).
//! - After the Tauri app is built, `bind_app_handle()` lets the logger emit
//!   a `log:append` event so the in-app logs window can append in real time.
//! - `push_external` ingests free-form lines from spawned children (the
//!   `uv-trampoline` sidecar mostly), with a heuristic level extractor
//!   (`parse_line_level`) that recognises Python's `logging` defaults.
//! - Every entry is also mirrored to stderr so `npm run tauri dev` still
//!   shows logs in the terminal.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use log::{Level, Log, Metadata, Record};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

/// Cap matches the agreed product limit (TRAY_ONLY_FEASIBILITY §4 - logs).
pub const MAX_LOG_LINES: usize = 500;

/// Event name listened to by the logs window.
pub const EVENT_LOG_APPEND: &str = "log:append";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogEntry {
    /// Unix epoch in milliseconds.
    pub ts_ms: u64,
    pub level: String,
    pub message: String,
    pub target: String,
}

pub struct LogStore {
    buffer: Mutex<VecDeque<LogEntry>>,
}

impl LogStore {
    pub fn new() -> Self {
        Self {
            buffer: Mutex::new(VecDeque::with_capacity(MAX_LOG_LINES)),
        }
    }

    pub fn push(&self, entry: LogEntry) {
        if let Ok(mut buf) = self.buffer.lock() {
            if buf.len() >= MAX_LOG_LINES {
                buf.pop_front();
            }
            buf.push_back(entry);
        }
    }

    pub fn snapshot(&self) -> Vec<LogEntry> {
        self.buffer
            .lock()
            .map(|b| b.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn clear(&self) {
        if let Ok(mut buf) = self.buffer.lock() {
            buf.clear();
        }
    }
}

static APP_HANDLE: OnceLock<AppHandle> = OnceLock::new();

struct AppLogger;

impl Log for AppLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= Level::Info
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let target = record.target();
        // Filter out noisy crates so the in-app log window stays focused on
        // our own messages. stderr keeps everything for power users.
        let is_ours = target.starts_with("reachy_mini_tray");

        let entry = LogEntry {
            ts_ms: now_ms(),
            level: record.level().as_str().to_string(),
            message: format!("{}", record.args()),
            target: target.to_string(),
        };

        // Mirror to stderr (terminal) for everyone.
        eprintln!(
            "[{}][{}][{}] {}",
            iso_short(entry.ts_ms),
            entry.level,
            entry.target,
            entry.message
        );

        if !is_ours {
            return;
        }

        if let Some(app) = APP_HANDLE.get() {
            if let Some(store) = app.try_state::<LogStore>() {
                store.push(entry.clone());
            }
            let _ = app.emit(EVENT_LOG_APPEND, &entry);
        }
    }

    fn flush(&self) {}
}

/// Install the logger as the process-wide `log` implementation. Must be
/// called before any `log::*` macro fires (i.e. before `tauri::Builder`).
pub fn init() {
    static LOGGER: AppLogger = AppLogger;
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(log::LevelFilter::Info);
}

/// Once the Tauri app is built, register its handle so subsequent log
/// records can be pushed to the ring buffer and emitted as events. Records
/// produced before this call are still printed to stderr; they just don't
/// reach the in-app window (which is fine - it's only opened on demand).
pub fn bind_app_handle(app: &AppHandle) {
    let _ = APP_HANDLE.set(app.clone());
}

/// Inject an external log line (typically from a spawned child process)
/// into the store, bypassing the `log` crate filter that excludes targets
/// outside our crate. Tagged with the provided `target` so the UI can
/// visually distinguish e.g. `daemon` vs `tray`.
pub fn push_external(app: &AppHandle, target: &str, level: &str, message: String) {
    let entry = LogEntry {
        ts_ms: now_ms(),
        level: normalize_level(level),
        message,
        target: target.to_string(),
    };
    eprintln!(
        "[{}][{}][{}] {}",
        iso_short(entry.ts_ms),
        entry.level,
        entry.target,
        entry.message
    );
    if let Some(store) = app.try_state::<LogStore>() {
        store.push(entry.clone());
    }
    let _ = app.emit(EVENT_LOG_APPEND, &entry);
}

/// Heuristic level extraction from a free-form log line. Recognises the
/// `[LEVEL]` and `LEVEL:` patterns used by Python's `logging` module and
/// uvicorn / fastapi defaults. Falls back to the caller-provided default.
pub fn parse_line_level(line: &str, default: &str) -> String {
    let upper = line.to_ascii_uppercase();
    for needle in [
        "[CRITICAL]",
        "CRITICAL:",
        "[ERROR]",
        "ERROR:",
        "[WARNING]",
        "WARNING:",
        "[WARN]",
        "WARN:",
        "[INFO]",
        "INFO:",
        "[DEBUG]",
        "DEBUG:",
    ] {
        if upper.contains(needle) {
            return normalize_level(needle.trim_matches(|c: char| !c.is_ascii_alphabetic()));
        }
    }
    normalize_level(default)
}

fn normalize_level(raw: &str) -> String {
    match raw.trim().to_ascii_uppercase().as_str() {
        "CRITICAL" | "ERROR" | "ERR" => "ERROR".into(),
        "WARNING" | "WARN" => "WARN".into(),
        "DEBUG" => "DEBUG".into(),
        "TRACE" => "TRACE".into(),
        _ => "INFO".into(),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn iso_short(ts_ms: u64) -> String {
    // Format: HH:MM:SS for stderr readability. Full ISO is JS-side concern.
    let secs = ts_ms / 1000;
    let h = (secs / 3600) % 24;
    let m = (secs / 60) % 60;
    let s = secs % 60;
    format!("{:02}:{:02}:{:02}", h, m, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_line_level_recognises_python_brackets() {
        assert_eq!(parse_line_level("2024-01-01 [ERROR] boom", "INFO"), "ERROR");
        assert_eq!(parse_line_level("[WARNING] heads up", "INFO"), "WARN");
        assert_eq!(parse_line_level("[CRITICAL] oops", "INFO"), "ERROR");
    }

    #[test]
    fn parse_line_level_recognises_uvicorn_colon() {
        assert_eq!(parse_line_level("INFO: starting", "WARN"), "INFO");
        assert_eq!(parse_line_level("ERROR: bind failed", "INFO"), "ERROR");
        assert_eq!(parse_line_level("WARNING: deprecated", "INFO"), "WARN");
    }

    #[test]
    fn parse_line_level_falls_back_to_default() {
        assert_eq!(parse_line_level("plain stdout text", "INFO"), "INFO");
        assert_eq!(parse_line_level("plain stderr text", "WARN"), "WARN");
    }

    #[test]
    fn parse_line_level_priority_is_critical_first() {
        // "[CRITICAL] foo INFO bar" must report CRITICAL/ERROR, not INFO,
        // because critical patterns are checked before info ones.
        assert_eq!(parse_line_level("[CRITICAL] INFO bar", "INFO"), "ERROR");
    }

    #[test]
    fn normalize_level_canonicalises_synonyms() {
        assert_eq!(normalize_level("err"), "ERROR");
        assert_eq!(normalize_level(" warning "), "WARN");
        assert_eq!(normalize_level("DEBUG"), "DEBUG");
        assert_eq!(normalize_level("nonsense"), "INFO");
    }

    #[test]
    fn iso_short_pads_components() {
        assert_eq!(iso_short(0), "00:00:00");
        assert_eq!(iso_short(3_661_000), "01:01:01");
    }
}
