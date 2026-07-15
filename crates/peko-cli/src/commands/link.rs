//! `peko link`: connect the project to a platform app id.
//!
//! Writes `app_id` under `[project]` in `peko.toml` (preserving formatting and
//! comments) so `peko deploy server` knows which hosted app to deploy to. The id
//! comes from the app's page on the Peko dashboard. With no argument, the
//! command reports the current link. Linking is a local edit; it needs no
//! network or session.

use std::process::ExitCode;

use peko_core::config::{MANIFEST_FILE, Manifest, ManifestKind};

use crate::cli::CLIInfo;
use crate::cli::reporting::Reporter;

/// Execute the `link` subcommand.
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

    // The app id lives under [project], so only an application can be linked.
    if loaded.manifest.kind() == ManifestKind::Package {
        reporter.error("only an app can be linked to a platform app");
        reporter.help("a package has no [project] table to hold an app id");
        return ExitCode::FAILURE;
    }

    let current = match &loaded.manifest {
        Manifest::Application(app) => app.project.app_id.clone(),
        Manifest::Package(_) => None,
    };

    // With no id, report the current link and stop.
    let Some(app_id) = cli_info.arguments.get(1) else {
        match current {
            Some(id) => reporter.info(format!("linked to app {id}")),
            None => {
                reporter.info("not linked to a platform app");
                reporter.help(format!(
                    "run '{} link <app-id>' with the id from your app's dashboard",
                    cli_info.executable
                ));
            }
        }
        return ExitCode::SUCCESS;
    };

    let app_id = app_id.trim();
    if app_id.is_empty() {
        reporter.error("the app id is empty");
        reporter.help(format!("run '{} link <app-id>'", cli_info.executable));
        return ExitCode::FAILURE;
    }

    let manifest_path = loaded.root.join(MANIFEST_FILE);
    if let Err(e) = Manifest::write_app_id(&manifest_path, app_id) {
        reporter.error(format!("could not update peko.toml: {e}"));
        return ExitCode::FAILURE;
    }

    if current.as_deref() == Some(app_id) {
        reporter.info(format!("already linked to app {app_id}"));
    } else {
        reporter.success(format!("linked to app {app_id}"));
    }
    reporter.info(format!(
        "run '{} deploy server' to deploy to server hosting",
        cli_info.executable
    ));
    ExitCode::SUCCESS
}
