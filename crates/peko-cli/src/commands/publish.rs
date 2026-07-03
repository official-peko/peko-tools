//! `peko publish`: pack a package and upload it to the registry.
//!
//! The project becomes a `.pkpkg` container that is verified locally, then
//! uploaded through the platform publish handshake in
//! `crate::registry::publish`. Authentication is the session from `peko login`.
//! The server reads the package metadata from the embedded `peko.toml`,
//! validates it, and queues the version for admin review.

use std::process::ExitCode;

use peko_core::config::{Manifest, ManifestKind};

use crate::cli::CLIInfo;
use crate::cli::reporting::Reporter;
use crate::registry::pack;

/// Execute the `publish` subcommand.
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

    // Verify the packed container before anything leaves the machine. A package
    // that fails verification must never be uploaded.
    let report = crate::registry::verify::verify(&bytes);
    for finding in &report.findings {
        match finding.severity {
            crate::registry::verify::Severity::Error => reporter.error(&finding.message),
            crate::registry::verify::Severity::Warning => reporter.warning(&finding.message),
        }
    }
    if !report.is_valid() {
        reporter.error(format!(
            "refusing to publish: the package failed verification with {} error(s)",
            report.error_count()
        ));
        reporter.help(format!("run '{} verify' for the full report", cli_info.executable));
        return ExitCode::FAILURE;
    }

    // Publishing requires a session from `peko login`.
    let Some(session) = crate::auth::Session::load() else {
        reporter.error("not logged in");
        reporter.help(format!(
            "run '{} login' to authenticate before publishing",
            cli_info.executable
        ));
        return ExitCode::FAILURE;
    };
    let base = crate::auth::platform_base(cli_info.flags.get_flag("base"));
    let id_token = match crate::auth::fresh_id_token(&session).await {
        Ok(token) => token,
        Err(crate::auth::AuthError::Unauthorized) => {
            reporter.error("session expired or revoked");
            reporter.help(format!(
                "run '{} login' to authenticate again",
                cli_info.executable
            ));
            return ExitCode::FAILURE;
        }
        Err(e) => {
            reporter.error(format!("could not authenticate: {e}"));
            return ExitCode::FAILURE;
        }
    };

    let progress = reporter.progress();
    progress.start_phase(&format!("Uploading {name} {version}"));
    let outcome = crate::registry::publish::publish(&base, &id_token, &bytes).await;
    progress.finish_phase();

    match outcome {
        Ok(done) => {
            reporter.success(format!(
                "published {} {} ({} bytes)",
                done.name,
                done.version,
                bytes.len()
            ));
            if done.status == "pending" {
                reporter.info(
                    "the version is pending admin review and appears on the public index once approved",
                );
            } else {
                reporter.info(format!("status: {}", done.status));
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            reporter.error(format!("publish failed: {e}"));
            ExitCode::FAILURE
        }
    }
}
