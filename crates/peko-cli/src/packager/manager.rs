//! Firebase-backed package manager.
//!
//! [`PackageManager`] is the cli's interface for installing, updating, and
//! removing Peko packages. It speaks directly to Firebase (Firestore for
//! package metadata, Cloud Storage for binary payloads) and writes packages
//! into a scope-dependent `Packages/` directory under either the user's
//! global `.Peko` root or the project's local `.peko` root.
//!
//! ## Error handling
//!
//! Public methods return `Result<(), PackageError>`. Callers (the `add`,
//! `update`, `remove` commands) own a [`Reporter`] and render the error via
//! `reporter.error(e)` when one surfaces. This module never writes to
//! stderr / stdout directly.
//!
//! ## On-disk invariants
//!
//! - Each package lives at `<packages_dir>/<name>/`, with one subdirectory
//!   per installed version (`<name>/<vX.Y.Z>/`).
//! - `<name>/Package.json` is written from the bucket's copy with four
//!   changes: the `versions` and `latest` keys are rewritten to reflect
//!   what's actually installed under `<name>/`, the `license` key is
//!   dropped, and the `description` key is set from the Firestore metadata.
//!   Any other fields from the bucket Package.json (label, name, custom
//!   metadata) pass through untouched.
//! - Version strings are `vMAJOR.MINOR.PATCH` (e.g. `v0.1.0`, `v1.2.3`).
//!
//! [`Reporter`]: crate::cli::reporting::Reporter

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use peko_core::error::PekoError;
use peko_core::packages::PekoPackageIndex;
use serde_json::Value;
use thiserror::Error;
use zip::ZipArchive;

use crate::cli::reporting::ProgressSink;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const LOCAL_SEARCH_DEPTH: usize = 5;

const FIRESTORE_BASE: &str =
    "https://firestore.googleapis.com/v1/projects/peko-platform/databases/(default)/documents";

/// Firebase Storage bucket. Newer projects use `<name>.firebasestorage.app`;
/// older projects use `<name>.appspot.com`. Switch if uploads fail with 404.
const STORAGE_BUCKET: &str = "peko-platform.firebasestorage.app";

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// One failure mode for a package operation.
#[derive(Debug, Error)]
pub enum PackageError {
    /// No `.peko` directory found in any ancestor of the cwd when running
    /// in [`Scope::Local`].
    #[error(
        "no `.peko` directory found in {start_dir} or up to {depth} parent directories. \
         Run inside a Peko project or use --global."
    )]
    NoLocalRoot { start_dir: PathBuf, depth: usize },

    /// Couldn't determine the user's home directory for [`Scope::Global`].
    #[error("could not locate user home directory for global package root")]
    NoGlobalRoot,

    /// Couldn't build a `reqwest::Client` (rarely fires).
    #[error("failed to build HTTP client: {0}")]
    HttpClient(#[source] reqwest::Error),

    /// A required package isn't installed at all.
    #[error("package `{0}` is not installed")]
    NotInstalled(String),

    /// A package isn't published with the requested version.
    #[error("version {version} not available for {package}. Available: {available}")]
    UnavailableVersion {
        package: String,
        version: String,
        available: String,
    },

    /// A specific version isn't installed locally (used by `remove_version`).
    #[error("version {version} not installed for {package}. Installed: {installed}")]
    UninstalledVersion {
        package: String,
        version: String,
        installed: String,
    },

    /// A package depends on itself transitively.
    #[error("dependency cycle at {0}")]
    Cycle(String),

    /// A package isn't published in Firestore.
    #[error("package `{0}` not found")]
    NotFound(String),

    /// A downloaded version's contents don't match what `HostPackage`
    /// expects.
    #[error("{package}@{version} is corrupt — missing main.peko or deps.json")]
    CorruptPackage { package: String, version: String },

    /// A downloaded version's `deps.json` failed to parse.
    #[error("{package}@{version} has invalid deps.json: {source}")]
    InvalidDeps {
        package: String,
        version: String,
        #[source]
        source: serde_json::Error,
    },

    /// `Package.json` parsed as something other than a JSON object.
    #[error("Package.json at {0} is not a JSON object")]
    PackageJsonNotObject(PathBuf),

    /// `Package.json` was missing and there were no Firebase metadata to
    /// seed a fresh copy from.
    #[error("missing Package.json and no Firebase metadata to seed it")]
    MissingPackageJson,

    /// Install completed but no version subdirectory ended up on disk.
    #[error("no version folders on disk after install")]
    NoVersionsInstalled,

    /// Firestore responded with a non-success HTTP status.
    #[error("Firebase error ({status}) fetching {package}: {body}")]
    Firebase {
        package: String,
        status: u16,
        body: String,
    },

    /// A binary download returned a non-success HTTP status.
    #[error("download failed ({status}): {url}")]
    Download { status: u16, url: String },

    /// A Firestore response didn't deserialize.
    #[error("malformed Firestore response for {package}: {source}")]
    MalformedFirestore {
        package: String,
        #[source]
        source: serde_json::Error,
    },

    /// A Firestore response parsed but lacked the expected `fields` key.
    #[error("Firestore response for {0} is missing the expected fields")]
    FirestoreMissingFields(String),

    /// A zip-extraction task panicked.
    #[error("extraction task panicked for {package}@{version}: {source}")]
    ExtractionPanic {
        package: String,
        version: String,
        #[source]
        source: tokio::task::JoinError,
    },

    /// A network operation failed (transport-level, before HTTP status).
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),

    /// An on-disk operation failed.
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// JSON parse failed (Package.json, deps.json, etc.).
    #[error("failed to parse JSON at {path}: {source}")]
    JsonParse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    /// A peko_core operation failed (e.g. building a package index).
    #[error(transparent)]
    Peko(#[from] PekoError),
}

