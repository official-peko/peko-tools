//! Dependency packaging for `peko deploy app`'s remote-build bundle.
//!
//! A remote Mac must build the app's Apple targets **hermetically** — with no
//! registry access, no entitlement to pull gated packages, and no reliance on
//! its own global package state (which may not match the developer's). Two
//! mechanisms achieve that:
//!
//! - [`mirror_dependency_cache`] copies every resolved registry/gated dependency
//!   (from both the project and global lockfiles) into `<staging>/pekoroot/`,
//!   mirroring the `~/.Peko` cache layout, plus the global lockfile/manifest. On
//!   the Mac the build worker points `PEKO_ROOT_PATH` at this mirror (with the
//!   Mac's real toolchains symlinked in), so those deps resolve from the bundle.
//! - [`vendor_path_deps`] copies the project's local `path = "…"` dependencies
//!   into `<source>/vendor/<name>/` and rewrites the packaged `peko.toml` and
//!   `peko.lock` to point there, since a path outside the project root would
//!   dangle once only `source/` is shipped.

use std::path::Path;

use peko_core::config::{
    DependencySpec, LOCKFILE_FILE, LockSource, Lockfile, MANIFEST_FILE, Manifest,
};
use peko_core::packages::registry_source_dir;

/// The bundle-relative directory that mirrors the parts of `~/.Peko` a remote
/// build needs (the resolved dependency cache and the global lockfile).
pub const PEKOROOT_DIR: &str = "pekoroot";

/// Mirror the resolved registry/gated dependency cache and the global lockfile
/// into `<staging>/pekoroot/`, so the remote build resolves every non-path
/// dependency from the bundle rather than the network or its own global state.
pub fn mirror_dependency_cache(
    peko_root: &Path,
    project_root: &Path,
    staging: &Path,
) -> Result<(), String> {
    let dst_root = staging.join(PEKOROOT_DIR);
    let dst_registry = dst_root.join("registry").join("src");

    // Every locked package from the project and global lockfiles. Registry and
    // gated packages live in the shared cache and are mirrored by name+version;
    // path packages are handled by `vendor_path_deps`.
    let mut seen: std::collections::HashSet<(String, String)> = Default::default();
    for lockfile_root in [project_root, &peko_root.join("global")] {
        let Ok(Some(lockfile)) = Lockfile::load_from_root(lockfile_root) else {
            continue;
        };
        for package in &lockfile.packages {
            if !matches!(package.source, LockSource::Registry | LockSource::Gated) {
                continue;
            }
            if !seen.insert((package.name.clone(), package.version.clone())) {
                continue;
            }
            let src = registry_source_dir(peko_root, &package.name, &package.version);
            if !src.is_dir() {
                return Err(format!(
                    "dependency {} {} is not in the local cache ({}); run a build first",
                    package.name,
                    package.version,
                    src.display()
                ));
            }
            let dst = dst_registry
                .join(&package.name)
                .join(format!("{}-{}", package.name, package.version));
            copy_dir_all(&src, &dst)
                .map_err(|e| format!("could not mirror {}: {e}", package.name))?;
        }
    }

    // The global lockfile + manifest, so the mirrored root resolves the app's
    // globally-installed dependencies (e.g. pekoui) project-independently.
    let global = peko_root.join("global");
    let global_lock = global.join(LOCKFILE_FILE);
    if global_lock.is_file() {
        let dst_global = dst_root.join("global");
        std::fs::create_dir_all(&dst_global)
            .map_err(|e| format!("could not create the mirrored global dir: {e}"))?;
        std::fs::copy(&global_lock, dst_global.join(LOCKFILE_FILE))
            .map_err(|e| format!("could not mirror the global lockfile: {e}"))?;
        let global_manifest = global.join(MANIFEST_FILE);
        if global_manifest.is_file() {
            std::fs::copy(&global_manifest, dst_global.join(MANIFEST_FILE))
                .map_err(|e| format!("could not mirror the global manifest: {e}"))?;
        }
    }

    Ok(())
}

/// Copy the project's local `path` dependencies into `<source>/vendor/<name>/`
/// and rewrite the packaged `peko.toml` + `peko.lock` to point at them. A no-op
/// when the project has no path dependencies.
///
/// `source` is the packaged source root (`<staging>/source`), which already
/// holds a copy of `peko.toml` and `peko.lock`.
pub fn vendor_path_deps(project_root: &Path, source: &Path) -> Result<(), String> {
    let Ok(Some(lockfile)) = Lockfile::load_from_root(project_root) else {
        return Ok(());
    };
    let path_packages: Vec<_> = lockfile
        .packages
        .iter()
        .filter(|package| package.source == LockSource::Path)
        .collect();
    if path_packages.is_empty() {
        return Ok(());
    }

    let manifest_path = source.join(MANIFEST_FILE);
    for package in &path_packages {
        let Some(rel) = &package.path else {
            return Err(format!(
                "path dependency {} has no recorded path in peko.lock",
                package.name
            ));
        };
        let src = project_root.join(rel);
        if !src.is_dir() {
            return Err(format!(
                "path dependency {} is missing at {}",
                package.name,
                src.display()
            ));
        }
        let vendored_rel = format!("vendor/{}", package.name);
        let dst = source.join(&vendored_rel);
        copy_source_excluding(&src, &dst)
            .map_err(|e| format!("could not vendor path dependency {}: {e}", package.name))?;
        // Point the packaged manifest at the vendored copy.
        Manifest::add_dependency(
            &manifest_path,
            &package.name,
            &DependencySpec::Path(vendored_rel),
        )
        .map_err(|e| format!("could not rewrite {} in peko.toml: {e}", package.name))?;
    }

    // Rewrite the packaged lockfile's path pointers to the vendored copies.
    if let Ok(Some(mut lockfile)) = Lockfile::load_from_root(source) {
        for package in &mut lockfile.packages {
            if package.source == LockSource::Path {
                package.path = Some(format!("vendor/{}", package.name).into());
            }
        }
        lockfile
            .save_to_root(source)
            .map_err(|e| format!("could not rewrite the packaged peko.lock: {e}"))?;
    }

    Ok(())
}

/// Recursively copy a directory tree, skipping `.DS_Store`. Used for the
/// dependency cache mirror, where the source is already a clean package tree.
pub fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == ".DS_Store" {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        if entry.file_type()?.is_dir() {
            copy_dir_all(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Build cache, output, installed deps, and VCS metadata excluded when vendoring
/// a path-dependency's source tree — mirrors `deploy_app::SOURCE_EXCLUDE_DIRS`.
const VENDOR_EXCLUDE_DIRS: &[&str] = &[
    ".peko",
    "build",
    "node_modules",
    "target",
    ".git",
    "dist",
    ".next",
];

/// Recursively copy a source tree, skipping build/cache/VCS directories and
/// `.DS_Store`. Used to vendor a path dependency's buildable source.
fn copy_source_excluding(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let from = entry.path();
        let to = dst.join(&name);
        if entry.file_type()?.is_dir() {
            if VENDOR_EXCLUDE_DIRS.contains(&name_str.as_ref()) {
                continue;
            }
            copy_source_excluding(&from, &to)?;
        } else if name_str != ".DS_Store" {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}
