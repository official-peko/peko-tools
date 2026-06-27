//! Toolchain detection, routing, and installation.
//!
//! The install manifest (`versions.json`) at the Peko root records which
//! toolchains are installed and where their Apple SDKs live. This module reads
//! it to route a build target to its toolchain directory and parsed
//! `toolchain.toml`, and updates it when a toolchain is installed.

pub mod resolve;
pub mod version;

use std::path::PathBuf;

use peko_core::config::ConfigError;
use peko_core::target::{Architecture, OperatingSystem};
use thiserror::Error;

pub use resolve::{ResolvedToolchain, resolve_for_target, resolve_toolchain, toolchains_root};
pub use version::InstallManifest;

/// The toolchain directory id, relative to `Compiler/toolchains`, for a target.
///
/// Returns `None` for a target with no toolchain (an unsupported os/arch
/// combination).
pub fn toolchain_dir_id(os: OperatingSystem, arch: Architecture) -> Option<&'static str> {
    use Architecture::{Arm, X86_64};
    use OperatingSystem::{Android, IOS, Linux, MacOS, Windows};
    Some(match (os, arch) {
        (MacOS, Arm) => "macos/arm64",
        (MacOS, X86_64) => "macos/x86_64",
        (IOS, Arm) => "ios/arm64",
        (IOS, X86_64) => "ios/x86_64",
        (Linux, Arm) => "linux/arm",
        (Linux, X86_64) => "linux/x86_64",
        (Android, _) => "android",
        (Windows, _) => "windows",
        _ => return None,
    })
}

/// One failure mode for a toolchain operation.
#[derive(Debug, Error)]
pub enum ToolchainError {
    /// An on-disk operation failed.
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// `versions.json` did not parse.
    #[error("couldn't parse versions.json at {path}: {source}")]
    VersionParse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    /// No toolchain exists for the requested target.
    #[error("no toolchain available for {target}")]
    Unsupported { target: String },

    /// The toolchain for the target is not recorded as installed.
    #[error("toolchain `{id}` is not installed")]
    NotInstalled { id: String },

    /// A `toolchain.toml` failed to load or parse.
    #[error("couldn't load toolchain at {path}: {source}")]
    Load {
        path: PathBuf,
        #[source]
        source: Box<ConfigError>,
    },
}
