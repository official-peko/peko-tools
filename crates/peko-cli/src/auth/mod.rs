//! CLI authentication: the `peko login` loopback device flow and the stored
//! Firebase session that authenticated platform calls reuse.
//!
//! Login runs a localhost callback flow. The browser signs in on the platform,
//! which redirects to a short-lived loopback server with a single-use code. No
//! token ever appears in a browser-visible URL. The code redeems server to
//! server for a Firebase custom token, and the CLI then establishes its own
//! Firebase session. The refresh token, uid, and Web API key persist in the
//! operating system keychain. Later calls exchange the refresh token for a
//! fresh ID token and send it as a bearer token.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::cli::reporting::Reporter;
use crate::keychain::{self, KeychainError};

/// The platform base URL used when nothing overrides it.
const DEFAULT_PLATFORM_URL: &str = "https://app.pekoui.com";

/// The environment variable that overrides the platform base URL.
const PLATFORM_URL_ENV: &str = "PEKO_PLATFORM_URL";

/// The keychain service and account under which the session is stored.
const KEYCHAIN_SERVICE: &str = "dev.peko.auth";
const KEYCHAIN_ACCOUNT: &str = "session";

/// How long the loopback server waits for the browser callback.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

/// One failure mode for an authentication operation.
#[derive(Debug, Error)]
pub enum AuthError {
    /// The loopback server could not accept or serve the callback.
    #[error("loopback server error: {0}")]
    Loopback(#[source] std::io::Error),

    /// The browser callback did not arrive within the timeout.
    #[error("timed out waiting for the browser sign-in")]
    Timeout,

    /// The user cancelled the sign-in.
    #[error("sign-in was cancelled ({0})")]
    Denied(String),

    /// The callback `state` did not match the value the CLI generated.
    #[error("state mismatch on the sign-in callback")]
    StateMismatch,

    /// The callback carried no authorization code.
    #[error("the sign-in callback carried no code")]
    MissingCode,

    /// The callback request could not be parsed.
    #[error("malformed sign-in callback request")]
    BadRequest,

    /// The HTTP client could not be built.
    #[error("failed to build HTTP client: {0}")]
    HttpClient(#[source] reqwest::Error),

    /// A network operation failed before an HTTP status was seen.
    #[error("network error: {0}")]
    Network(#[source] reqwest::Error),

    /// A response body could not be decoded.
    #[error("could not decode the platform response: {0}")]
    Decode(#[source] reqwest::Error),

    /// Redeeming the one-time code failed.
    #[error("could not redeem the sign-in code (HTTP {0})")]
    Redeem(u16),

    /// Establishing the Firebase session failed.
    #[error("could not establish the session (HTTP {0})")]
    Firebase(u16),

    /// Refreshing the ID token failed.
    #[error("could not refresh the session token (HTTP {0})")]
    Refresh(u16),

    /// The whoami call failed.
    #[error("could not read identity (HTTP {0})")]
    Whoami(u16),

    /// The stored session was rejected by the platform.
    #[error("the session is expired or revoked")]
    Unauthorized,

    /// The session could not be serialized for storage.
    #[error("could not serialize the session: {0}")]
    Serialize(#[source] serde_json::Error),

    /// A keychain operation failed.
    #[error(transparent)]
    Keychain(#[from] KeychainError),
}

/// Resolve the platform base URL. An explicit override wins, then the
/// environment variable, then the default. A trailing slash is trimmed so
/// path joins stay clean.
pub fn platform_base(override_url: Option<String>) -> String {
    let raw = override_url
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var(PLATFORM_URL_ENV).ok())
        .unwrap_or_else(|| DEFAULT_PLATFORM_URL.to_owned());
    raw.trim_end_matches('/').to_owned()
}

/// The stored CLI session. The refresh token establishes fresh ID tokens; the
/// Web API key identifies the Firebase project the tokens belong to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub refresh_token: String,
    pub uid: String,
    pub api_key: String,
}

impl Session {
    /// Load the session from the keychain, or `None` when not signed in.
    pub fn load() -> Option<Session> {
        let raw = keychain::get(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)?;
        serde_json::from_str(&raw).ok()
    }

    /// Persist the session to the keychain, replacing any existing value.
    pub fn store(&self) -> Result<(), AuthError> {
        let raw = serde_json::to_string(self).map_err(AuthError::Serialize)?;
        keychain::set(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT, &raw)?;
        Ok(())
    }

    /// Remove the session from the keychain. A missing session is not an error.
    pub fn clear() -> Result<(), AuthError> {
        keychain::delete(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)?;
        Ok(())
    }
}

/// The identity behind a session, as reported by the platform.
#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub uid: String,
    pub email: Option<String>,
    #[serde(rename = "displayName")]
    pub display_name: Option<String>,
    pub role: Option<String>,
    pub tier: Option<String>,
}

/// Run the login flow against `base` and return the established session. The
/// reporter surfaces the browser URL and the waiting state.
pub async fn login(base: &str, reporter: &Reporter) -> Result<Session, AuthError> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(AuthError::Loopback)?;
    let port = listener.local_addr().map_err(AuthError::Loopback)?.port();
    let state = random_state();

    let auth_url = format!("{base}/cli/auth?port={port}&state={state}");
    reporter.status("Login", format!("opening {auth_url}"));
    if open::that(&auth_url).is_err() {
        reporter.help(format!(
            "could not open a browser automatically; visit this URL to continue:\n{auth_url}"
        ));
    }
    reporter.status("Waiting", "complete the sign-in in the browser");

    let code = wait_for_callback(listener, &state, CALLBACK_TIMEOUT).await?;

    let http = http_client()?;
    let redeemed = redeem_code(&http, base, &code).await?;
    let firebase =
        sign_in_with_custom_token(&http, &redeemed.api_key, &redeemed.custom_token).await?;

    let session = Session {
        refresh_token: firebase.refresh_token,
        uid: redeemed.uid,
        api_key: redeemed.api_key,
    };
    session.store()?;
    Ok(session)
}

/// Exchange the session's refresh token for a fresh ID token. Authenticated
/// platform calls send the result as a bearer token.
pub async fn fresh_id_token(session: &Session) -> Result<String, AuthError> {
    let http = http_client()?;
    let url = format!(
        "https://securetoken.googleapis.com/v1/token?key={}",
        session.api_key
    );
    let response = http
        .post(&url)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", session.refresh_token.as_str()),
        ])
        .send()
        .await
        .map_err(AuthError::Network)?;
    if response.status() == reqwest::StatusCode::UNAUTHORIZED
        || response.status() == reqwest::StatusCode::BAD_REQUEST
    {
        return Err(AuthError::Unauthorized);
    }
    if !response.status().is_success() {
        return Err(AuthError::Refresh(response.status().as_u16()));
    }
    let parsed: RefreshResponse = response.json().await.map_err(AuthError::Decode)?;
    Ok(parsed.id_token)
}

