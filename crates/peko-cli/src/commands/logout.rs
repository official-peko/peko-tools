//! `peko logout`: clear the stored CLI session from the keychain.

use std::process::ExitCode;

use crate::auth;
use crate::cli::CLIInfo;
use crate::cli::reporting::Reporter;

/// Execute the `logout` subcommand.
pub async fn execute(_cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    if auth::Session::load().is_none() {
        reporter.info("no active session");
        return ExitCode::SUCCESS;
    }
    match auth::Session::clear() {
        Ok(()) => {
            reporter.success("logged out");
            ExitCode::SUCCESS
        }
        Err(e) => {
            reporter.error(format!("could not clear the session: {e}"));
            ExitCode::FAILURE
        }
    }
}
