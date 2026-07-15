//! Dependency resolution against the registry index.
//!
//! Resolution accumulates every requirement seen for each package, then picks
//! the highest non-yanked version that satisfies their intersection and is
//! compatible with the compiler. Transitive dependencies come from the index
//! entry rather than the body. The process iterates to a fixpoint, so a package
//! required by two parents converges on one version that satisfies both, and a
//! genuinely unsatisfiable set surfaces as a conflict. Path dependencies are
//! read from their local `peko.toml` and override registry entries of the same
//! name.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use peko_core::config::{Dependency, LoadedManifest, LockSource, MANIFEST_FILE, Manifest};
use semver::{Version, VersionReq};

use super::RegistryError;
use super::client::RegistryClient;
use super::gated::GatedMeta;
use super::index::IndexEntry;

/// An upper bound on resolution passes, a backstop against a non-converging
/// requirement set.
const MAX_PASSES: usize = 1000;

/// One package selected by resolution.
#[derive(Debug, Clone)]
pub struct ResolvedPackage {
    /// The package name.
    pub name: String,
    /// The exact selected version.
    pub version: String,
    /// The `.pkpkg` checksum for a registry package.
    pub checksum: Option<String>,
    /// Where the package resolved from.
    pub source: LockSource,
    /// The resolved directory for a path dependency.
    pub path: Option<PathBuf>,
}

/// The accumulated state of an in-progress resolution.
#[derive(Default)]
struct ResolveState {
    /// Every requirement seen for each registry package.
    requirements: BTreeMap<String, Vec<VersionReq>>,
    /// Path dependencies mapped to their local directory.
    paths: BTreeMap<String, PathBuf>,
    /// Path dependency versions, read from their manifests.
    path_versions: BTreeMap<String, String>,
    /// The chosen index entry for each registry package.
    chosen: BTreeMap<String, IndexEntry>,
    /// Fetched index entries, cached per package.
    indices: BTreeMap<String, Vec<IndexEntry>>,
    /// Packages resolved as proprietary, entitlement-gated ones (absent from the
    /// public index, pinned by toolchain via the platform `/meta` endpoint).
    gated: BTreeMap<String, GatedMeta>,
}

/// Resolves a project's dependency graph against a registry client.
pub struct Resolver<'a> {
    client: &'a RegistryClient,
    compiler: Version,
}

