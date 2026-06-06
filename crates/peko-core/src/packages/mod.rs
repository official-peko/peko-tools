//! # Peko Core Packages
//!
//! Discovery and tracking of Pekoscript packages installed on the host.
//!
//! Packages live in a `Packages/` directory inside a Peko root (either the
//! global install location or a project-local one). Each package directory
//! contains a `Package.json` manifest, optional `README.md` and `LICENSE`
//! files, and one subdirectory per published version. Each version
//! subdirectory must contain `main.peko` (entry point) and `deps.json`
//! (dependency manifest).
//!
//! This module discovers those packages on disk and exposes them as
//! [`ExternalModuleInfo`](crate::ExternalModuleInfo) values that the
//! simulator can resolve imports against.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{read_to_string, PekoError, PekoResult};
use crate::ExternalModuleInfo;

/// Entry-point file name expected inside every package version directory.
const ENTRY_FILE_NAME: &str = "main.peko";

/// Contents of a package's top-level `Package.json` manifest.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PackageJSON {
    /// Module identifier used in `import` statements.
    pub name: String,

    /// Human-readable display name.
    pub label: String,

    /// Version string identifying the most recently published release. Should
    /// also appear in [`versions`](Self::versions).
    pub latest: String,

    /// Free-form description shown in package listings.
    pub description: String,

    /// All published versions. Each must have a matching subdirectory under
    /// the package's folder.
    pub versions: Vec<String>,
}

/// Metadata for a single version of a host-installed package.
#[derive(Clone, Debug)]
pub struct HostPackageVersion {
    /// Absolute path to this version's directory.
    pub path: PathBuf,

    /// Map from dependency name to required version string, sourced from
    /// `deps.json`.
    pub dependencies: HashMap<String, String>,

    /// Per-version README contents, if a `README.md` exists in this version's
    /// directory.
    pub readme: Option<String>,
}

/// A discovered package on the host: its manifest, all its installed
/// versions, and any top-level documentation.
#[derive(Clone, Debug)]
pub struct HostPackage {
    /// Parsed `Package.json` manifest.
    pub info: PackageJSON,

    /// Absolute path to the package's root directory (the parent of each
    /// version directory).
    pub folder: PathBuf,

    /// Per-version metadata keyed by version string. Each entry corresponds
    /// to a version listed in [`info.versions`](PackageJSON::versions).
    pub version_folders: HashMap<String, HostPackageVersion>,

    /// Top-level `README.md` for the package, if present.
    pub readme: Option<String>,

    /// `LICENSE` file contents, if present.
    pub license: Option<String>,
}

impl HostPackage {
    /// Converts this package into an [`ExternalModuleInfo`] view suitable
    /// for module-resolution at the simulator level.
    #[must_use]
    pub fn to_external_module(&self) -> ExternalModuleInfo {
        ExternalModuleInfo {
            module_name: self.info.name.clone(),
            versions: self.info.versions.clone(),
            description: self.info.description.clone(),
            directory: self.folder.clone(),
            entry_file_name: ENTRY_FILE_NAME.to_owned(),
        }
    }

    /// Loads a [`HostPackage`] from a directory on disk.
    ///
    /// Distinguishes three outcomes:
    ///
    /// * `Ok(Some(pkg))`: `Package.json` was present, parsed, and every
    ///   version directory it referenced existed with the expected
    ///   `main.peko` and `deps.json` files.
    /// * `Ok(None)`: no `Package.json` at the path. The directory is
    ///   simply not a Peko package; callers iterating a packages folder
    ///   can treat this as "skip silently."
    /// * `Err(_)`: `Package.json` was present but unreadable or malformed,
    ///   or a referenced version directory was incomplete. Returned as
    ///   either [`PekoError::Io`] for filesystem failures or
    ///   [`PekoError::PackageParse`] for JSON parse failures.
    ///
    /// # Errors
    ///
    /// Returns an error when the package manifest or any associated
    /// metadata file fails to read or parse.
    pub fn from_package_directory(path: impl AsRef<Path>) -> PekoResult<Option<Self>> {
        let path = path.as_ref();
        let package_json_path = path.join("Package.json");

        // No manifest -> not a package. Distinct from manifest-parse errors,
        // which propagate as Err.
        if !package_json_path.exists() {
            return Ok(None);
        }

        let manifest_text = read_to_string(&package_json_path)?;
        let package_json: PackageJSON =
            serde_json::from_str(&manifest_text).map_err(|source| PekoError::PackageParse {
                path: package_json_path.clone(),
                source,
            })?;

        // Top-level README / LICENSE are optional (load if present).
        let readme = if path.join("README.md").exists() {
            Some(read_to_string(path.join("README.md"))?)
        } else {
            None
        };
        let license = if path.join("LICENSE").exists() {
            Some(read_to_string(path.join("LICENSE"))?)
        } else {
            None
        };

        let mut local_package = HostPackage {
            info: package_json.clone(),
            folder: path.to_path_buf(),
            version_folders: HashMap::new(),
            readme,
            license,
        };

        for version in &package_json.versions {
            let version_folder = path.join(version);

            // An incomplete version directory invalidates the whole
            // package, bail with Ok(None) so it's not treated as a real
            // error, just an unusable package.
            if !version_folder.exists()
                || !version_folder.join("main.peko").exists()
                || !version_folder.join("deps.json").exists()
            {
                return Ok(None);
            }

            let deps_path = version_folder.join("deps.json");
            let deps_text = read_to_string(&deps_path)?;
            let dependencies: HashMap<String, String> =
                serde_json::from_str(&deps_text).map_err(|source| PekoError::PackageParse {
                    path: deps_path,
                    source,
                })?;

            let version_readme_path = version_folder.join("README.md");
            let readme = if version_readme_path.exists() {
                Some(read_to_string(&version_readme_path)?)
            } else {
                None
            };

            local_package.version_folders.insert(
                version.clone(),
                HostPackageVersion {
                    path: version_folder,
                    dependencies,
                    readme,
                },
            );
        }

        Ok(Some(local_package))
    }
}

