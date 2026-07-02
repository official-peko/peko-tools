//! Resolving, downloading, unpacking, and locking a project's dependencies.
//!
//! [`install`] is lock-aware: when `peko.lock` already pins versions that
//! satisfy the manifest's direct dependencies, it reproduces that exact set
//! without contacting the resolver. Otherwise, and always for [`update`], it
//! resolves fresh and rewrites the lockfile. Each registry blob is downloaded,
//! checksum-verified, and unpacked into the shared source cache, where the
//! compiler resolves imports against it.

use std::path::Path;

use peko_core::config::{
    Dependency, LoadedManifest, LockSource, LockedPackage, Lockfile, Manifest,
};
use semver::Version;

use crate::cli::reporting::ProgressSink;

use super::pack;
use super::{Cache, RegistryClient, RegistryError, Resolver};

/// Install a project's dependencies, reproducing from `peko.lock` when it
/// satisfies the manifest and otherwise resolving fresh.
pub async fn install(
    peko_root: &Path,
    loaded: &LoadedManifest,
    progress: &dyn ProgressSink,
) -> Result<Lockfile, RegistryError> {
    if let Some(lockfile) = Lockfile::load_from_root(&loaded.root)?
        && lock_satisfies_manifest(&lockfile, loaded)
    {
        fetch_locked(peko_root, &lockfile, progress).await?;
        return Ok(lockfile);
    }
    update(peko_root, loaded, progress).await
}

/// Resolve a project's dependencies fresh, download them, and rewrite
/// `peko.lock`, ignoring any existing lockfile.
pub async fn update(
    peko_root: &Path,
    loaded: &LoadedManifest,
    progress: &dyn ProgressSink,
) -> Result<Lockfile, RegistryError> {
    let cache = Cache::new(peko_root);
    let client = RegistryClient::new(cache)?;
    let resolver = Resolver::new(&client);

    progress.message("Resolving dependencies");
    let resolved = resolver.resolve(loaded).await?;

    let mut locked = Vec::with_capacity(resolved.len());
    for package in resolved {
        match package.source {
            LockSource::Registry => {
                let checksum = package.checksum.clone().unwrap_or_default();
                fetch_registry(
                    &client,
                    &package.name,
                    &package.version,
                    &checksum,
                    progress,
                )
                .await?;
                locked.push(LockedPackage {
                    name: package.name,
                    version: package.version,
                    checksum: package.checksum,
                    source: LockSource::Registry,
                    path: None,
                });
            }
            LockSource::Path => {
                let path = package
                    .path
                    .as_deref()
                    .map(|dir| relativize(&loaded.root, dir));
                locked.push(LockedPackage {
                    name: package.name,
                    version: package.version,
                    checksum: None,
                    source: LockSource::Path,
                    path,
                });
            }
        }
    }

    let lockfile = Lockfile::new(locked);
    lockfile.save_to_root(&loaded.root)?;
    Ok(lockfile)
}

/// Ensure a project's dependencies are present, reproducing from `peko.lock`.
///
/// A project with no dependencies short-circuits without a network request or
/// a lockfile write. Called at the start of a build so imports resolve against
/// installed source.
pub async fn ensure_dependencies(
    peko_root: &Path,
    project_root: &Path,
    progress: &dyn ProgressSink,
) -> Result<(), RegistryError> {
    let loaded = Manifest::discover(project_root)?;
    if loaded.manifest.dependencies().is_empty() {
        return Ok(());
    }
    install(peko_root, &loaded, progress).await.map(|_| ())
}

/// Download and unpack every registry package named by a lockfile.
async fn fetch_locked(
    peko_root: &Path,
    lockfile: &Lockfile,
    progress: &dyn ProgressSink,
) -> Result<(), RegistryError> {
    let cache = Cache::new(peko_root);
    let client = RegistryClient::new(cache)?;
    for package in &lockfile.packages {
        if package.source == LockSource::Registry {
            let checksum = package.checksum.clone().unwrap_or_default();
            fetch_registry(
                &client,
                &package.name,
                &package.version,
                &checksum,
                progress,
            )
            .await?;
        }
    }
    Ok(())
}

/// Download a registry version, verify it, and unpack it when not already
/// present.
async fn fetch_registry(
    client: &RegistryClient,
    name: &str,
    version: &str,
    checksum: &str,
    progress: &dyn ProgressSink,
) -> Result<(), RegistryError> {
    progress.tick(&format!("Fetching {name} {version}"));
    let bytes = client.download_blob(name, version, checksum).await?;
    if !client.cache().is_unpacked(name, version) {
        let dest = client.cache().source_dir(name, version);
        pack::unpack(&bytes, &dest)?;
    }
    Ok(())
}

/// `true` if the lockfile pins a satisfying version for every direct
/// dependency of the manifest.
///
/// Transitive entries are taken on trust, since the lockfile is authoritative
/// for them. A direct dependency that is missing or whose locked version no
/// longer satisfies its requirement forces a fresh resolution.
fn lock_satisfies_manifest(lockfile: &Lockfile, loaded: &LoadedManifest) -> bool {
    loaded
        .manifest
        .dependencies()
        .iter()
        .all(|(name, dependency)| {
            let Some(entry) = lockfile.packages.iter().find(|locked| &locked.name == name) else {
                return false;
            };
            match dependency {
                Dependency::Registry { version, .. } => {
                    entry.source == LockSource::Registry
                        && Version::parse(&entry.version)
                            .is_ok_and(|locked| version.matches(&locked))
                }
                Dependency::Path { .. } => entry.source == LockSource::Path,
            }
        })
}

/// Express `path` relative to `root` when possible.
fn relativize(root: &Path, path: &Path) -> std::path::PathBuf {
    path.strip_prefix(root)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| path.to_path_buf())
}
