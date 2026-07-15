//! Routing a build target to its installed toolchain.

use std::path::{Path, PathBuf};

use peko_core::config::Toolchain;
use peko_core::target::{Architecture, OperatingSystem, PekoTarget};

use super::version::InstallManifest;
use super::{ToolchainError, toolchain_dir_id};

/// A located toolchain: its directory and parsed `toolchain.toml`.
#[derive(Debug, Clone)]
pub struct ResolvedToolchain {
    /// The toolchain directory, the base for the toolchain's relative paths.
    pub dir: PathBuf,
    /// The parsed `toolchain.toml`.
    pub toolchain: Toolchain,
}

/// The toolchains root under a Peko root.
pub fn toolchains_root(peko_root: &Path) -> PathBuf {
    peko_root.join("Compiler").join("toolchains")
}

/// Route a target to its installed toolchain, loading its `toolchain.toml`.
pub fn resolve_toolchain(
    peko_root: &Path,
    manifest: &InstallManifest,
    os: OperatingSystem,
    arch: Architecture,
) -> Result<ResolvedToolchain, ToolchainError> {
    let dir_id = toolchain_dir_id(os, arch).ok_or_else(|| ToolchainError::Unsupported {
        target: format!("{}/{}", os.name(), arch.name()),
    })?;

    if !manifest.is_installed(os, arch) {
        return Err(ToolchainError::NotInstalled {
            id: dir_id.to_owned(),
        });
    }

    let dir = toolchains_root(peko_root).join(dir_id);
    // Android ships one shared (multi-arch) toolchain directory; the arm64 and
    // x86_64 variants differ only in their toolchain.toml (triple, lib/include
    // paths, ELF machine), so x86_64 loads a sibling file rather than a whole
    // second directory (which would duplicate the large shared sysroot).
    let toolchain_toml = if os == OperatingSystem::Android && arch == Architecture::X86_64 {
        dir.join("toolchain.x86_64.toml")
    } else {
        dir.join("toolchain.toml")
    };
    let toolchain = Toolchain::load(&toolchain_toml).map_err(|source| ToolchainError::Load {
        path: toolchain_toml,
        source: Box::new(source),
    })?;

    Ok(ResolvedToolchain { dir, toolchain })
}

/// Load the install manifest and resolve the toolchain for a target.
pub fn resolve_for_target(
    peko_root: &Path,
    target: &PekoTarget,
) -> Result<ResolvedToolchain, ToolchainError> {
    let manifest = InstallManifest::load(peko_root)?;
    resolve_toolchain(
        peko_root,
        &manifest,
        target.operating_system,
        target.architecture,
    )
}
