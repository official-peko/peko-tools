//! # Peko Core Packages
//!
//! Discovery of installed Pekoscript packages in the registry source cache.
//!
//! Unpacked package source lives at
//! `<peko_root>/registry/src/<name>/<name>-<version>/`. Each version directory
//! holds a `peko.toml`. This module scans those directories and exposes them as
//! [`ExternalModuleInfo`](crate::ExternalModuleInfo) values the simulator
//! resolves imports against. A package's submodules and entry resolve within
//! the source directory named by `[lib].root` in its manifest.

use indexmap::IndexMap;
use std::path::{Path, PathBuf};

use crate::config::{LockSource, Lockfile, MANIFEST_FILE, Manifest};
use crate::error::PekoResult;
use crate::{ExternalModuleInfo, ExternalModuleVersion};

/// The registry source-cache root under a Peko root.
///
/// Unpacked package source lives at `<peko_root>/registry/src`. The package
/// installer and discovery resolve this path the same way.
pub fn registry_source_root(peko_root: &Path) -> PathBuf {
    peko_root.join("registry").join("src")
}

/// The unpacked source directory of a package version.
///
/// A version is stored at `registry/src/<name>/<name>-<version>`, so versions
/// of one package coexist.
pub fn registry_source_dir(peko_root: &Path, name: &str, version: &str) -> PathBuf {
    registry_source_root(peko_root)
        .join(name)
        .join(format!("{name}-{version}"))
}

/// Aggregated view of installed packages discoverable for import resolution.
///
/// Constructed at startup by the compiler driver and the language server, then
/// queried by the simulator when resolving `import` statements.
#[derive(Clone, Debug, Default)]
pub struct PekoPackageIndex {
    modules: IndexMap<String, ExternalModuleInfo>,
}

impl PekoPackageIndex {
    /// Build an index from a global Peko root and an optional project-local
    /// one.
    ///
    /// Both roots are scanned under `registry/src`. A local install of a
    /// package shadows its global counterpart.
    ///
    /// # Errors
    ///
    /// Returns `Ok` even when a root has no source cache yet; only a future
    /// hard failure mode would surface an error.
    pub fn new(
        global_peko_root: impl AsRef<Path>,
        local_peko_root: Option<impl AsRef<Path>>,
    ) -> PekoResult<Self> {
        let mut modules = IndexMap::new();
        scan_source_root(
            &registry_source_root(global_peko_root.as_ref()),
            &mut modules,
        );
        if let Some(local) = local_peko_root {
            scan_source_root(&registry_source_root(local.as_ref()), &mut modules);
        }
        Ok(PekoPackageIndex { modules })
    }

    /// Build an index scoped to a project's lockfile.
    ///
    /// Each locked package contributes exactly its pinned version: registry
    /// packages from the source cache, path packages from their local
    /// directory. Imports then resolve against the locked set rather than
    /// whatever versions happen to be installed.
    pub fn from_lockfile(
        peko_root: &Path,
        project_root: &Path,
        lockfile: &Lockfile,
    ) -> PekoPackageIndex {
        let mut modules = IndexMap::new();
        for package in &lockfile.packages {
            let version_dir = match package.source {
                LockSource::Registry => {
                    registry_source_dir(peko_root, &package.name, &package.version)
                }
                LockSource::Path => match &package.path {
                    Some(path) => project_root.join(path),
                    None => continue,
                },
            };
            if let Some(info) = discover_single_version(&package.name, &version_dir) {
                modules.insert(package.name.clone(), info);
            }
        }
        PekoPackageIndex { modules }
    }

    /// A flat name to info map of every discoverable external module.
    pub fn get_external_modules(&self) -> IndexMap<String, ExternalModuleInfo> {
        self.modules.clone()
    }
}

/// Scan one `registry/src` root, inserting each discovered package.
///
/// Entries scanned from a later root overwrite earlier ones, so a local install
/// shadows a global one. Unreadable directories and packages that carry no
/// parseable version are skipped.
fn scan_source_root(src_root: &Path, modules: &mut IndexMap<String, ExternalModuleInfo>) {
    let Ok(entries) = std::fs::read_dir(src_root) else {
        return;
    };
    for entry in entries.flatten() {
        let package_dir = entry.path();
        if !package_dir.is_dir() {
            continue;
        }
        let Some(name) = package_dir
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned)
        else {
            continue;
        };
        if let Some(info) = discover_package(&name, &package_dir) {
            modules.insert(name, info);
        }
    }
}

/// Build an [`ExternalModuleInfo`] for one package by reading every installed
/// version's manifest.
fn discover_package(name: &str, package_dir: &Path) -> Option<ExternalModuleInfo> {
    let mut versions = Vec::new();
    let mut description = String::new();

    let entries = std::fs::read_dir(package_dir).ok()?;
    for entry in entries.flatten() {
        let version_dir = entry.path();
        if !version_dir.is_dir() {
            continue;
        }
        if let Some((found_description, version)) = read_version_dir(&version_dir) {
            if description.is_empty() {
                description = found_description;
            }
            versions.push(version);
        }
    }

    if versions.is_empty() {
        return None;
    }
    Some(ExternalModuleInfo::new(
        name.to_owned(),
        description,
        versions,
    ))
}

/// Build an [`ExternalModuleInfo`] for a single version directory.
fn discover_single_version(name: &str, version_dir: &Path) -> Option<ExternalModuleInfo> {
    let (description, version) = read_version_dir(version_dir)?;
    Some(ExternalModuleInfo::new(
        name.to_owned(),
        description,
        vec![version],
    ))
}

/// Read one version directory's manifest into its description and an
/// [`ExternalModuleVersion`].
fn read_version_dir(version_dir: &Path) -> Option<(String, ExternalModuleVersion)> {
    let loaded = Manifest::load(version_dir.join(MANIFEST_FILE)).ok()?;

    let entry_path = loaded.entry();
    let source_root = entry_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| version_dir.to_path_buf());
    let entry_file = entry_path
        .file_name()
        .and_then(|file| file.to_str())
        .unwrap_or("lib.peko")
        .to_owned();

    let description = loaded.manifest.description().to_owned();
    let version = ExternalModuleVersion::new(
        loaded.manifest.version().to_string(),
        source_root,
        entry_file,
    );
    Some((description, version))
}
