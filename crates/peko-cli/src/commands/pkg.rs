//! `peko pkg`: author and bundle host packages for distribution.
//!
//! Two subcommands:
//!
//! - `pkg new <name>` scaffolds a fresh package directory with a
//!   `Package.json`, `README.md`, and an initial `v0.1.0` version
//!   folder containing the standard `main.peko` / `deps.json` /
//!   `libs/` / `source/` layout.
//! - `pkg build` walks up to find the nearest enclosing package
//!   directory, runs the binary builder, and writes the resulting
//!   `.pkpkg` next to the current directory.

use std::io::{self, Seek};
use std::path::PathBuf;
use std::process::ExitCode;

use peko_core::packages::HostPackage;

use crate::cli::reporting::Reporter;
use crate::cli::CLIInfo;
use crate::packager::builder::PackageComponentBinaryBuilder;

/// `Package.json` template used by `pkg new`. `{name}` is substituted
/// at generation time.
const PACKAGE_JSON_TEMPLATE: &str = r#"{
    "name": "{name}",
    "label": "<how your package will show up in searches>",
    "latest": "v0.1.0",
    "description": "",
    "versions": ["v0.1.0"]
}
"#;

/// Maximum depth of the parent walk that `pkg build` does to find an
/// enclosing `Package.json`.
const PACKAGE_SEARCH_LIMIT: usize = 5;

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

/// `pkg new <name>`: scaffold a fresh package directory.
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

    // Clobber an existing folder/file at the same path only when
    // --force is set.
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

    // Scaffold the package layout. Any IO failure aborts cleanly.
    let dirs_to_create: &[&str] = &[".peko/packages", "v0.1.0/libs", "v0.1.0/source"];

    for dir in dirs_to_create {
        let path = package_folder.join(dir);
        if let Err(e) = std::fs::create_dir_all(&path) {
            reporter.error(format!("could not create {}: {e}", path.display()));
            return ExitCode::FAILURE;
        }
    }

    // Files to write at scaffold time. Each is `(relative_path,
    // contents)`. The Package.json gets its `{name}` placeholder filled
    // in here.
    let package_json = PACKAGE_JSON_TEMPLATE.replace("{name}", package_name);
    let files_to_write: &[(&str, &[u8])] = &[
        ("README.md", b""),
        ("Package.json", package_json.as_bytes()),
        ("v0.1.0/README.md", b""),
        ("v0.1.0/main.peko", b""),
        ("v0.1.0/deps.json", b"{}"),
    ];

    for (rel_path, bytes) in files_to_write {
        let path = package_folder.join(rel_path);
        if let Err(e) = std::fs::write(&path, bytes) {
            reporter.error(format!("could not write {}: {e}", path.display()));
            return ExitCode::FAILURE;
        }
    }

    let canonical = package_folder
        .canonicalize()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| package_folder.display().to_string());
    reporter.success(format!(
        "created new package '{package_name}' at {canonical}"
    ));
    ExitCode::SUCCESS
}

/// `pkg build`: assemble a `.pkpkg` for the nearest enclosing package.
fn execute_build(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    // Verify --version has a value if the flag was passed at all.
    if cli_info.flags.has_flag("version") && cli_info.flags.get_flag("version").is_none() {
        reporter.error("flag 'version' requires a value");
        reporter.help(format!(
            "run '{} help pkg' to see how this command works",
            cli_info.executable
        ));
        return ExitCode::FAILURE;
    }

    // Walk up from the current directory looking for Package.json,
    // bounded by PACKAGE_SEARCH_LIMIT levels.
    let mut package_folder = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            reporter.error(format!("cannot read current directory: {e}"));
            return ExitCode::FAILURE;
        }
    };
    let cwd = package_folder.clone();
    let mut search_remaining = PACKAGE_SEARCH_LIMIT;

    while search_remaining > 0
        && package_folder.parent().is_some()
        && !package_folder.join("Package.json").exists()
    {
        package_folder = package_folder
            .parent()
            .unwrap_or(&package_folder)
            .to_path_buf();
        search_remaining -= 1;
    }

    if !package_folder.join("Package.json").exists() {
        reporter.error("couldn't find a Package.json in the current directory or its parents");
        return ExitCode::FAILURE;
    }

    let package = match HostPackage::from_package_directory(&package_folder) {
        Ok(Some(p)) => p,
        Ok(None) => {
            reporter.error(format!(
                "could not load package from {}",
                package_folder.display()
            ));
            return ExitCode::FAILURE;
        }
        Err(e) => {
            reporter.error(format!(
                "could not load package from {}: {e}",
                package_folder.display()
            ));
            return ExitCode::FAILURE;
        }
    };

    let progress = reporter.progress();
    progress.start_phase(&format!("Building package {}", package.info.name));

    let mut package_binary = match package.build_binary(cli_info.flags.get_flag("version")) {
        Ok(tmp) => tmp,
        Err(e) => {
            progress.finish_phase();
            reporter.error(format!("could not build package binary: {e}"));
            return ExitCode::FAILURE;
        }
    };

    // Caller convention: seek to the start before reading the temp file.
    if let Err(e) = package_binary.seek(io::SeekFrom::Start(0)) {
        progress.finish_phase();
        reporter.error(format!("could not rewind package binary: {e}"));
        return ExitCode::FAILURE;
    }

    // Output: <pkgname>[_<version>].pkpkg, next to the current dir.
    let version_suffix = cli_info
        .flags
        .get_flag("version")
        .map(|v| format!("_{v}"))
        .unwrap_or_default();
    let package_binary_output = cwd.join(format!("{}{version_suffix}.pkpkg", package.info.name));

    if package_binary_output.exists() {
        if let Err(e) = std::fs::remove_file(&package_binary_output) {
            progress.finish_phase();
            reporter.error(format!(
                "could not remove existing {}: {e}",
                package_binary_output.display()
            ));
            return ExitCode::FAILURE;
        }
    }

    let mut output = match std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&package_binary_output)
    {
        Ok(f) => f,
        Err(e) => {
            progress.finish_phase();
            reporter.error(format!(
                "could not create {}: {e}",
                package_binary_output.display()
            ));
            return ExitCode::FAILURE;
        }
    };

    if let Err(e) = io::copy(&mut package_binary, &mut output) {
        progress.finish_phase();
        reporter.error(format!(
            "could not write {}: {e}",
            package_binary_output.display()
        ));
        return ExitCode::FAILURE;
    }

    progress.finish_phase();

    let canonical = package_binary_output
        .canonicalize()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| package_binary_output.display().to_string());
    reporter.success(format!(
        "built package {} to {}",
        package.info.name, canonical
    ));
    ExitCode::SUCCESS
}
