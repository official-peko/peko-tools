//! `peko version`: print the cli version and exit.

use std::process::ExitCode;

use crate::cli::reporting::Reporter;
use crate::cli::CLIInfo;

/// Execute the `version` subcommand.
pub async fn execute(_cli_info: &CLIInfo, _reporter: &Reporter) -> ExitCode {
    // Pulled from Cargo.toml at compile time so the cli's reported
    // version always tracks the crate's actual version.
    println!("peko {}", env!("CARGO_PKG_VERSION"));
    ExitCode::SUCCESS
}
