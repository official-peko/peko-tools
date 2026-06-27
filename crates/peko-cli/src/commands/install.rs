//! `peko install`: resolve, download, and lock the project's dependencies.
//!
//! Reads `[dependencies]` from the project's `peko.toml`, resolves exact
//! versions against the registry index, downloads and verifies each `.pkpkg`,
//! unpacks it into the shared source cache, and writes `peko.lock`.

use std::process::ExitCode;

use peko_core::config::Manifest;

use crate::cli::CLIInfo;
use crate::cli::reporting::Reporter;
use crate::registry::install;

/// Execute the `install` subcommand.
pub async fn execute(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    let cwd = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(e) => {
            reporter.error(format!("cannot read current directory: {e}"));
            return ExitCode::FAILURE;
        }
    };

    let loaded = match Manifest::discover(&cwd) {
        Ok(loaded) => loaded,
        Err(e) => {
            reporter.error(format!("could not load a peko.toml here: {e}"));
            return ExitCode::FAILURE;
        }
    };

    let progress = reporter.progress();
    progress.start_phase("Installing dependencies");
    let result = install::install(cli_info.get_peko_root(), &loaded, progress).await;
    progress.finish_phase();

    match result {
        Ok(lockfile) => {
            reporter.success(format!("locked {} package(s)", lockfile.packages.len()));
            ExitCode::SUCCESS
        }
        Err(e) => {
            reporter.error(format!("install failed: {e}"));
            ExitCode::FAILURE
        }
    }
}
