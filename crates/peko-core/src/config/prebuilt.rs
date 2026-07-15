//! The `prebuilt.toml` manifest of a prebuilt (source-hidden) package.
//!
//! A package published for proprietary distribution ships definition-only
//! `.peko` stubs plus prebuilt object files instead of its implementation
//! source. `peko package build` writes a `prebuilt/prebuilt.toml` next to the
//! generated `prebuilt/stubs/` tree and `prebuilt/objects/<triple>/` object
//! trees. A consumer that depends on such a package typechecks and codegens
//! against the stubs and links the prebuilt objects for its target triple,
//! never compiling the package's bodies.
//!
//! Layout under `<package>/prebuilt/`:
//!   - `prebuilt.toml`          — this manifest.
//!   - `stubs/<relpath>.peko`   — one definition-only stub per source file.
//!   - `objects/<triple>/*.o`   — the package's own compiled module + native
//!                                objects, one tree per target triple.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The directory name, under a package root, that holds all prebuilt output.
pub const PREBUILT_DIR: &str = "prebuilt";
/// The manifest file name, under `prebuilt/`.
pub const PREBUILT_MANIFEST_FILE: &str = "prebuilt.toml";
/// The stub tree directory name, under `prebuilt/`.
pub const PREBUILT_STUBS_DIR: &str = "stubs";
/// The object tree directory name, under `prebuilt/`.
pub const PREBUILT_OBJECTS_DIR: &str = "objects";

/// The parsed `prebuilt.toml`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct PrebuiltManifest {
    /// The single `[prebuilt]` table.
    pub prebuilt: PrebuiltSection,
}

/// The `[prebuilt]` table plus its nested `[prebuilt.objects]` map.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct PrebuiltSection {
    /// The package name (matches the package's `[package].name`).
    pub name: String,
    /// The package version.
    pub version: String,
    /// The compiler/toolchain version the objects were built with (ABI
    /// lockstep). Bundles are served per `(triple, toolchain)`, so the platform
    /// labels the upload with this and a consumer only links objects built by
    /// the same toolchain. Empty on manifests written before this field existed.
    #[serde(default)]
    pub toolchain: String,
    /// The entry stub file name (relative to `stubs/`), e.g. `lib.peko`.
    pub entry: String,
    /// Every generated stub, relative to `stubs/`.
    #[serde(default)]
    pub stubs: Vec<String>,
    /// Prebuilt objects keyed by target triple ([`PekoTarget::to_triple`]).
    /// Each path is relative to the `prebuilt/` directory.
    ///
    /// [`PekoTarget::to_triple`]: crate::target::PekoTarget::to_triple
    #[serde(default)]
    pub objects: BTreeMap<String, Vec<String>>,
}

impl PrebuiltManifest {
    /// The `prebuilt/` directory under a package root.
    pub fn dir(package_root: &Path) -> PathBuf {
        package_root.join(PREBUILT_DIR)
    }

    /// The `prebuilt/prebuilt.toml` path under a package root.
    pub fn manifest_path(package_root: &Path) -> PathBuf {
        Self::dir(package_root).join(PREBUILT_MANIFEST_FILE)
    }

    /// The `prebuilt/stubs/` directory under a package root.
    pub fn stubs_dir(package_root: &Path) -> PathBuf {
        Self::dir(package_root).join(PREBUILT_STUBS_DIR)
    }

    /// The `prebuilt/objects/` directory under a package root.
    pub fn objects_dir(package_root: &Path) -> PathBuf {
        Self::dir(package_root).join(PREBUILT_OBJECTS_DIR)
    }

    /// `true` if `package_root` holds a prebuilt manifest, i.e. the package is
    /// distributed prebuilt (source-hidden) rather than from source.
    pub fn is_prebuilt(package_root: &Path) -> bool {
        Self::manifest_path(package_root).is_file()
    }

    /// Load the prebuilt manifest under `package_root`, or `None` when the
    /// package is not prebuilt or the manifest cannot be read/parsed.
    pub fn load(package_root: &Path) -> Option<PrebuiltManifest> {
        let text = std::fs::read_to_string(Self::manifest_path(package_root)).ok()?;
        toml::from_str(&text).ok()
    }

    /// The absolute paths of the prebuilt objects for `triple`, resolved
    /// against the `prebuilt/` directory. Empty when the triple has no
    /// prebuilt objects recorded.
    pub fn objects_for(&self, package_root: &Path, triple: &str) -> Vec<PathBuf> {
        let dir = Self::dir(package_root);
        self.prebuilt
            .objects
            .get(triple)
            .map(|paths| paths.iter().map(|path| dir.join(path)).collect())
            .unwrap_or_default()
    }
}
