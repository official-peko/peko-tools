//! `peko icon`: generate the per-platform app icon set and inspect the icon
//! configuration.
//!
//! `generate` reworks the icon source into the standard pixel sizes for each
//! target platform and writes them under an output directory. `show` prints the
//! resolved icon source and target platforms. The icon builder in the IDE saves
//! its master PNG and layered `.pekoicon` document, and this command turns that
//! master into the per-platform set the bundler consumes.

use std::fs::{self, File};
use std::path::PathBuf;
use std::process::ExitCode;

use peko_core::target::OperatingSystem;

use crate::cli::CLIInfo;
use crate::cli::reporting::Reporter;
use crate::project::{PekoProject, UIProjectInfo};

/// The canonical name for a platform, or None for an unknown target.
fn platform_name(os: OperatingSystem) -> Option<&'static str> {
    match os {
        OperatingSystem::MacOS => Some("macos"),
        OperatingSystem::Windows => Some("windows"),
        OperatingSystem::Linux => Some("linux"),
        OperatingSystem::IOS => Some("ios"),
        OperatingSystem::Android => Some("android"),
        OperatingSystem::Unknown => None,
    }
}

/// The operating system for a platform name.
fn os_for_name(name: &str) -> Option<OperatingSystem> {
    match name {
        "macos" => Some(OperatingSystem::MacOS),
        "windows" => Some(OperatingSystem::Windows),
        "linux" => Some(OperatingSystem::Linux),
        "ios" => Some(OperatingSystem::IOS),
        "android" => Some(OperatingSystem::Android),
        _ => None,
    }
}

/// The standard app-icon pixel sizes for a platform.
fn sizes_for(platform: &str) -> &'static [u32] {
    match platform {
        "macos" => &[16, 32, 64, 128, 256, 512, 1024],
        "ios" => &[20, 29, 40, 58, 60, 76, 80, 87, 120, 152, 167, 180, 1024],
        "android" => &[48, 72, 96, 144, 192, 512],
        "windows" => &[16, 24, 32, 48, 64, 128, 256],
        "linux" => &[16, 32, 48, 64, 128, 256, 512],
        _ => &[],
    }
}

/// Execute the `icon` subcommand.
pub async fn execute(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    let subcommand = cli_info
        .arguments
        .get(1)
        .map(String::as_str)
        .unwrap_or("show");

    let project = match PekoProject::from_current_directory() {
        Ok(project) => project,
        Err(error) => {
            reporter.error(format!("not in a Peko project: {error}"));
            return ExitCode::FAILURE;
        }
    };
    let Some(ui) = project.ui_project_info.as_ref() else {
        reporter.error("peko icon is only available for UI projects");
        return ExitCode::FAILURE;
    };

    match subcommand {
        "show" => {
            let names: Vec<&str> = ui.platforms.iter().filter_map(|p| platform_name(*p)).collect();
            reporter.info(format!("app icon source: {size}x{size}", size = ui.icon.width));
            reporter.info(format!(
                "target platforms: {}",
                if names.is_empty() { "(none)".to_owned() } else { names.join(", ") }
            ));
            ExitCode::SUCCESS
        }
        "generate" => generate(cli_info, reporter, ui),
        other => {
            reporter.error(format!(
                "unknown icon subcommand '{other}' (expected 'generate' or 'show')"
            ));
            ExitCode::FAILURE
        }
    }
}

/// Render the icon source into the per-platform size set.
fn generate(cli_info: &CLIInfo, reporter: &Reporter, ui: &UIProjectInfo) -> ExitCode {
    let only = cli_info.flags.get_flag("platform");
    let out_dir = cli_info
        .flags
        .get_flag("out")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("build/icons"));

    let platforms: Vec<&str> = match &only {
        Some(name) => vec![name.as_str()],
        None => ui.platforms.iter().filter_map(|p| platform_name(*p)).collect(),
    };
    if platforms.is_empty() {
        reporter.error("no target platforms; declare target_platforms or pass --platform <os>");
        return ExitCode::FAILURE;
    }

    let mut count = 0usize;
    for platform in platforms {
        let sizes = sizes_for(platform);
        if sizes.is_empty() {
            reporter.error(format!("unknown platform '{platform}'"));
            return ExitCode::FAILURE;
        }
        // Match the platform's display shape (squircle for Apple, circle for
        // Android), applied once before resizing to each size.
        let os = os_for_name(platform).unwrap_or(OperatingSystem::Unknown);
        let shaped = ui.icon_for(os).shaped_for(os);
        let dir = out_dir.join(platform);
        if let Err(error) = fs::create_dir_all(&dir) {
            reporter.error(format!("could not create {}: {error}", dir.display()));
            return ExitCode::FAILURE;
        }
        for &size in sizes {
            let resized = shaped.resize(size, size);
            let path = dir.join(format!("AppIcon-{size}.png"));
            let mut file = match File::create(&path) {
                Ok(file) => file,
                Err(error) => {
                    reporter.error(format!("could not write {}: {error}", path.display()));
                    return ExitCode::FAILURE;
                }
            };
            resized.to_png(&mut file);
            count += 1;
        }
    }

    reporter.success(format!("generated {count} icon files under {}", out_dir.display()));
    ExitCode::SUCCESS
}
