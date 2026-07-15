//! Subprocess helpers for a GUI host. On Windows a console child launched from
//! a windowless parent (Peko Studio, or peko itself when Studio spawned it)
//! pops a console window unless CREATE_NO_WINDOW is set. These helpers apply
//! that flag, and launch batch-wrapped tools (npm) through their `.cmd` name.

use std::ffi::OsStr;
use std::process::Command;

/// A [`Command`] for `program` that does not open a console window on Windows.
pub fn hidden<S: AsRef<OsStr>>(program: S) -> Command {
    let mut command = Command::new(program);
    hide_window(&mut command);
    command
}

/// A hidden [`Command`] for npm. npm is a batch script on Windows, so it is
/// invoked as `npm.cmd` there and `npm` elsewhere.
pub fn npm() -> Command {
    hidden(if cfg!(windows) { "npm.cmd" } else { "npm" })
}

/// A hidden [`Command`] for npx, used to run a framework's own scaffolder. Like
/// npm, it is a batch script on Windows (`npx.cmd`).
pub fn npx() -> Command {
    hidden(if cfg!(windows) { "npx.cmd" } else { "npx" })
}

/// Apply the no-console-window creation flag on Windows. A no-op elsewhere.
pub fn hide_window(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = command;
}
