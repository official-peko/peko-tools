//! `peko format`: normalize the indentation and spacing of Pekoscript files.
//!
//! Formats each `.peko` file given as an argument in place. `--check` reports
//! which files are not formatted and writes nothing, for use in CI. `--stdout`
//! prints the formatted text instead of writing it back.

use std::path::Path;
use std::process::ExitCode;

use crate::cli::CLIInfo;
use crate::cli::reporting::Reporter;

/// Execute the `format` subcommand.
pub async fn execute(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    let paths: Vec<&String> = cli_info.arguments.iter().skip(1).collect();
    if paths.is_empty() {
        reporter.error("no files given");
        reporter.help("usage: peko format <file.peko> [more.peko ...] [--check] [--stdout]");
        return ExitCode::FAILURE;
    }

    let check = cli_info.flags.has_flag("check");
    let to_stdout = cli_info.flags.has_flag("stdout");

    let mut had_error = false;
    let mut unformatted = 0usize;
    let mut written = 0usize;

    for path_str in paths {
        let path = Path::new(path_str);
        let original = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) => {
                reporter.error(format!("could not read {path_str}: {error}"));
                had_error = true;
                continue;
            }
        };

        let formatted = peko_core::formatter::format_source(
            &original,
            path,
            peko_core::formatter::data_structures::FormatConfig::default(),
        );

        if to_stdout {
            print!("{formatted}");
            continue;
        }

        if formatted == original {
            continue;
        }

        if check {
            reporter.warning(format!("{path_str} is not formatted"));
            unformatted += 1;
            continue;
        }

        if let Err(error) = std::fs::write(path, formatted.as_bytes()) {
            reporter.error(format!("could not write {path_str}: {error}"));
            had_error = true;
            continue;
        }
        written += 1;
    }

    if had_error {
        return ExitCode::FAILURE;
    }

    if check {
        if unformatted > 0 {
            reporter.error(format!("{unformatted} file(s) are not formatted"));
            return ExitCode::FAILURE;
        }
        reporter.success("all files are formatted");
    } else if !to_stdout {
        reporter.success(format!("formatted {written} file(s)"));
    }

    ExitCode::SUCCESS
}