/// Convenience alias matching the rest of the crate's `PekoResult`-style
/// idiom.
pub type PackageResult<T> = std::result::Result<T, PackageError>;

/// Wrap an `io::Result<T>` so the failure path captures the path that was
/// being acted on.
fn io_at<T>(path: &Path, op: io::Result<T>) -> PackageResult<T> {
    op.map_err(|source| PackageError::Io {
        path: path.to_path_buf(),
        source,
    })
}

// ---------------------------------------------------------------------------
// Scope & remote-package shape
// ---------------------------------------------------------------------------

/// Where a package operation writes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// `~/.Peko/Packages`. Used with `--global`.
    Global,
    /// `<project>/.peko/Packages`. Found by walking up from the CWD.
    Local,
}

/// Package metadata pulled from Firestore. Private to this module.
#[derive(Debug, Clone)]
struct RemotePackage {
    name: String,
    description: String,
    version: String,
    versions: Vec<String>,
    /// License text (stored inline in the Firestore document, not as a URL).
    license: String,
}

// ---------------------------------------------------------------------------
// PackageManager
// ---------------------------------------------------------------------------

/// Coordinates package install / update / remove against Firebase plus the
/// local on-disk cache.
pub struct PackageManager {
    scope: Scope,
    /// `.Peko` or `.peko`, parent of `Packages`. [`PekoPackageIndex::new`]
    /// expects this path because it appends `Packages` internally.
    peko_root: PathBuf,
    packages_dir: PathBuf,
    client: reqwest::Client,
}

impl PackageManager {
    // -- Construction -------------------------------------------------------

