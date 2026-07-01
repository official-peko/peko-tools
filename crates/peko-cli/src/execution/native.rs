//! Native C source compilation for the build.
//!
//! A package declares its C build in the `[native]` table of its manifest:
//! the source files, include directories, per-OS compile flags, and per-OS
//! link arguments. The Peko codegen emits objects only; this module compiles
//! every reachable package's native sources to object files with clang so the
//! linker can resolve the C symbols those objects define (the GC runtime, the
//! value conversion helpers, and any package's own C interop).
//!
//! The set of reachable packages mirrors import resolution: the project
//! itself, the in-repo `std` package (the temporary development override), and
//! every locked dependency.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use peko_core::config::{LockSource, Lockfile, Manifest, Toolchain, resolve_flag};
use peko_core::packages::registry_source_dir;
use peko_core::target::PekoTarget;

/// The result of compiling the reachable packages' native C: the object and
/// static-library paths for the linker, plus the per-OS `[native.link]`
/// arguments those packages request at the final link.
pub(crate) struct NativeBuild {
    /// Compiled object files and prebuilt static libraries, in link order.
    pub objects: Vec<PathBuf>,
    /// Raw linker arguments gathered from every reachable package's
    /// `[native.link]` table for the target operating system.
    pub link_args: Vec<String>,
}

/// Compile every reachable package's `[native]` C sources to object files and
/// return their paths for the linker, along with the packages' `[native.link]`
/// arguments for the target.
///
/// Objects are written under `objects_directory/native`. A source is
/// recompiled only when its object is missing or older than the source, so
/// repeat builds skip unchanged C.
pub(crate) fn compile_native_sources(
    peko_root: &Path,
    project_root: &Path,
    target: PekoTarget,
    objects_directory: &Path,
    toolchain: &Toolchain,
    toolchain_dir: &Path,
) -> Result<NativeBuild, String> {
    let native_directory = objects_directory.join("native");
    std::fs::create_dir_all(&native_directory).ok();

    let mut objects = Vec::new();
    let mut link_args = Vec::new();

    for package_root in reachable_package_roots(peko_root, project_root) {
        let loaded = match Manifest::load(package_root.join("peko.toml")) {
            Ok(loaded) => loaded,
            Err(_) => continue,
        };
        let Some(native) = loaded.manifest.native() else {
            continue;
        };
        let package_name = loaded.manifest.name().to_owned();
        let package_root = loaded.root;

        // Include directories: the package's own (relative to its root) plus
        // the toolchain's (relative to the toolchain dir).
        let mut include_flags = Vec::new();
        for include in &native.include {
            include_flags.push(format!("-I{}", package_root.join(include).display()));
        }
        for include in &toolchain.build.include {
            include_flags.push(format!("-I{}", toolchain_dir.join(include).display()));
        }

        for source in &native.sources {
            let source_file = package_root.join(source);
            let object_file = native_directory.join(object_name(&package_name, source));

            if is_up_to_date(&object_file, &source_file) {
                objects.push(object_file);
                continue;
            }

            let mut command = Command::new("clang");
            command.arg("-c");
            command.arg("-target").arg(&toolchain.meta.triple);

            // Toolchain C flags, with any toolchain-relative paths resolved.
            for flag in &toolchain.build.c_flags {
                command.arg(resolve_flag(toolchain_dir, flag));
            }
            // Package compile flags: unconditional, then per-OS.
            for flag in &native.flags.all {
                command.arg(flag);
            }
            for flag in native.flags.for_os(target.operating_system) {
                command.arg(flag);
            }
            for include_flag in &include_flags {
                command.arg(include_flag);
            }
            command.arg("-o").arg(&object_file);
            command.arg(&source_file);

            let output = command.output().map_err(|error| {
                format!(
                    "could not run clang to compile {}: {error}",
                    source_file.display()
                )
            })?;

            if !output.status.success() {
                return Err(format!(
                    "clang failed to compile {}:\n{}",
                    source_file.display(),
                    String::from_utf8_lossy(&output.stderr)
                ));
            }

            objects.push(object_file);
        }

        // Prebuilt static libraries for this target. The archive is passed to
        // the linker after the package's own objects so its members resolve
        // the symbols those objects reference.
        for lib in native
            .libs
            .for_target(target.operating_system, target.architecture)
        {
            objects.push(package_root.join(lib));
        }

        // Final-link arguments this package requests: unconditional, then
        // per-OS. These reach the linker driver verbatim (for example
        // `-framework Cocoa` or `-lc++` for the desktop webview on macOS).
        for arg in &native.link.all {
            link_args.push(arg.clone());
        }
        for arg in native.link.for_os(target.operating_system) {
            link_args.push(arg.clone());
        }
    }

    Ok(NativeBuild { objects, link_args })
}

