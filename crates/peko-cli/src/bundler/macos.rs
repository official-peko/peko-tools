//! macOS `.app` bundler.
//!
//! Produces a single universal `.app` bundle at `<build_dir>/<Name>.app`.
//! The arm64 and x86_64 executables are compiled separately and combined
//! into one universal binary with `lipo`. The bundle has the standard
//! macOS layout: `Contents/Info.plist`, `Contents/MacOS/exec` (the
//! universal binary), `Contents/Resources/icon.icns`, and
//! `Contents/Resources/PrivacyInfo.xcprivacy`.
//!
//! macOS signing is optional. [`sign`] signs the bundle in place with a
//! Developer ID key and the hardened runtime through the `apple-codesign`
//! crate when a macOS key is registered. When a notary key is registered,
//! the signed bundle is submitted to Apple's notary service and the ticket
//! is stapled, all in process through the same crate.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;

use peko_core::target::{Architecture, OperatingSystem, PekoTarget};

use crate::bundler::{BundleError, BundleResult, CleanupGuard, io_at, run_tool, signing};
use crate::cli::CLIInfo;
use crate::cli::reporting::{ProgressSink, Reporter};
use crate::execution;
use crate::project::{PekoProject, ProjectIcon};

/// Build a universal macOS `.app` bundle for the project.
pub fn bundle(
    cli_info: &CLIInfo,
    project: &mut PekoProject,
    macos_build_directory: PathBuf,
    progress: &dyn ProgressSink,
) -> BundleResult<()> {
    if macos_build_directory.exists() {
        let removal = if macos_build_directory.is_dir() {
            fs::remove_dir_all(&macos_build_directory)
        } else {
            fs::remove_file(&macos_build_directory)
        };
        io_at(&macos_build_directory, removal)?;
    }
    io_at(
        &macos_build_directory,
        fs::create_dir_all(&macos_build_directory),
    )?;

    let guard = CleanupGuard::new(macos_build_directory.clone());

    // Three user-visible phases; the two inner compiles contribute their
    // own units via add_to_total.
    progress.add_to_total(3);

    progress.tick("macOS: preparing .app bundle layout");

    let app_file_name = format!("{}.app", project.name);
    let app = macos_build_directory.join(&app_file_name);
    let app_contents = app.join("Contents");
    let exec_dir = app_contents.join("MacOS");
    let resources_dir = app_contents.join("Resources");
    io_at(&exec_dir, fs::create_dir_all(&exec_dir))?;
    io_at(&resources_dir, fs::create_dir_all(&resources_dir))?;

    // Info.plist into Contents.
    let plist_src = project
        .get_root()
        .join(".peko/bundling/configfiles/macos/Info.plist");
    io_at(
        &plist_src,
        fs::copy(&plist_src, app_contents.join("Info.plist")),
    )?;

    // Privacy manifest into Contents/Resources.
    let privacy_src = project
        .get_root()
        .join(".peko/bundling/configfiles/macos/PrivacyInfo.xcprivacy");
    io_at(
        &privacy_src,
        fs::copy(&privacy_src, resources_dir.join("PrivacyInfo.xcprivacy")),
    )?;

    // App icon. Current macOS renders the real app icon from a compiled asset
    // catalog (Assets.car) named by CFBundleIconName. A bare .icns is treated
    // as legacy and drawn shrunken on a light rounded tile, so the icon is
    // written as an asset catalog, with a plain .icns alongside it.
    let icon = &project.ui_project_info.as_ref().unwrap().icon;
    compile_app_icon(icon, &resources_dir)?;

    // Project assets into Contents/Resources with subdirectories
    // preserved. The assets package's Apple native layer resolves
    // "icons/home.png" via [NSBundle pathForResource:@"icons/home"
    // ofType:@"png"], which needs the real folder structure under
    // Resources, so the hierarchical names are kept.
    crate::bundler::copy_project_assets(project, &resources_dir)?;

    // Per-architecture executables compiled into an intermediates folder,
    // then combined into one universal binary.
    let intermediates = macos_build_directory.join("intermediates");
    io_at(&intermediates, fs::create_dir_all(&intermediates))?;
    let arm_exec = intermediates.join("exec-arm64");
    let x86_64_exec = intermediates.join("exec-x86_64");

    progress.tick("macOS: compiling arm64 binary");
    let arm_target = PekoTarget::new(OperatingSystem::MacOS, Architecture::Arm, false);
    let arm_diagnostics = execution::incremental::compile_project(
        cli_info.get_peko_root(),
        project,
        arm_target,
        project.get_root().join(".peko/incremental"),
        Some(arm_exec.clone()),
        false,
        Vec::new(),
        None,
        None,
        !cli_info.flags.has_flag("release"),
        progress,
    )?;
    if let Some(diagnostics) = arm_diagnostics {
        return Err(BundleError::CompileDiagnostics(diagnostics));
    }

    progress.tick("macOS: compiling x86_64 binary");
    let x86_64_target = PekoTarget::new(OperatingSystem::MacOS, Architecture::X86_64, false);
    let x86_64_diagnostics = execution::incremental::compile_project(
        cli_info.get_peko_root(),
        project,
        x86_64_target,
        project.get_root().join(".peko/incremental"),
        Some(x86_64_exec.clone()),
        false,
        Vec::new(),
        None,
        None,
        !cli_info.flags.has_flag("release"),
        progress,
    )?;
    if let Some(diagnostics) = x86_64_diagnostics {
        return Err(BundleError::CompileDiagnostics(diagnostics));
    }

    // Combine both slices into one universal binary at Contents/MacOS/exec
    // using the llvm-lipo that ships with the Peko LLVM toolchain.
    let universal_exec = exec_dir.join("exec");
    let llvm_lipo = resolve_llvm_lipo(cli_info.get_peko_root());
    run_tool(
        "llvm-lipo",
        Command::new(&llvm_lipo)
            .arg("-create")
            .arg(&arm_exec)
            .arg(&x86_64_exec)
            .arg("-output")
            .arg(&universal_exec),
    )?;

    // The per-architecture executables are consumed by lipo.
    io_at(&intermediates, fs::remove_dir_all(&intermediates))?;

    guard.commit();
    Ok(())
}

