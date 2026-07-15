//! Android bundler.
//!
//! Resource processing uses aapt2 for both build modes. A per-host aapt2
//! jar lives at `Compiler/bin/aapt2/<os>/aapt2.jar` and contains a native
//! executable; [`resolve_aapt2`] selects the jar for the host operating
//! system, extracts that executable next to the jar on first use, and runs
//! it directly.
//!
//! Both modes assemble an app bundle with aapt2 and bundletool
//! ([`build_app_bundle`]). Release builds keep the `.aab` as the artifact
//! and [`sign`] signs it with `jarsigner` using the project's registered
//! upload key. Debug builds turn the bundle into an installable, signed
//! universal `.apk` with `bundletool build-apks`, which signs the apk
//! with the toolchain development keystore.
//!
//! Java tools (`bundletool`, `jarsigner`) run from the JDK shipped at
//! `Compiler/java`.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use peko_core::target::{Architecture, OperatingSystem, PekoTarget};
use zip::write::{ExtendedFileOptions, FileOptions};
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::bundler::{
    BundleError, BundleResult, CleanupGuard, io_at, java_tool, recursive_zip_add, run_tool, signing,
};
use crate::cli::CLIInfo;
use crate::cli::reporting::ProgressSink;
use crate::execution;
use crate::project::PekoProject;

/// Alias of the key inside the toolchain development keystore at
/// Compiler/bundling/androiddevkey.keystore. bundletool selects the key
/// by alias when signing the debug apk.
const DEV_KEYSTORE_ALIAS: &str = "pekodevkey";

