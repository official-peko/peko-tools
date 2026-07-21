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

/// Whether the CLI is emitting machine-readable JSON on stdout.
///
/// A process-wide flag rather than a threaded parameter: the subprocess helpers
/// below are called from deep inside the bundlers, which have no reporter to
/// hand. It is written once while the reporter is built and only ever read
/// afterwards.
static JSON_STDOUT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Record that stdout carries JSON events. Called once when the reporter is
/// constructed in JSON mode.
pub fn set_json_stdout(enabled: bool) {
    JSON_STDOUT.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

/// Whether stdout is reserved for JSON events.
pub fn json_stdout() -> bool {
    JSON_STDOUT.load(std::sync::atomic::Ordering::Relaxed)
}

/// Send a child's stdout to this process's stderr while stdout carries JSON.
///
/// Build tools write freely to stdout (`hdiutil`, `productbuild`, `jarsigner`,
/// npm), and an inherited stdout interleaves that prose with the JSON event
/// stream, leaving a host unable to parse a line without guessing. Routing the
/// child to stderr keeps stdout strictly machine-readable without discarding
/// the output: a host that wants the build log still reads it from stderr.
///
/// A no-op when stdout is not JSON, so human runs keep tool output where users
/// expect it.
pub fn route_stdout_to_stderr(command: &mut Command) {
    if !json_stdout() {
        return;
    }
    if let Some(stderr) = stderr_stdio() {
        command.stdout(stderr);
    }
}

/// A [`Stdio`](std::process::Stdio) writing to this process's stderr.
///
/// The descriptor is *duplicated*: `Stdio::from` takes ownership and closes the
/// handle when the child is spawned, so passing stderr itself would close it for
/// the rest of the run and silence every later diagnostic.
fn stderr_stdio() -> Option<std::process::Stdio> {
    #[cfg(unix)]
    {
        use std::os::fd::AsFd;
        let cloned = std::io::stderr().as_fd().try_clone_to_owned().ok()?;
        Some(std::process::Stdio::from(cloned))
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsHandle;
        let cloned = std::io::stderr().as_handle().try_clone_to_owned().ok()?;
        Some(std::process::Stdio::from(cloned))
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test, not two: the mode is a process-wide flag, so parallel tests
    /// toggling it would race each other.
    #[test]
    fn routing_is_mode_gated_and_keeps_the_parent_stderr_open() {
        // Human mode leaves stdout alone, where users expect tool output.
        set_json_stdout(false);
        assert!(!json_stdout());

        // JSON mode routes it. Duplicating the descriptor is the whole point:
        // handing a child the real stderr would close it on spawn and silence
        // the rest of the run, so spawn twice and then write.
        set_json_stdout(true);
        assert!(json_stdout());
        for _ in 0..2 {
            let mut command = Command::new(if cfg!(windows) { "cmd" } else { "true" });
            if cfg!(windows) {
                command.args(["/C", "exit"]);
            }
            route_stdout_to_stderr(&mut command);
            let _ = command.status();
        }
        use std::io::Write;
        // Would fail with EBADF if the descriptor had been consumed.
        assert!(std::io::stderr().write(b"").is_ok());
        assert!(std::io::stderr().flush().is_ok());

        set_json_stdout(false);
    }
}