/// The prebuilt Android helper DEX files shipped by every reachable package.
///
/// A package that needs application Java on Android (std::webview, for example)
/// ships a precompiled `c/webview/android/classes.dex` the same way it ships
/// prebuilt static libraries, so no user Java build is needed. The bundler
/// merges these into the APK and the manifest is marked `hasCode`. Returned in
/// package order with duplicates removed.
pub(crate) fn collect_android_dex_files(peko_root: &Path, project_root: &Path) -> Vec<PathBuf> {
    let mut dex_files = Vec::new();
    for package_root in reachable_package_roots(peko_root, project_root) {
        let loaded = match Manifest::load(package_root.join("peko.toml")) {
            Ok(loaded) => loaded,
            Err(_) => continue,
        };
        if loaded.manifest.native().is_none() {
            continue;
        }
        let dex = loaded.root.join("c/webview/android/classes.dex");
        if dex.is_file() {
            dex_files.push(dex);
        }
    }
    dex_files
}

/// The roots of every package whose native sources the build links: the
/// project, the in-repo `std` override, and each locked dependency. Duplicate
/// roots are removed.
fn reachable_package_roots(peko_root: &Path, project_root: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut seen = HashSet::new();

    let add = |root: PathBuf, roots: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>| {
        let key = root.canonicalize().unwrap_or_else(|_| root.clone());
        if seen.insert(key) {
            roots.push(root);
        }
    };

    add(project_root.to_path_buf(), &mut roots, &mut seen);

    // The temporary development override: resolve `std` from the compiler
    // repo's own std/ directory rather than an installed package. Mirrors
    // `external_modules_for`. Remove once std is published and locked normally.
    if let Some(repo_std) = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(|root| root.join("std"))
    {
        add(repo_std, &mut roots, &mut seen);
    }

    if let Ok(Some(lockfile)) = Lockfile::load_from_root(project_root) {
        for package in &lockfile.packages {
            let root = match package.source {
                LockSource::Registry => {
                    registry_source_dir(peko_root, &package.name, &package.version)
                }
                LockSource::Path => match &package.path {
                    Some(path) => project_root.join(path),
                    None => continue,
                },
            };
            add(root, &mut roots, &mut seen);
        }
    }

    roots
}

/// A filesystem-safe, package-scoped object name for a native source. Two
/// packages may ship a `alloc.c`, so the package name leads and the source's
/// relative path is flattened into the stem.
fn object_name(package_name: &str, source: &Path) -> String {
    let flattened = source.to_string_lossy().replace(['/', '\\', '.', ' '], "_");
    format!("{package_name}__{flattened}.o")
}

/// Whether `object_file` exists and is at least as new as `source_file`.
fn is_up_to_date(object_file: &Path, source_file: &Path) -> bool {
    let object_modified = match std::fs::metadata(object_file).and_then(|meta| meta.modified()) {
        Ok(modified) => modified,
        Err(_) => return false,
    };
    let source_modified = match std::fs::metadata(source_file).and_then(|meta| meta.modified()) {
        Ok(modified) => modified,
        Err(_) => return false,
    };
    object_modified >= source_modified
}