/// Build an Android artifact for the project. Debug builds produce a
/// dev-signed `.apk`; release builds produce an unsigned `.aab` that
/// [`sign`] signs afterward.
pub fn bundle(
    cli_info: &CLIInfo,
    project: &mut PekoProject,
    android_build_directory: PathBuf,
    progress: &dyn ProgressSink,
) -> BundleResult<()> {
    let release = cli_info.flags.has_flag("release");

    // Clean the build directory if it already exists, then recreate it.
    if android_build_directory.exists() {
        let removal = if android_build_directory.is_dir() {
            fs::remove_dir_all(&android_build_directory)
        } else {
            fs::remove_file(&android_build_directory)
        };
        io_at(&android_build_directory, removal)?;
    }
    io_at(
        &android_build_directory,
        fs::create_dir_all(&android_build_directory),
    )?;

    // Arm the cleanup guard: if any step below fails, the partial build
    // directory is removed before the error propagates out.
    let guard = CleanupGuard::new(android_build_directory.clone());

    // Five user-visible phases below; the inner compile contributes its
    // own (rechecks + recompiles + link) via add_to_total.
    progress.add_to_total(5);

    progress.tick("Android: preparing app directory tree");

    // Lay out the app's directory tree.
    let app_directory = android_build_directory.join(&project.name);
    io_at(&app_directory, fs::create_dir_all(&app_directory))?;
    let assets_dir = app_directory.join("assets");
    io_at(&assets_dir, fs::create_dir_all(&assets_dir))?;

    // Project assets go in the assets dir so they package into the app's
    // assets/, where the assets package's Android native layer reads them
    // via AAssetManager_open(name). Subdirectories are preserved. The
    // app's Java/Kotlin startup must call
    // peko_asset_set_android_manager(AAssetManager_fromJava(...)) once
    // before any asset request, or the native layer has no manager to
    // open assets from.
    crate::bundler::copy_project_assets(project, &assets_dir)?;

    // Select the ABI to build. arm64-v8a is the default (real devices); x86_64
    // is for emulators (fast under KVM on an x86 host). `--arch x86_64` picks it;
    // anything else falls back to arm64. Each ABI's `.so` goes in its own
    // `lib/<abi>/` dir, which is exactly what the universal APK packages.
    let architecture = match cli_info.flags.get_flag("arch").as_deref() {
        Some("x86_64") => Architecture::X86_64,
        _ => Architecture::Arm,
    };
    let abi = match architecture {
        Architecture::X86_64 => "x86_64",
        _ => "arm64-v8a",
    };
    let lib_directory = app_directory.join("lib").join(abi);
    io_at(&lib_directory, fs::create_dir_all(&lib_directory))?;

    // Link as a shared library so the Android app's NativeActivity can dlopen it.
    progress.tick(&format!("Android: compiling native library ({abi})"));
    let android_target = PekoTarget::new(OperatingSystem::Android, architecture, false);
    let diagnostics = execution::incremental::compile_project(
        cli_info.get_peko_root(),
        project,
        android_target,
        project.get_root().join(".peko/incremental"),
        Some(lib_directory.join("libPekoApp.so")),
        true,
        // shared library
        Vec::new(),
        None,
        None,
        !cli_info.flags.has_flag("release"),
        cli_info.flags.has_flag("demo"),
        progress,
    )?;
    if let Some(diagnostics) = diagnostics {
        return Err(BundleError::CompileDiagnostics(diagnostics));
    }

    // App icon. The flat masked icon in res/mipmap covers launchers before
    // API 26 (adaptive icons).
    let ui = project.ui_project_info.as_ref().unwrap();
    let mipmap_directory = app_directory.join("res/mipmap");
    io_at(&mipmap_directory, fs::create_dir_all(&mipmap_directory))?;
    let icon_path = mipmap_directory.join("icon.png");
    ui.icon_for(OperatingSystem::Android)
        .shaped_for(OperatingSystem::Android)
        .to_png(&mut io_at(&icon_path, File::create(&icon_path))?);

    // Adaptive icon (API 26+): a foreground layer over a background layer the
    // launcher masks to its own shape. Emitted only when the icon builder saved
    // a foreground/background split; otherwise the flat icon above is used.
    if let Some((foreground, background)) = ui.android_adaptive() {
        // Layers are 108dp and the inner 72dp is the safe zone the launcher
        // shows. The foreground is inset to that safe zone so no mask clips it.
        // Layers are written at xxxhdpi (432px) with no density scaling.
        let layer_px = 432;
        let safe_inset = (108.0 - 72.0) / 2.0 / 108.0;

        let drawable_directory = app_directory.join("res/drawable-nodpi");
        io_at(&drawable_directory, fs::create_dir_all(&drawable_directory))?;

        let background_path = drawable_directory.join("icon_background.png");
        background.resize(layer_px, layer_px).to_png(&mut io_at(
            &background_path,
            File::create(&background_path),
        )?);

        let foreground_path = drawable_directory.join("icon_foreground.png");
        foreground
            .shaped(crate::project::IconShape::Square, safe_inset)
            .resize(layer_px, layer_px)
            .to_png(&mut io_at(
                &foreground_path,
                File::create(&foreground_path),
            )?);

        let anydpi_directory = app_directory.join("res/mipmap-anydpi-v26");
        io_at(&anydpi_directory, fs::create_dir_all(&anydpi_directory))?;
        let adaptive_xml = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
<adaptive-icon xmlns:android=\"http://schemas.android.com/apk/res/android\">\n\
    <background android:drawable=\"@drawable/icon_background\"/>\n\
    <foreground android:drawable=\"@drawable/icon_foreground\"/>\n\
</adaptive-icon>\n";
        let adaptive_path = anydpi_directory.join("icon.xml");
        io_at(&adaptive_path, fs::write(&adaptive_path, adaptive_xml))?;
    }

    // strings.xml + AndroidManifest.xml: copy the project's configfile
    // templates into the build tree.
    let values_directory = app_directory.join("res/values");
    io_at(&values_directory, fs::create_dir_all(&values_directory))?;

    let strings_src = project
        .get_root()
        .join(".peko/bundling/configfiles/android/strings.xml");
    let strings_dst = values_directory.join("strings.xml");
    io_at(&strings_src, fs::copy(&strings_src, &strings_dst))?;

    let manifest_src = project
        .get_root()
        .join(".peko/bundling/configfiles/android/AndroidManifest.xml");
    let manifest_dst = app_directory.join("AndroidManifest.xml");
    io_at(&manifest_src, fs::copy(&manifest_src, &manifest_dst))?;

    if release {
        // The unsigned .aab is the release artifact; sign() signs it.
        build_app_bundle(
            cli_info,
            project,
            &android_build_directory,
            &app_directory,
            false,
            progress,
        )?;
    } else {
        build_debug_apk(
            cli_info,
            project,
            &android_build_directory,
            &app_directory,
            progress,
        )?;
    }

    io_at(&app_directory, fs::remove_dir_all(&app_directory))?;

    guard.commit();
    Ok(())
}

