//! `peko verify`: scan a `.pkpkg` container and report what it holds.
//!
//! With a path argument the command verifies that container file. With no
//! argument it packs the enclosing project in memory and verifies the result,
//! so a package can be checked before it is written or published. It prints the
//! container header, the manifest keys, and the packed file tree, then every
//! problem found; it exits non-zero when any error is present.

use std::path::Path;
use std::process::ExitCode;

use peko_core::config::{Compression, Manifest};

use crate::cli::CLIInfo;
use crate::cli::reporting::Reporter;
use crate::registry::pack;
use crate::registry::verify::{PackageReport, Severity};

/// Execute the `verify` subcommand.
pub async fn execute(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    let Some((label, bytes)) = load_container(cli_info, reporter) else {
        return ExitCode::FAILURE;
    };

    let report = pack_report(&bytes);
    render(&report, &label, reporter);

    if report.is_valid() {
        reporter.success(format!(
            "package is valid ({} error(s), {} warning(s))",
            report.error_count(),
            report.warning_count()
        ));
        ExitCode::SUCCESS
    } else {
        reporter.error(format!(
            "package failed verification with {} error(s)",
            report.error_count()
        ));
        ExitCode::FAILURE
    }
}

/// Verify container `bytes`, exposed so `publish` can reuse the same pass.
pub fn pack_report(bytes: &[u8]) -> PackageReport {
    crate::registry::verify::verify(bytes)
}

/// Resolve the container bytes to verify: a `.pkpkg` file given as an argument,
/// or the current project packed in memory when no argument is given.
fn load_container(cli_info: &CLIInfo, reporter: &Reporter) -> Option<(String, Vec<u8>)> {
    if let Some(argument) = cli_info.arguments.get(1) {
        let path = Path::new(argument);
        match std::fs::read(path) {
            Ok(bytes) => Some((path.display().to_string(), bytes)),
            Err(error) => {
                reporter.error(format!("could not read {}: {error}", path.display()));
                None
            }
        }
    } else {
        let cwd = match std::env::current_dir() {
            Ok(dir) => dir,
            Err(error) => {
                reporter.error(format!("cannot read current directory: {error}"));
                return None;
            }
        };
        let loaded = match Manifest::discover(&cwd) {
            Ok(loaded) => loaded,
            Err(error) => {
                reporter.error(format!("could not load a peko.toml here: {error}"));
                reporter.help("pass a path to a .pkpkg file, or run this inside a package");
                return None;
            }
        };
        match pack::pack(&loaded) {
            Ok(bytes) => Some((format!("{} (packed in memory)", loaded.manifest.name()), bytes)),
            Err(error) => {
                reporter.error(format!("could not pack the project: {error}"));
                None
            }
        }
    }
}

/// Print the report body and every finding.
fn render(report: &PackageReport, label: &str, reporter: &Reporter) {
    let title = match &report.manifest {
        Some(manifest) => format!("{} {}", manifest.name, manifest.version),
        None => "unknown package".to_string(),
    };
    reporter.status("Verifying", format!("{title}  <- {label}"));
    reporter.raw("");

    // Container header.
    let compression = match report.compression {
        Compression::None => "stored",
        Compression::Zstd => "zstd",
    };
    let signing = if report.signed {
        format!("signed ({} byte signature)", report.signature_len)
    } else {
        "unsigned".to_string()
    };
    field(reporter, "container", format!(
        "format v{}, {compression}, {signing}",
        report.container_version
    ));

    // Manifest keys.
    if let Some(manifest) = &report.manifest {
        field(reporter, "manifest", format!("{:?}", manifest.kind));
        field(reporter, "name", &manifest.name);
        field(reporter, "version", &manifest.version);
        field(reporter, "description", present(manifest.description.as_deref()));
        field(reporter, "license", present(manifest.license.as_deref()));
        field(reporter, "authors", list(&manifest.authors));
        field(reporter, "repository", present(manifest.repository.as_deref()));
        field(reporter, "keywords", list(&manifest.keywords));
        field(reporter, "categories", list(&manifest.categories));
        field(reporter, "min compiler", present(manifest.min_compiler.as_deref()));
        if let Some(root) = &manifest.lib_root {
            field(reporter, "lib entry", root);
        }
        field(reporter, "platforms", list(&manifest.platforms));
        field(reporter, "native", if manifest.native_sources == 0 {
            "none".to_string()
        } else {
            format!("{} C source(s)", manifest.native_sources)
        });

        if manifest.dependencies.is_empty() {
            field(reporter, "dependencies", "none".to_string());
        } else {
            field(reporter, "dependencies", format!("{}", manifest.dependencies.len()));
            for (name, spec) in &manifest.dependencies {
                reporter.raw(format!("{:>indent$}    {name} = {spec}", "", indent = LABEL_WIDTH));
            }
        }
    }

    // Payload.
    if let Some(payload) = &report.payload {
        field(reporter, "payload", format!(
            "{} file(s), {} uncompressed{}",
            payload.files.len(),
            human_size(payload.uncompressed_size),
            if payload.entry_present { ", entry present" } else { "" }
        ));
    }

    field(reporter, "checksum", &report.checksum);
    field(reporter, "size", human_size(report.file_size as u64));
    reporter.raw("");

    // Findings.
    for finding in &report.findings {
        match finding.severity {
            Severity::Error => reporter.error(&finding.message),
            Severity::Warning => reporter.warning(&finding.message),
        }
    }
    if report.findings.is_empty() {
        reporter.info("no problems found");
    }
}

/// Column width for the aligned `label  value` rows.
const LABEL_WIDTH: usize = 14;

/// Print one aligned `label  value` row.
fn field(reporter: &Reporter, label: &str, value: impl std::fmt::Display) {
    reporter.raw(format!("{label:>LABEL_WIDTH$}  {value}"));
}

/// An optional string rendered for display, or a dim placeholder when absent.
fn present(value: Option<&str>) -> String {
    match value {
        Some(value) if !value.trim().is_empty() => value.to_string(),
        _ => "(none)".to_string(),
    }
}

/// A comma-joined list, or a placeholder when empty.
fn list(values: &[String]) -> String {
    if values.is_empty() {
        "(none)".to_string()
    } else {
        values.join(", ")
    }
}

/// A byte count rendered in a human-friendly unit.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}
