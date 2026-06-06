//! `peko add`: install a package into the project (or globally).

use std::process::ExitCode;

use crate::cli::reporting::Reporter;
use crate::cli::CLIInfo;
use crate::packager::manager::{PackageManager, Scope};

/// Execute the `add` subcommand against the user's working directory.
pub async fn execute(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    let Some(package_name) = cli_info.arguments.get(1) else {
        reporter.error("`add` requires a package name");
        reporter.help(format!(
            "run '{} help add' to see how this command works",
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
            reporter.error(format!("cannot install package here: {e}"));
            reporter.help(format!(
                "run '{} help add' to see how this command works",
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
                    "run '{} help add' to see how this command works",
                    cli_info.executable
                ));
                return ExitCode::FAILURE;
            }
        }
    } else {
        None
    };

    let progress = reporter.progress();
    progress.start_phase(&format!("Installing package {package_name}"));

    let installation_result = match &requested_version {
        Some(version) => {
            packager
                .install_version(package_name, version, progress)
                .await
        }
        None => packager.install_package(package_name, progress).await,
    };

    progress.finish_phase();

    if let Err(e) = installation_result {
        reporter.error(format!("install failed: {e}"));
        return ExitCode::FAILURE;
    }

    reporter.success(format!("installed package '{package_name}'"));
    ExitCode::SUCCESS
}
