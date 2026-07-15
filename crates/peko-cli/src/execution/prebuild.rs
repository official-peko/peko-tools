//! `peko build --prebuild`: prebuild a library package for proprietary
//! distribution.
//!
//! The flow turns a normal library package source tree into a **prebuilt
//! distributable** under `<package>/prebuilt/`:
//!
//!   1. Prebuild the package's own `.peko` modules (and native sources) to
//!      object files for the host target triple, into
//!      `objects/<triple>/`. The modules are compiled through the ordinary
//!      **import** path (a throwaway harness project that imports the package),
//!      so their mangled symbols are exactly what a consumer that imports the
//!      package by name references and links against.
//!   2. Generate a **definition-only `.peko` stub** for every source file into
//!      `stubs/<relpath>.peko`: the public interface (signatures, visibility,
//!      fields, enum variants, trait slots) with all function, method, and
//!      constructor bodies stripped. Consumers typecheck and codegen against
//!      these stubs and link the prebuilt objects, so the implementation source
//!      never ships.
//!   3. Write `prebuilt.toml`, the prebuilt manifest (name, version, entry, the
//!      stub list, and the per-triple object table).
//!
//! A consumer that depends on such a package resolves its imports against the
//! stub tree (see `Manifest::to_external_module`) and links the prebuilt
//! objects (see the prebuilt link path in `execution::incremental`), never
//! compiling the package's bodies.
//!
//! Entered from `peko build --prebuild`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use peko_core::config::{Manifest, PrebuiltManifest};
use peko_core::formatter::data_structures::FormatConfig;
use peko_core::target::PekoTarget;

use crate::cli::CLIInfo;
use crate::cli::reporting::Reporter;
use crate::project::PekoProject;