    /// Build a `PackageManager`. Resolves the scope's root, creates the
    /// packages directory if missing, and constructs a single HTTP client
    /// shared across all subsequent operations.
    pub fn new(scope: Scope, start_dir: impl AsRef<Path>) -> PackageResult<Self> {
        let peko_root = match scope {
            Scope::Global => global_root().ok_or(PackageError::NoGlobalRoot)?,
            Scope::Local => find_local_root(start_dir.as_ref())?,
        };
        let packages_dir = peko_root.join("Packages");
        io_at(&packages_dir, std::fs::create_dir_all(&packages_dir))?;

        let client = reqwest::Client::builder()
            .user_agent(concat!("peko-cli/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(PackageError::HttpClient)?;

        Ok(Self {
            scope,
            peko_root,
            packages_dir,
            client,
        })
    }

    pub fn scope(&self) -> Scope {
        self.scope
    }

    pub fn packages_dir(&self) -> &Path {
        &self.packages_dir
    }

    /// Build a fresh filesystem-backed package index. Cheap; call after any
    /// mutation.
    pub fn index(&self) -> PackageResult<PekoPackageIndex> {
        Ok(match self.scope {
            Scope::Global => PekoPackageIndex::new(&self.peko_root, Option::<&Path>::None)?,
            Scope::Local => {
                let global = global_root().unwrap_or_else(|| self.peko_root.clone());
                PekoPackageIndex::new(global, Some(&self.peko_root))?
            }
        })
    }

    // -- Public mutation API ------------------------------------------------

    /// Install a package: latest version only, plus its dependencies.
    ///
    /// Phase updates are reported through the `progress` sink as the
    /// install proceeds, outer messages (`Fetching metadata...`) ride
    /// the bar's current state, and each version-install (download,
    /// extract, verify) advances the bar via `tick`. Dependencies
    /// recurse and contribute their own phase counts. The caller is
    /// expected to have already called `progress.start_phase(...)`.
    pub async fn install_package(
        &self,
        name: &str,
        progress: &dyn ProgressSink,
    ) -> PackageResult<()> {
        progress.message(&format!("Fetching metadata for {name}"));
        let pkg = self.fetch_package(name).await?;
        let latest = pkg.version.clone();
        self.install_one(&pkg, &latest, &mut HashSet::new(), progress)
            .await
    }

    /// Add a specific version alongside any existing versions.
    pub async fn install_version(
        &self,
        name: &str,
        version: &str,
        progress: &dyn ProgressSink,
    ) -> PackageResult<()> {
        progress.message(&format!("Fetching metadata for {name}"));
        let pkg = self.fetch_package(name).await?;
        if !pkg.versions.iter().any(|v| v == version) {
            return Err(PackageError::UnavailableVersion {
                package: name.to_owned(),
                version: version.to_owned(),
                available: pkg.versions.join(", "),
            });
        }
        self.install_one(&pkg, version, &mut HashSet::new(), progress)
            .await
    }

    /// Update an installed package to its latest published version. Fails
    /// if the package isn't installed locally.
    pub async fn update_package(
        &self,
        name: &str,
        progress: &dyn ProgressSink,
    ) -> PackageResult<()> {
        if !self.package_dir(name).is_dir() {
            return Err(PackageError::NotInstalled(name.to_owned()));
        }
        progress.message(&format!("Fetching latest version of {name}"));
        let pkg = self.fetch_package(name).await?;
        let latest = pkg.version.clone();
        self.install_one(&pkg, &latest, &mut HashSet::new(), progress)
            .await
    }

    /// Remove a package entirely. Dependencies are kept.
    pub async fn remove_package(
        &self,
        name: &str,
        progress: &dyn ProgressSink,
    ) -> PackageResult<()> {
        let dir = self.package_dir(name);
        if !dir.is_dir() {
            return Err(PackageError::NotInstalled(name.to_owned()));
        }
        progress.message(&format!("Removing {name}"));
        tokio::fs::remove_dir_all(&dir)
            .await
            .map_err(|source| PackageError::Io {
                path: dir.clone(),
                source,
            })?;
        Ok(())
    }

    /// Remove a single version. If it was the last installed version,
    /// removes the whole package directory. Otherwise rewrites
    /// `Package.json` so `versions[]` and `latest` reflect what's left on
    /// disk.
    pub async fn remove_version(
        &self,
        name: &str,
        version: &str,
        progress: &dyn ProgressSink,
    ) -> PackageResult<()> {
        let pkg_dir = self.package_dir(name);
        if !pkg_dir.is_dir() {
            return Err(PackageError::NotInstalled(name.to_owned()));
        }
        let version_dir = self.version_dir(name, version);
        if !version_dir.is_dir() {
            return Err(PackageError::UninstalledVersion {
                package: name.to_owned(),
                version: version.to_owned(),
                installed: scan_version_folders(&pkg_dir).join(", "),
            });
        }

        progress.message(&format!("Removing {name} {version}"));
        tokio::fs::remove_dir_all(&version_dir)
            .await
            .map_err(|source| PackageError::Io {
                path: version_dir.clone(),
                source,
            })?;

        let remaining = scan_version_folders(&pkg_dir);
        if remaining.is_empty() {
            tokio::fs::remove_dir_all(&pkg_dir)
                .await
                .map_err(|source| PackageError::Io {
                    path: pkg_dir.clone(),
                    source,
                })?;
            return Ok(());
        }
        self.sync_package_json_from_disk(&pkg_dir, &remaining).await
    }

    // -- Private: install pipeline -----------------------------------------

    fn package_dir(&self, id: &str) -> PathBuf {
        self.packages_dir.join(id)
    }

    fn version_dir(&self, id: &str, v: &str) -> PathBuf {
        self.package_dir(id).join(v)
    }

    /// Install one (pkg, version) and recurse into its deps. `in_progress`
    /// tracks the install stack for cycle detection. Boxed because async
    /// fns can't recurse directly.
    ///
    /// The returned future is not `Send`-bound because `&dyn ProgressSink`
    /// is not `Sync` and the cli runs on a single-threaded tokio runtime.
    fn install_one<'a>(
        &'a self,
        pkg: &'a RemotePackage,
        version: &'a str,
        in_progress: &'a mut HashSet<String>,
        progress: &'a dyn ProgressSink,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = PackageResult<()>> + 'a>> {
        Box::pin(async move {
            let cycle_key = format!("{}@{}", pkg.name, version);
            if !in_progress.insert(cycle_key.clone()) {
                return Err(PackageError::Cycle(cycle_key));
            }
            // Always remove the cycle key when this frame returns,
            // regardless of which path produced the result.
            let result = self
                .install_one_inner(pkg, version, in_progress, progress)
                .await;
            in_progress.remove(&cycle_key);
            result
        })
    }

    async fn install_one_inner(
        &self,
        pkg: &RemotePackage,
        version: &str,
        in_progress: &mut HashSet<String>,
        progress: &dyn ProgressSink,
    ) -> PackageResult<()> {
        if !pkg.versions.iter().any(|v| v == version) {
            return Err(PackageError::UnavailableVersion {
                package: pkg.name.clone(),
                version: version.to_owned(),
                available: pkg.versions.join(", "),
            });
        }

        // Three known phases per install (download, extract, verify);
        // dependencies recurse and contribute their own.
        progress.add_to_total(3);

        let pkg_dir = self.package_dir(&pkg.name);
        let version_dir = self.version_dir(&pkg.name, version);
        tokio::fs::create_dir_all(&pkg_dir)
            .await
            .map_err(|source| PackageError::Io {
                path: pkg_dir.clone(),
                source,
            })?;

        // 1. Write LICENSE from the Firestore `license` string (the actual
        //    license text, not a URL). Package.json is written separately
        //    further down so we can override versions[]+latest with disk
        //    reality.
        let license_path = pkg_dir.join("LICENSE");
        tokio::fs::write(&license_path, &pkg.license)
            .await
            .map_err(|source| PackageError::Io {
                path: license_path,
                source,
            })?;

        // 2. Download + extract the version zip from
        //    Storage://packages/{name}/{version}.zip.
        progress.tick(&format!("Downloading {} {version}", pkg.name));
        let zip_url = version_zip_url(&pkg.name, version);
        let zip_tmp = pkg_dir.join(format!(".{version}.zip"));
        self.download_to_file(&zip_url, &zip_tmp).await?;

        // Clean any prior partial extraction.
        if version_dir.exists() {
            if let Err(source) = tokio::fs::remove_dir_all(&version_dir).await {
                let _ = tokio::fs::remove_file(&zip_tmp).await;
                return Err(PackageError::Io {
                    path: version_dir,
                    source,
                });
            }
        }
        if let Err(source) = tokio::fs::create_dir_all(&version_dir).await {
            let _ = tokio::fs::remove_file(&zip_tmp).await;
            return Err(PackageError::Io {
                path: version_dir,
                source,
            });
        }

        progress.tick(&format!("Extracting {} {version}", pkg.name));
        // Zip extraction is sync, run on a blocking thread.
        let zip_path = zip_tmp.clone();
        let dest = version_dir.clone();
        let extract_outcome =
            tokio::task::spawn_blocking(move || extract_zip_to_dir(&zip_path, &dest)).await;
        let _ = tokio::fs::remove_file(&zip_tmp).await;
        match extract_outcome {
            Ok(Ok(())) => {}
            Ok(Err(source)) => {
                let _ = tokio::fs::remove_dir_all(&version_dir).await;
                return Err(PackageError::Io {
                    path: version_dir,
                    source,
                });
            }
            Err(source) => {
                let _ = tokio::fs::remove_dir_all(&version_dir).await;
                return Err(PackageError::ExtractionPanic {
                    package: pkg.name.clone(),
                    version: version.to_owned(),
                    source,
                });
            }
        }

        // 3. Validate the extracted contents match what HostPackage expects.
        progress.tick(&format!("Verifying {} {version}", pkg.name));
        let deps_path = version_dir.join("deps.json");
        if !version_dir.join("main.peko").exists() || !deps_path.exists() {
            let _ = tokio::fs::remove_dir_all(&version_dir).await;
            return Err(PackageError::CorruptPackage {
                package: pkg.name.clone(),
                version: version.to_owned(),
            });
        }

        // 4. Write Package.json reflecting on-disk versions + fresh metadata.
        let installed = scan_version_folders(&pkg_dir);
        self.write_synced_package_json(&pkg_dir, &installed, Some(pkg))
            .await?;

        // 5. Recurse into deps from the version we just extracted.
        let deps_bytes = tokio::fs::read(&deps_path)
            .await
            .map_err(|source| PackageError::Io {
                path: deps_path.clone(),
                source,
            })?;
        let deps: HashMap<String, String> =
            serde_json::from_slice(&deps_bytes).map_err(|source| PackageError::InvalidDeps {
                package: pkg.name.clone(),
                version: version.to_owned(),
                source,
            })?;

        if !deps.is_empty() {
            progress.message(&format!(
                "Resolving {} dependencies of {} {version}",
                deps.len(),
                pkg.name
            ));
        }
        for (dep_name, dep_version) in deps {
            let dep_pkg = self.fetch_package(&dep_name).await?;
            self.install_one(&dep_pkg, &dep_version, in_progress, progress)
                .await?;
        }

        Ok(())
    }

    /// Rewrite `Package.json` after `remove_version`, the file already
    /// exists; sync `versions[]` and `latest` with what's on disk.
    ///
    /// Works on the raw JSON value. Every field from the original
    /// `Package.json` other than `license` is kept.
    async fn sync_package_json_from_disk(
        &self,
        pkg_dir: &Path,
        installed_versions: &[String],
    ) -> PackageResult<()> {
        let json_path = pkg_dir.join("Package.json");
        let text = io_at(&json_path, std::fs::read_to_string(&json_path))?;
        let mut value: Value =
            serde_json::from_str(&text).map_err(|source| PackageError::JsonParse {
                path: json_path.clone(),
                source,
            })?;
        let Some(obj) = value.as_object_mut() else {
            return Err(PackageError::PackageJsonNotObject(json_path));
        };
        obj.insert(
            "versions".to_owned(),
            Value::Array(
                installed_versions
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect(),
            ),
        );
        obj.insert(
            "latest".to_owned(),
            Value::String(pick_latest(installed_versions)),
        );
        obj.remove("license");

        let bytes =
            serde_json::to_vec_pretty(&value).map_err(|source| PackageError::JsonParse {
                path: json_path.clone(),
                source,
            })?;
        atomic_write(&json_path, &bytes).await
    }

    /// Write `Package.json` for an install/update.
    ///
    /// The bucket's `Package.json` passes through verbatim, parsed as a
    /// raw `serde_json::Value` (not the typed `PackageJSON` struct) so
    /// unknown fields aren't accidentally dropped. Only `versions` and
    /// `latest` are modified to reflect what's actually on disk.
    async fn write_synced_package_json(
        &self,
        pkg_dir: &Path,
        installed_versions: &[String],
        pkg: Option<&RemotePackage>,
    ) -> PackageResult<()> {
        if installed_versions.is_empty() {
            return Err(PackageError::NoVersionsInstalled);
        }

        let json_path = pkg_dir.join("Package.json");

        // 1. If there's no on-disk Package.json yet, download a fresh copy
        //    from the bucket.
        if !json_path.exists() {
            let pkg = pkg.ok_or(PackageError::MissingPackageJson)?;
            let json_url = package_json_url(&pkg.name);
            self.download_to_file(&json_url, &json_path).await?;
        }

        // 2. Read the current Package.json as a generic JSON value so every
        //    field from the bucket survives the round-trip.
        let text = io_at(&json_path, std::fs::read_to_string(&json_path))?;
        let mut value: Value =
            serde_json::from_str(&text).map_err(|source| PackageError::JsonParse {
                path: json_path.clone(),
                source,
            })?;

        // 3. Set `versions` and `latest` to match what is on disk. Drop the
        //    `license` key. Set the `description` key from the Firestore
        //    metadata. Every other key the bucket Package.json contained
        //    (label, name, custom fields) is left as it was.
        let Some(obj) = value.as_object_mut() else {
            return Err(PackageError::PackageJsonNotObject(json_path));
        };
        obj.insert(
            "versions".to_owned(),
            Value::Array(
                installed_versions
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect(),
            ),
        );
        obj.insert(
            "latest".to_owned(),
            Value::String(pick_latest(installed_versions)),
        );
        obj.remove("license");
        if let Some(pkg) = pkg {
            obj.insert(
                "description".to_owned(),
                Value::String(pkg.description.clone()),
            );
        }

        // 4. Write back, atomically.
        let bytes =
            serde_json::to_vec_pretty(&value).map_err(|source| PackageError::JsonParse {
                path: json_path.clone(),
                source,
            })?;
        atomic_write(&json_path, &bytes).await
    }

    // -- Private: Firestore + Storage operations ---------------------------

    /// Fetch a package's Firestore document by exact name.
    async fn fetch_package(&self, name: &str) -> PackageResult<RemotePackage> {
        let url = format!("{FIRESTORE_BASE}/packages/{}", urlencode(name));
        let resp = self.client.get(&url).send().await?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(PackageError::NotFound(name.to_owned()));
        }
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(PackageError::Firebase {
                package: name.to_owned(),
                status: status.as_u16(),
                body: text,
            });
        }
        let doc: Value =
            serde_json::from_str(&text).map_err(|source| PackageError::MalformedFirestore {
                package: name.to_owned(),
                source,
            })?;
        parse_firestore_doc(&doc)
            .ok_or_else(|| PackageError::FirestoreMissingFields(name.to_owned()))
    }