/// Verify the session and read the account identity from the platform.
pub async fn current_user(base: &str, session: &Session) -> Result<User, AuthError> {
    let id_token = fresh_id_token(session).await?;
    let http = http_client()?;
    let url = format!("{base}/api/cli/whoami");
    let response = http
        .get(&url)
        .bearer_auth(&id_token)
        .send()
        .await
        .map_err(AuthError::Network)?;
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(AuthError::Unauthorized);
    }
    if !response.status().is_success() {
        return Err(AuthError::Whoami(response.status().as_u16()));
    }
    let parsed: WhoamiResponse = response.json().await.map_err(AuthError::Decode)?;
    Ok(parsed.user)
}

// ---------------------------------------------------------------------------
// HTTP contract types
// ---------------------------------------------------------------------------

/// The response from `POST /api/cli/token`.
#[derive(Deserialize)]
struct TokenResponse {
    #[serde(rename = "customToken")]
    custom_token: String,
    uid: String,
    #[serde(rename = "apiKey")]
    api_key: String,
}

/// The relevant fields of a Firebase `signInWithCustomToken` response.
#[derive(Deserialize)]
struct FirebaseSession {
    #[serde(rename = "refreshToken")]
    refresh_token: String,
}

/// The relevant field of a Firebase secure-token refresh response.
#[derive(Deserialize)]
struct RefreshResponse {
    id_token: String,
}

