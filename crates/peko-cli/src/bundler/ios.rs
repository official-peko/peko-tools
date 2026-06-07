//! iOS app bundler.
//!
//! Debug mode produces an unsigned `.app` bundle for both arm64 and
//! x86_64 simulator architectures. The bundles live directly at
//! `<build_dir>/arm/<Name>.app/` and `<build_dir>/x86_64/<Name>.app/`,
//! with no `Payload/` wrapping, giving a runnable simulator bundle.
//!
//! Release packaging and signing are handled by [`sign`]: it signs the
//! arm64 device `.app` with the `apple-codesign` crate, embeds the
//! provisioning profile, wraps the bundle in a `Payload/` directory, and
//! zips the result into a `.ipa`.

use std::fs::{self, File};
use std::path::PathBuf;

use peko_core::target::{Architecture, OperatingSystem, PekoTarget};
use zip::write::{ExtendedFileOptions, FileOptions};
use zip::{CompressionMethod, ZipWriter};

use crate::bundler::{BundleError, BundleResult, CleanupGuard, io_at, recursive_zip_add, signing};
use crate::cli::CLIInfo;
use crate::cli::reporting::ProgressSink;
use crate::execution;
use crate::project::PekoProject;

/// Build iOS app bundles for the project (both arm64 and x86_64).
pub fn bundle(
    cli_info: &CLIInfo,
    project: &mut PekoProject,
    ios_build_directory: PathBuf,
    progress: &dyn ProgressSink,
) -> BundleResult<()> {
    // Clean the build directory if it already exists, then recreate it.
    if ios_build_directory.exists() {
        let removal = if ios_build_directory.is_dir() {
            fs::remove_dir_all(&ios_build_directory)
        } else {
            fs::remove_file(&ios_build_directory)
        };
        io_at(&ios_build_directory, removal)?;
    }
    io_at(
        &ios_build_directory,
        fs::create_dir_all(&ios_build_directory),
    )?;

    let guard = CleanupGuard::new(ios_build_directory.clone());

    // Four user-visible phases; the two inner compiles contribute
    // their own units via add_to_total.
    progress.add_to_total(4);

    progress.tick("iOS: preparing .app bundle layout");

    let arm_build_dir = ios_build_directory.join("arm");
    let x86_64_build_dir = ios_build_directory.join("x86_64");
    io_at(&arm_build_dir, fs::create_dir_all(&arm_build_dir))?;
    io_at(&x86_64_build_dir, fs::create_dir_all(&x86_64_build_dir))?;

    // Bare `.app` directly under each per-arch dir. Release packaging
    // wraps the arm64 bundle in `Payload/` and zips it into a `.ipa` in
    // sign().
    let app_file_name = format!("{}.app", project.name);
    let arm_app = arm_build_dir.join(&app_file_name);
    let x86_64_app = x86_64_build_dir.join(&app_file_name);
    io_at(&arm_app, fs::create_dir_all(&arm_app))?;
    io_at(&x86_64_app, fs::create_dir_all(&x86_64_app))?;

    // Info.plist - same bytes for both architectures.
    let plist_src = project
        .get_root()
        .join(".peko/bundling/configfiles/ios/Info.plist");
    io_at(&plist_src, fs::copy(&plist_src, arm_app.join("Info.plist")))?;
    io_at(
        &plist_src,
        fs::copy(&plist_src, x86_64_app.join("Info.plist")),
    )?;

    // Icons. Each architecture gets the same set: two raw PNGs (76x76
    // and 60x60) plus a CAR asset catalog generated from the original.
    let icon = &project.ui_project_info.as_ref().unwrap().icon;
    for app_dir in [&arm_app, &x86_64_app] {
        let icon_76 = app_dir.join("AppIcon76x76.png");
        let icon_60 = app_dir.join("AppIcon60x60.png");
        let car = app_dir.join("Assets.car");

        icon.resize(76, 76)
            .to_png(&mut io_at(&icon_76, File::create(&icon_76))?);
        icon.resize(60, 60)
            .to_png(&mut io_at(&icon_60, File::create(&icon_60))?);
        icon.to_car(&mut io_at(&car, File::create(&car))?);

        // Project assets go in the .app root on iOS (there is no
        // Contents/Resources subdir like macOS). Subdirectories are
        // preserved so [NSBundle pathForResource:ofType:] resolves the
        // hierarchical names.
        crate::bundler::copy_project_assets(project, app_dir)?;
    }

    // Entitlements plist from the bundling config folder. Embedded as a
    // Mach-O section at link time so the simulator keychain grants access.
    let entitlements = project
        .get_root()
        .join(".peko/bundling/configfiles/ios/app.entitlements");

    // Compile + link the native binary for each architecture.
    progress.tick("iOS: compiling arm64 binary");
    let arm_target = PekoTarget::new(OperatingSystem::IOS, Architecture::Arm, false);
    let (_, arm_diagnostics) = execution::incremental::compile_project(
        cli_info.get_peko_root(),
        project,
        arm_target,
        project.get_root().join(".peko/incremental"),
        Some(arm_app.join(&project.name)),
        false,
        Vec::new(),
        None,
        None,
        None,
        Some(entitlements.clone()),
        progress,
    )?;
    if let Some(diagnostics) = arm_diagnostics {
        return Err(BundleError::CompileDiagnostics(diagnostics));
    }

    progress.tick("iOS: compiling x86_64 simulator binary");
    let x86_64_target = PekoTarget::new(OperatingSystem::IOS, Architecture::X86_64, false);
    let (_, x86_64_diagnostics) = execution::incremental::compile_project(
        cli_info.get_peko_root(),
        project,
        x86_64_target,
        project.get_root().join(".peko/incremental"),
        Some(x86_64_app.join(&project.name)),
        false,
        Vec::new(),
        None,
        None,
        None,
        Some(entitlements.clone()),
        progress,
    )?;
    if let Some(diagnostics) = x86_64_diagnostics {
        return Err(BundleError::CompileDiagnostics(diagnostics));
    }

    // Both arm64 and x86_64 simulator bundles are produced. Release
    // signing and .ipa packaging are handled by sign().

    // Simulator bundles must carry a signature to launch. Ad-hoc sign
    // each one. Capability entitlements live in the linked Mach-O section,
    // not in this signature.
    progress.tick("iOS: ad-hoc signing simulator bundles");
    signing::adhoc_sign_apple_bundle(&arm_app)?;
    signing::adhoc_sign_apple_bundle(&x86_64_app)?;

    guard.commit();
    Ok(())
}

