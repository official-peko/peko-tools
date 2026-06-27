//! `peko clangflags`: print the clang flags peko_core would pass when
//! compiling a C/C++/Objective-C source for a given target.
//!
//! The flags come from the installed toolchain's `toolchain.toml` (its target
//! triple, C++ standard, compile flags, and include directories), so a project
//! that brings native sources can invoke clang with the same flags Peko uses.

use std::process::ExitCode;

use peko_core::config::resolve_flag;
use peko_core::target::{Architecture, OperatingSystem};

use crate::cli::CLIInfo;
use crate::cli::reporting::Reporter;
use crate::toolchain::{InstallManifest, resolve_toolchain};

/// Execute the `clangflags` subcommand.
pub async fn execute(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    let target_operating_system = match require_os(cli_info, reporter) {
        Some(os) => os,
        None => return ExitCode::FAILURE,
    };
    let target_architecture = match require_arch(cli_info, reporter) {
        Some(arch) => arch,
        None => return ExitCode::FAILURE,
    };

    let peko_root = cli_info.get_peko_root();
    let manifest = match InstallManifest::load(peko_root) {
        Ok(manifest) => manifest,
        Err(e) => {
            reporter.error(format!("could not read versions.json: {e}"));
            return ExitCode::FAILURE;
        }
    };
    let resolved = match resolve_toolchain(
        peko_root,
        &manifest,
        target_operating_system,
        target_architecture,
    ) {
        Ok(resolved) => resolved,
        Err(e) => {
            reporter.error(format!("could not resolve toolchain: {e}"));
            return ExitCode::FAILURE;
        }
    };

    let build = &resolved.toolchain.build;
    let dir = &resolved.dir;

    let mut parts: Vec<String> = vec!["-c".to_owned()];

    // `--nostd` suppresses the `-std=` flag (useful when compiling plain C or
    // Objective-C).
    if !cli_info.flags.has_flag("nostd")
        && let Some(cxx_std) = &build.cxx_std
    {
        parts.push(format!("-std={cxx_std}"));
    }

    parts.push("-target".to_owned());
    parts.push(resolved.toolchain.meta.triple.clone());

    for flag in &build.c_flags {
        parts.push(resolve_flag(dir, flag));
    }
    for include in &build.include {
        parts.push(format!("-I{}", dir.join(include).display()));
    }

    // The flags are the command's product and go to stdout so they can be
    // captured; errors and help go to stderr via the reporter.
    println!("{}", parts.join(" "));
    ExitCode::SUCCESS
}

/// Validate and parse the `--os=<value>` flag. Reports the error through
/// `reporter` and returns `None` if missing or invalid.
fn require_os(cli_info: &CLIInfo, reporter: &Reporter) -> Option<OperatingSystem> {
    if !cli_info.flags.has_flag("os") {
        reporter.error(format!(
            "'{} clangflags' requires the 'os' flag",
            cli_info.executable
        ));
        reporter.help(format!(
            "run '{} help clangflags' to see how this command works",
            cli_info.executable
        ));
        return None;
    }

    let value = match cli_info.flags.get_flag("os") {
        Some(v) => v,
        None => {
            reporter.error("'os' flag requires a value");
            reporter.help(format!(
                "run '{} help clangflags' to see how this command works",
                cli_info.executable
            ));
            return None;
        }
    };

    match OperatingSystem::from_name(&value) {
        OperatingSystem::Unknown => {
            reporter.error(format!("'{value}' is not a valid Operating System target"));
            reporter.help(format!(
                "run '{} help clangflags' to see how this command works",
                cli_info.executable
            ));
            None
        }
        os => Some(os),
    }
}

/// Validate and parse the `--arch=<value>` flag. Reports the error through
/// `reporter` and returns `None` if missing or invalid.
fn require_arch(cli_info: &CLIInfo, reporter: &Reporter) -> Option<Architecture> {
    if !cli_info.flags.has_flag("arch") {
        reporter.error(format!(
            "'{} clangflags' requires the 'arch' flag",
            cli_info.executable
        ));
        reporter.help(format!(
            "run '{} help clangflags' to see how this command works",
            cli_info.executable
        ));
        return None;
    }

    let value = match cli_info.flags.get_flag("arch") {
        Some(v) => v,
        None => {
            reporter.error("'arch' flag requires a value");
            reporter.help(format!(
                "run '{} help clangflags' to see how this command works",
                cli_info.executable
            ));
            return None;
        }
    };

    match Architecture::from_name(&value) {
        Architecture::Unknown => {
            reporter.error(format!("'{value}' is not a valid CPU Architecture target"));
            reporter.help(format!(
                "run '{} help clangflags' to see how this command works",
                cli_info.executable
            ));
            None
        }
        arch => Some(arch),
    }
}
