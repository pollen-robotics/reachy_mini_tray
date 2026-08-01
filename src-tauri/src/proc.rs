//! Spawning helpers for external tools (`lsof`, `netstat`, `taskkill`,
//! `powershell`, `uv`, ...).
//!
//! # Why this exists
//!
//! On Windows, every `std::process::Command` spawned from a GUI app opens a
//! visible console window for a fraction of a second unless the
//! `CREATE_NO_WINDOW` creation flag is set. The tray shells out at boot
//! (orphan sweep: `netstat` + `powershell` + `taskkill`) and before every
//! daemon launch (forced update: `uv pip install`), so without the flag
//! users see a burst of black terminal windows flashing on screen - the
//! exact bug the desktop app fixed in production. Route every external
//! command through [`hidden_command`] so the flag can never be forgotten.

use std::ffi::OsStr;
use std::process::Command;

/// `CREATE_NO_WINDOW`: suppress the console window that Windows otherwise
/// pops up for each spawned console process.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Build a [`Command`] that never flashes a console window on Windows.
/// On Unix this is exactly `Command::new(program)`.
pub(crate) fn hidden_command(program: impl AsRef<OsStr>) -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}
