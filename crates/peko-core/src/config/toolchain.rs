//! The `toolchain.toml` description: typed model, parsing, and loading.
//!
//! A toolchain describes one installed build target: its operating system,
//! architecture, and clang target triple, plus the flags and inputs used to
//! compile and link C, C++, and Objective-C against it. Paths are relative to
//! the toolchain's own directory unless they are absolute on-device runtime
//! paths.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::{ConfigError, architecture_from_str, operating_system_from_str};
use crate::target::{Architecture, OperatingSystem};

/// A parsed toolchain description.
#[derive(Debug, Clone)]
pub struct Toolchain {
    /// The `[toolchain]` table.
    pub meta: ToolchainMeta,
    /// The `[build]` table.
    pub build: ToolchainBuild,
    /// The `[link]` table.
    pub link: ToolchainLink,
}

/// The `[toolchain]` table identifying the target.
#[derive(Debug, Clone)]
pub struct ToolchainMeta {
    /// The toolchain identifier, for example `macos-arm64`.
    pub id: String,
    /// The operating system this toolchain targets.
    pub target: OperatingSystem,
    /// The architecture this toolchain targets.
    pub arch: Architecture,
    /// The clang target triple, for example `aarch64-apple-macosx`.
    pub triple: String,
}

/// The `[build]` table describing C, C++, and Objective-C compilation.
#[derive(Debug, Clone, Default)]
pub struct ToolchainBuild {
    /// The C++ standard, for example `c++17`.
    pub cxx_std: Option<String>,
    /// Flags passed on every C-family compile.
    pub c_flags: Vec<String>,
    /// Additional flags passed only when compiling Objective-C.
    pub objc_flags: Vec<String>,
    /// Include directories, relative to the toolchain directory.
    pub include: Vec<PathBuf>,
}

/// The `[link]` table describing the link step.
#[derive(Debug, Clone, Default)]
pub struct ToolchainLink {
    /// The lld driver: `ld.lld`, `ld64.lld`, or `lld-link`.
    pub driver: String,
    /// Library search paths, relative to the toolchain directory.
    pub lib_paths: Vec<PathBuf>,
    /// Libraries linked by name (the `-l` / `-defaultlib:` set).
    pub libs: Vec<String>,
    /// Apple frameworks linked with `-framework`.
    pub frameworks: Vec<String>,
    /// Extra objects and archives linked in order, relative to the toolchain
    /// directory.
    pub objects: Vec<PathBuf>,
    /// Raw driver arguments.
    pub flags: Vec<String>,
    /// Dynamic library sonames copied into the final app bundle, resolved
    /// against the toolchain's `lib` directory.
    pub bundle_dylibs: Vec<String>,
}

/// Resolve a flag token against a toolchain directory.
///
/// A token, or the value after `=`, that names a relative path existing under
/// `dir` is rewritten to that absolute path. Flags, absolute paths, and values
/// that do not exist as a path are returned unchanged. This lets a toolchain's
/// `flags` and `c_flags` mix literal flags with toolchain-relative paths.
pub fn resolve_flag(dir: &Path, token: &str) -> String {
    if let Some((prefix, value)) = token.split_once('=') {
        if let Some(resolved) = resolve_relative(dir, value) {
            return format!("{prefix}={resolved}");
        }
        return token.to_owned();
    }
    resolve_relative(dir, token).unwrap_or_else(|| token.to_owned())
}

/// Rewrite a relative path that exists under `dir` to its absolute form.
fn resolve_relative(dir: &Path, value: &str) -> Option<String> {
    if value.is_empty() || value.starts_with('-') {
        return None;
    }
    let path = Path::new(value);
    if !path.is_relative() {
        return None;
    }
    let candidate = dir.join(path);
    candidate
        .exists()
        .then(|| candidate.to_string_lossy().into_owned())
}

impl Toolchain {
    /// Parse a toolchain from TOML text without touching the file system.
    ///
    /// The `source` path is used only to label errors.
    pub fn parse(text: &str, source: &Path) -> Result<Toolchain, ConfigError> {
        let raw: RawToolchain = toml::from_str(text).map_err(|err| ConfigError::Parse {
            path: source.to_path_buf(),
            source: err,
        })?;
        raw.validate(source)
    }

    /// Read and parse the toolchain at the given path.
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Toolchain, ConfigError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Toolchain::parse(&text, path)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawToolchain {
    toolchain: RawToolchainMeta,
    #[serde(default)]
    build: RawBuild,
    #[serde(default)]
    link: RawLink,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawToolchainMeta {
    id: String,
    target: String,
    arch: String,
    triple: String,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawBuild {
    cxx_std: Option<String>,
    #[serde(default)]
    c_flags: Vec<String>,
    #[serde(default)]
    objc_flags: Vec<String>,
    #[serde(default)]
    include: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawLink {
    #[serde(default)]
    driver: String,
    #[serde(default)]
    lib_paths: Vec<String>,
    #[serde(default)]
    libs: Vec<String>,
    #[serde(default)]
    frameworks: Vec<String>,
    #[serde(default)]
    objects: Vec<String>,
    #[serde(default)]
    flags: Vec<String>,
    #[serde(default)]
    bundle_dylibs: Vec<String>,
}

impl RawToolchain {
    fn validate(self, source: &Path) -> Result<Toolchain, ConfigError> {
        let target = operating_system_from_str(&self.toolchain.target).ok_or_else(|| {
            ConfigError::invalid(
                source,
                format!("unknown toolchain.target '{}'", self.toolchain.target),
            )
        })?;
        let arch = architecture_from_str(&self.toolchain.arch).ok_or_else(|| {
            ConfigError::invalid(
                source,
                format!("unknown toolchain.arch '{}'", self.toolchain.arch),
            )
        })?;
        if self.link.driver.is_empty() {
            return Err(ConfigError::invalid(source, "link.driver is required"));
        }

        Ok(Toolchain {
            meta: ToolchainMeta {
                id: self.toolchain.id,
                target,
                arch,
                triple: self.toolchain.triple,
            },
            build: ToolchainBuild {
                cxx_std: self.build.cxx_std,
                c_flags: self.build.c_flags,
                objc_flags: self.build.objc_flags,
                include: self.build.include.into_iter().map(PathBuf::from).collect(),
            },
            link: ToolchainLink {
                driver: self.link.driver,
                lib_paths: self.link.lib_paths.into_iter().map(PathBuf::from).collect(),
                libs: self.link.libs,
                frameworks: self.link.frameworks,
                objects: self.link.objects.into_iter().map(PathBuf::from).collect(),
                flags: self.link.flags,
                bundle_dylibs: self.link.bundle_dylibs,
            },
        })
    }
}