/// The response from `GET /api/cli/whoami`.
#[derive(Deserialize)]
struct WhoamiResponse {
    user: User,
}

// ---------------------------------------------------------------------------
// HTTP calls
// ---------------------------------------------------------------------------

/// Build the HTTP client used for every authentication call.
fn http_client() -> Result<reqwest::Client, AuthError> {
    reqwest::Client::builder()
        .user_agent(concat!("peko-cli/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(AuthError::HttpClient)
}

/// Redeem the one-time code server to server.
async fn redeem_code(
    http: &reqwest::Client,
    base: &str,
    code: &str,
) -> Result<TokenResponse, AuthError> {
    let url = format!("{base}/api/cli/token");
    let response = http
        .post(&url)
        .json(&serde_json::json!({ "code": code }))
        .send()
        .await
        .map_err(AuthError::Network)?;
    if !response.status().is_success() {
        return Err(AuthError::Redeem(response.status().as_u16()));
    }
    response.json().await.map_err(AuthError::Decode)
}

/// Establish the CLI's own Firebase session from the custom token.
async fn sign_in_with_custom_token(
    http: &reqwest::Client,
    api_key: &str,
    custom_token: &str,
) -> Result<FirebaseSession, AuthError> {
    let url = format!(
        "https://identitytoolkit.googleapis.com/v1/accounts:signInWithCustomToken?key={api_key}"
    );
    let response = http
        .post(&url)
        .json(&serde_json::json!({ "token": custom_token, "returnSecureToken": true }))
        .send()
        .await
        .map_err(AuthError::Network)?;
    if !response.status().is_success() {
        return Err(AuthError::Firebase(response.status().as_u16()));
    }
    response.json().await.map_err(AuthError::Decode)
}

// ---------------------------------------------------------------------------
// Loopback callback server
// ---------------------------------------------------------------------------

/// A page shown in the browser after a successful callback.
const SUCCESS_PAGE: &str = "<!doctype html><title>Peko</title><body style=\"font-family:sans-serif;padding:3rem\">\
<h1>Signed in</h1><p>You can close this tab and return to the terminal.</p></body>";

/// A page shown in the browser after a failed callback.
const ERROR_PAGE: &str = "<!doctype html><title>Peko</title><body style=\"font-family:sans-serif;padding:3rem\">\
<h1>Sign-in failed</h1><p>Return to the terminal for details.</p></body>";

/// Accept loopback connections until the `/callback` request arrives, verify
/// its `state`, and return the authorization code.
async fn wait_for_callback(
    listener: TcpListener,
    expected_state: &str,
    timeout: Duration,
) -> Result<String, AuthError> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let accepted = tokio::time::timeout_at(deadline, listener.accept())
            .await
            .map_err(|_| AuthError::Timeout)?
            .map_err(AuthError::Loopback)?;
        let (mut stream, _) = accepted;

        let target = match read_request_target(&mut stream).await {
            Ok(target) => target,
            Err(_) => continue,
        };

        // Ignore anything the browser sends that is not the callback, such as a
        // favicon probe.
        if !target.starts_with("/callback") {
            respond(&mut stream, "404 Not Found", ERROR_PAGE).await.ok();
            continue;
        }

        let parsed = match reqwest::Url::parse(&format!("http://127.0.0.1{target}")) {
            Ok(url) => url,
            Err(_) => {
                respond(&mut stream, "400 Bad Request", ERROR_PAGE)
                    .await
                    .ok();
                return Err(AuthError::BadRequest);
            }
        };

        let mut code = None;
        let mut state = None;
        let mut error = None;
        for (key, value) in parsed.query_pairs() {
            match key.as_ref() {
                "code" => code = Some(value.into_owned()),
                "state" => state = Some(value.into_owned()),
                "error" => error = Some(value.into_owned()),
                _ => {}
            }
        }

        if let Some(reason) = error {
            respond(&mut stream, "200 OK", ERROR_PAGE).await.ok();
            return Err(AuthError::Denied(reason));
        }
        if state.as_deref() != Some(expected_state) {
            respond(&mut stream, "200 OK", ERROR_PAGE).await.ok();
            return Err(AuthError::StateMismatch);
        }
        match code {
            Some(code) => {
                respond(&mut stream, "200 OK", SUCCESS_PAGE).await.ok();
                return Ok(code);
            }
            None => {
                respond(&mut stream, "200 OK", ERROR_PAGE).await.ok();
                return Err(AuthError::MissingCode);
            }
        }
    }
}

