//! `peko test`: type-check a Pekoscript file without producing
//! output.
//!
//! This is a fast pass that runs the parser and the type / semantic
//! simulator, then prints diagnostics. No code is generated, no object
//! file is produced, no linker is invoked.

use std::path::PathBuf;
use std::process::ExitCode;

use peko_core::target::{Architecture, OperatingSystem, PekoTarget};

use crate::cli::reporting::Reporter;
use crate::cli::CLIInfo;
use crate::execution;
use crate::project::PekoProject;

/// Execute the `test` subcommand.
pub async fn execute(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    let Some(source_arg) = cli_info.arguments.get(1) else {
        reporter.error("`test` requires a path to a Pekoscript source file");
        reporter.help(format!(
            "run '{} help test' to see how this command works",
            cli_info.executable
        ));
        return ExitCode::FAILURE;
    };

    // ---- Target resolution -----------------------------------------------
    // Target only affects which platform-specific imports the type
    // checker walks; we don't actually codegen here.
    let target_operating_system = match resolve_os(cli_info, reporter) {
        Some(os) => os,
        None => return ExitCode::FAILURE,
    };
    let target_architecture = match resolve_arch(cli_info, reporter) {
        Some(arch) => arch,
        None => return ExitCode::FAILURE,
    };

    let test_target = PekoTarget::new(target_operating_system, target_architecture, true);

    // ---- Project root resolution -----------------------------------------
    let main_file = PathBuf::from(source_arg);
    let canonical_parent = match main_file.canonicalize() {
        Ok(p) => p
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from(".")),
        Err(e) => {
            reporter.error(format!(
                "cannot resolve path '{}': {e}",
                main_file.display()
            ));
            return ExitCode::FAILURE;
        }
    };
    let compilation_root = PekoProject::from_directory(&canonical_parent)
        .map(|p| p.get_root().to_path_buf())
        .unwrap_or(canonical_parent);

    // ---- Type-check ------------------------------------------------------
    let progress = reporter.progress();
    progress.start_phase(&format!("Type-checking {source_arg}"));

    let outcome = match execution::test(
        cli_info.get_peko_root(),
        test_target,
        main_file,
        compilation_root,
    ) {
        Ok(o) => o,
        Err(e) => {
            progress.finish_phase();
            reporter.error(format!("type-check failed: {e}"));
            return ExitCode::FAILURE;
        }
    };

    progress.finish_phase();

    // ---- Surface diagnostics --------------------------------------------
    reporter.report_diagnostics(&outcome.diagnostics);

    if outcome.diagnostics.get_error_count() > 0 {
        let count = outcome.diagnostics.get_error_count();
        let plural = if count == 1 { "" } else { "s" };
        reporter.error(format!("{source_arg} has {count} error{plural}"));
        return ExitCode::FAILURE;
    }

    reporter.success(format!("{source_arg} has no errors"));
    ExitCode::SUCCESS
}

// ---------------------------------------------------------------------------
// Flag parsing helpers (mirroring compile.rs)
// ---------------------------------------------------------------------------

fn resolve_os(cli_info: &CLIInfo, reporter: &Reporter) -> Option<OperatingSystem> {
    if !cli_info.flags.has_flag("os") {
        return Some(PekoTarget::default().operating_system);
    }
    let value = match cli_info.flags.get_flag("os") {
        Some(v) => v,
        None => {
            reporter.error("'os' flag requires a value");
            reporter.help(format!(
                "run '{} help test' to see how this command works",
                cli_info.executable
            ));
            return None;
        }
    };
    match OperatingSystem::from_name(&value) {
        OperatingSystem::Unknown => {
            reporter.error(format!("'{value}' is not a valid Operating System target"));
            reporter.help(format!(
                "run '{} help test' to see how this command works",
                cli_info.executable
            ));
            None
        }
        os => Some(os),
    }
}

fn resolve_arch(cli_info: &CLIInfo, reporter: &Reporter) -> Option<Architecture> {
    if !cli_info.flags.has_flag("arch") {
        return Some(PekoTarget::default().architecture);
    }
    let value = match cli_info.flags.get_flag("arch") {
        Some(v) => v,
        None => {
            reporter.error("'arch' flag requires a value");
            reporter.help(format!(
                "run '{} help test' to see how this command works",
                cli_info.executable
            ));
            return None;
        }
    };
    match Architecture::from_name(&value) {
        Architecture::Unknown => {
            reporter.error(format!("'{value}' is not a valid CPU Architecture target"));
            reporter.help(format!(
                "run '{} help test' to see how this command works",
                cli_info.executable
            ));
            None
        }
        arch => Some(arch),
    }
}
