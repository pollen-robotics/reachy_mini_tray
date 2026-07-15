//! Filesystem paths for app data + first-run detection.
//!
//! The data directory is intentionally aligned with `reachy_mini_desktop_app`
//! (`com.pollen-robotics.reachy-mini`, *without* the `-daemon` suffix) so:
//!
//! - users migrating from the desktop app can reuse the existing `.venv`
//!   and skip a full ~3 minute bootstrap on first launch of this tray app;
//! - `uv-trampoline` (the sidecar shipped in MVP) finds the same layout it
//!   was designed for, without recompilation.
//!
//! The bundle identifier (`com.pollen-robotics.reachy-mini-daemon`) is kept
//! distinct in `tauri.conf.json` so the two apps remain independently signed
//! processes from macOS' point of view.
//!
//! "Bootstrap done" is detected by the presence of `.venv/bin/python3`
//! (resp. `.venv\Scripts\python.exe` on Windows). This matches
//! `uv_wrapper::venv_exists()` in the desktop app and is more robust than a
//! sentinel file: a partial bootstrap (interrupted by quit, crash, power
//! loss) leaves no Python binary, so the next launch restarts setup
//! automatically.

use std::path::PathBuf;

// Folder names shared with `reachy_mini_desktop_app::uv_wrapper::get_data_dir()`.
#[cfg(target_os = "macos")]
const SHARED_FOLDER_MACOS: &str = "com.pollen-robotics.reachy-mini";
#[cfg(target_os = "windows")]
const SHARED_FOLDER_WINDOWS: &str = "Reachy Mini Control";
#[cfg(target_os = "linux")]
const SHARED_FOLDER_LINUX: &str = "reachy-mini-control";

/// Name of the venv used by the daemon. Shared with the desktop app.
const VENV_NAME: &str = ".venv";

pub fn data_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        return std::env::var("HOME").ok().map(|home| {
            PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join(SHARED_FOLDER_MACOS)
        });
    }

    #[cfg(target_os = "windows")]
    {
        return std::env::var("LOCALAPPDATA")
            .ok()
            .map(|local| PathBuf::from(local).join(SHARED_FOLDER_WINDOWS));
    }

    #[cfg(target_os = "linux")]
    {
        if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
            return Some(PathBuf::from(xdg).join(SHARED_FOLDER_LINUX));
        }
        return std::env::var("HOME").ok().map(|home| {
            PathBuf::from(home)
                .join(".local/share")
                .join(SHARED_FOLDER_LINUX)
        });
    }

    #[allow(unreachable_code)]
    None
}

/// Path to the daemon's Python interpreter inside `.venv`.
/// Existence of this file == bootstrap is complete.
pub fn venv_python_path() -> Option<PathBuf> {
    let venv = data_dir()?.join(VENV_NAME);
    if cfg!(target_os = "windows") {
        Some(venv.join("Scripts").join("python.exe"))
    } else {
        Some(venv.join("bin").join("python3"))
    }
}

/// Returns true once `.venv/bin/python3` exists in the shared data dir.
/// Mirrors `uv_wrapper::venv_exists(data_dir, ".venv")`.
pub fn is_bootstrap_done() -> bool {
    venv_python_path().map(|p| p.exists()).unwrap_or(false)
}

/// Path to the Python interpreter inside an arbitrary venv (e.g. `.venv`
/// or `apps_venv`) in the shared data dir. Mirrors
/// `uv_wrapper::python_exe_path(data_dir, venv_name)`.
pub fn venv_python_for(venv_name: &str) -> Option<PathBuf> {
    let venv = data_dir()?.join(venv_name);
    if cfg!(target_os = "windows") {
        Some(venv.join("Scripts").join("python.exe"))
    } else {
        Some(venv.join("bin").join("python3"))
    }
}

/// Path to the `uv` executable the trampoline downloaded into the data dir.
/// Mirrors `uv_wrapper::uv_exe_path(data_dir)`. The tray reuses it to run
/// `uv pip install -U` for a user-triggered daemon upgrade.
pub fn uv_exe_path() -> Option<PathBuf> {
    let dir = data_dir()?;
    if cfg!(target_os = "windows") {
        Some(dir.join("uv.exe"))
    } else {
        Some(dir.join("uv"))
    }
}

/// Wipe the `.venv` directory (and its sibling `apps_venv` if present) so
/// the next launch reruns the full first-time setup. Triggered by the
/// `Reset setup…` tray menu item.
///
/// Note: we deliberately do NOT remove `uv` itself, the cpython-* folders,
/// or any cache. Re-downloading `uv` and CPython is the slowest part of
/// the bootstrap (>1 min on a slow link) and deleting them would punish
/// the user for what is mostly a "reinstall reachy-mini" operation.
pub fn reset_bootstrap() -> std::io::Result<()> {
    let Some(dir) = data_dir() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "data dir unavailable",
        ));
    };

    for venv in [".venv", "apps_venv"] {
        let path = dir.join(venv);
        if path.exists() {
            std::fs::remove_dir_all(&path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_dir_is_some_on_supported_platforms() {
        // We can only really assert the call is wired correctly; the
        // actual path depends on the host's HOME / LOCALAPPDATA, which
        // CI guarantees but local devs may run with quirky setups.
        let dir = data_dir();
        assert!(
            dir.is_some() || std::env::var_os("HOME").is_none(),
            "expected data_dir() to resolve when HOME/LOCALAPPDATA is set",
        );
    }

    #[test]
    fn venv_python_path_is_inside_data_dir() {
        if let (Some(root), Some(py)) = (data_dir(), venv_python_path()) {
            assert!(py.starts_with(&root), "venv path must be under data dir");
            assert!(
                py.components()
                    .any(|c| c.as_os_str() == std::ffi::OsStr::new(VENV_NAME)),
                "venv path must include the {} directory",
                VENV_NAME,
            );
        }
    }

    #[test]
    fn venv_python_filename_is_platform_specific() {
        if let Some(py) = venv_python_path() {
            let last = py.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if cfg!(target_os = "windows") {
                assert_eq!(last, "python.exe");
            } else {
                assert_eq!(last, "python3");
            }
        }
    }
}
