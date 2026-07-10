//! PATH configuration: write the `env` sourcing script and add a source line to
//! the user's shell rc files, so `peko` is on PATH in new shells.

#[cfg(not(windows))]
use std::io::Write;
#[cfg(not(windows))]
use std::path::Path;

use super::layout::Layout;
use super::{Result, SetupError};

#[cfg(not(windows))]
const MARKER: &str = "# added by peko setup";
#[cfg(not(windows))]
const ENV_CONTENTS: &str =
    "export PEKO_ROOT_PATH=\"$HOME/.Peko\"\nexport PATH=\"$HOME/.Peko/Compiler/bin/peko:$PATH\"\n";

/// Configure PATH on a Unix host: write the `env` sourcing script and add a
/// source line to the shell rc files.
#[cfg(not(windows))]
pub fn configure(layout: &Layout) -> Result<()> {
    let env_file = layout.env_file();
    std::fs::write(&env_file, ENV_CONTENTS)
        .map_err(|e| SetupError::io(format!("write {}", env_file.display()), e))?;

    let source_line = format!("{MARKER}\n. \"$HOME/.Peko/env\"\n");
    if let Some(home) = dirs::home_dir() {
        for name in [".zshrc", ".bashrc", ".profile"] {
            ensure_source_line(&home.join(name), &source_line)?;
        }
    }
    Ok(())
}

/// Configure PATH on Windows: persist PEKO_ROOT_PATH and prepend the peko bin
/// directory to the user's PATH through PowerShell's Environment API, which
/// writes the user-scoped registry values a new shell picks up. There is no
/// shell rc file to source, so the Unix `env` script is not written.
#[cfg(windows)]
pub fn configure(layout: &Layout) -> Result<()> {
    let root = layout.root().display().to_string();
    let bin = layout.bin_peko().display().to_string();
    let script = format!(
        "[Environment]::SetEnvironmentVariable('PEKO_ROOT_PATH', '{root}', 'User'); \
         $p = [Environment]::GetEnvironmentVariable('PATH', 'User'); \
         if ($null -eq $p) {{ $p = '' }} \
         if ($p -notlike '*{bin}*') {{ \
           if ($p -eq '') {{ $np = '{bin}' }} else {{ $np = $p + ';' + '{bin}' }}; \
           [Environment]::SetEnvironmentVariable('PATH', $np, 'User') \
         }}"
    );
    let output = crate::proc::hidden("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .map_err(|e| SetupError::io("run powershell to configure PATH", e))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SetupError::PathConfig(format!(
            "could not set the user PATH: {}",
            stderr.trim()
        )));
    }
    Ok(())
}

#[cfg(not(windows))]
fn ensure_source_line(rc: &Path, source_line: &str) -> Result<()> {
    let existing = std::fs::read_to_string(rc).unwrap_or_default();
    if existing.contains(MARKER) {
        return Ok(());
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(rc)
        .map_err(|e| SetupError::PathConfig(format!("open {}: {e}", rc.display())))?;
    let prefix = if existing.is_empty() || existing.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    write!(file, "{prefix}{source_line}")
        .map_err(|e| SetupError::PathConfig(format!("write {}: {e}", rc.display())))?;
    Ok(())
}
