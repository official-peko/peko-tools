//! `peko clean`: remove a project's build artifacts.
//!
//! Wipes the incremental build cache (`.peko/incremental`) and the build output
//! directory (`build/`). For a UI project whose framework defines a `clean`
//! script in `package.json`, it also runs the framework's own clean
//! (`npm run clean`) so framework caches (`.next`, `dist`, and the like) are
//! cleared too. Replaces the old `peko build --clean` flag.

use std::path::Path;
use std::process::ExitCode;

use crate::cli::CLIInfo;
use crate::cli::reporting::Reporter;
use crate::project::PekoProject;

/// Execute the `clean` subcommand.
pub async fn execute(_cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    let project = match PekoProject::from_current_directory() {
        Ok(project) => project,
        Err(e) => {
            reporter.error(format!("could not load project: {e}"));
            return ExitCode::FAILURE;
        }
    };
    let root = project.get_root().to_path_buf();

    let mut removed_any = false;
    removed_any |= remove_tree(&root.join(".peko/incremental"), "incremental build cache", reporter);
    removed_any |= remove_tree(&root.join("build"), "build output", reporter);

    // A UI project drives a web framework; run its own clean so framework build
    // caches are cleared alongside Peko's.
    if project.ui_project_info.is_some() {
        removed_any |= run_framework_clean(&root, reporter);
    }

    if removed_any {
        reporter.success(format!("cleaned {}", project.name));
    } else {
        reporter.info(format!("nothing to clean for {}", project.name));
    }
    ExitCode::SUCCESS
}

/// Remove a directory tree, reporting the outcome. Returns whether anything was
/// removed.
fn remove_tree(path: &Path, label: &str, reporter: &Reporter) -> bool {
    if !path.exists() {
        return false;
    }
    reporter.status("Cleaning", label);
    if let Err(e) = std::fs::remove_dir_all(path) {
        reporter.warning(format!("could not remove {}: {e}", path.display()));
        return false;
    }
    true
}

/// Run the UI framework's own `clean` script when it defines one. Returns
/// whether a clean script ran.
fn run_framework_clean(root: &Path, reporter: &Reporter) -> bool {
    if !clean_script_present(root) {
        return false;
    }
    reporter.status("Cleaning", "framework output (npm run clean)");
    match crate::proc::npm()
        .arg("run")
        .arg("clean")
        .current_dir(root)
        .status()
    {
        Ok(status) if status.success() => true,
        Ok(_) => {
            reporter.warning("framework clean script exited with an error");
            false
        }
        Err(e) => {
            reporter.warning(format!("could not run the framework clean script: {e}"));
            false
        }
    }
}

/// `true` when the project's `package.json` defines a `clean` script.
fn clean_script_present(root: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(root.join("package.json")) else {
        return false;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    json.get("scripts")
        .and_then(|scripts| scripts.get("clean"))
        .and_then(|clean| clean.as_str())
        .is_some()
}
