//! `peko pkg`: author and pack library packages for distribution.
//!
//! - `pkg new <name>` scaffolds a fresh library: a `peko.toml` with
//!   `[package]` and `[lib]`, plus a `source/lib.peko` entry.
//! - `pkg build` packs the enclosing project into a `.pkpkg` container
//!   (`zstd(tar(source))` with the verbatim `peko.toml` embedded) next to the
//!   current directory.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use peko_core::config::Manifest;

use crate::cli::CLIInfo;
use crate::cli::reporting::Reporter;
use crate::registry::pack;

/// The `peko.toml` scaffolded by `pkg new`. `{name}` is filled in at
/// generation time.
const PEKO_TOML_TEMPLATE: &str = "[package]\n\
                                  name = \"{name}\"\n\
                                  version = \"0.1.0\"\n\
                                  description = \"\"\n\
                                  \n\
                                  [lib]\n\
                                  root = \"source/lib.peko\"\n\
                                  \n\
                                  [dependencies]\n";

/// Execute the `pkg` subcommand.
pub async fn execute(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    let Some(subcommand) = cli_info.arguments.get(1) else {
        reporter.error("`pkg` requires a subcommand");
        reporter.help(format!(
            "run '{} help pkg' to see how this command works",
            cli_info.executable
        ));
        return ExitCode::FAILURE;
    };

    match subcommand.as_str() {
        "new" => execute_new(cli_info, reporter),
        "build" => execute_build(cli_info, reporter),
        other => {
            reporter.error(format!("no such subcommand '{other}' for 'pkg' command"));
            reporter.help(format!(
                "run '{} help pkg' to see a list of valid subcommands",
                cli_info.executable
            ));
            ExitCode::FAILURE
        }
    }
}

/// `pkg new <name>`: scaffold a fresh library package directory.
fn execute_new(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    let Some(package_name) = cli_info.arguments.get(2) else {
        reporter.error("`pkg new` requires a package name");
        reporter.help(format!(
            "run '{} help pkg' to see how this command works",
            cli_info.executable
        ));
        return ExitCode::FAILURE;
    };

    let package_folder = PathBuf::from(package_name);
    if package_folder.exists() {
        if !cli_info.flags.has_flag("force") {
            reporter.error(format!(
                "package '{package_name}' already exists in the current directory"
            ));
            reporter.help(format!(
                "run '{} pkg new {package_name} --force' to overwrite",
                cli_info.executable
            ));
            return ExitCode::FAILURE;
        }
        reporter.info(format!(
            "--force specified, removing existing '{package_name}'"
        ));
        let removal = if package_folder.is_dir() {
            std::fs::remove_dir_all(&package_folder)
        } else {
            std::fs::remove_file(&package_folder)
        };
        if let Err(e) = removal {
            reporter.error(format!("could not remove existing '{package_name}': {e}"));
            return ExitCode::FAILURE;
        }
    }

    if let Err(e) = std::fs::create_dir_all(package_folder.join("source")) {
        reporter.error(format!("could not create {}: {e}", package_folder.display()));
        return ExitCode::FAILURE;
    }

    let manifest = PEKO_TOML_TEMPLATE.replace("{name}", package_name);
    let files: &[(&str, &[u8])] = &[
        ("peko.toml", manifest.as_bytes()),
        ("README.md", b""),
        ("source/lib.peko", b""),
    ];
    for (relative, bytes) in files {
        let path = package_folder.join(relative);
        if let Err(e) = std::fs::write(&path, bytes) {
            reporter.error(format!("could not write {}: {e}", path.display()));
            return ExitCode::FAILURE;
        }
    }

    reporter.success(format!(
        "created new package '{package_name}' at {}",
        display_path(&package_folder)
    ));
    ExitCode::SUCCESS
}

/// `pkg build`: pack the enclosing project into a `.pkpkg`.
fn execute_build(_cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
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

    let progress = reporter.progress();
    progress.start_phase(&format!("Packing {}", loaded.manifest.name()));
    let bytes = match pack::pack(&loaded) {
        Ok(bytes) => bytes,
        Err(e) => {
            progress.finish_phase();
            reporter.error(format!("could not pack package: {e}"));
            return ExitCode::FAILURE;
        }
    };
    progress.finish_phase();

    let output = cwd.join(format!(
        "{}-{}.pkpkg",
        loaded.manifest.name(),
        loaded.manifest.version()
    ));
    if let Err(e) = std::fs::write(&output, &bytes) {
        reporter.error(format!("could not write {}: {e}", output.display()));
        return ExitCode::FAILURE;
    }

    reporter.success(format!("packed package to {}", display_path(&output)));
    ExitCode::SUCCESS
}

/// The canonical display form of a path, falling back to its lexical form.
fn display_path(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}