/// A directory of installed packages (either the global one or a
/// project-local one).
#[derive(Clone, Debug)]
pub struct PackageRoot {
    /// Path to the `Packages/` directory.
    pub path: PathBuf,

    /// All successfully discovered packages, keyed by their directory name.
    pub installed_packages: HashMap<String, HostPackage>,
}

impl PackageRoot {
    /// Scans `root_directory` for installed packages and returns the result.
    ///
    /// Each immediate subdirectory is treated as a candidate package and
    /// loaded via [`HostPackage::from_package_directory`]. Subdirectories
    /// without a `Package.json` are silently ignored. So are:
    ///
    /// * Per-entry I/O errors (a single broken `DirEntry` won't tank the
    ///   whole scan).
    /// * Non-UTF-8 directory names.
    /// * Packages with malformed manifests or incomplete version
    ///   directories (these come back as `Err` from
    ///   `from_package_directory` and are dropped).
    ///
    /// Only a failure to open `root_directory` itself is propagated.
    ///
    /// # Errors
    ///
    /// Returns [`PekoError::Io`] when `root_directory` cannot be read.
    pub fn from_directory(root_directory: impl AsRef<Path>) -> PekoResult<Self> {
        let path = root_directory.as_ref().to_path_buf();

        let mut root = PackageRoot {
            path: path.clone(),
            installed_packages: HashMap::new(),
        };

        let entries = std::fs::read_dir(&path).map_err(|source| PekoError::Io {
            path: path.clone(),
            source,
        })?;

        for entry in entries {
            // Per-entry I/O errors and non-UTF-8 names are silently skipped
            // so a single broken entry doesn't tank discovery.
            let Ok(entry) = entry else { continue };
            let package_dir = entry.path();

            let Some(file_name) = package_dir.file_name() else {
                continue;
            };
            let Some(package_name) = file_name.to_str() else {
                continue;
            };
            let package_name = package_name.to_owned();

            // A malformed individual package is dropped from this scan.
            // The user can re-run with appropriate logging to surface it.
            if let Ok(Some(host_package)) = HostPackage::from_package_directory(&package_dir) {
                root.installed_packages.insert(package_name, host_package);
            }
        }

        Ok(root)
    }
}

/// Aggregated view of the global and project-local package directories.
///
/// Constructed at startup by the compiler driver and queried by the
/// simulator when resolving `import` statements.
#[derive(Clone, Debug)]
pub struct PekoPackageIndex {
    global_packages: PackageRoot,
    local_packages: Option<PackageRoot>,
}

impl PekoPackageIndex {
    /// Builds a fresh package index from a global Peko root and an optional
    /// project-local one.
    ///
    /// `global_peko_root.join("Packages")` is scanned for globally
    /// installed packages; if `local_peko_root` is provided,
    /// `local_peko_root.join("Packages")` is also scanned.
    ///
    /// Local packages override global ones with the same name in
    /// [`Self::get_external_modules`].
    ///
    /// # Errors
    ///
    /// Returns [`PekoError::Io`] when either of the resolved `Packages/`
    /// directories cannot be read.
    pub fn new(
        global_peko_root: impl AsRef<Path>,
        local_peko_root: Option<impl AsRef<Path>>,
    ) -> PekoResult<Self> {
        let global_packages =
            PackageRoot::from_directory(global_peko_root.as_ref().join("Packages"))?;

        let local_packages = match local_peko_root {
            Some(local_root) => Some(PackageRoot::from_directory(
                local_root.as_ref().join("Packages"),
            )?),
            None => None,
        };

        Ok(Self {
            global_packages,
            local_packages,
        })
    }

    /// Returns a flat name→info map of every discoverable external module.
    ///
    /// Global packages are added first; local packages then overwrite any
    /// matching entries, so a project-local install of a package shadows
    /// its global counterpart.
    #[must_use]
    pub fn get_external_modules(&self) -> HashMap<String, ExternalModuleInfo> {
        let mut externals = HashMap::new();

        for (package_name, package) in &self.global_packages.installed_packages {
            externals.insert(package_name.clone(), package.to_external_module());
        }

        if let Some(local_packages) = &self.local_packages {
            for (package_name, package) in &local_packages.installed_packages {
                externals.insert(package_name.clone(), package.to_external_module());
            }
        }

        externals
    }
}
