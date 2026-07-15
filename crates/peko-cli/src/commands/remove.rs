//! `peko remove`: drop a dependency from `peko.toml` and refresh the lockfile.
//!
//! Deletes the dependency's entry from `[dependencies]`, then re-resolves and
//! rewrites `peko.lock` so the removed package no longer appears.

use std::process::ExitCode;

use peko_core::config::{MANIFEST_FILE, Manifest};

use crate::cli::CLIInfo;
use crate::cli::reporting::Reporter;
use crate::registry::install;

/// Execute the `remove` subcommand.
pub async fn execute(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    let Some(name) = cli_info.arguments.get(1) else {
        reporter.error("`remove` requires a package name");
        reporter.help(format!(
            "run '{} help remove' to see how this command works",
            cli_info.executable
        ));
        return ExitCode::FAILURE;
    };

    // A global remove targets the shared global manifest, mirroring `add --global`.
    if cli_info.flags.has_flag("global") {
        return remove_global(cli_info, reporter, name).await;
    }

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

    let manifest_path = loaded.root.join(MANIFEST_FILE);
    match Manifest::remove_dependency(&manifest_path, name) {
        Ok(true) => reporter.info(format!("removed {name} from peko.toml")),
        Ok(false) => {
            reporter.info(format!("{name} is not a dependency in peko.toml"));
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            reporter.error(format!("could not update peko.toml: {e}"));
            return ExitCode::FAILURE;
        }
    }

    let loaded = match Manifest::discover(&cwd) {
        Ok(loaded) => loaded,
        Err(e) => {
            reporter.error(format!("could not reload peko.toml: {e}"));
            return ExitCode::FAILURE;
        }
    };

    let progress = reporter.progress();
    progress.start_phase("Resolving dependencies");
    let result = install::update(cli_info.get_peko_root(), &loaded, progress).await;
    progress.finish_phase();

    match result {
        Ok(_) => {
            reporter.success(format!("removed {name}"));
            ExitCode::SUCCESS
        }
        Err(e) => {
            reporter.error(format!(
                "removed {name} from peko.toml, but resolution failed: {e}"
            ));
            reporter.help(format!(
                "run '{} install' once the registry is reachable",
                cli_info.executable
            ));
            ExitCode::FAILURE
        }
    }
}

/// Remove a package from the shared global manifest under the Peko root and
/// re-resolve it (mirrors `add --global`).
async fn remove_global(cli_info: &CLIInfo, reporter: &Reporter, name: &str) -> ExitCode {
    let peko_root = cli_info.get_peko_root();
    let global_dir = peko_root.join("global");
    let manifest_path = global_dir.join(MANIFEST_FILE);
    if !manifest_path.exists() {
        reporter.info(format!("{name} is not a global package"));
        return ExitCode::SUCCESS;
    }

    match Manifest::remove_dependency(&manifest_path, name) {
        Ok(true) => reporter.info(format!("removed {name} from the global packages")),
        Ok(false) => {
            reporter.info(format!("{name} is not a global package"));
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            reporter.error(format!("could not update the global manifest: {e}"));
            return ExitCode::FAILURE;
        }
    }

    let loaded = match Manifest::discover(&global_dir) {
        Ok(loaded) => loaded,
        Err(e) => {
            reporter.error(format!("could not reload the global manifest: {e}"));
            return ExitCode::FAILURE;
        }
    };

    let progress = reporter.progress();
    progress.start_phase("Resolving global packages");
    let result = install::update(peko_root, &loaded, progress).await;
    progress.finish_phase();

    match result {
        Ok(_) => {
            reporter.success(format!("removed {name} globally"));
            ExitCode::SUCCESS
        }
        Err(e) => {
            reporter.error(format!(
                "removed {name} from the global packages, but resolution failed: {e}"
            ));
            ExitCode::FAILURE
        }
    }
}