/// Sign the arm64 device `.app` and package it into a `.ipa`.
///
/// The provisioning profile is embedded as `embedded.mobileprovision`,
/// entitlements come from a registered entitlements file or are extracted
/// from the profile, the bundle is signed with the `apple-codesign` crate,
/// and the signed `.app` is wrapped in `Payload/` and zipped into a
/// `<Name>.ipa`. Returns `false` when no iOS key is registered.
pub fn sign(
    _cli_info: &CLIInfo,
    project: &PekoProject,
    ios_build_directory: PathBuf,
) -> BundleResult<bool> {
    let Some(ui_info) = project.ui_project_info.as_ref() else {
        return Ok(false);
    };

    let key = match signing::resolve_apple(project.get_root(), &ui_info.bundle_id, "ios")? {
        Some(key) => key,
        None => return Ok(false),
    };

    // iOS distribution requires a provisioning profile.
    let Some(profile) = key.profile.as_ref() else {
        return Err(BundleError::Signing(
            "iOS signing requires a provisioning profile; none registered".to_string(),
        ));
    };

    let app_file_name = format!("{}.app", project.name);
    let arm_app = ios_build_directory.join("arm").join(&app_file_name);
    if !arm_app.exists() {
        return Err(BundleError::Signing(format!(
            "device .app not found at {}",
            arm_app.display()
        )));
    }

    // Embed the provisioning profile inside the bundle.
    let embedded = arm_app.join("embedded.mobileprovision");
    io_at(&embedded, fs::copy(profile, &embedded).map(|_| ()))?;

    // Entitlements come from a registered file when present, otherwise
    // from the provisioning profile.
    let entitlements_xml = match key.entitlements.as_ref() {
        Some(path) => Some(io_at(path, fs::read_to_string(path))?),
        None => signing::entitlements_from_profile(profile)?,
    };

    signing::sign_apple_bundle(&arm_app, &key, entitlements_xml.as_deref())?;

    // Wrap the signed bundle in Payload/ and zip into a .ipa.
    let ipa_path = ios_build_directory.join(format!("{}.ipa", project.name));
    if ipa_path.exists() {
        io_at(&ipa_path, fs::remove_file(&ipa_path))?;
    }
    let ipa_file = io_at(&ipa_path, File::create(&ipa_path))?;
    let mut ipa_writer = ZipWriter::new(ipa_file);

    // A directory entry keeps Payload/ present even before files are
    // added under it.
    ipa_writer.add_directory::<&str, ExtendedFileOptions>(
        "Payload/",
        FileOptions::default().compression_method(CompressionMethod::Stored),
    )?;
    let payload_prefix = format!("Payload/{app_file_name}");
    recursive_zip_add(&mut ipa_writer, &arm_app, &payload_prefix)?;
    ipa_writer.finish()?;

    Ok(true)
}
