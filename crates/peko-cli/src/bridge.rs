//! Native-bridge token client: mint a per-device token from the platform.
//!
//! `POST /api/bridge/token` on `app.pekoui.com` (the same bearer session as
//! `peko deploy`) returns a short-lived ES256 JWT the device presents
//! on the `/__peko__` WebSocket handshake. The CLI is App-Check-exempt and holds
//! the login session, so the pekoui runtime shells out to `peko bridge token` to
//! acquire and refresh these; `peko run` mints one for the dev loop.

use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;

/// One failure mode for a bridge-token request.
#[derive(Debug, Error)]
pub enum BridgeTokenError {
    /// The HTTP client could not be built.
    #[error("failed to build HTTP client: {0}")]
    HttpClient(#[source] reqwest::Error),

    /// A network operation failed before an HTTP status was seen.
    #[error("network error: {0}")]
    Network(#[source] reqwest::Error),

    /// The response body could not be decoded.
    #[error("could not decode the platform response: {0}")]
    Decode(#[source] reqwest::Error),

    /// The platform rejected the session (no/invalid session or App Check).
    #[error("the platform rejected this session")]
    Unauthorized,

    /// Forbidden with a user-facing explanation, usually an unverified email.
    #[error("{0}")]
    Forbidden(String),

    /// The app id is not one this account owns (or is unknown).
    #[error("the app was not found on your account")]
    NotFound,

    /// The request was rate limited.
    #[error("rate limited; try again shortly")]
    RateLimited,

    /// The bridge signing key is not configured on the platform yet.
    #[error("the native bridge is not available on the platform yet")]
    NotConfigured,

    /// The request was malformed; the string is the server's explanation.
    #[error("{0}")]
    BadRequest(String),

    /// Any other non-success status.
    #[error("bridge token request failed (HTTP {0})")]
    Http(u16),
}

/// A minted bridge token and the device it is bound to.
#[derive(Deserialize)]
pub struct BridgeToken {
    /// The ES256 JWT to present on the `/__peko__` handshake.
    pub token: String,
    /// The device id the token is bound to (assigned on first pair). Pass it back
    /// on refresh to keep the same identity.
    #[serde(rename = "deviceId")]
    pub device_id: String,
    /// Seconds until the token expires (typically 900).
    #[serde(rename = "expiresIn")]
    pub expires_in: u64,
}

/// A `{ "error" }` or `{ "message" }` explanation body.
#[derive(Deserialize)]
struct ErrorBody {
    error: Option<String>,
    message: Option<String>,
}

/// Request a bridge token for `app_id`. `device_id` is omitted on the first pair
/// (the platform assigns one) and passed on refresh to keep the same identity.
pub async fn request_bridge_token(
    base: &str,
    id_token: &str,
    app_id: &str,
    device_id: Option<&str>,
) -> Result<BridgeToken, BridgeTokenError> {
    let http = reqwest::Client::builder()
        .user_agent(concat!("peko-cli/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(BridgeTokenError::HttpClient)?;

    let mut body = serde_json::json!({ "appId": app_id });
    if let Some(device) = device_id {
        body["deviceId"] = serde_json::Value::String(device.to_owned());
    }

    let resp = http
        .post(format!("{base}/api/bridge/token"))
        .bearer_auth(id_token)
        .json(&body)
        .send()
        .await
        .map_err(BridgeTokenError::Network)?;

    match resp.status() {
        s if s.is_success() => {}
        reqwest::StatusCode::UNAUTHORIZED => return Err(BridgeTokenError::Unauthorized),
        reqwest::StatusCode::FORBIDDEN => {
            return Err(BridgeTokenError::Forbidden(error_body(resp).await));
        }
        reqwest::StatusCode::NOT_FOUND => return Err(BridgeTokenError::NotFound),
        reqwest::StatusCode::TOO_MANY_REQUESTS => return Err(BridgeTokenError::RateLimited),
        reqwest::StatusCode::SERVICE_UNAVAILABLE => return Err(BridgeTokenError::NotConfigured),
        reqwest::StatusCode::BAD_REQUEST => {
            return Err(BridgeTokenError::BadRequest(error_body(resp).await));
        }
        other => return Err(BridgeTokenError::Http(other.as_u16())),
    }

    resp.json::<BridgeToken>()
        .await
        .map_err(BridgeTokenError::Decode)
}

/// Read a server explanation from an error response, falling back to a generic
/// message.
async fn error_body(resp: reqwest::Response) -> String {
    resp.json::<ErrorBody>()
        .await
        .ok()
        .and_then(|b| b.error.or(b.message))
        .unwrap_or_else(|| "the platform rejected the request".to_owned())
}