/// Writes the project icon into `Contents/Resources/Assets.car`, the
/// asset-catalog form current macOS uses to render the real app icon at every
/// size. The catalog is named by CFBundleIconName in Info.plist. A plain
/// `AppIcon.icns` is written alongside it as the legacy icon some contexts
/// still read.
///
/// The asset catalog is built in process by [`ProjectIcon::to_car_macos`], so
/// bundling does not depend on Xcode or `actool`.
fn compile_app_icon(icon: &ProjectIcon, resources_dir: &Path) -> BundleResult<()> {
    let car = resources_dir.join("Assets.car");
    icon.to_car_macos(&mut io_at(&car, File::create(&car))?);

    let icns = resources_dir.join("AppIcon.icns");
    icon.to_icns(&mut io_at(&icns, File::create(&icns))?);
    Ok(())
}

/// Optionally sign and notarize the macOS `.app` bundle.
///
/// macOS signing is optional. When no macOS key is registered the unsigned
/// bundle is left in place. When a key is registered, the bundle is signed
/// in place with the hardened runtime through the `apple-codesign` crate.
/// Developer ID signing needs no provisioning profile. When a notary key
/// is registered, the signed bundle is submitted to Apple's notary service
/// and the ticket is stapled. A signed disk image holding the bundle is
/// written next to it for distribution, and is notarized and stapled when a
/// notary key is registered.
pub fn sign(
    _cli_info: &CLIInfo,
    project: &PekoProject,
    macos_build_directory: PathBuf,
    reporter: &Reporter,
) -> BundleResult<signing::OptionalSignOutcome> {
    let Some(ui_info) = project.ui_project_info.as_ref() else {
        return Ok(signing::OptionalSignOutcome::NoKey);
    };

    let key = match signing::resolve_apple(project.get_root(), &ui_info.bundle_id, "macos")? {
        Some(key) => key,
        None => return Ok(signing::OptionalSignOutcome::NoKey),
    };

    let app_file_name = format!("{}.app", project.name);
    let app = macos_build_directory.join(&app_file_name);
    if !app.exists() {
        return Err(BundleError::Signing(format!(
            "macOS .app not found at {}",
            app.display()
        )));
    }

    let entitlements_xml = match key.entitlements.as_ref() {
        Some(path) => Some(io_at(path, fs::read_to_string(path))?),
        None => None,
    };

    signing::sign_apple_bundle(&app, &key, entitlements_xml.as_deref(), true)?;

    // Notarize when notary credentials are registered. Without them the
    // bundle is signed but not notarized.
    let notary = signing::resolve_notary(project.get_root(), "macos");
    match &notary {
        Some(creds) => signing::notarize_and_staple(&app, creds)?,
        None => {
            reporter.warning("macOS: no notary key registered; bundle signed but not notarized")
        }
    }

    // Package the signed bundle into a distributable disk image. The image
    // is signed with the same key. When notary credentials are registered
    // the image is notarized and stapled.
    let dmg = package_dmg(&macos_build_directory, &project.name, &app)?;
    signing::sign_dmg(&dmg, &key)?;
    if let Some(creds) = &notary {
        signing::notarize_and_staple(&dmg, creds)?;
    }

    Ok(signing::OptionalSignOutcome::Signed)
}

