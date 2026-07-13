//! Uploading a packed `.pkpkg` to the platform publish endpoints.
//!
//! Publishing is a three-step handshake against the platform: request an upload
//! slot, PUT the raw container bytes to the returned presigned URL, then signal
//! completion. The server reads the package name, version, dependencies, and
//! platforms from the embedded `peko.toml`, validates them, computes the
//! checksum, rejects a duplicate version, and queues the version for admin
//! review. Authentication is the bearer ID token from `peko login`.

use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;

/// The maximum container size the platform accepts.
const MAX_PACKAGE_BYTES: usize = 50 * 1024 * 1024;

/// One failure mode for a publish operation.
#[derive(Debug, Error)]
pub enum PublishError {
    /// The container exceeds the platform size limit.
    #[error("the package is {size} bytes, over the {max} byte limit")]
    TooLarge { size: usize, max: usize },

    /// The HTTP client could not be built.
    #[error("failed to build HTTP client: {0}")]
    HttpClient(#[source] reqwest::Error),

    /// A network operation failed before an HTTP status was seen.
    #[error("network error: {0}")]
    Network(#[source] reqwest::Error),

    /// A response body could not be decoded.
    #[error("could not decode the platform response: {0}")]
    Decode(#[source] reqwest::Error),

    /// The platform rejected the session token.
    #[error("the platform rejected this session")]
    Unauthorized,

    /// The platform forbade the operation with a user-facing explanation, e.g.
    /// an account whose email is not yet verified. The string is the server's
    /// own message, already suitable to show the user.
    #[error("{0}")]
    Forbidden(String),

    /// Requesting the upload slot failed.
    #[error("could not start the upload (HTTP {0})")]
    Start(u16),

    /// The upload to the presigned URL failed.
    #[error("could not upload the package (HTTP {0})")]
    Upload(u16),

    /// The platform declined the package with an explanation.
    #[error("the platform rejected the package: {0}")]
    Rejected(String),

    /// Completing the upload failed for another reason.
    #[error("could not complete the upload (HTTP {0})")]
    Complete(u16),
}

/// The response from `POST /api/publish/start`.
#[derive(Deserialize)]
struct StartResponse {
    #[serde(rename = "requestId")]
    request_id: String,
    #[serde(rename = "uploadUrl")]
    upload_url: String,
}

/// The response from `POST /api/publish/complete`.
#[derive(Deserialize)]
struct CompleteResponse {
    status: String,
    name: String,
    version: String,
}

/// The `{ "error" }` body a `400` complete response carries.
#[derive(Deserialize)]
struct ErrorResponse {
    error: String,
}

/// The result of a completed publish handshake.
pub struct PublishOutcome {
    /// The review status the server assigned, such as `pending`.
    pub status: String,
    /// The package name the server read from the embedded manifest.
    pub name: String,
    /// The package version the server read from the embedded manifest.
    pub version: String,
}

/// Build the HTTP client used for the publish handshake. The overall timeout is
/// generous to allow a large container to upload.
fn http_client() -> Result<reqwest::Client, PublishError> {
    reqwest::Client::builder()
        .user_agent(concat!("peko-cli/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(300))
        .build()
        .map_err(PublishError::HttpClient)
}

/// Read the `{ "error" }` explanation from a 403 response, falling back to a
/// generic message. A 403 here means the account is not allowed to publish,
/// most often because its email is not verified yet.
async fn forbidden_message(response: reqwest::Response) -> String {
    response
        .json::<ErrorResponse>()
        .await
        .map(|e| e.error)
        .unwrap_or_else(|_| "your account is not permitted to publish packages".to_owned())
}

/// Upload `bytes` to `base` as an authenticated publish. `id_token` is the
/// bearer token from `peko login`.
pub async fn publish(
    base: &str,
    id_token: &str,
    bytes: &[u8],
) -> Result<PublishOutcome, PublishError> {
    if bytes.len() > MAX_PACKAGE_BYTES {
        return Err(PublishError::TooLarge {
            size: bytes.len(),
            max: MAX_PACKAGE_BYTES,
        });
    }

    let http = http_client()?;

    // 1. Request an upload slot.
    let start_url = format!("{base}/api/publish/start");
    let start = http
        .post(&start_url)
        .bearer_auth(id_token)
        .send()
        .await
        .map_err(PublishError::Network)?;
    if start.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(PublishError::Unauthorized);
    }
    if start.status() == reqwest::StatusCode::FORBIDDEN {
        return Err(PublishError::Forbidden(forbidden_message(start).await));
    }
    if !start.status().is_success() {
        return Err(PublishError::Start(start.status().as_u16()));
    }
    let start: StartResponse = start.json().await.map_err(PublishError::Decode)?;

    // 2. Upload the raw container to the presigned URL. This request goes to
    // storage directly and carries no platform bearer token.
    let upload = http
        .put(&start.upload_url)
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        .body(bytes.to_vec())
        .send()
        .await
        .map_err(PublishError::Network)?;
    if !upload.status().is_success() {
        return Err(PublishError::Upload(upload.status().as_u16()));
    }

    // 3. Signal completion. The server validates the staged blob and queues it.
    let complete_url = format!("{base}/api/publish/complete");
    let complete = http
        .post(&complete_url)
        .bearer_auth(id_token)
        .json(&serde_json::json!({ "requestId": start.request_id }))
        .send()
        .await
        .map_err(PublishError::Network)?;
    if complete.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(PublishError::Unauthorized);
    }
    if complete.status() == reqwest::StatusCode::FORBIDDEN {
        return Err(PublishError::Forbidden(forbidden_message(complete).await));
    }
    if complete.status() == reqwest::StatusCode::BAD_REQUEST {
        let message = complete
            .json::<ErrorResponse>()
            .await
            .map(|e| e.error)
            .unwrap_or_else(|_| "bad request".to_owned());
        return Err(PublishError::Rejected(message));
    }
    if !complete.status().is_success() {
        return Err(PublishError::Complete(complete.status().as_u16()));
    }

    let done: CompleteResponse = complete.json().await.map_err(PublishError::Decode)?;
    Ok(PublishOutcome {
        status: done.status,
        name: done.name,
        version: done.version,
    })
}