/// Resolve the aapt2 executable for the host operating system.
///
/// The host jar lives at `Compiler/bin/aapt2/<os>/aapt2.jar`, where `<os>`
/// is `macos`, `linux`, or `windows`. The jar contains a native executable
/// plus its dependent files. The contents are extracted once into the same
/// per-host directory and the executable is reused on later builds.
fn resolve_aapt2(cli_info: &CLIInfo) -> BundleResult<PathBuf> {
    let peko_root = cli_info.get_peko_root();
    let base = peko_root
        .join("Compiler/bin/aapt2")
        .join(std::env::consts::OS);
    let jar = base.join("aapt2.jar");
    let bin_name = if cfg!(windows) { "aapt2.exe" } else { "aapt2" };
    let binary = base.join(bin_name);

    if binary.exists() {
        return Ok(binary);
    }

    io_at(&base, fs::create_dir_all(&base))?;
    let jar_file = io_at(&jar, File::open(&jar))?;
    let mut archive = ZipArchive::new(jar_file)?;
    archive.extract(&base)?;

    #[cfg(unix)]
    if binary.exists() {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = io_at(&binary, fs::metadata(&binary))?.permissions();
        permissions.set_mode(0o755);
        io_at(&binary, fs::set_permissions(&binary, permissions))?;
    }

    if !binary.exists() {
        return Err(BundleError::Io {
            path: binary,
            source: io::Error::new(
                io::ErrorKind::NotFound,
                "aapt2 executable not found inside the host aapt2.jar",
            ),
        });
    }
    Ok(binary)
}

