//! macOS `.app` bundler.
//!
//! Produces a `.app` bundle for both arm64 and x86_64 (separate
//! `<build_dir>/arm/<Name>.app/` and `<build_dir>/x86_64/<Name>.app/`
//! trees). Each bundle has the standard macOS layout:
//! `Contents/Info.plist`, `Contents/MacOS/exec` (the compiled binary),
//! and `Contents/Resources/icon.icns`.
//!
//! macOS signing is optional. [`sign`] signs both bundles in place with a
//! Developer ID key using the `apple-codesign` crate when a macOS key is
//! registered for the project.

use std::fs::{self, File};
use std::path::PathBuf;

use peko_core::target::{Architecture, OperatingSystem, PekoTarget};

use crate::bundler::{io_at, signing, BundleError, BundleResult, CleanupGuard};
use crate::cli::reporting::ProgressSink;
use crate::cli::CLIInfo;
use crate::execution;
use crate::project::PekoProject;

/// Build macOS `.app` bundles for the project (both arm64 and x86_64).
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

    // Three user-visible phases; the two inner compiles contribute
    // their own units via add_to_total.
    progress.add_to_total(3);

    progress.tick("macOS: preparing .app bundle layout");

    let arm_build_dir = macos_build_directory.join("arm");
    let x86_64_build_dir = macos_build_directory.join("x86_64");
    io_at(&arm_build_dir, fs::create_dir_all(&arm_build_dir))?;
    io_at(&x86_64_build_dir, fs::create_dir_all(&x86_64_build_dir))?;

    // Lay out the standard .app/Contents/{MacOS,Resources} subtrees for
    // both architectures.
    let app_file_name = format!("{}.app", project.name);
    let arm_app = arm_build_dir.join(&app_file_name);
    let x86_64_app = x86_64_build_dir.join(&app_file_name);
    io_at(&arm_app, fs::create_dir_all(&arm_app))?;
    io_at(&x86_64_app, fs::create_dir_all(&x86_64_app))?;

    let arm_app_contents = arm_app.join("Contents");
    let x86_64_app_contents = x86_64_app.join("Contents");
    io_at(&arm_app_contents, fs::create_dir_all(&arm_app_contents))?;
    io_at(
        &x86_64_app_contents,
        fs::create_dir_all(&x86_64_app_contents),
    )?;

    let arm_exec_dir = arm_app_contents.join("MacOS");
    let x86_64_exec_dir = x86_64_app_contents.join("MacOS");
    io_at(&arm_exec_dir, fs::create_dir_all(&arm_exec_dir))?;
    io_at(&x86_64_exec_dir, fs::create_dir_all(&x86_64_exec_dir))?;

    let arm_resources_dir = arm_app_contents.join("Resources");
    let x86_64_resources_dir = x86_64_app_contents.join("Resources");
    io_at(&arm_resources_dir, fs::create_dir_all(&arm_resources_dir))?;
    io_at(
        &x86_64_resources_dir,
        fs::create_dir_all(&x86_64_resources_dir),
    )?;

    // Info.plist - same bytes for both architectures.
    let plist_src = project
        .get_root()
        .join(".peko/bundling/configfiles/macos/Info.plist");
    io_at(
        &plist_src,
        fs::copy(&plist_src, arm_app_contents.join("Info.plist")),
    )?;
    io_at(
        &plist_src,
        fs::copy(&plist_src, x86_64_app_contents.join("Info.plist")),
    )?;

    // Icons. Same .icns content for both architectures.
    let icon = &project.ui_project_info.as_ref().unwrap().icon;
    let arm_icon = arm_resources_dir.join("icon.icns");
    let x86_64_icon = x86_64_resources_dir.join("icon.icns");
    icon.to_icns(&mut io_at(&arm_icon, File::create(&arm_icon))?);
    icon.to_icns(&mut io_at(&x86_64_icon, File::create(&x86_64_icon))?);

    // Project assets, copied into Resources for both architectures with
    // subdirectories preserved. The assets package's Apple native layer
    // resolves "icons/home.png" via [NSBundle pathForResource:@"icons/home"
    // ofType:@"png"], which needs the real folder structure under
    // Resources (not flattened), so we keep the hierarchical names.
    crate::bundler::copy_project_assets(project, &arm_resources_dir)?;
    crate::bundler::copy_project_assets(project, &x86_64_resources_dir)?;

    // Compile + link the native binary for each architecture.
    progress.tick("macOS: compiling arm64 binary");
    let arm_target = PekoTarget::new(OperatingSystem::MacOS, Architecture::Arm, false);
    let (_, arm_diagnostics) = execution::incremental::compile_project(
        cli_info.get_peko_root(),
        project,
        arm_target,
        project.get_root().join(".peko/incremental"),
        Some(arm_exec_dir.join("exec")),
        false,
        Vec::new(),
        None,
        None,
        None,
        None,
        progress,
    )?;
    if let Some(diagnostics) = arm_diagnostics {
        return Err(BundleError::CompileDiagnostics(diagnostics));
    }

    progress.tick("macOS: compiling x86_64 binary");
    let x86_64_target = PekoTarget::new(OperatingSystem::MacOS, Architecture::X86_64, false);
    let (_, x86_64_diagnostics) = execution::incremental::compile_project(
        cli_info.get_peko_root(),
        project,
        x86_64_target,
        project.get_root().join(".peko/incremental"),
        Some(x86_64_exec_dir.join("exec")),
        false,
        Vec::new(),
        None,
        None,
        None,
        None,
        progress,
    )?;
    if let Some(diagnostics) = x86_64_diagnostics {
        return Err(BundleError::CompileDiagnostics(diagnostics));
    }

    // Both arm64 and x86_64 bundles are produced. Optional Developer ID
    // signing is handled by sign().

    guard.commit();
    Ok(())
}

/// Optionally sign the macOS `.app` bundles with a Developer ID key.
///
/// macOS signing is optional. When no macOS key is registered the
/// unsigned bundles are left in place. When a key is registered, both the
/// arm64 and x86_64 bundles are signed in place with the `apple-codesign`
/// crate. Developer ID signing needs no provisioning profile.
pub fn sign(
    _cli_info: &CLIInfo,
    project: &PekoProject,
    macos_build_directory: PathBuf,
) -> BundleResult<signing::OptionalSignOutcome> {
    let Some(ui_info) = project.ui_project_info.as_ref() else {
        return Ok(signing::OptionalSignOutcome::NoKey);
    };

    let key = match signing::resolve_apple(project.get_root(), &ui_info.bundle_id, "macos")? {
        Some(key) => key,
        None => return Ok(signing::OptionalSignOutcome::NoKey),
    };

    let app_file_name = format!("{}.app", project.name);
    let entitlements_xml = match key.entitlements.as_ref() {
        Some(path) => Some(io_at(path, fs::read_to_string(path))?),
        None => None,
    };

    for arch_dir in ["arm", "x86_64"] {
        let app = macos_build_directory.join(arch_dir).join(&app_file_name);
        if app.exists() {
            signing::sign_apple_bundle(&app, &key, entitlements_xml.as_deref())?;
        }
    }

    Ok(signing::OptionalSignOutcome::Signed)
}
