//! `peko add`: declare a dependency in `peko.toml` and install it.
//!
//! Writes the dependency into the `[dependencies]` table (a registry version
//! requirement, or a local `{ path = ... }` with `--path`), then re-resolves
//! and refreshes `peko.lock`.

use std::process::ExitCode;

use peko_core::config::{DependencySpec, MANIFEST_FILE, Manifest};

use crate::cli::CLIInfo;
use crate::cli::reporting::Reporter;
use crate::registry::install;

/// Execute the `add` subcommand.
pub async fn execute(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    let Some(name) = cli_info.arguments.get(1) else {
        reporter.error("`add` requires a package name");
        reporter.help(format!(
            "run '{} help add' to see how this command works",
            cli_info.executable
        ));
        return ExitCode::FAILURE;
    };

    let spec = match dependency_spec(cli_info, reporter) {
        Ok(spec) => spec,
        Err(code) => return code,
    };

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
    if let Err(e) = Manifest::add_dependency(&manifest_path, name, &spec) {
        reporter.error(format!("could not update peko.toml: {e}"));
        return ExitCode::FAILURE;
    }
    reporter.info(format!("added {name} to peko.toml"));

    resolve_after_edit(cli_info, reporter, &cwd, name, "added").await
}

/// Build the dependency spec from the `--path` / `--version` flags.
fn dependency_spec(cli_info: &CLIInfo, reporter: &Reporter) -> Result<DependencySpec, ExitCode> {
    if cli_info.flags.has_flag("path") {
        return match cli_info.flags.get_flag("path") {
            Some(path) => Ok(DependencySpec::Path(path)),
            None => {
                reporter.error("flag 'path' requires a value");
                Err(ExitCode::FAILURE)
            }
        };
    }

    if cli_info.flags.has_flag("version") {
        return match cli_info.flags.get_flag("version") {
            Some(version) => Ok(DependencySpec::Version(version)),
            None => {
                reporter.error("flag 'version' requires a value");
                Err(ExitCode::FAILURE)
            }
        };
    }

    Ok(DependencySpec::Version(String::from("*")))
}

/// Re-resolve the project after a manifest edit, reporting the outcome.
async fn resolve_after_edit(
    cli_info: &CLIInfo,
    reporter: &Reporter,
    cwd: &std::path::Path,
    name: &str,
    verb: &str,
) -> ExitCode {
    let loaded = match Manifest::discover(cwd) {
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
            reporter.success(format!("{verb} {name}"));
            ExitCode::SUCCESS
        }
        Err(e) => {
            reporter.error(format!("{verb} {name} in peko.toml, but resolution failed: {e}"));
            reporter.help(format!(
                "run '{} install' once the registry is reachable",
                cli_info.executable
            ));
            ExitCode::FAILURE
        }
    }
}