    /// Download a binary or text asset to disk atomically. Buffers in
    /// memory, then writes via a `.partial` rename so a failed download
    /// never leaves a corrupt file in place.
    async fn download_to_file(&self, url: &str, dest: &Path) -> PackageResult<()> {
        let resp = self.client.get(url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(PackageError::Download {
                status: status.as_u16(),
                url: url.to_owned(),
            });
        }
        let bytes = resp.bytes().await?;
        if let Some(parent) = dest.parent() {
            io_at(parent, tokio::fs::create_dir_all(parent).await)?;
        }
        atomic_write(dest, &bytes).await
    }
}

// ---------------------------------------------------------------------------
// File-scope helpers
// ---------------------------------------------------------------------------

/// Locate the user's home directory and append `.Peko`. Returns `None` if
/// neither `HOME` (Unix) nor `USERPROFILE` (Windows) is set.
fn global_root() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("HOME") {
        return Some(PathBuf::from(home).join(".Peko"));
    }
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        return Some(PathBuf::from(profile).join(".Peko"));
    }
    None
}

/// Walk up from `start_dir` (inclusive) looking for a `.peko` directory.
fn find_local_root(start_dir: &Path) -> PackageResult<PathBuf> {
    let mut current = start_dir.to_path_buf();
    for _ in 0..=LOCAL_SEARCH_DEPTH {
        let candidate = current.join(".peko");
        if candidate.is_dir() {
            return Ok(candidate);
        }
        if !current.pop() {
            break;
        }
    }
    Err(PackageError::NoLocalRoot {
        start_dir: start_dir.to_path_buf(),
        depth: LOCAL_SEARCH_DEPTH,
    })
}