/// Compile and link the laid-out app tree into an unsigned `.aab` app
/// bundle with aapt2 and bundletool. Returns the path to the `.aab`.
fn build_app_bundle(
    cli_info: &CLIInfo,
    project: &PekoProject,
    android_build_directory: &Path,
    app_directory: &Path,
    debuggable: bool,
    progress: &dyn ProgressSink,
) -> BundleResult<PathBuf> {
    let aapt2 = resolve_aapt2(cli_info)?;

    // Compile the resource tree into aapt2's intermediate container.
    progress.tick("Android: compiling resources with aapt2");
    let compiled_resources = android_build_directory.join("compiled.zip");
    run_tool(
        "aapt2",
        Command::new(&aapt2)
            .arg("compile")
            .arg("--dir")
            .arg(app_directory.join("res"))
            .arg("-o")
            .arg(&compiled_resources),
    )?;

    // Link into protobuf form, the format an app bundle requires.
    progress.tick("Android: linking resources for the bundle");
    let proto_apk = android_build_directory.join("base-proto.apk");
    let mut link = Command::new(&aapt2);
    link.arg("link")
        .arg("--proto-format")
        .arg("-o")
        .arg(&proto_apk)
        .arg("-I")
        .arg(
            cli_info
                .get_peko_root()
                .join("Compiler/bundling/android.jar"),
        )
        .arg("--manifest")
        .arg(app_directory.join("AndroidManifest.xml"))
        .arg("-R")
        .arg(&compiled_resources)
        .arg("--auto-add-overlay");
    // Debug builds set android:debuggable at link time. Release builds leave
    // it unset, which Google Play requires for published bundles.
    if debuggable {
        link.arg("--debug-mode");
    }
    run_tool("aapt2", &mut link)?;

    // Extract the proto APK so its parts can be rearranged into the base
    // module layout bundletool expects.
    let proto_extracted = android_build_directory.join("proto");
    let proto_apk_file = io_at(&proto_apk, File::open(&proto_apk))?;
    ZipArchive::new(proto_apk_file)?
        .extract_unwrapped_root_dir(&proto_extracted, zip::read::root_dir_common_filter)?;
    io_at(&proto_apk, fs::remove_file(&proto_apk))?;

    // Assemble the base module zip. bundletool expects the manifest under
    // manifest/, the protobuf resource table as resources.pb, compiled
    // resources under res/, native libraries under lib/, and loose files
    // under assets/.
    progress.tick("Android: assembling base module");
    let module_zip = android_build_directory.join("base.zip");
    let module_file = io_at(&module_zip, File::create(&module_zip))?;
    let mut module_writer = ZipWriter::new(module_file);

    module_writer.start_file::<&str, ExtendedFileOptions>(
        "manifest/AndroidManifest.xml",
        FileOptions::default().compression_method(CompressionMethod::Stored),
    )?;
    let manifest_bytes = io_at(
        &proto_extracted.join("AndroidManifest.xml"),
        fs::read(proto_extracted.join("AndroidManifest.xml")),
    )?;
    io_at(&module_zip, module_writer.write_all(&manifest_bytes))?;

    module_writer.start_file::<&str, ExtendedFileOptions>(
        "resources.pb",
        FileOptions::default().compression_method(CompressionMethod::Stored),
    )?;
    let resources_bytes = io_at(
        &proto_extracted.join("resources.pb"),
        fs::read(proto_extracted.join("resources.pb")),
    )?;
    io_at(&module_zip, module_writer.write_all(&resources_bytes))?;

    let proto_res = proto_extracted.join("res");
    if proto_res.is_dir() {
        recursive_zip_add(&mut module_writer, &proto_res, "res")?;
    }
    let assets_dir = app_directory.join("assets");
    if assets_dir.is_dir() {
        recursive_zip_add(&mut module_writer, &assets_dir, "assets")?;
    }
    recursive_zip_add(&mut module_writer, &app_directory.join("lib"), "lib")?;

    // Prebuilt application DEX shipped by native packages (std::webview needs a
    // Java WebViewClient and JS bridge that cannot be built from JNI alone). The
    // bundle keeps DEX under dex/; bundletool places it at the apk root as
    // classes.dex. The manifest is marked hasCode to load it.
    let dex_files = crate::execution::native::collect_android_dex_files(
        cli_info.get_peko_root(),
        project.get_root(),
        cli_info.flags.has_flag("demo"),
    );
    for (index, dex) in dex_files.iter().enumerate() {
        let name = if index == 0 {
            "dex/classes.dex".to_owned()
        } else {
            format!("dex/classes{}.dex", index + 1)
        };
        module_writer.start_file::<&str, ExtendedFileOptions>(
            name.as_str(),
            FileOptions::default().compression_method(CompressionMethod::Stored),
        )?;
        let dex_bytes = io_at(dex, fs::read(dex))?;
        io_at(&module_zip, module_writer.write_all(&dex_bytes))?;
    }

    module_writer.finish()?;
    io_at(&proto_extracted, fs::remove_dir_all(&proto_extracted))?;
    io_at(&compiled_resources, fs::remove_file(&compiled_resources))?;

    // Build the app bundle with bundletool.
    progress.tick("Android: building app bundle with bundletool");
    let aab_output = android_build_directory.join(format!("{}.aab", project.name));
    if aab_output.exists() {
        io_at(&aab_output, fs::remove_file(&aab_output))?;
    }
    run_tool(
        "bundletool",
        Command::new(java_tool(cli_info.get_peko_root(), "java"))
            .arg("--sun-misc-unsafe-memory-access=allow")
            .arg("-jar")
            .arg(cli_info.get_peko_root().join("Compiler/bin/bundletool.jar"))
            .arg("build-bundle")
            .arg("--modules")
            .arg(&module_zip)
            .arg("--output")
            .arg(&aab_output),
    )?;

    io_at(&module_zip, fs::remove_file(&module_zip))?;
    Ok(aab_output)
}

