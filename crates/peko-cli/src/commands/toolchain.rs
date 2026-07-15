//! `peko toolchain`: inspect and install build toolchains.
//!
//! `list` reads the install manifest (`versions.json`) and loads each installed
//! toolchain's `toolchain.toml`, reporting which parse and which fail. It is
//! the quickest way to validate the toolchain descriptions against the
//! installed directories.

use std::process::ExitCode;

use peko_core::target::{Architecture, OperatingSystem};

use crate::cli::CLIInfo;
use crate::cli::reporting::Reporter;
use crate::toolchain::{InstallManifest, resolve_toolchain};

/// The canonical (os, arch) targets a toolchain can exist for.
const TARGETS: &[(OperatingSystem, Architecture)] = &[
    (OperatingSystem::MacOS, Architecture::Arm),
    (OperatingSystem::MacOS, Architecture::X86_64),
    (OperatingSystem::IOS, Architecture::Arm),
    (OperatingSystem::IOS, Architecture::X86_64),
    (OperatingSystem::Linux, Architecture::Arm),
    (OperatingSystem::Linux, Architecture::X86_64),
    (OperatingSystem::Android, Architecture::Arm),
    (OperatingSystem::Android, Architecture::X86_64),
    (OperatingSystem::Windows, Architecture::X86_64),
];

/// Execute the `toolchain` subcommand.
pub async fn execute(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    let Some(subcommand) = cli_info.arguments.get(1) else {
        reporter.error("`toolchain` requires a subcommand");
        reporter.help(format!(
            "run '{} help toolchain' to see how this command works",
            cli_info.executable
        ));
        return ExitCode::FAILURE;
    };

    match subcommand.as_str() {
        "list" => execute_list(cli_info, reporter),
        other => {
            reporter.error(format!("no such subcommand '{other}' for 'toolchain' command"));
            reporter.help(format!(
                "run '{} help toolchain' to see a list of valid subcommands",
                cli_info.executable
            ));
            ExitCode::FAILURE
        }
    }
}

/// `toolchain list`: load and report every installed toolchain.
fn execute_list(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    let peko_root = cli_info.get_peko_root();

    let manifest = match InstallManifest::load(peko_root) {
        Ok(manifest) => manifest,
        Err(e) => {
            reporter.error(format!("could not read versions.json: {e}"));
            return ExitCode::FAILURE;
        }
    };

    println!(
        "host: {}/{} ({})",
        manifest.host.os, manifest.host.arch, manifest.host.triple
    );
    println!("toolchains: {}", manifest.toolchains.version);

    let mut failures = 0;
    for &(os, arch) in TARGETS {
        if !manifest.is_installed(os, arch) {
            continue;
        }

        match resolve_toolchain(peko_root, &manifest, os, arch) {
            Ok(resolved) => {
                let toolchain = &resolved.toolchain;
                println!(
                    "  {}/{} [{}] driver={} includes={} libs={} frameworks={} objects={} dylibs={}",
                    os.name(),
                    arch.name(),
                    toolchain.meta.triple,
                    toolchain.link.driver,
                    toolchain.build.include.len(),
                    toolchain.link.libs.len(),
                    toolchain.link.frameworks.len(),
                    toolchain.link.objects.len(),
                    toolchain.link.bundle_dylibs.len(),
                );
            }
            Err(e) => {
                failures += 1;
                reporter.error(format!("{}/{}: {e}", os.name(), arch.name()));
            }
        }
    }

    if failures == 0 {
        reporter.success("all installed toolchains parsed");
        ExitCode::SUCCESS
    } else {
        reporter.error(format!("{failures} toolchain(s) failed to load"));
        ExitCode::FAILURE
    }
}
