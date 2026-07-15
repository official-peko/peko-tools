//! The registry client: index fetch and blob download over the CDN.
//!
//! The base URL comes from `PEKO_REGISTRY_URL`, defaulting to a placeholder
//! until the web platform is live. The index URL layout and the blob URL layout
//! are provisional and reconcile with the server once it exists. Every fetch
//! falls back to the on-disk cache when the network is unreachable.

use std::path::Path;
use std::time::Duration;

use super::RegistryError;
use super::cache::Cache;
use super::index::{self, IndexEntry};
use super::pack;

/// The registry base URL used until the platform ships a real one.
// TODO(platform): point PEKO_REGISTRY_URL at the live R2/CDN base; this
// The live registry read surface: a static index and blob store behind the CDN.
const DEFAULT_REGISTRY_URL: &str = "https://registry.pekoui.com";

/// The environment variable that overrides the registry base URL.
const REGISTRY_URL_ENV: &str = "PEKO_REGISTRY_URL";

/// Resolve the registry base URL from the environment or the default.
fn registry_base() -> String {
    std::env::var(REGISTRY_URL_ENV).unwrap_or_else(|_| DEFAULT_REGISTRY_URL.to_owned())
}

/// A client for the static index and the blob store.
pub struct RegistryClient {
    base_url: String,
    /// The platform API base (e.g. `https://app.pekoui.com`), used for the
    /// gated `/api/packages/*` endpoints. Distinct from `base_url`, which is
    /// the static index/blob CDN.
    platform_base: String,
    http: reqwest::Client,
    cache: Cache,
    /// The session bearer token, when the cli is logged in. Sent on requests so
    /// the platform can enforce entitlement: proprietary packages (e.g.
    /// pekoshots) are served only to an authenticated, paid account and return
    /// `403 Forbidden` otherwise. Public packages ignore it.
    id_token: Option<String>,
}

impl RegistryClient {
    /// Build a client over the given cache.
    pub fn new(cache: Cache) -> Result<RegistryClient, RegistryError> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("peko-cli/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(RegistryError::HttpClient)?;

        Ok(RegistryClient {
            base_url: registry_base(),
            platform_base: crate::auth::platform_base(None),
            http,
            cache,
            id_token: None,
        })
    }

    /// Fetch a gated package's public metadata for this CLI's toolchain.
    ///
    /// Used by the resolver when the public index has no entry for a package:
    /// the package may be a proprietary, toolchain-pinned one served through the
    /// platform. `Ok(None)` means the platform has no such build (so the caller
    /// keeps the original not-found error); `Ok(Some(meta))` carries the
    /// concrete toolchain and checksum to lock. Requires no auth.
    pub async fn fetch_gated_meta(
        &self,
        name: &str,
    ) -> Result<Option<super::gated::GatedMeta>, RegistryError> {
        let toolchain = super::gated::toolchain_version();
        match super::gated::fetch_meta(&self.platform_base, name, toolchain).await {
            Ok(meta) => Ok(meta),
            // A transport-level failure is treated as "no gated meta" so the
            // resolver reports the public-index error rather than a network one.
            Err(super::gated::GatedError::Network(_)) => Ok(None),
            Err(other) => Err(other.into_registry(name)),
        }
    }

    /// Attach a session bearer token so entitlement-gated packages can be
    /// fetched. `None` leaves requests anonymous (public packages only).
    pub fn set_id_token(&mut self, token: Option<String>) {
        self.id_token = token;
    }

    /// The cache this client downloads into.
    pub fn cache(&self) -> &Cache {
        &self.cache
    }

    /// Fetch a package's index entries, caching the body on success and
    /// falling back to the cached body when the network is unreachable.
    pub async fn fetch_index(&self, name: &str) -> Result<Vec<IndexEntry>, RegistryError> {
        // The index file is served per package under a lowercased name.
        let url = format!("{}/index/{}", self.base_url, name.to_lowercase());

        match self.http.get(&url).send().await {
            Ok(response) if response.status().is_success() => {
                let body = response.text().await.map_err(RegistryError::Network)?;
                self.write_index_cache(name, &body)?;
                index::parse_index(&body)
            }
            Ok(response) if response.status() == reqwest::StatusCode::NOT_FOUND => {
                Err(RegistryError::NotFound(name.to_owned()))
            }
            Ok(response) => Err(RegistryError::Http {
                status: response.status().as_u16(),
                url,
            }),
            Err(network) => match self.read_index_cache(name)? {
                Some(body) => index::parse_index(&body),
                None => Err(RegistryError::Network(network)),
            },
        }
    }

    /// Download a version's blob, verify its checksum, and store it in the
    /// cache. A cached blob with a matching checksum is reused without a
    /// network request.
    pub async fn download_blob(
        &self,
        name: &str,
        version: &str,
        expected_checksum: &str,
    ) -> Result<Vec<u8>, RegistryError> {
        let blob_path = self.cache.blob_path(name, version);
        if blob_path.is_file() {
            let bytes = read_file(&blob_path)?;
            if pack::checksum(&bytes) == expected_checksum {
                return Ok(bytes);
            }
        }

        // Immutable blobs are served under packages/<name>/<name>-<version>.pkpkg.
        let url = format!("{}/packages/{name}/{name}-{version}.pkpkg", self.base_url);
        let mut request = self.http.get(&url);
        if let Some(token) = &self.id_token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.map_err(RegistryError::Network)?;
        // The platform gates proprietary packages: an unauthenticated or
        // unentitled request is refused so the source-hidden package is never
        // served to a non-paying account.
        if response.status() == reqwest::StatusCode::FORBIDDEN
            || response.status() == reqwest::StatusCode::UNAUTHORIZED
        {
            return Err(RegistryError::Forbidden {
                package: name.to_owned(),
            });
        }
        if !response.status().is_success() {
            return Err(RegistryError::Http {
                status: response.status().as_u16(),
                url,
            });
        }

        let bytes = response
            .bytes()
            .await
            .map_err(RegistryError::Network)?
            .to_vec();
        let actual = pack::checksum(&bytes);
        if actual != expected_checksum {
            return Err(RegistryError::ChecksumMismatch {
                package: name.to_owned(),
                version: version.to_owned(),
                expected: expected_checksum.to_owned(),
                actual,
            });
        }

        write_file(&blob_path, &bytes)?;
        Ok(bytes)
    }

    /// Read a package's cached index body, if present.
    fn read_index_cache(&self, name: &str) -> Result<Option<String>, RegistryError> {
        let path = self.cache.index_path(name);
        if !path.is_file() {
            return Ok(None);
        }
        std::fs::read_to_string(&path)
            .map(Some)
            .map_err(|source| RegistryError::Io { path, source })
    }

    /// Store a package's index body in the cache.
    fn write_index_cache(&self, name: &str, body: &str) -> Result<(), RegistryError> {
        let path = self.cache.index_path(name);
        write_file(&path, body.as_bytes())
    }
}

/// Read a file into memory.
fn read_file(path: &Path) -> Result<Vec<u8>, RegistryError> {
    std::fs::read(path).map_err(|source| RegistryError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Write a file, creating parent directories.
fn write_file(path: &Path, bytes: &[u8]) -> Result<(), RegistryError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| RegistryError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(path, bytes).map_err(|source| RegistryError::Io {
        path: path.to_path_buf(),
        source,
    })
}