/// List subdirectories of a package folder (its version folders), sorted
/// ascending by semantic version.
fn scan_version_folders(pkg_dir: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(pkg_dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = rd
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    out.sort_by(|a, b| parse_version(a).cmp(&parse_version(b)));
    out
}

/// Pick the highest version. Versions are `vMAJOR.MINOR.PATCH`.
fn pick_latest(versions: &[String]) -> String {
    versions
        .iter()
        .max_by(|a, b| parse_version(a).cmp(&parse_version(b)))
        .cloned()
        .unwrap_or_default()
}

/// Parse `vMAJOR.MINOR.PATCH` into a comparable tuple. Unparseable parts
/// become 0, so malformed strings sort to the bottom rather than crashing.
fn parse_version(v: &str) -> (u32, u32, u32) {
    let stripped = v.strip_prefix('v').unwrap_or(v);
    let mut parts = stripped.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
}

/// Append `.partial` to a path, preserving the original extension.
fn with_partial(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".partial");
    s.into()
}

/// Write `bytes` to `dest` atomically via a `.partial` rename.
async fn atomic_write(dest: &Path, bytes: &[u8]) -> PackageResult<()> {
    let tmp = with_partial(dest);
    tokio::fs::write(&tmp, bytes)
        .await
        .map_err(|source| PackageError::Io {
            path: tmp.clone(),
            source,
        })?;
    tokio::fs::rename(&tmp, dest)
        .await
        .map_err(|source| PackageError::Io {
            path: dest.to_path_buf(),
            source,
        })?;
    Ok(())
}

