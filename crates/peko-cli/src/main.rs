//! # Peko CLI
//!
//! Console interface for the Pekoscript toolchain. Argument parsing,
//! global flag handling, and dispatch to per-subcommand modules under
//! `commands/`.
#![allow(clippy::too_many_arguments)]

pub mod bundler;
pub mod cli;
pub mod execution;
pub mod packager;
pub mod project;

pub mod commands;

use std::process::ExitCode;

use crate::cli::CLIInfo;
use crate::cli::reporting::{IndicatifSink, Reporter, Verbosity};

#[tokio::main]
async fn main() -> ExitCode {
    // ---- Parse argv into CLIInfo -----------------------------------------
    let cli_info = match CLIInfo::new(vec!["-".to_string(), "--".to_string()]) {
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
    let mut reporter = Reporter::new().with_progress(IndicatifSink::new());
    if cli_info.flags.has_flag("no-color") {
        reporter.disable_color();
    }
    if cli_info.flags.has_flag("quiet") {
        reporter.set_verbosity(Verbosity::Quiet);
    } else if cli_info.flags.has_flag("verbose") {
        reporter.set_verbosity(Verbosity::Verbose);
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

    // ---- Verify the Peko root looks healthy ------------------------------
    //
    // Skip this check for `check` (which exists to report on it) and
    // `version` (which never touches the root).
    if !cli_info.perform_root_checkup()
        && subcommand_name != "check"
        && subcommand_name != "version"
    {
        reporter.error(format!(
            "Peko toolchain installation looks corrupted. Run '{} check' for details.",
            cli_info.executable
        ));
        return ExitCode::FAILURE;
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
    println!();
    println!("Run '{executable} help <command>' for per-command help.");
}
