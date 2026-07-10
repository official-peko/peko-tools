//! The `versions.json` install manifest written by Peko setup.
//!
//! It records the install root, host, component versions, the installed
//! toolchains, and the Apple SDK locations. Routing reads it to learn which
//! toolchains are available; installation updates the installed list.

use std::path::{Path, PathBuf};

use peko_core::target::{Architecture, OperatingSystem};
use serde::{Deserialize, Serialize};

use super::ToolchainError;

/// The install manifest file name, found at the Peko root.
pub const VERSION_FILE: &str = "versions.json";

/// A parsed `versions.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallManifest {
    /// The manifest format version.
    pub schema: u32,
    /// The Peko install root.
    pub install_root: PathBuf,
    /// The host the toolchain runs on.
    pub host: HostInfo,
    /// The installed `peko-tools` release.
    pub peko_tools: PekoToolsInfo,
    /// The installed standard library release.
    pub sdk: ComponentInfo,
    /// The installed toolchains.
    pub toolchains: ToolchainsInfo,
    /// The Apple SDK locations, present on a macOS host.
    #[serde(default)]
    pub apple_sdks: AppleSdks,
    /// Whether the installer configured the shell PATH.
    #[serde(default)]
    pub path_configured: bool,
    /// The RFC 3339 timestamp of the last update.
    #[serde(default)]
    pub updated_at: String,
}

/// The host description.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostInfo {
    pub os: String,
    pub arch: String,
    pub triple: String,
}

/// The installed `peko-tools` release.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PekoToolsInfo {
    pub channel: String,
    pub tag: String,
    pub version: String,
}

/// A tagged, versioned component.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentInfo {
    pub tag: String,
    pub version: String,
}

/// The installed toolchains.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolchainsInfo {
    pub tag: String,
    pub version: String,
    /// The installed toolchain ids, for example `macos/arm64` or `ios`.
    #[serde(default)]
    pub installed: Vec<String>,
}

/// The Apple SDK locations, symlinked into the toolchains on a macOS host.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppleSdks {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub macos: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ios_device: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ios_sim: Option<PathBuf>,
}

impl InstallManifest {
    /// The manifest path under a Peko root.
    pub fn path_in(peko_root: &Path) -> PathBuf {
        peko_root.join(VERSION_FILE)
    }

    /// Read and parse the manifest under a Peko root.
    pub fn load(peko_root: &Path) -> Result<InstallManifest, ToolchainError> {
        let path = InstallManifest::path_in(peko_root);
        let text = std::fs::read_to_string(&path).map_err(|source| ToolchainError::Io {
            path: path.clone(),
            source,
        })?;
        serde_json::from_str(&text).map_err(|source| ToolchainError::VersionParse { path, source })
    }

    /// Serialize the manifest under a Peko root.
    pub fn save(&self, peko_root: &Path) -> Result<(), ToolchainError> {
        let path = InstallManifest::path_in(peko_root);
        let text =
            serde_json::to_string_pretty(self).map_err(|source| ToolchainError::VersionParse {
                path: path.clone(),
                source,
            })?;
        std::fs::write(&path, text).map_err(|source| ToolchainError::Io { path, source })
    }

    /// Whether a toolchain for the target is recorded as installed.
    ///
    /// The installed list mixes granularities (`macos/arm64` but a single
    /// `ios`), so a target matches either its directory id or the bare
    /// operating-system name.
    pub fn is_installed(&self, os: OperatingSystem, arch: Architecture) -> bool {
        match super::toolchain_dir_id(os, arch) {
            Some(dir_id) => self
                .toolchains
                .installed
                .iter()
                .any(|entry| entry == dir_id || entry == os.name()),
            None => false,
        }
    }

    /// The Apple SDK path for a target, when one is recorded.
    ///
    /// macOS uses the macOS SDK; an iOS device uses the device SDK; the iOS
    /// simulator (x86_64) uses the simulator SDK.
    pub fn apple_sdk_for(&self, os: OperatingSystem, arch: Architecture) -> Option<&Path> {
        match (os, arch) {
            (OperatingSystem::MacOS, _) => self.apple_sdks.macos.as_deref(),
            (OperatingSystem::IOS, Architecture::Arm) => self.apple_sdks.ios_device.as_deref(),
            (OperatingSystem::IOS, _) => self.apple_sdks.ios_sim.as_deref(),
            _ => None,
        }
    }
}