/// Minimal URL path-segment encoder. Only "unreserved" RFC 3986 characters
/// pass through; everything else is `%`-encoded.
fn urlencode(s: &str) -> String {
    s.chars()
        .flat_map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => vec![c],
            _ => format!("%{:02X}", c as u32).chars().collect(),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Firestore document parsing
// ---------------------------------------------------------------------------

fn parse_firestore_doc(doc: &Value) -> Option<RemotePackage> {
    let fields = doc.get("fields")?;
    Some(RemotePackage {
        name: fs_string(fields, "name").unwrap_or_default(),
        description: fs_string(fields, "description").unwrap_or_default(),
        version: fs_string(fields, "latest")
            .or_else(|| fs_string(fields, "version"))
            .unwrap_or_default(),
        versions: fs_string_array(fields, "versions"),
        license: fs_string(fields, "license").unwrap_or_default(),
    })
}

fn fs_string(fields: &Value, key: &str) -> Option<String> {
    fields
        .get(key)?
        .get("stringValue")?
        .as_str()
        .map(str::to_owned)
}

fn fs_string_array(fields: &Value, key: &str) -> Vec<String> {
    fields
        .get(key)
        .and_then(|v| v.get("arrayValue"))
        .and_then(|v| v.get("values"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| {
                    v.get("stringValue")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Build a Firebase Storage download URL for a file at
/// `packages/{name}/{file}`.
///
/// Storage URLs are of the form:
/// `https://firebasestorage.googleapis.com/v0/b/{bucket}/o/{percent-encoded-path}?alt=media`
fn storage_url(name: &str, file: &str) -> String {
    let path = format!("packages/{name}/{file}");
    let encoded = urlencode(&path);
    format!("https://firebasestorage.googleapis.com/v0/b/{STORAGE_BUCKET}/o/{encoded}?alt=media")
}

/// URL for a package's `Package.json`. Stored at
/// `packages/{name}/Package.json`.
fn package_json_url(name: &str) -> String {
    storage_url(name, "Package.json")
}

/// URL for a specific version's zip. Stored at
/// `packages/{name}/{version}.zip`.
fn version_zip_url(name: &str, version: &str) -> String {
    storage_url(name, &format!("{version}.zip"))
}

// ---------------------------------------------------------------------------
// Zip extraction
// ---------------------------------------------------------------------------

/// Extract `zip_path` into `dest_dir` directly (no top-level folder
/// wrapping). Refuses entries whose canonical paths would escape
/// `dest_dir`.
fn extract_zip_to_dir(zip_path: &Path, dest_dir: &Path) -> io::Result<()> {
    let file = File::open(zip_path)?;
    let mut archive = ZipArchive::new(file).map_err(io::Error::other)?;

    std::fs::create_dir_all(dest_dir)?;
    let dest_canonical = std::fs::canonicalize(dest_dir)?;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(io::Error::other)?;
        let Some(relative) = entry.enclosed_name() else {
            continue;
        };
        if relative.as_os_str().is_empty() {
            continue;
        }

        let out_path = dest_dir.join(&relative);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
            let parent_canonical = std::fs::canonicalize(parent)?;
            if !parent_canonical.starts_with(&dest_canonical) {
                return Err(io::Error::other(format!(
                    "zip entry escapes destination: {}",
                    relative.display()
                )));
            }
        }

        if entry.is_dir() {
            std::fs::create_dir_all(&out_path)?;
        } else {
            let mut out = File::create(&out_path)?;
            io::copy(&mut entry, &mut out)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Some(mode) = entry.unix_mode() {
                    std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(mode))?;
                }
            }
        }
    }
    Ok(())
}