/// Read the request target (the path and query) from the first request line.
async fn read_request_target(stream: &mut TcpStream) -> Result<String, AuthError> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream.read(&mut chunk).await.map_err(AuthError::Loopback)?;
        if read == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..read]);
        if let Some(pos) = buf.windows(2).position(|w| w == b"\r\n") {
            let line = String::from_utf8_lossy(&buf[..pos]);
            return line
                .split_whitespace()
                .nth(1)
                .map(str::to_owned)
                .ok_or(AuthError::BadRequest);
        }
        if buf.len() > 8192 {
            break;
        }
    }
    Err(AuthError::BadRequest)
}

/// Write a minimal HTTP response and close the connection.
async fn respond(stream: &mut TcpStream, status: &str, body: &str) -> Result<(), AuthError> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(AuthError::Loopback)?;
    stream.flush().await.map_err(AuthError::Loopback)
}

/// Generate a random hex `state` for CSRF protection on the callback.
fn random_state() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::getrandom(&mut bytes).is_err() {
        // The OS RNG is effectively always available. This keeps login usable
        // if it is not, at a lower entropy the callback state does not depend
        // on for correctness.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        bytes.copy_from_slice(&nanos.to_le_bytes());
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[test]
    fn platform_base_prefers_override_and_trims_slash() {
        let base = platform_base(Some("https://example.test/".to_owned()));
        assert_eq!(base, "https://example.test");
    }

    #[test]
    fn platform_base_ignores_empty_override() {
        // An empty override falls through rather than producing an empty base.
        let base = platform_base(Some(String::new()));
        assert!(!base.is_empty());
    }

    #[test]
    fn random_state_is_thirty_two_hex_chars() {
        let state = random_state();
        assert_eq!(state.len(), 32);
        assert!(state.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// Connect to a spawned callback server and send one request line.
    async fn drive_callback(request_line: &str, expected_state: &str) -> Result<String, AuthError> {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let expected = expected_state.to_owned();
        let server = tokio::spawn(async move {
            wait_for_callback(listener, &expected, Duration::from_secs(5)).await
        });

        let mut client = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let request = format!("{request_line}\r\nHost: localhost\r\n\r\n");
        client.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.ok();

        server.await.unwrap()
    }

    #[tokio::test]
    async fn callback_returns_the_code_on_matching_state() {
        let result = drive_callback("GET /callback?code=abc123&state=st8 HTTP/1.1", "st8").await;
        assert_eq!(result.unwrap(), "abc123");
    }

    #[tokio::test]
    async fn callback_rejects_a_state_mismatch() {
        let result =
            drive_callback("GET /callback?code=abc&state=wrong HTTP/1.1", "expected").await;
        assert!(matches!(result, Err(AuthError::StateMismatch)));
    }

    #[tokio::test]
    async fn callback_reports_a_denied_sign_in() {
        let result =
            drive_callback("GET /callback?error=access_denied&state=s HTTP/1.1", "s").await;
        assert!(matches!(result, Err(AuthError::Denied(_))));
    }
}
