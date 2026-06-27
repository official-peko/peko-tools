//! `peko publish`: pack a package and upload it to the registry.
//!
//! Packing is implemented: the project becomes a `.pkpkg` container and its
//! checksum is computed. Uploading the blob to R2, appending the index line,
//! and mirroring a row to the search database are scaffolded until the web
//! platform is live. Authentication via `peko login` is withheld for now.

use std::process::ExitCode;

use peko_core::config::{Manifest, ManifestKind};

use crate::cli::CLIInfo;
use crate::cli::reporting::Reporter;
use crate::registry::pack;

/// Execute the `publish` subcommand.
pub async fn execute(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    let _ = cli_info;

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

    if loaded.manifest.kind() != ManifestKind::Package {
        reporter.error("only packages can be published");
        reporter.help("a publishable package defines a [package] and [lib] table");
        return ExitCode::FAILURE;
    }

    let name = loaded.manifest.name().to_owned();
    let version = loaded.manifest.version().to_string();

    let progress = reporter.progress();
    progress.start_phase(&format!("Packing {name} {version}"));
    let bytes = match pack::pack(&loaded) {
        Ok(bytes) => bytes,
        Err(e) => {
            progress.finish_phase();
            reporter.error(format!("could not pack package: {e}"));
            return ExitCode::FAILURE;
        }
    };
    progress.finish_phase();

    let checksum = pack::checksum(&bytes);

    // TODO(platform): once the web platform is live, upload `bytes` to R2 at
    // `blobs/<name>/<name>-<version>.pkpkg`, append an index line (with this
    // checksum, the dependency list, features, platforms, and min compiler) to
    // `index/<name>.jsonl`, and mirror a search row. This step is gated on
    // `peko login` for authentication, which is also withheld for now.
    reporter.warning("publishing is not available yet; the web platform is not live");
    reporter.info(format!(
        "packed {name} {version} ({} bytes, {checksum})",
        bytes.len()
    ));
    reporter.help("run 'peko pkg build' to write the .pkpkg locally for inspection");
    ExitCode::SUCCESS
}