/// Build a distributable disk image holding the signed `.app`. The image
/// contains the bundle and a link to the Applications folder for drag
/// installation. The image is written next to the bundle and its path is
/// returned. Disk image creation runs on macOS.
fn package_dmg(build_dir: &Path, name: &str, app: &Path) -> BundleResult<PathBuf> {
    let staging = build_dir.join("dmg-staging");
    if staging.exists() {
        io_at(&staging, fs::remove_dir_all(&staging))?;
    }
    io_at(&staging, fs::create_dir_all(&staging))?;

    let staged_app = staging.join(format!("{name}.app"));
    run_tool("ditto", Command::new("ditto").arg(app).arg(&staged_app))?;

    run_tool(
        "ln",
        Command::new("ln")
            .arg("-s")
            .arg("/Applications")
            .arg(staging.join("Applications")),
    )?;

    let dmg = build_dir.join(format!("{name}.dmg"));
    if dmg.exists() {
        io_at(&dmg, fs::remove_file(&dmg))?;
    }

    run_tool(
        "hdiutil",
        Command::new("hdiutil")
            .arg("create")
            .arg("-volname")
            .arg(name)
            .arg("-srcfolder")
            .arg(&staging)
            .arg("-ov")
            .arg("-format")
            .arg("UDZO")
            .arg(&dmg),
    )?;

    io_at(&staging, fs::remove_dir_all(&staging))?;

    Ok(dmg)
}

/// Resolve the `llvm-lipo` used to build the universal binary. Selects the
/// host binary under the Peko compiler bin directory and falls back to the
/// `llvm-lipo` on the system PATH.
fn resolve_llvm_lipo(peko_root: &Path) -> PathBuf {
    let host_binary = match std::env::consts::OS {
        "linux" => match std::env::consts::ARCH {
            "arm" | "aarch64" => "llvm-lipo-linux-arm",
            _ => "llvm-lipo-linux-x86_64",
        },
        "macos" => match std::env::consts::ARCH {
            "arm" | "aarch64" => "llvm-lipo-darwin-arm",
            _ => "llvm-lipo-darwin-x86_64",
        },
        _ => "llvm-lipo-windows.exe",
    };
    let bundled = peko_root.join("Compiler/bin/llvm-lipo").join(host_binary);
    if bundled.exists() {
        bundled
    } else {
        PathBuf::from("llvm-lipo")
    }
}