/// Build a dev-signed universal `.apk` for local installation.
///
/// The app bundle is built first, then `bundletool build-apks` produces a
/// signed universal apk in an `.apks` archive. bundletool signs the apk
/// with the development keystore (v1, v2, and v3) and aligns it, so no
/// separate signing or alignment step is needed. The universal apk is
/// extracted out of the archive as the final `.apk`.
fn build_debug_apk(
    cli_info: &CLIInfo,
    project: &PekoProject,
    android_build_directory: &Path,
    app_directory: &Path,
    progress: &dyn ProgressSink,
) -> BundleResult<()> {
    let aab = build_app_bundle(
        cli_info,
        project,
        android_build_directory,
        app_directory,
        true,
        progress,
    )?;

    progress.tick("Android: generating signed APK with bundletool");
    let apks_path = android_build_directory.join("debug.apks");
    if apks_path.exists() {
        io_at(&apks_path, fs::remove_file(&apks_path))?;
    }
    let dev_keystore = cli_info
        .get_peko_root()
        .join("Compiler/bundling/androiddevkey.keystore");
    run_tool(
        "bundletool",
        Command::new(java_tool(cli_info.get_peko_root(), "java"))
            .arg("--sun-misc-unsafe-memory-access=allow")
            .arg("-jar")
            .arg(cli_info.get_peko_root().join("Compiler/bin/bundletool.jar"))
            .arg("build-apks")
            .arg(format!("--bundle={}", aab.display()))
            .arg(format!("--output={}", apks_path.display()))
            .arg("--mode=universal")
            .arg(format!("--ks={}", dev_keystore.display()))
            .arg("--ks-pass=pass:password")
            .arg(format!("--ks-key-alias={DEV_KEYSTORE_ALIAS}"))
            .arg("--key-pass=pass:password"),
    )?;

    // build-apks --mode=universal writes an .apks archive (a zip) holding
    // a single universal apk. Extract it as the installable apk.
    let final_apk = android_build_directory.join(format!("{}.apk", project.name));
    let apks_file = io_at(&apks_path, File::open(&apks_path))?;
    let mut apks_archive = ZipArchive::new(apks_file)?;
    let mut apk_index = None;
    for index in 0..apks_archive.len() {
        let entry = apks_archive.by_index(index)?;
        if entry.name().ends_with(".apk") {
            apk_index = Some(index);
            break;
        }
    }
    let Some(index) = apk_index else {
        return Err(BundleError::Io {
            path: apks_path,
            source: io::Error::new(
                io::ErrorKind::NotFound,
                "bundletool produced no apk inside the apks archive",
            ),
        });
    };
    {
        let mut entry = apks_archive.by_index(index)?;
        let mut out = io_at(&final_apk, File::create(&final_apk))?;
        io_at(&final_apk, io::copy(&mut entry, &mut out).map(|_| ()))?;
    }

    io_at(&aab, fs::remove_file(&aab))?;
    io_at(&apks_path, fs::remove_file(&apks_path))?;
    Ok(())
}

/// Sign the release `.aab` with the project's registered Android upload
/// key. Returns `false` when no Android key is registered.
pub fn sign(
    cli_info: &CLIInfo,
    project: &PekoProject,
    android_build_directory: PathBuf,
) -> BundleResult<bool> {
    let Some(ui_info) = project.ui_project_info.as_ref() else {
        return Ok(false);
    };

    let key = match signing::resolve_android(project.get_root(), &ui_info.bundle_id)? {
        Some(key) => key,
        None => return Ok(false),
    };

    let aab = android_build_directory.join(format!("{}.aab", project.name));
    if !aab.exists() {
        return Err(BundleError::Signing(format!(
            "release bundle not found at {}",
            aab.display()
        )));
    }

    let jarsigner = java_tool(cli_info.get_peko_root(), "jarsigner");
    signing::jarsigner_sign_aab(&aab, &key, &jarsigner)?;
    Ok(true)
}