/// Run the prebuild flow: prebuild objects, generate stubs, and write the
/// manifest, then pack the all-platforms `.pkbundle`.
pub fn run(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    let loaded = match Manifest::discover(".") {
        Ok(loaded) => loaded,
        Err(error) => {
            reporter.error(format!("could not load peko.toml: {error}"));
            return ExitCode::FAILURE;
        }
    };
    if !matches!(loaded.manifest, Manifest::Package(_)) {
        reporter.error(
            "`build --prebuild` must run in a library package (a peko.toml with [package] and [lib])",
        );
        return ExitCode::FAILURE;
    }

    let name = loaded.manifest.name().to_string();
    let version = loaded.manifest.version().to_string();
    let root = loaded.root.clone();
    let entry = loaded.manifest.entry(&root);
    let source_root = entry
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| root.clone());
    let entry_name = entry
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("lib.peko")
        .to_owned();

    let out_dir = PrebuiltManifest::dir(&root);
    let stubs_dir = PrebuiltManifest::stubs_dir(&root);
    let objects_dir = PrebuiltManifest::objects_dir(&root);
    let manifest_path = PrebuiltManifest::manifest_path(&root);

    reporter.status("Prebuilding", format!("package {name} {version}"));

    // Clear any prior prebuilt output. The manifest must be absent while the
    // objects are prebuilt below, so `to_external_module`/`is_prebuilt` see the
    // package as from-source (real bodies) rather than redirecting to stubs.
    // Cleared before collecting sources so a prior run's stubs are not picked
    // up as package sources.
    let _ = std::fs::remove_file(&manifest_path);
    let _ = std::fs::remove_dir_all(&out_dir);

    // Collect the package's own `.peko` sources. The `prebuilt/` output tree is
    // excluded (it was just removed, and never holds implementation source).
    let mut peko_files = Vec::new();
    collect_peko_files(&source_root, &out_dir, &mut peko_files);
    peko_files.sort();
    if peko_files.is_empty() {
        reporter.error(format!(
            "no .peko files found under {}",
            source_root.display()
        ));
        return ExitCode::FAILURE;
    }

    // Stage 1: generate a definition-only stub for every source file. This is
    // target-independent, so it runs once.
    let mut stub_entries: Vec<String> = Vec::new();
    for file in &peko_files {
        let source = match std::fs::read_to_string(file) {
            Ok(source) => source,
            Err(error) => {
                reporter.error(format!("could not read {}: {error}", file.display()));
                return ExitCode::FAILURE;
            }
        };
        let stub = peko_core::formatter::format_source(
            &source,
            file,
            FormatConfig {
                definitions_only: true,
                ..FormatConfig::default()
            },
        );
        let relative = file.strip_prefix(&source_root).unwrap_or(file);
        let destination = stubs_dir.join(relative);
        if let Some(parent) = destination.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            reporter.error(format!("could not create {}: {error}", parent.display()));
            return ExitCode::FAILURE;
        }
        if let Err(error) = std::fs::write(&destination, stub.as_bytes()) {
            reporter.error(format!(
                "could not write {}: {error}",
                destination.display()
            ));
            return ExitCode::FAILURE;
        }
        stub_entries.push(relative.to_string_lossy().replace('\\', "/"));
    }

    // Ship the package's FFI headers (`*.peko.h`) alongside the stubs, at the
    // same relative paths. A consumer resolves the stubs' `import c::...` FFI
    // imports against these; the C implementation is not shipped (it is linked
    // from the prebuilt objects), only the declaration header. Without them a
    // prebuilt package that binds native code would not resolve its imports.
    let mut header_files = Vec::new();
    collect_ffi_headers(&source_root, &out_dir, &mut header_files);
    for header in &header_files {
        let relative = header.strip_prefix(&source_root).unwrap_or(header);
        let destination = stubs_dir.join(relative);
        if let Some(parent) = destination.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            reporter.error(format!("could not create {}: {error}", parent.display()));
            return ExitCode::FAILURE;
        }
        if let Err(error) = std::fs::copy(header, &destination) {
            reporter.error(format!(
                "could not copy FFI header {}: {error}",
                header.display()
            ));
            return ExitCode::FAILURE;
        }
    }

    // Stage 2: resolve the target triples to prebuild for. `--target os-arch,...`
    // overrides the default cross-set, which is every platform+arch peko
    // supports so a distributable package covers them all.
    const DEFAULT_TARGETS: &[&str] = &[
        "macos-arm",
        "macos-x86_64",
        "ios-arm",
        "android-arm",
        "android-x86_64",
        "windows-x86_64",
        "linux-x86_64",
    ];
    let descriptors: Vec<String> = match cli_info.flags.get_flag("target") {
        Some(csv) => csv
            .split(',')
            .map(|part| part.trim().to_owned())
            .filter(|part| !part.is_empty())
            .collect(),
        None => DEFAULT_TARGETS.iter().map(|d| (*d).to_owned()).collect(),
    };
    let mut targets = Vec::new();
    for descriptor in &descriptors {
        match PekoTarget::from_descriptor(descriptor) {
            Ok(target) => targets.push(target),
            Err(error) => {
                reporter.error(format!(
                    "unknown target `{descriptor}`: {error} (use os-arch, e.g. macos-arm, windows-x86_64)"
                ));
                return ExitCode::FAILURE;
            }
        }
    }

    // Stage 3: prebuild the package's objects per target. Failures are collected
    // so the report covers every platform instead of stopping at the first.
    let mut per_triple: Vec<(String, Vec<String>)> = Vec::new();
    let mut failures: Vec<(String, String)> = Vec::new();
    for target in targets {
        let triple = target.to_triple();
        let triple_out = objects_dir.join(&triple);
        if let Err(error) = std::fs::create_dir_all(&triple_out) {
            failures.push((triple, format!("could not create output dir: {error}")));
            continue;
        }
        // The package's own prebuilt static libs for this target, if any.
        let native_libs: Vec<PathBuf> = loaded
            .manifest
            .native()
            .map(|native| {
                native
                    .libs
                    .for_target(target.operating_system, target.architecture)
                    .into_iter()
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();

        reporter.status("Prebuilding", format!("{name} {version} for {triple}"));
        match prebuild_objects(
            cli_info.get_peko_root(),
            &root,
            &name,
            &version,
            &source_root,
            &entry,
            &peko_files,
            &native_libs,
            target,
            &triple_out,
            reporter,
        ) {
            Ok(object_paths) if !object_paths.is_empty() => {
                let relatives: Vec<String> = object_paths
                    .iter()
                    .map(|path| {
                        path.strip_prefix(&out_dir)
                            .unwrap_or(path)
                            .to_string_lossy()
                            .replace('\\', "/")
                    })
                    .collect();
                reporter.success(format!("{triple}: {} object(s)", relatives.len()));
                per_triple.push((triple, relatives));
            }
            Ok(_) => {
                let _ = std::fs::remove_dir_all(&triple_out);
                failures.push((triple, "produced no objects".to_owned()));
            }
            Err(error) => {
                // Drop the partial output so a failed triple is not half-recorded.
                let _ = std::fs::remove_dir_all(&triple_out);
                failures.push((triple, error));
            }
        }
    }

    if per_triple.is_empty() {
        reporter.error("no target prebuilt successfully");
        for (triple, error) in &failures {
            reporter.error(format!("  {triple}: {error}"));
        }
        return ExitCode::FAILURE;
    }

    // Stage 4: write the prebuilt manifest with every successful triple.
    let mut manifest_text = String::new();
    manifest_text.push_str("# Generated by `peko build --prebuild`. Do not edit by hand.\n\n");
    manifest_text.push_str("[prebuilt]\n");
    manifest_text.push_str(&format!("name = \"{name}\"\n"));
    manifest_text.push_str(&format!("version = \"{version}\"\n"));
    // The compiler/toolchain version these objects were built with. The gated
    // bundle is served per toolchain, and a consumer only links objects built by
    // its own toolchain (ABI lockstep).
    manifest_text.push_str(&format!("toolchain = \"{}\"\n", env!("CARGO_PKG_VERSION")));
    manifest_text.push_str(&format!("entry = \"{entry_name}\"\n"));
    manifest_text.push_str("stubs = [\n");
    for stub in &stub_entries {
        manifest_text.push_str(&format!("    \"{stub}\",\n"));
    }
    manifest_text.push_str("]\n\n");
    manifest_text.push_str("[prebuilt.objects]\n");
    for (triple, relatives) in &per_triple {
        manifest_text.push_str(&format!("\"{triple}\" = [\n"));
        for object in relatives {
            manifest_text.push_str(&format!("    \"{object}\",\n"));
        }
        manifest_text.push_str("]\n");
    }

    if let Err(error) = std::fs::write(&manifest_path, manifest_text) {
        reporter.error(format!(
            "could not write {}: {error}",
            manifest_path.display()
        ));
        return ExitCode::FAILURE;
    }

    let triples: Vec<&str> = per_triple
        .iter()
        .map(|(triple, _)| triple.as_str())
        .collect();
    reporter.success(format!(
        "prebuilt {name} {version}: {} stub(s), {} target(s) [{}] → {}",
        stub_entries.len(),
        per_triple.len(),
        triples.join(", "),
        out_dir.display()
    ));

    // Stage 5: pack the whole all-platforms prebuilt tree into ONE uploadable
    // bundle. The registry serves this single file (keyed by toolchain) to
    // entitled accounts; the admin uploads it via the web console. Only whole
    // successful builds are bundled, so a partial build never ships.
    if failures.is_empty() {
        match crate::registry::pack::pack_prebuilt(&loaded, &out_dir) {
            Ok(bytes) => {
                let bundle_path = root.join(format!("{name}-{version}.pkbundle"));
                if let Err(error) = std::fs::write(&bundle_path, &bytes) {
                    reporter.error(format!("could not write bundle: {error}"));
                    return ExitCode::FAILURE;
                }
                reporter.success(format!(
                    "bundle → {} ({:.1} MB, all {} platforms, toolchain {})",
                    bundle_path.display(),
                    bytes.len() as f64 / (1024.0 * 1024.0),
                    per_triple.len(),
                    env!("CARGO_PKG_VERSION"),
                ));
            }
            Err(error) => {
                reporter.error(format!("could not pack prebuilt bundle: {error}"));
                return ExitCode::FAILURE;
            }
        }
    }

    if !failures.is_empty() {
        reporter.warning(format!("{} target(s) failed:", failures.len()));
        for (triple, error) in &failures {
            reporter.warning(format!("  {triple}: {error}"));
        }
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Compile the package's own `.peko` modules (and native sources) to objects
/// for `target`, into `triple_out`, and return their absolute paths.
///
/// The package is compiled through the ordinary consumer path: a throwaway
/// harness project that imports the package by name and each of its submodules
/// is built with [`compile_project`]. Because the modules are reached by import
/// (not compiled as a root project), their top-level module names — and so
/// their mangled symbols — are exactly what a consumer that imports the package
/// references. The package's own object files are then collected out of the
/// harness's incremental object tree.
///
/// [`compile_project`]: crate::execution::incremental::compile_project
#[allow(clippy::too_many_arguments)]
fn prebuild_objects(
    peko_root: &Path,
    package_root: &Path,
    package_name: &str,
    package_version: &str,
    source_root: &Path,
    entry: &Path,
    peko_files: &[PathBuf],
    native_libs: &[PathBuf],
    target: PekoTarget,
    triple_out: &Path,
    reporter: &Reporter,
) -> Result<Vec<PathBuf>, String> {
    let package_root_canon = package_root
        .canonicalize()
        .unwrap_or_else(|_| package_root.to_path_buf());

    // Build the throwaway harness project in a fresh temp directory.
    let harness = std::env::temp_dir().join(format!(
        "peko-prebuild-{package_name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&harness);
    std::fs::create_dir_all(harness.join("src"))
        .map_err(|error| format!("could not create harness dir: {error}"))?;

    // The harness imports the package (its entry) and every submodule by its
    // canonical package path, so every module is compiled with the naming a
    // consumer would produce.
    let mut main_source = String::new();
    for file in peko_files {
        let import_path = package_import_path(package_name, source_root, entry, file);
        main_source.push_str(&format!("import {import_path}\n"));
    }
    main_source.push_str("fn on_start() {}\n");
    std::fs::write(harness.join("src/main.peko"), main_source)
        .map_err(|error| format!("could not write harness main: {error}"))?;

    std::fs::write(
        harness.join("peko.toml"),
        format!(
            "[project]\nname = \"__peko_prebuild_{package_name}\"\nbundle = \"dev.peko.prebuild.{package_name}\"\nversion = \"0.0.0\"\nentry = \"src/main.peko\"\n"
        ),
    )
    .map_err(|error| format!("could not write harness peko.toml: {error}"))?;

    // A hand-written lockfile pins the package as a path dependency so the
    // harness resolves it without any registry/network access.
    std::fs::write(
        harness.join("peko.lock"),
        format!(
            "version = 1\n\n[[package]]\nname = \"{package_name}\"\nversion = \"{package_version}\"\nsource = \"path\"\npath = \"{}\"\n",
            package_root_canon.display()
        ),
    )
    .map_err(|error| format!("could not write harness peko.lock: {error}"))?;

    let mut project = PekoProject::from_directory(&harness)
        .map_err(|error| format!("could not load harness project: {error}"))?;

    let incremental_dir = harness.join(".peko/incremental");
    let binary_out = harness.join("prebuild-bin");

    let progress = reporter.progress();
    let diagnostics = crate::execution::incremental::compile_project(
        peko_root,
        &mut project,
        target,
        incremental_dir.clone(),
        Some(binary_out),
        false,
        Vec::new(),
        None,
        None,
        true,
        progress,
    )
    .map_err(|error| format!("harness compile failed: {error}"))?;

    if let Some(diagnostics) = diagnostics
        && diagnostics.has_errors()
    {
        reporter.report_diagnostics(&diagnostics);
        return Err(format!(
            "the package did not compile ({} error(s))",
            diagnostics.get_error_count()
        ));
    }

    // Collect the package's own module objects out of the harness object tree.
    let harness_objects = incremental_dir
        .join("objects")
        .join(target.operating_system.to_string())
        .join(target.architecture.to_string());

    let mut collected = Vec::new();
    for file in peko_files {
        let file_canon = file.canonicalize().unwrap_or_else(|_| file.clone());
        let source_object = harness_objects.join(format!("{}.o", pathbuf_to_fileid(&file_canon)));
        if !source_object.is_file() {
            // The entry file's module may have collapsed a trailing `main`, or
            // a file might not have produced a module; skip missing objects.
            continue;
        }
        let clean = file
            .strip_prefix(source_root)
            .unwrap_or(file)
            .to_string_lossy()
            .replace(['/', '\\'], "__");
        let destination = triple_out.join(format!("{clean}.o"));
        std::fs::copy(&source_object, &destination)
            .map_err(|error| format!("could not copy {}: {error}", source_object.display()))?;
        collected.push(destination);
    }

    // Collect the package's own native objects (named `<pkg>-<version>__...`).
    let native_dir = harness_objects.join("native");
    if native_dir.is_dir() {
        let native_out = triple_out.join("native");
        let sanitized_version = package_version.replace(['/', '\\', '.', ' '], "_");
        let prefix = format!("{package_name}-{sanitized_version}__");
        if let Ok(entries) = std::fs::read_dir(&native_dir) {
            for entry in entries.flatten() {
                let file_name = entry.file_name();
                let file_name = file_name.to_string_lossy();
                if !file_name.starts_with(&prefix) {
                    continue;
                }
                std::fs::create_dir_all(&native_out).ok();
                let destination = native_out.join(file_name.as_ref());
                std::fs::copy(entry.path(), &destination)
                    .map_err(|error| format!("could not copy native object: {error}"))?;
                collected.push(destination);
            }
        }
    }

    // Copy the package's own prebuilt static libraries ([native.libs]) for this
    // target, so a consumer that links the prebuilt package also links them (a
    // from-source consumer would get them from `native.libs.for_target`). Paths
    // are relative to the package root.
    if !native_libs.is_empty() {
        let native_out = triple_out.join("native");
        std::fs::create_dir_all(&native_out)
            .map_err(|error| format!("could not create native dir: {error}"))?;
        for lib in native_libs {
            let source = if lib.is_absolute() {
                lib.clone()
            } else {
                package_root.join(lib)
            };
            if !source.is_file() {
                return Err(format!(
                    "declared native.lib `{}` not found at {}",
                    lib.display(),
                    source.display()
                ));
            }
            let file_name = source
                .file_name()
                .ok_or_else(|| format!("native.lib `{}` has no file name", lib.display()))?;
            let destination = native_out.join(file_name);
            std::fs::copy(&source, &destination)
                .map_err(|error| format!("could not copy native.lib: {error}"))?;
            collected.push(destination);
        }
    }

    // Best-effort cleanup of the harness.
    let _ = std::fs::remove_dir_all(&harness);

    Ok(collected)
}

/// The canonical `import` path for a package source file: the bare package name
/// for the entry, or `package::seg::seg` for a submodule, mirroring how a
/// consumer imports it (so the compiled module takes the same name).
fn package_import_path(
    package_name: &str,
    source_root: &Path,
    entry: &Path,
    file: &Path,
) -> String {
    let entry_canon = entry.canonicalize().unwrap_or_else(|_| entry.to_path_buf());
    let file_canon = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
    if file_canon == entry_canon {
        return package_name.to_owned();
    }

    let relative = file.strip_prefix(source_root).unwrap_or(file);
    let mut segments: Vec<String> = relative
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(piece) => Some(piece.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    if let Some(last) = segments.last_mut()
        && let Some(stripped) = last.strip_suffix(".peko")
    {
        *last = stripped.to_owned();
    }
    // A `foo/main.peko` submodule is imported as `package::foo`.
    if segments.last().map(String::as_str) == Some("main") {
        segments.pop();
    }

    if segments.is_empty() {
        package_name.to_owned()
    } else {
        format!("{package_name}::{}", segments.join("::"))
    }
}

/// Encode a canonical path as the incremental cache's object file id: native
/// separators become `----` and `:` becomes `__colon__`. Mirrors
/// `execution::incremental`'s private encoder so the harness's object files can
/// be located by source path.
fn pathbuf_to_fileid(canonical: &Path) -> String {
    let mut string = canonical.display().to_string();
    if let Some(stripped) = string.strip_prefix(r"\\?\") {
        string = stripped.to_owned();
    } else if let Some(stripped) = string.strip_prefix("//?/") {
        string = stripped.to_owned();
    }
    string
        .replace(std::path::MAIN_SEPARATOR, "----")
        .replace(':', "__colon__")
}

/// Recursively collect every `.peko` file under `dir`, skipping the `exclude`
/// subtree (the package's own `prebuilt/` output).
fn collect_peko_files(dir: &Path, exclude: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == exclude {
            continue;
        }
        if path.is_dir() {
            collect_peko_files(&path, exclude, out);
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("peko") {
            out.push(path);
        }
    }
}

/// Recursively collect every FFI header (`*.peko.h`) under `dir`, excluding the
/// prebuilt output tree.
fn collect_ffi_headers(dir: &Path, exclude: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == exclude {
            continue;
        }
        if path.is_dir() {
            collect_ffi_headers(&path, exclude, out);
        } else if path.to_string_lossy().ends_with(".peko.h") {
            out.push(path);
        }
    }
}
