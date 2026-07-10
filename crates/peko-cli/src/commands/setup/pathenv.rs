//! PATH configuration: write the `env` sourcing script and add a source line to
//! the user's shell rc files, so `peko` is on PATH in new shells.

use std::io::Write;
use std::path::Path;

use super::layout::Layout;
use super::{Result, SetupError};

const MARKER: &str = "# added by peko setup";
const ENV_CONTENTS: &str =
    "export PEKO_ROOT_PATH=\"$HOME/.Peko\"\nexport PATH=\"$HOME/.Peko/Compiler/bin/peko:$PATH\"\n";

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
