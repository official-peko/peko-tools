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
    let Some(raw_name) = cli_info.arguments.get(1) else {
        reporter.error("`add` requires a package name");
        reporter.help(format!(
            "run '{} help add' to see how this command works",
            cli_info.executable
        ));
        return ExitCode::FAILURE;
    };

    // Support the `name@version` form (for example `peko add std@0.1.1`). Without
    // this the whole `name@version` string is treated as the package name and
    // never resolves.
    let (name, inline_version) = match raw_name.split_once('@') {
        Some((n, v)) if !n.is_empty() && !v.is_empty() => (n, Some(v)),
        _ => (raw_name.as_str(), None),
    };

    let spec = match dependency_spec(cli_info, reporter, inline_version) {
        Ok(spec) => spec,
        Err(code) => return code,
    };

    // A global install goes into the shared global manifest instead of the
    // project, so the package is importable from every project (like std).
    if cli_info.flags.has_flag("global") {
        return add_global(cli_info, reporter, name, &spec).await;
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
    if let Err(e) = Manifest::add_dependency(&manifest_path, name, &spec) {
        reporter.error(format!("could not update peko.toml: {e}"));
        return ExitCode::FAILURE;
    }
    reporter.info(format!("added {name} to peko.toml"));

    resolve_after_edit(cli_info, reporter, &cwd, name, "added").await
}

/// Install a package into the shared global manifest under the Peko root and
/// resolve it. Globally installed packages are importable from every project.
async fn add_global(
    cli_info: &CLIInfo,
    reporter: &Reporter,
    name: &str,
    spec: &DependencySpec,
) -> ExitCode {
    let peko_root = cli_info.get_peko_root();
    let global_dir = peko_root.join("global");
    let manifest_path = global_dir.join(MANIFEST_FILE);

    // Seed a minimal global library manifest on first use.
    if !manifest_path.exists() {
        if let Err(e) = std::fs::create_dir_all(&global_dir) {
            reporter.error(format!("could not create the global packages directory: {e}"));
            return ExitCode::FAILURE;
        }
        let seed = "[package]\nname = \"peko-global\"\nversion = \"0.0.0\"\n\n[dependencies]\n";
        if let Err(e) = std::fs::write(&manifest_path, seed) {
            reporter.error(format!("could not create the global manifest: {e}"));
            return ExitCode::FAILURE;
        }
    }

    if let Err(e) = Manifest::add_dependency(&manifest_path, name, spec) {
        reporter.error(format!("could not update the global manifest: {e}"));
        return ExitCode::FAILURE;
    }
    reporter.info(format!("added {name} to the global packages"));

    let loaded = match Manifest::discover(&global_dir) {
        Ok(loaded) => loaded,
        Err(e) => {
            reporter.error(format!("could not reload the global manifest: {e}"));
            return ExitCode::FAILURE;
        }
    };

    let progress = reporter.progress();
    progress.start_phase("Installing globally");
    let result = install::update(peko_root, &loaded, progress).await;
    progress.finish_phase();

    match result {
        Ok(_) => {
            reporter.success(format!("installed {name} globally"));
            ExitCode::SUCCESS
        }
        Err(e) => {
            reporter.error(format!(
                "added {name} to the global packages, but resolution failed: {e}"
            ));
            reporter.help(format!(
                "run '{} install' once the registry is reachable",
                cli_info.executable
            ));
            ExitCode::FAILURE
        }
    }
}

/// Build the dependency spec from the `--path` / `--version` flags, or an
/// inline `name@version` requirement. Precedence: `--path`, then `--version`,
/// then the inline version, then any version (`*`).
fn dependency_spec(
    cli_info: &CLIInfo,
    reporter: &Reporter,
    inline_version: Option<&str>,
) -> Result<DependencySpec, ExitCode> {
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

    if let Some(version) = inline_version {
        return Ok(DependencySpec::Version(version.to_string()));
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
