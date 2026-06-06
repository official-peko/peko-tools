//! `peko remove`: uninstall a package from the project (or globally).

use std::process::ExitCode;

use crate::cli::reporting::Reporter;
use crate::cli::CLIInfo;
use crate::packager::manager::{PackageManager, Scope};

/// Execute the `remove` subcommand.
pub async fn execute(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    let Some(package_name) = cli_info.arguments.get(1) else {
        reporter.error("`remove` requires a package name");
        reporter.help(format!(
            "run '{} help remove' to see how this command works",
            cli_info.executable
        ));
        return ExitCode::FAILURE;
    };

    let scope = if cli_info.flags.has_flag("global") {
        Scope::Global
    } else {
        Scope::Local
    };

    let current_dir = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(e) => {
            reporter.error(format!("cannot read current directory: {e}"));
            return ExitCode::FAILURE;
        }
    };

    let packager = match PackageManager::new(scope, current_dir) {
        Ok(packager) => packager,
        Err(e) => {
            reporter.error(format!("cannot remove package here: {e}"));
            reporter.help(format!(
                "run '{} help remove' to see how this command works",
                cli_info.executable
            ));
            return ExitCode::FAILURE;
        }
    };

    // If `--version` was passed, resolve its value up front so we can
    // fail fast on a missing value.
    let requested_version = if cli_info.flags.has_flag("version") {
        match cli_info.flags.get_flag("version") {
            Some(v) => Some(v),
            None => {
                reporter.error("flag 'version' requires a value");
                reporter.help(format!(
                    "run '{} help remove' to see how this command works",
                    cli_info.executable
                ));
                return ExitCode::FAILURE;
            }
        }
    } else {
        None
    };

    let progress = reporter.progress();
    progress.start_phase(&format!("Removing package {package_name}"));

    let removal_result = match &requested_version {
        Some(version) => {
            packager
                .remove_version(package_name, version, progress)
                .await
        }
        None => packager.remove_package(package_name, progress).await,
    };

    progress.finish_phase();

    if let Err(e) = removal_result {
        reporter.error(format!("remove failed: {e}"));
        return ExitCode::FAILURE;
    }

    reporter.success(format!("removed package '{package_name}'"));
    ExitCode::SUCCESS
}
