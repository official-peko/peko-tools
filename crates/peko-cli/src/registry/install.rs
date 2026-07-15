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
    let mut client = RegistryClient::new(cache)?;

    // Attach the login session (best-effort) so gated packages resolve+download.
    let id_token: Option<String> = match crate::auth::Session::load() {
        Some(session) => crate::auth::fresh_id_token(&session).await.ok(),
        None => None,
    };
    client.set_id_token(id_token.clone());
    let base = crate::auth::platform_base(None);
    let toolchain = super::gated::toolchain_version();

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
                    &base,
                    id_token.as_deref(),
                    toolchain,
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
            LockSource::Gated => {
                fetch_gated(
                    &client,
                    &base,
                    id_token.as_deref(),
                    toolchain,
                    &package.name,
                    &package.version,
                    progress,
                )
                .await?;
                locked.push(LockedPackage {
                    name: package.name,
                    version: package.version,
                    checksum: package.checksum,
                    source: LockSource::Gated,
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
    let mut client = RegistryClient::new(cache)?;

    // The login session (best-effort) lets the platform serve entitlement-gated
    // proprietary packages; anonymous fetches get only public packages.
    let id_token: Option<String> = match crate::auth::Session::load() {
        Some(session) => crate::auth::fresh_id_token(&session).await.ok(),
        None => None,
    };
    client.set_id_token(id_token.clone());

    // A gated package is served as one all-platforms bundle for this CLI's exact
    // toolchain version (ABI lockstep); the platform matches it to a bundle.
    let base = crate::auth::platform_base(None);
    let toolchain = super::gated::toolchain_version();

    for package in &lockfile.packages {
        match package.source {
            LockSource::Registry => {
                let checksum = package.checksum.clone().unwrap_or_default();
                fetch_registry(
                    &client,
                    &base,
                    id_token.as_deref(),
                    toolchain,
                    &package.name,
                    &package.version,
                    &checksum,
                    progress,
                )
                .await?;
            }
            LockSource::Gated => {
                fetch_gated(
                    &client,
                    &base,
                    id_token.as_deref(),
                    toolchain,
                    &package.name,
                    &package.version,
                    progress,
                )
                .await?;
            }
            LockSource::Path => {}
        }
    }
    Ok(())
}

/// Download a registry version, verify it, and unpack it when not already
/// present.
///
/// A public package is a source `.pkpkg` from the CDN. When that is unavailable
/// (a gated package is not served publicly) and the user is signed in, the
/// gated (all-platforms prebuilt bundle) path is tried transparently — so
/// `peko add pekoshots` just works for an authorized account.
#[allow(clippy::too_many_arguments)]
async fn fetch_registry(
    client: &RegistryClient,
    base: &str,
    id_token: Option<&str>,
    toolchain: &str,
    name: &str,
    version: &str,
    checksum: &str,
    progress: &dyn ProgressSink,
) -> Result<(), RegistryError> {
    if client.cache().is_unpacked(name, version) {
        return Ok(());
    }
    progress.tick(&format!("Fetching {name} {version}"));

    match client.download_blob(name, version, checksum).await {
        Ok(bytes) => {
            let dest = client.cache().source_dir(name, version);
            pack::unpack(&bytes, &dest)
        }
        Err(public_error) => {
            // The public download failed; the package may be gated. Try the
            // gated (all-platforms bundle) path when signed in.
            let Some(id_token) = id_token else {
                return Err(public_error);
            };
            progress.tick(&format!("Fetching {name} {version} (gated)"));
            match super::gated::download_bundle(base, id_token, name, toolchain).await {
                Ok(bytes) => {
                    let dest = client.cache().source_dir(name, version);
                    pack::unpack(&bytes, &dest)
                }
                // No gated bundle either (and a transient/network failure): keep
                // the original public error, which is the better message for a
                // genuinely public-but-unavailable or misspelled package.
                Err(super::gated::GatedError::NoBundle { .. })
                | Err(super::gated::GatedError::Network(_)) => Err(public_error),
                Err(other) => Err(other.into_registry(name)),
            }
        }
    }
}

/// Download and unpack a gated (proprietary, prebuilt) package's all-platforms
/// bundle for this toolchain, when not already present.
///
/// Unlike a public package, a gated bundle is served only to an authenticated,
/// entitled account, so a missing session is a hard error with a clear message
/// rather than a silent public fallback.
async fn fetch_gated(
    client: &RegistryClient,
    base: &str,
    id_token: Option<&str>,
    toolchain: &str,
    name: &str,
    version: &str,
    progress: &dyn ProgressSink,
) -> Result<(), RegistryError> {
    if client.cache().is_unpacked(name, version) {
        return Ok(());
    }
    let Some(id_token) = id_token else {
        return Err(RegistryError::SignInRequired {
            package: name.to_owned(),
        });
    };
    progress.tick(&format!("Fetching {name} {version} (gated)"));
    match super::gated::download_bundle(base, id_token, name, toolchain).await {
        Ok(bytes) => {
            let dest = client.cache().source_dir(name, version);
            pack::unpack(&bytes, &dest)
        }
        Err(other) => Err(other.into_registry(name)),
    }
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
                Dependency::Registry { version, .. } => match entry.source {
                    LockSource::Registry => {
                        Version::parse(&entry.version).is_ok_and(|locked| version.matches(&locked))
                    }
                    // A gated package is pinned by toolchain, not the manifest
                    // version requirement, so a locked gated entry is always
                    // considered a valid pin for a registry-style dependency.
                    LockSource::Gated => true,
                    LockSource::Path => false,
                },
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
