//! Gated (proprietary, entitlement-required) package download.
//!
//! A public package is a source `.pkpkg` served from the CDN. A **gated**
//! package (e.g. `pekoshots`) instead ships a single **all-platforms** prebuilt
//! bundle per toolchain that the platform serves only to an authenticated, paid
//! account. The gate is enforced server-side; the CLI simply authenticates with
//! the login session and surfaces the outcome. `peko add`/`install` use this
//! path transparently — no extra flags — when the public download is
//! unavailable and the user is signed in.
//!
//! Protocol (platform API):
//! ```text
//! GET {base}/api/packages/{name}/download?toolchain=<version>
//!     Authorization: Bearer <id_token>
//! 200 -> { url, sha256, size, matchedPattern, ... }  then GET url (no auth)
//! 401 not signed in · 403 not paid · 400 missing toolchain · 404 no-bundle · 429
//! ```
//! The `toolchain` matches (exact or wildcard) the version a bundle was uploaded
//! under. The signed `url` is short-lived and self-authenticating, so the bundle
//! bytes are fetched without the bearer header and verified against `sha256`.
//! The signed `url` is short-lived and self-authenticating, so the bundle bytes
//! are fetched without the bearer header and verified against `sha256`.

use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::RegistryError;

/// Why a gated download did not yield a bundle.
pub enum GatedError {
    /// 401 — no valid session.
    NotSignedIn,
    /// 403 — signed in but not on a paying tier.
    NotPaid { tier: Option<String> },
    /// 404 "No prebuilt bundle for toolchain …" — no gated bundle serves this
    /// toolchain (the package may not be gated, or is not built for this CLI).
    NoBundle { toolchain: String },
    /// 429 — rate limited.
    RateLimited,
    /// 400 — the request was rejected (missing/invalid params).
    BadRequest(String),
    /// The downloaded bytes did not match the advertised sha256.
    Checksum,
    /// A non-handled HTTP status.
    Http(u16),
    /// A transport error.
    Network(reqwest::Error),
}

impl GatedError {
    /// Convert a gated failure into a `RegistryError` with a user-facing message.
    pub fn into_registry(self, package: &str) -> RegistryError {
        let package = package.to_owned();
        match self {
            GatedError::NotSignedIn => RegistryError::SignInRequired { package },
            GatedError::NotPaid { tier } => RegistryError::EntitlementRequired {
                package,
                tier: tier.unwrap_or_default(),
            },
            GatedError::NoBundle { toolchain } => {
                RegistryError::NoPrebuiltBundle { package, toolchain }
            }
            GatedError::RateLimited => RegistryError::RateLimited { package },
            GatedError::BadRequest(detail) => RegistryError::GatedRequest { package, detail },
            GatedError::Checksum => RegistryError::GatedRequest {
                package,
                detail: "downloaded bundle failed its sha256 check".to_owned(),
            },
            GatedError::Http(status) => RegistryError::GatedRequest {
                package,
                detail: format!("unexpected status {status}"),
            },
            GatedError::Network(error) => RegistryError::Network(error),
        }
    }
}

/// The compiler/toolchain version string the CLI reports. A gated bundle is one
/// all-platforms file per toolchain, and the platform matches this against the
/// (possibly wildcard) toolchain a bundle was uploaded under.
pub fn toolchain_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[derive(Deserialize)]
struct DownloadInfo {
    url: String,
    #[serde(default)]
    sha256: Option<String>,
}

/// The public metadata the platform serves for a gated package and a given
/// toolchain. This is everything the resolver needs to pin a lockfile entry
/// without downloading (or being signed in): the concrete `toolchain` a build
/// matched and its `sha256`. Only the eventual `/download` is gated.
#[derive(Deserialize)]
pub struct GatedMeta {
    /// The package name.
    #[allow(dead_code)]
    pub name: String,
    /// Whether the package is proprietary/entitlement-gated.
    #[serde(default)]
    pub gated: bool,
    /// The entitlement tier the download requires (e.g. `"paid"`).
    #[serde(default)]
    pub entitlement: Option<String>,
    /// The concrete toolchain version this build was uploaded under. This is the
    /// version pinned in the lockfile.
    #[serde(default)]
    pub toolchain: Option<String>,
    /// The bundle checksum, matching the one the `/download` response returns.
    #[serde(default)]
    pub sha256: Option<String>,
    /// Whether a build exists for the requested toolchain.
    #[serde(default)]
    pub available: bool,
}