impl<'a> Resolver<'a> {
    /// Build a resolver over the given client, using this build's compiler
    /// version for `min_compiler` checks.
    pub fn new(client: &'a RegistryClient) -> Resolver<'a> {
        let compiler =
            Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or_else(|_| Version::new(0, 0, 0));
        Resolver { client, compiler }
    }

    /// Resolve every transitive dependency of `root` into a flat package set.
    pub async fn resolve(
        &self,
        root: &LoadedManifest,
    ) -> Result<Vec<ResolvedPackage>, RegistryError> {
        let mut state = ResolveState::default();
        seed(root.manifest.dependencies(), &root.root, &mut state)?;

        for _ in 0..MAX_PASSES {
            if !self.refine(&mut state).await? {
                return Ok(state.into_resolved());
            }
        }
        Err(RegistryError::Conflict {
            package: String::from("<graph>"),
            detail: String::from("dependency resolution did not converge"),
        })
    }

    /// Run one resolution pass, returning whether anything changed.
    async fn refine(&self, state: &mut ResolveState) -> Result<bool, RegistryError> {
        let mut changed = false;
        let names: Vec<String> = state.requirements.keys().cloned().collect();

        for name in names {
            if state.paths.contains_key(&name) || state.gated.contains_key(&name) {
                continue;
            }

            let requirements = state.requirements[&name].clone();
            let entries = match self.ensure_index(&name, state).await {
                Ok(entries) => entries,
                // No public index entry. The package may be a proprietary,
                // toolchain-pinned one served through the platform. Resolve it
                // via the public `/meta` endpoint (no auth); the gated
                // `/download` is only reached later, at fetch time.
                Err(RegistryError::NotFound(missing)) => {
                    match self.client.fetch_gated_meta(&name).await? {
                        Some(meta) if meta.gated && meta.available => {
                            state.gated.insert(name.clone(), meta);
                            changed = true;
                            continue;
                        }
                        _ => return Err(RegistryError::NotFound(missing)),
                    }
                }
                Err(other) => return Err(other),
            };
            let entry = pick_version(&name, &entries, &requirements, &self.compiler)?;

            let is_new = state
                .chosen
                .get(&name)
                .is_none_or(|prev| prev.version != entry.version);
            if !is_new {
                continue;
            }
            changed = true;

            for (dep_name, requirement) in &entry.deps {
                if state.paths.contains_key(dep_name) {
                    continue;
                }
                let requirement =
                    VersionReq::parse(requirement).map_err(|_| RegistryError::InvalidVersion {
                        package: dep_name.clone(),
                        version: requirement.clone(),
                    })?;
                if add_requirement(&mut state.requirements, dep_name, requirement) {
                    changed = true;
                }
            }
            state.chosen.insert(name, entry);
        }

        Ok(changed)
    }

    /// Fetch a package's index entries, caching them for later passes.
    async fn ensure_index(
        &self,
        name: &str,
        state: &mut ResolveState,
    ) -> Result<Vec<IndexEntry>, RegistryError> {
        if let Some(entries) = state.indices.get(name) {
            return Ok(entries.clone());
        }
        let entries = self.client.fetch_index(name).await?;
        state.indices.insert(name.to_owned(), entries.clone());
        Ok(entries)
    }
}

impl ResolveState {
    /// Flatten the resolved state into a package list.
    fn into_resolved(self) -> Vec<ResolvedPackage> {
        let mut resolved =
            Vec::with_capacity(self.paths.len() + self.chosen.len() + self.gated.len());
        for (name, meta) in self.gated {
            // A gated package is pinned by the toolchain it was built for, not a
            // sparse-index version. The checksum is normalized to `sha256:<hex>`
            // to line up with registry checksums.
            let checksum = meta.sha256.map(|hex| {
                if hex.contains(':') {
                    hex
                } else {
                    format!("sha256:{hex}")
                }
            });
            resolved.push(ResolvedPackage {
                name,
                version: meta.toolchain.unwrap_or_default(),
                checksum,
                source: LockSource::Gated,
                path: None,
            });
        }
        for (name, dir) in self.paths {
            let version = self.path_versions.get(&name).cloned().unwrap_or_default();
            resolved.push(ResolvedPackage {
                name,
                version,
                checksum: None,
                source: LockSource::Path,
                path: Some(dir),
            });
        }
        for (name, entry) in self.chosen {
            resolved.push(ResolvedPackage {
                name,
                version: entry.version,
                checksum: Some(entry.checksum),
                source: LockSource::Registry,
                path: None,
            });
        }
        resolved
    }
}

/// Seed resolution state from a manifest's dependencies.
///
/// Registry dependencies add a requirement. Path dependencies are read from
/// their local manifest and recursed into, since their subtree resolves
/// locally without the registry.
fn seed(
    deps: &BTreeMap<String, Dependency>,
    base_dir: &Path,
    state: &mut ResolveState,
) -> Result<(), RegistryError> {
    for (name, dep) in deps {
        match dep {
            Dependency::Registry { version, .. } => {
                add_requirement(&mut state.requirements, name, version.clone());
            }
            Dependency::Path { path, .. } => {
                seed_path(name, base_dir.join(path), state)?;
            }
        }
    }
    Ok(())
}

/// Seed a path dependency and its transitive dependencies.
fn seed_path(name: &str, dir: PathBuf, state: &mut ResolveState) -> Result<(), RegistryError> {
    if state.paths.contains_key(name) {
        return Ok(());
    }

    // Canonicalize so `..` chains from nested path dependencies collapse, and
    // errors and lockfile entries show a clean path. A directory that cannot
    // be canonicalized does not exist, which the manifest check then reports.
    let dir = dir.canonicalize().unwrap_or(dir);
    let manifest_path = dir.join(MANIFEST_FILE);
    if !manifest_path.is_file() {
        return Err(RegistryError::InvalidPathDependency {
            name: name.to_owned(),
            path: dir,
        });
    }

    let loaded = Manifest::load(&manifest_path)?;
    state
        .path_versions
        .insert(name.to_owned(), loaded.manifest.version().to_string());
    state.paths.insert(name.to_owned(), dir);
    seed(loaded.manifest.dependencies(), &loaded.root, state)
}

/// Add a requirement for a package, returning whether it was new.
fn add_requirement(
    requirements: &mut BTreeMap<String, Vec<VersionReq>>,
    name: &str,
    requirement: VersionReq,
) -> bool {
    let bucket = requirements.entry(name.to_owned()).or_default();
    let text = requirement.to_string();
    if bucket.iter().any(|existing| existing.to_string() == text) {
        return false;
    }
    bucket.push(requirement);
    true
}

/// Pick the highest non-yanked version satisfying every requirement and the
/// compiler constraint.
fn pick_version(
    name: &str,
    entries: &[IndexEntry],
    requirements: &[VersionReq],
    compiler: &Version,
) -> Result<IndexEntry, RegistryError> {
    let mut best: Option<(Version, &IndexEntry)> = None;
    for entry in entries {
        if entry.yanked {
            continue;
        }
        let Ok(version) = Version::parse(&entry.version) else {
            continue;
        };
        if !requirements.iter().all(|req| req.matches(&version)) {
            continue;
        }
        if !compiler_satisfies(entry, compiler) {
            continue;
        }
        if best
            .as_ref()
            .is_none_or(|(best_version, _)| version > *best_version)
        {
            best = Some((version, entry));
        }
    }

    if let Some((_, entry)) = best {
        return Ok(entry.clone());
    }

    let available = entries
        .iter()
        .map(|entry| entry.version.clone())
        .collect::<Vec<_>>()
        .join(", ");
    if requirements.len() > 1 {
        Err(RegistryError::Conflict {
            package: name.to_owned(),
            detail: format!(
                "no version satisfies all of [{}]; available: {available}",
                requirements
                    .iter()
                    .map(VersionReq::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        })
    } else {
        Err(RegistryError::NoMatchingVersion {
            package: name.to_owned(),
            requirement: requirements
                .first()
                .map(VersionReq::to_string)
                .unwrap_or_else(|| String::from("*")),
            available,
        })
    }
}

/// `true` if the entry's `min_compiler` requirement admits the compiler.
///
/// A missing or unparseable requirement is treated as no constraint.
fn compiler_satisfies(entry: &IndexEntry, compiler: &Version) -> bool {
    match entry.min_compiler.as_deref() {
        Some(text) => VersionReq::parse(text)
            .map(|req| req.matches(compiler))
            .unwrap_or(true),
        None => true,
    }
}
