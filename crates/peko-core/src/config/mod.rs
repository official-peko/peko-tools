//! Peko configuration files.
//!
//! Two on-disk files are described here. `peko.toml` is the project manifest:
//! it identifies a project as an application or a package and carries its
//! dependencies, platform targets, and native build description. `toolchain.toml`
//! describes one installed toolchain: its target, architecture, and the flags
//! used to compile and link C, C++, and Objective-C against it.
//!
//! This module owns parsing, format-preserving edits, discovery, and the typed
//! data model. Asset discovery, incremental build state, and icon rendering live
//! in higher layers.
//!
//! Identity of a manifest comes from which tables are present:
//!
//! - `[package]` and `[lib]` mark a publishable package.
//! - `[project]` with `[ui]` marks a UI application.
//! - `[project]` without `[ui]` marks a CLI application.
//!
//! The application and package forms are mutually exclusive.

mod container;
mod lock;
mod manifest;
mod prebuilt;
mod toolchain;

pub use container::{
    CONTAINER_HEADER_LEN, CONTAINER_MAGIC, CONTAINER_VERSION, Compression, Container,
    ContainerError, ContainerHeader, decode_container, encode_container,
};
pub use lock::{LOCKFILE_FILE, LOCKFILE_VERSION, LockSource, LockedPackage, Lockfile};
pub use manifest::{
    ApplicationManifest, Demo, Dependency, DependencySpec, Framework, Icon, Lib, LoadedManifest,
    Manifest, ManifestKind, Native, NativeFlags, NativeLink, PackageManifest, PackageMeta,
    Platforms, Project, ServerFramework, Ui, Vendor,
};
pub use prebuilt::{
    PREBUILT_DIR, PREBUILT_MANIFEST_FILE, PREBUILT_OBJECTS_DIR, PREBUILT_STUBS_DIR,
    PrebuiltManifest, PrebuiltSection,
};
pub use toolchain::{Toolchain, ToolchainBuild, ToolchainLink, ToolchainMeta, resolve_flag};

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::target::{Architecture, OperatingSystem};

/// The file name of a project manifest.
pub const MANIFEST_FILE: &str = "peko.toml";

/// The file name of a toolchain description.
pub const TOOLCHAIN_FILE: &str = "toolchain.toml";

/// The source directory that holds a project's Peko source files.
pub const SOURCE_DIR: &str = "source";

/// A parsed Peko configuration file.
#[derive(Debug, Clone)]
pub enum PekoConfig {
    /// A `peko.toml` project manifest.
    Manifest(Box<Manifest>),
    /// A `toolchain.toml` toolchain description.
    Toolchain(Box<Toolchain>),
}

impl PekoConfig {
    /// Parse a configuration file, choosing the format from the file name.
    ///
    /// A path ending in `peko.toml` parses as a manifest. A path ending in
    /// `toolchain.toml` parses as a toolchain. Any other name is an
    /// [`ConfigError::UnknownFile`].
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<PekoConfig, ConfigError> {
        let path = path.as_ref();
        match path.file_name().and_then(|name| name.to_str()) {
            Some(MANIFEST_FILE) => {
                let loaded = Manifest::load(path)?;
                Ok(PekoConfig::Manifest(Box::new(loaded.manifest)))
            }
            Some(TOOLCHAIN_FILE) => Ok(PekoConfig::Toolchain(Box::new(Toolchain::load(path)?))),
            _ => Err(ConfigError::UnknownFile {
                path: path.to_path_buf(),
            }),
        }
    }
}

/// One failure mode for loading or parsing a Peko configuration file.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// No manifest was found in the searched directory or its parents within
    /// the search-depth limit.
    #[error("couldn't find {MANIFEST_FILE} in the current directory or its parents")]
    NotFound,

    /// The file exists but couldn't be read or written.
    #[error("couldn't access config file at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The file name is neither `peko.toml` nor `toolchain.toml`.
    #[error("{path} is not a recognized Peko config file name")]
    UnknownFile { path: PathBuf },

    /// The TOML text did not parse.
    #[error("couldn't parse {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    /// The TOML text did not parse during a format-preserving edit.
    #[error("couldn't parse {path} for editing: {source}")]
    Edit {
        path: PathBuf,
        #[source]
        source: toml_edit::TomlError,
    },

    /// The file parsed as TOML but is not a valid configuration.
    #[error("invalid config at {path}: {detail}")]
    Invalid { path: PathBuf, detail: String },
}

impl ConfigError {
    /// Build an [`ConfigError::Invalid`] for the given path and detail.
    pub(crate) fn invalid(path: &Path, detail: impl Into<String>) -> ConfigError {
        ConfigError::Invalid {
            path: path.to_path_buf(),
            detail: detail.into(),
        }
    }
}

/// Map a manifest platform identifier to a concrete operating system.
///
/// The mapping defers to [`OperatingSystem::from_name`]. An identifier that
/// does not name a concrete operating system returns `None`, so a manifest
/// listing an unrecognized platform is rejected during validation rather than
/// silently treated as unknown.
pub(crate) fn operating_system_from_str(identifier: &str) -> Option<OperatingSystem> {
    match OperatingSystem::from_name(identifier) {
        OperatingSystem::Unknown => None,
        os => Some(os),
    }
}

/// Map a toolchain architecture identifier to a concrete architecture.
///
/// The mapping defers to [`Architecture::from_name`]. An identifier that does
/// not name a concrete architecture returns `None`.
pub(crate) fn architecture_from_str(identifier: &str) -> Option<Architecture> {
    match Architecture::from_name(identifier) {
        Architecture::Unknown => None,
        arch => Some(arch),
    }
}