/// Fetch the public metadata for `name` at `toolchain`. Requires no auth.
///
/// Returns `Ok(None)` when the platform has no gated build for the package at
/// this toolchain (404) or is unreachable, so the caller falls back to the
/// public-index resolution error. `Ok(Some(meta))` means the package is known
/// to the platform; the caller inspects `gated`/`available` to decide.
pub async fn fetch_meta(
    base: &str,
    name: &str,
    toolchain: &str,
) -> Result<Option<GatedMeta>, GatedError> {
    let http = reqwest::Client::builder()
        .user_agent(concat!("peko-cli/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(GatedError::Network)?;

    let url = format!("{base}/api/packages/{name}/meta?toolchain={toolchain}");
    let response = match http.get(&url).send().await {
        Ok(response) => response,
        // Offline or the platform is unreachable: treat as "no gated meta" so
        // resolution surfaces the original public-index error instead.
        Err(_) => return Ok(None),
    };

    match response.status().as_u16() {
        200 => Ok(Some(response.json().await.map_err(GatedError::Network)?)),
        // 404 -> no build for this package/toolchain (or not a gated package).
        404 => Ok(None),
        429 => Err(GatedError::RateLimited),
        400 => Err(GatedError::BadRequest(
            response.text().await.unwrap_or_default(),
        )),
        other => Err(GatedError::Http(other)),
    }
}

/// Download and verify a gated package's single all-platforms bundle for
/// `toolchain`. The platform matches `toolchain` against the (possibly wildcard)
/// version a bundle was uploaded under and returns a short-lived signed URL.
pub async fn download_bundle(
    base: &str,
    id_token: &str,
    name: &str,
    toolchain: &str,
) -> Result<Vec<u8>, GatedError> {
    let http = reqwest::Client::builder()
        .user_agent(concat!("peko-cli/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(GatedError::Network)?;

    let url = format!("{base}/api/packages/{name}/download?toolchain={toolchain}");
    let response = http
        .get(&url)
        .bearer_auth(id_token)
        .send()
        .await
        .map_err(GatedError::Network)?;

    match response.status().as_u16() {
        200 => {}
        401 => return Err(GatedError::NotSignedIn),
        403 => {
            let tier = response
                .json::<serde_json::Value>()
                .await
                .ok()
                .and_then(|body| body.get("tier").and_then(|t| t.as_str()).map(str::to_owned));
            return Err(GatedError::NotPaid { tier });
        }
        400 => {
            return Err(GatedError::BadRequest(
                response.text().await.unwrap_or_default(),
            ));
        }
        404 => {
            return Err(GatedError::NoBundle {
                toolchain: toolchain.to_owned(),
            });
        }
        429 => return Err(GatedError::RateLimited),
        other => return Err(GatedError::Http(other)),
    }

    let info: DownloadInfo = response.json().await.map_err(GatedError::Network)?;

    // The signed URL is self-authenticating; do NOT attach the bearer token.
    let blob = http
        .get(&info.url)
        .send()
        .await
        .map_err(GatedError::Network)?;
    if !blob.status().is_success() {
        return Err(GatedError::Http(blob.status().as_u16()));
    }
    let bytes = blob.bytes().await.map_err(GatedError::Network)?.to_vec();

    if let Some(expected) = info.sha256.filter(|value| !value.is_empty()) {
        let actual = format!("{:x}", Sha256::digest(&bytes));
        let expected = expected.trim_start_matches("sha256:");
        if !actual.eq_ignore_ascii_case(expected) {
            return Err(GatedError::Checksum);
        }
    }
    Ok(bytes)
}
