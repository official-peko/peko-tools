//! # Peko CLI
//!
//! Console interface for the Pekoscript toolchain. Argument parsing,
//! global flag handling, and dispatch to per-subcommand modules under
//! `commands/`.
#![allow(clippy::too_many_arguments)]

pub mod auth;
pub mod bridge;
pub mod bundler;
pub mod cli;
pub mod deploy;
pub mod execution;
pub mod keychain;
pub mod proc;
pub mod project;
pub mod registry;
pub mod toolchain;

pub mod commands;

// TEMPORARY: local harness for testing the new package resolution. Remove once
// resolution is validated. Activated by the global `--testtmp` flag.
pub mod testtmp;

use std::process::ExitCode;

use crate::cli::CLIInfo;
use crate::cli::reporting::{IndicatifSink, Reporter, Verbosity};

#[tokio::main]
async fn main() -> ExitCode {
    // ---- Parse argv into CLIInfo -----------------------------------------
    //
    // The invoked subcommand's declared value-flags tell the parser which bare
    // `--flag value` tokens consume the following argv slot as a value. The
    // subcommand is the first non-flag argv token (global pre-subcommand flags
    // are all bare switches, so they never consume it).
    let value_flags = value_flags_for_argv();
    let cli_info = match CLIInfo::new(vec!["-".to_string(), "--".to_string()], value_flags) {
        Ok(info) => info,
        Err(errors) => {
            // No reporter is constructed yet - emit plain stderr.
            for error in &errors {
                eprintln!("Error: {error}");
            }
            return ExitCode::FAILURE;
        }
    };

    // ---- Configure the Reporter from global flags ------------------------
    //
    // Global flags are picked up out of `cli_info.flags`. Subcommands see
    // the same flags, so individual commands can also branch on them if
    // they want to, but the canonical wiring is via the Reporter.
    // `--json` emits machine-readable NDJSON on stdout. The progress spinner is
    // left off in that mode so it does not corrupt the event stream.
    let mut reporter = if cli_info.flags.has_flag("json") {
        Reporter::new()
    } else {
        Reporter::new().with_progress(IndicatifSink::new())
    };
    if cli_info.flags.has_flag("json") {
        reporter.set_json(true);
    }
    if cli_info.flags.has_flag("no-color") {
        reporter.disable_color();
    }
    if cli_info.flags.has_flag("quiet") {
        reporter.set_verbosity(Verbosity::Quiet);
    } else if cli_info.flags.has_flag("verbose") {
        reporter.set_verbosity(Verbosity::Verbose);
    }

    // ---- TEMPORARY: resolution test harness ------------------------------
    //
    // `--testtmp` short-circuits normal dispatch and runs the local
    // resolution harness. Remove with the `testtmp` module once resolution
    // is validated.
    if cli_info.flags.has_flag("testtmp") {
        return testtmp::run(&cli_info, &reporter).await;
    }

    // ---- TEMPORARY: AST-to-IR language harness ---------------------------
    //
    // `--astir "<source>"` parses, type-checks, and codegens a snippet with
    // no standard library, then prints the diagnostics and the LLVM IR. Used
    // to exercise V2 language features in isolation. Remove once the V2
    // language work settles.
    if cli_info.flags.has_flag("astir") {
        let source = cli_info.arguments.first().cloned().unwrap_or_default();
        let (ir, diagnostics) = execution::compile_snippet_to_ir(&source);
        reporter.report_diagnostics(&diagnostics);
        println!("{ir}");
        return ExitCode::SUCCESS;
    }

    // `--astir-std "<source>"` is the same harness but loads the std package
    // from `std/peko.toml` and prepends the implicit import prelude, so a
    // snippet can resolve std::core, optionals, and the bare Object/Option.
    if cli_info.flags.has_flag("astir-std") {
        let source = cli_info.arguments.first().cloned().unwrap_or_default();
        let diagnostics = execution::simulate_snippet_with_std(&source);
        reporter.report_diagnostics(&diagnostics);
        return ExitCode::SUCCESS;
    }

    // ---- No subcommand: print master help ------------------------------
    if cli_info.arguments.is_empty() {
        print_master_help(&cli_info.executable);
        return ExitCode::SUCCESS;
    }

    let subcommand_name = cli_info.arguments[0].as_str();

    // ---- Handle `peko help [name]` ---------------------------------------
    if subcommand_name == "help" {
        return run_help(&cli_info, &reporter);
    }

    // ---- `peko lsp`: the language server --------------------------------
    //
    // The server speaks LSP over stdio, so stdout must carry only the JSON-RPC
    // stream. Dispatch here, before the root checkup or any reporter output,
    // so nothing else writes to stdout. Logs go to stderr.
    if subcommand_name == "lsp" {
        peko_lsp::serve().await;
        return ExitCode::SUCCESS;
    }

    // ---- Verify the Peko root looks healthy ------------------------------
    //
    // Skip this check for `check` (which exists to report on it), `version`
    // (which never touches the root), and `setup` (which creates the root and
    // must run before it exists). PEKO_SKIP_ROOT_CHECKUP also skips it, so setup
    // can invoke other subcommands (such as `add` for the global packages) while
    // the root is still being built, before it has been certified.
    if !cli_info.perform_root_checkup()
        && subcommand_name != "check"
        && subcommand_name != "version"
        && subcommand_name != "setup"
        && std::env::var_os("PEKO_SKIP_ROOT_CHECKUP").is_none()
    {
        reporter.error(format!(
            "Peko toolchain installation looks corrupted. Run '{} setup' to install it.",
            cli_info.executable
        ));
        return ExitCode::FAILURE;
    }

    // ---- `peko run --devtools`: main-thread window ----------------------
    //
    // The devtools window's event loop must run on the process main thread
    // (winit requires it), so this path takes a synchronous entry instead of
    // the async dispatch below. No `.await` has run yet on this code path, so
    // control is still on the main thread here.
    if subcommand_name == "run" && cli_info.flags.has_flag("devtools") {
        return commands::run::execute_with_devtools(&cli_info, &reporter);
    }

    // ---- `peko run --ide`: newline-delimited JSON control transport -----
    //
    // The same dev loop as devtools, driven over stdin/stdout instead of a
    // window, so an embedding IDE owns the app lifecycle and receives dev
    // events. The reader and writer are plain blocking threads, so this takes
    // the synchronous entry alongside the devtools path.
    if subcommand_name == "run" && cli_info.flags.has_flag("ide") {
        return commands::run::execute_with_ide(&cli_info, &reporter);
    }

    // ---- Look up and dispatch the subcommand -----------------------------
    let Some(command) = commands::lookup(subcommand_name) else {
        reporter.error(format!("command '{subcommand_name}' doesn't exist"));
        reporter.help(format!(
            "run '{} help' to see the list of available commands",
            cli_info.executable
        ));
        return ExitCode::FAILURE;
    };

    (command.execute)(&cli_info, &reporter).await
}

