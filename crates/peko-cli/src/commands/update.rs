//! `peko update`: update an installed package to a newer version.

use std::process::ExitCode;

use crate::cli::reporting::Reporter;
use crate::cli::CLIInfo;
use crate::packager::manager::{PackageManager, Scope};

/// Execute the `update` subcommand.
pub async fn execute(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    let Some(package_name) = cli_info.arguments.get(1) else {
        reporter.error("`update` requires a package name");
        reporter.help(format!(
            "run '{} help update' to see how this command works",
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
            reporter.error(format!("cannot update package here: {e}"));
            reporter.help(format!(
                "run '{} help update' to see how this command works",
                cli_info.executable
            ));
            return ExitCode::FAILURE;
        }
    };

    let progress = reporter.progress();
    progress.start_phase(&format!("Updating package {package_name}"));

    let update_result = packager.update_package(package_name, progress).await;

    progress.finish_phase();

    if let Err(e) = update_result {
        reporter.error(format!("update failed: {e}"));
        return ExitCode::FAILURE;
    }

    reporter.success(format!("updated package '{package_name}'"));
    ExitCode::SUCCESS
}
