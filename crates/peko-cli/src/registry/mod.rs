//! The registry client, resolver, and `.pkpkg` packing for the cli.
//!
//! Packages are distributed as immutable `.pkpkg` source bundles addressed by a
//! static JSON-lines index. Resolution reads only the index, selects exact
//! versions, writes `peko.lock`, then downloads and unpacks each blob into the
//! shared source cache, where the compiler resolves imports against it.
//!
//! The network transport is real, but the registry base URL is a placeholder
//! until the web platform is live. Calls fall back to the on-disk cache when
//! the base URL is unreachable.

pub mod cache;
pub mod client;
pub mod gated;
pub mod index;
pub mod install;
pub mod pack;
pub mod publish;
pub mod resolver;
pub mod verify;

use std::path::PathBuf;

use thiserror::Error;

pub use cache::Cache;
pub use client::RegistryClient;
pub use index::IndexEntry;
pub use resolver::{ResolvedPackage, Resolver};
pub use verify::{PackageReport, verify};

/// One failure mode for a registry operation.
#[derive(Debug, Error)]
pub enum RegistryError {
    /// An on-disk operation failed.
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// A manifest or lockfile operation failed.
    #[error(transparent)]
    Config(#[from] peko_core::config::ConfigError),

    /// A `.pkpkg` container could not be read.
    #[error("invalid package container: {0}")]
    Container(#[source] peko_core::config::ContainerError),

    /// An index line did not parse.
    #[error("malformed index at line {line}: {source}")]
    IndexParse {
        line: usize,
        #[source]
        source: serde_json::Error,
    },

    /// An index entry could not be serialized.
    #[error("could not serialize index entry: {0}")]
    IndexSerialize(#[source] serde_json::Error),

    /// The HTTP client could not be built.
    #[error("failed to build HTTP client: {0}")]
    HttpClient(#[source] reqwest::Error),

    /// A network operation failed before an HTTP status was seen.
    #[error("network error: {0}")]
    Network(#[source] reqwest::Error),

    /// A request returned a non-success HTTP status.
    #[error("registry request failed ({status}): {url}")]
    Http { status: u16, url: String },

    /// The platform refused to serve a proprietary, entitlement-gated package.
    #[error(
        "access to `{package}` was denied: it is a proprietary package that requires an active paid Peko account. Run `peko login`, and make sure your subscription is active"
    )]
    Forbidden { package: String },

    /// A gated package requires signing in.
    #[error("`{package}` is a proprietary package. Run `peko login` to download it")]
    SignInRequired { package: String },

    /// A gated package requires a paid account.
    #[error(
        "`{package}` requires an active paid Peko account (tier `{tier}`). Upgrade your plan to download it"
    )]
    EntitlementRequired { package: String, tier: String },

    /// No gated bundle serves the requested toolchain (the package may not be
    /// gated, or is not built for this CLI version).
    #[error(
        "no prebuilt build of `{package}` available for toolchain `{toolchain}`. It may not be a gated package, or not built for your CLI version"
    )]
    NoPrebuiltBundle { package: String, toolchain: String },

    /// The gated download was rate-limited.
    #[error("download of `{package}` was rate-limited; try again in a moment")]
    RateLimited { package: String },

    /// The gated download request was rejected.
    #[error("could not download gated package `{package}`: {detail}")]
    GatedRequest { package: String, detail: String },

    /// A package is not published.
    #[error("package `{0}` not found in the registry")]
    NotFound(String),

    /// No published version satisfies a requirement.
    #[error("no version of `{package}` satisfies `{requirement}`. Available: {available}")]
    NoMatchingVersion {
        package: String,
        requirement: String,
        available: String,
    },

    /// A downloaded blob's checksum did not match the index.
    #[error("checksum mismatch for {package}@{version}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        package: String,
        version: String,
        expected: String,
        actual: String,
    },

    /// Two dependencies require incompatible versions of one package.
    #[error("dependency conflict on `{package}`: {detail}")]
    Conflict { package: String, detail: String },

    /// A version string was not valid semver.
    #[error("invalid version `{version}` for `{package}`")]
    InvalidVersion { package: String, version: String },

    /// A local path dependency does not point at a valid project.
    #[error("path dependency `{name}` at {path} is not a valid package")]
    InvalidPathDependency { name: String, path: PathBuf },
}
