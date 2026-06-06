//! `peko check`: verify the Peko toolchain installation is healthy.

use std::process::ExitCode;

use crate::cli::reporting::Reporter;
use crate::cli::CLIInfo;

/// Execute the `check` subcommand.
pub async fn execute(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    if cli_info.perform_deep_root_checkup() {
        reporter.success("Peko toolchain installation looks healthy");
        ExitCode::SUCCESS
    } else {
        reporter.error("Peko toolchain installation is missing files or is misconfigured");
        // TODO: when a repair / reinstall command lands, point at it
        // here. Previously the cli suggested `configure` but that
        // command no longer exists.
        reporter.help("reinstall the Peko toolchain to fix this");
        ExitCode::FAILURE
    }
}
