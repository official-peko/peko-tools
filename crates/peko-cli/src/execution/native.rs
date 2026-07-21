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

use peko_core::config::{
    LockSource, Lockfile, Manifest, PrebuiltManifest, Toolchain, resolve_flag,
};
use peko_core::packages::registry_source_dir;
use peko_core::target::{OperatingSystem, PekoTarget};

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
    demo: bool,
) -> Result<NativeBuild, String> {
    let native_directory = objects_directory.join("native");
    std::fs::create_dir_all(&native_directory).ok();

    let mut objects = Vec::new();
    let mut link_args = Vec::new();

    for package_root in reachable_package_roots(peko_root, project_root, demo) {
        let loaded = match Manifest::load(package_root.join("peko.toml")) {
            Ok(loaded) => loaded,
            Err(_) => continue,
        };
        let Some(native) = loaded.manifest.native() else {
            continue;
        };
        let package_name = loaded.manifest.name().to_owned();
        let package_version = loaded.manifest.version().to_string();
        let package_root = loaded.root;

        // A prebuilt (source-hidden) package ships its native objects in the
        // prebuilt object table (added to the link separately), and its C
        // sources are absent, so there is nothing to compile here. Its final
        // link arguments (frameworks, `-l` flags) still apply, so gather those.
        if PrebuiltManifest::is_prebuilt(&package_root) {
            for arg in &native.link.all {
                link_args.push(arg.clone());
            }
            for arg in native.link.for_os(target.operating_system) {
                link_args.push(arg.clone());
            }
            continue;
        }

        // Include directories: the global FFI header directory
        // (Compiler/include, which holds peko.h), the package's own (relative
        // to its root), and the toolchain's (relative to the toolchain dir).
        let mut include_flags = vec![format!(
            "-I{}",
            peko_root.join("Compiler").join("include").display()
        )];
        for include in &native.include {
            include_flags.push(format!("-I{}", package_root.join(include).display()));
        }
        for include in &toolchain.build.include {
            include_flags.push(format!("-I{}", toolchain_dir.join(include).display()));
        }

        for source in &native.sources {
            let source_file = package_root.join(source);
            let object_file =
                native_directory.join(object_name(&package_name, &package_version, source));

            if is_up_to_date(&object_file, &source_file) {
                objects.push(object_file);
                continue;
            }

            let mut command = Command::new(host_clang(peko_root, target.operating_system));
            command.arg("-c");
            command.arg("-target").arg(&toolchain.meta.triple);

            // C++ sources get the toolchain C++ standard. C and Objective-C
            // sources do not: -std=c++NN is rejected for those languages.
            let is_cpp = matches!(
                source_file.extension().and_then(|e| e.to_str()),
                Some("cpp" | "cc" | "cxx" | "c++")
            );
            if is_cpp && let Some(cxx_std) = &toolchain.build.cxx_std {
                command.arg(format!("-std={cxx_std}"));
            }

            // Toolchain C flags, with any toolchain-relative paths resolved.
            for flag in &toolchain.build.c_flags {
                command.arg(resolve_flag(toolchain_dir, flag));
            }
            // Objective-C sources also get the toolchain's Objective-C flags,
            // such as module support for SDKs whose frameworks are module-only.
            let is_objc = matches!(
                source_file.extension().and_then(|e| e.to_str()),
                Some("m" | "mm")
            );
            if is_objc {
                for flag in &toolchain.build.objc_flags {
                    command.arg(resolve_flag(toolchain_dir, flag));
                }
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
pub(crate) fn collect_android_dex_files(
    peko_root: &Path,
    project_root: &Path,
    demo: bool,
) -> Vec<PathBuf> {
    let mut dex_files = Vec::new();
    for package_root in reachable_package_roots(peko_root, project_root, demo) {
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

/// A reachable dependency shipped **prebuilt** (source-hidden): its generated
/// definition-only stub tree and the prebuilt object files for one target.
pub(crate) struct PrebuiltDependency {
    /// The canonical `prebuilt/stubs` directory. A module whose source file
    /// resolves under this root is a prebuilt module: the consumer typechecks
    /// and codegens its declarations but does not emit or link its object,
    /// linking the shipped object below instead.
    pub stub_root: PathBuf,
    /// The prebuilt objects (module `.o` plus native `.o`/`.a`) for the target
    /// triple, in link order.
    pub objects: Vec<PathBuf>,
}

/// Every reachable dependency that is distributed prebuilt, with its stub root
/// and the prebuilt objects for `target`. Mirrors [`reachable_package_roots`]
/// so a prebuilt dependency is recognized wherever it is reached (global
/// packages and project lockfile alike).
pub(crate) fn prebuilt_dependencies(
    peko_root: &Path,
    project_root: &Path,
    target: PekoTarget,
    demo: bool,
) -> Vec<PrebuiltDependency> {
    let triple = target.to_triple();
    let mut deps = Vec::new();
    for package_root in reachable_package_roots(peko_root, project_root, demo) {
        let Some(prebuilt) = PrebuiltManifest::load(&package_root) else {
            continue;
        };
        let stub_root = PrebuiltManifest::stubs_dir(&package_root);
        let stub_root = stub_root.canonicalize().unwrap_or(stub_root);
        deps.push(PrebuiltDependency {
            stub_root,
            objects: prebuilt.objects_for(&package_root, &triple),
        });
    }
    deps
}

/// The roots of every package whose native sources the build links: the
/// project, the in-repo `std` override, and each locked dependency. Duplicate
/// roots are removed.
pub(crate) fn reachable_package_roots(
    peko_root: &Path,
    project_root: &Path,
    demo: bool,
) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut seen = HashSet::new();
    let mut std_resolved = false;

    // Demo-scoped project dependencies (`demo = true`) are linked only in demo
    // builds. In a normal/release build their packages are dropped here, so
    // their native sources and prebuilt objects never reach the link.
    let demo_scoped: HashSet<String> = if demo {
        HashSet::new()
    } else {
        Manifest::discover(project_root)
            .map(|loaded| {
                loaded
                    .manifest
                    .dependencies()
                    .iter()
                    .filter(|(_, dep)| dep.is_demo())
                    .map(|(name, _)| name.clone())
                    .collect()
            })
            .unwrap_or_default()
    };

    let add = |root: PathBuf, roots: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>| {
        let key = root.canonicalize().unwrap_or_else(|_| root.clone());
        if seen.insert(key) {
            roots.push(root);
        }
    };

    add(project_root.to_path_buf(), &mut roots, &mut seen);

    // Packages installed with `peko add --global` (std and pekoui, for example)
    // are importable from every project. Resolve them from the shared global
    // lockfile so their native C compiles from the locked version. Mirrors
    // `external_modules_for`.
    let global_root = peko_root.join("global");
    if let Ok(Some(lockfile)) = Lockfile::load_from_root(&global_root) {
        for package in &lockfile.packages {
            let root = match package.source {
                LockSource::Registry | LockSource::Gated => {
                    registry_source_dir(peko_root, &package.name, &package.version)
                }
                LockSource::Path => match &package.path {
                    Some(path) => global_root.join(path),
                    None => continue,
                },
            };
            if package.name == "std" {
                std_resolved = true;
            }
            add(root, &mut roots, &mut seen);
        }
    }

    // std is auto-imported and its runtime and GC C must always compile, even
    // when no global lockfile pins it. Fall back to the installed 0.1.0 source.
    if !std_resolved {
        let installed_std = registry_source_dir(peko_root, "std", "0.1.0");
        if installed_std.join("peko.toml").is_file() {
            add(installed_std, &mut roots, &mut seen);
        }
    }

    if let Ok(Some(lockfile)) = Lockfile::load_from_root(project_root) {
        for package in &lockfile.packages {
            // A demo-scoped dependency is excluded from a non-demo build.
            if demo_scoped.contains(&package.name) {
                continue;
            }
            let root = match package.source {
                LockSource::Registry | LockSource::Gated => {
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

/// The os-arch key for the current host under `Compiler/llvm18`. The bundle
/// directories are named for the running host: macos-arm64, macos-x86_64,
/// linux-arm64, linux-x86_64, windows-x86_64.
pub(crate) fn llvm18_host_key() -> String {
    let arch = match std::env::consts::ARCH {
        "aarch64" | "arm" => "arm64",
        other => other,
    };
    format!("{}-{}", std::env::consts::OS, arch)
}

/// The directory of the bundled LLVM 18 tools for the current host, inside the
/// SDK at `Compiler/llvm18/<host-key>/bin`. The clang there resolves its builtin
/// headers from the sibling `../lib/clang`, and llvm-rc finds clang beside it.
pub(crate) fn llvm18_bin(peko_root: &Path) -> PathBuf {
    peko_root
        .join("Compiler")
        .join("llvm18")
        .join(llvm18_host_key())
        .join("bin")
}

/// A bundled LLVM tool for the current host, with the `.exe` suffix added when
/// Peko itself runs on Windows. The SDK's shipped tool is always used, never a
/// tool from PATH, so a build resolves to the same compiler and linker
/// regardless of what is installed on the machine.
pub(crate) fn llvm18_tool(peko_root: &Path, tool: &str) -> PathBuf {
    let name = if cfg!(windows) {
        format!("{tool}.exe")
    } else {
        tool.to_owned()
    };
    llvm18_bin(peko_root).join(name)
}

/// The clang binary used to compile native C sources.
///
/// The bundled LLVM 18 clang for the host compiles for every non-Apple target
/// through `-target`, shipping its resource directory (the builtin headers such
/// as stddef.h) at the sibling `../lib/clang`.
///
/// Apple targets are the exception. Recent iOS SDKs ship module maps that
/// `requires` Apple clang features (for example
/// `found_incompatible_headers__check_search_paths`) which upstream LLVM lacks,
/// and their frameworks are module-only, so the bundled clang cannot build
/// them. The host's Apple clang, matched to the installed SDK, is used for iOS.
/// The host is always macOS when an Apple target is built.
fn host_clang(peko_root: &Path, operating_system: OperatingSystem) -> PathBuf {
    if matches!(operating_system, OperatingSystem::IOS) {
        return PathBuf::from("clang");
    }
    llvm18_tool(peko_root, "clang")
}

/// A filesystem-safe, package-scoped object name for a native source. Two
/// packages may ship a `alloc.c`, so the package name and version lead and the
/// source's relative path is flattened into the stem. The version keeps two
/// installed versions of one package from aliasing to the same object, so a
/// version switch recompiles instead of reusing the prior version's object.
fn object_name(package_name: &str, package_version: &str, source: &Path) -> String {
    let version = package_version.replace(['/', '\\', '.', ' '], "_");
    let flattened = source.to_string_lossy().replace(['/', '\\', '.', ' '], "_");
    format!("{package_name}-{version}__{flattened}.o")
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