/// The value-taking flags of the subcommand named on the command line.
///
/// Scans the raw argv for the first non-flag token (the subcommand) and returns
/// its declared `value_flags`, so the argument parser knows which bare
/// `--flag value` tokens consume a following value. Returns an empty slice when
/// there is no subcommand or it is not a known command.
fn value_flags_for_argv() -> &'static [&'static str] {
    std::env::args()
        .skip(1)
        .find(|arg| !arg.starts_with('-'))
        .and_then(|name| commands::lookup(&name))
        .map(|command| command.value_flags)
        .unwrap_or(&[])
}

/// `peko help [<command>]` - either print the master help index or the
/// per-command help blob.
fn run_help(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    let Some(target) = cli_info.arguments.get(1) else {
        print_master_help(&cli_info.executable);
        return ExitCode::SUCCESS;
    };

    // Special case: `peko help help` shows the master index too.
    if target == "help" {
        print_master_help(&cli_info.executable);
        return ExitCode::SUCCESS;
    }

    let Some(command) = commands::lookup(target.as_str()) else {
        reporter.error(format!(
            "cannot print help for '{target}' since no such command exists"
        ));
        reporter.help(format!(
            "run '{} help' to see the list of available commands",
            cli_info.executable
        ));
        return ExitCode::FAILURE;
    };

    println!("{}", command.help);
    ExitCode::SUCCESS
}

/// Print the master help index, listing every subcommand and its
/// one-line summary.
fn print_master_help(executable: &str) {
    println!("Usage: {executable} <command> [options] [arguments]");
    println!();
    println!("Commands:");
    let widest = commands::ALL_COMMANDS
        .iter()
        .map(|c| c.name.len())
        .max()
        .unwrap_or(0);
    for cmd in commands::ALL_COMMANDS {
        println!(
            "    {name:<width$}    {summary}",
            name = cmd.name,
            width = widest,
            summary = cmd.summary
        );
    }
    println!();
    println!("Global options:");
    println!("    --verbose             enable extra-noisy output");
    println!("    --quiet               suppress informational output");
    println!("    --no-color            disable ANSI color in output");
    println!("    --json                emit machine-readable JSON events (for tooling)");
    println!();
    println!("Run '{executable} help <command>' for per-command help.");
}
