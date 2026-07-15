//! `peko deploy server`: build the app into a deployable artifact and hand it to
//! the platform's server-hosting pipeline.
//!
//! The platform builds and runs the app on its own infrastructure; the CLI only
//! talks to `app.pekoui.com` (the same base URL and bearer auth as
//! `peko deploy package`), never to AWS directly. The handshake mirrors publishing:
//!
//!   1. `POST /api/deploy/start`    → a presigned S3 POST (url + policy fields)
//!   2. `POST <upload.url>`         → the gzipped artifact (multipart/form-data)
//!   3. `POST /api/deploy/complete` → the server build begins
//!
//! The artifact is a gzipped tarball whose root holds a `Dockerfile` plus the
//! framework's built output it copies. The Dockerfile builds a container that
//! listens on `0.0.0.0:3000`; CodeBuild runs `docker build` on it, so the
//! contract is framework-agnostic. `complete` carries the framework and an
//! optional health-check path. See `artifact_spec` for the per-framework recipes
//! (Next.js, Nuxt, SvelteKit, Remix / React Router, Astro, Angular).

use std::path::Path;
use std::time::Duration;

use flate2::Compression;
use flate2::write::GzEncoder;
use serde::Deserialize;
use thiserror::Error;

/// A safety cap on the artifact size. The standalone output is bounded, so this
/// only guards against accidentally shipping source or `node_modules`.
const MAX_ARTIFACT_BYTES: usize = 500 * 1024 * 1024;

/// The pekoui native-bridge front proxy, embedded from the pekoui package and
/// written into every artifact. It binds the container's public port, starts the
/// framework server on an internal loopback port, reverse-proxies everything to
/// it, and terminates the `/__peko__` WebSocket bridge itself. Each Dockerfile
/// launches it, passing the framework's own start command as its arguments.
const BRIDGE_SERVER_JS: &str = include_str!("../../../toolkit/pekoui/server/bridge-server.mjs");

/// The artifact-root name of the embedded bridge server.
const BRIDGE_SERVER_NAME: &str = "peko-bridge-server.mjs";

/// How to package one SSR framework's build output. `required` is the build
/// output that must exist (else the build was misconfigured); `includes` are the
/// paths added to the tarball (missing optional ones are skipped); `dockerfile`
/// is the container recipe, which must serve on port 3000 (the per-app
/// `/__peko__` WebSocket rides the same server); `hint` explains how to produce
/// `required` when it is missing.
struct ArtifactSpec {
    required: &'static str,
    includes: &'static [&'static str],
    dockerfile: &'static str,
    hint: &'static str,
}

/// The packaging recipe for a server framework id. Unknown ids fall back to
/// Next (the default). Each Dockerfile follows the framework's own
/// self-hosting docs; the platform build pipeline may still need per-framework
/// tuning on first real deploys.
fn artifact_spec(framework: &str) -> ArtifactSpec {
    match framework {
        "nuxt" => ArtifactSpec {
            required: ".output",
            includes: &[".output"],
            dockerfile: "FROM node:22-slim\nWORKDIR /app\nCOPY .output ./.output\n\
                COPY peko-bridge-server.mjs ./\nEXPOSE 3000\n\
                CMD [\"node\", \"peko-bridge-server.mjs\", \"node\", \".output/server/index.mjs\"]\n",
            hint: "build with the Nitro node-server preset so .output is emitted",
        },
        "sveltekit" => ArtifactSpec {
            required: "build",
            includes: &["build", "package.json"],
            dockerfile: "FROM node:22-slim\nWORKDIR /app\nCOPY package.json ./\n\
                COPY build ./build\nCOPY peko-bridge-server.mjs ./\nEXPOSE 3000\n\
                CMD [\"node\", \"peko-bridge-server.mjs\", \"node\", \"build\"]\n",
            hint: "use @sveltejs/adapter-node so a build/ server is emitted",
        },
        "remix" => ArtifactSpec {
            required: "build",
            includes: &["build", "package.json", "package-lock.json"],
            dockerfile: "FROM node:22-slim\nWORKDIR /app\n\
                COPY package.json package-lock.json ./\nRUN npm ci --omit=dev\n\
                COPY build ./build\nCOPY peko-bridge-server.mjs ./\nEXPOSE 3000\n\
                CMD [\"node\", \"peko-bridge-server.mjs\", \"npx\", \"react-router-serve\", \"./build/server/index.js\"]\n",
            hint: "run the framework build so build/server and build/client are emitted",
        },
        "astro" => ArtifactSpec {
            required: "dist",
            includes: &["dist", "package.json", "package-lock.json"],
            dockerfile: "FROM node:22-slim\nWORKDIR /app\n\
                COPY package.json package-lock.json ./\nRUN npm ci --omit=dev\n\
                COPY dist ./dist\nCOPY peko-bridge-server.mjs ./\nEXPOSE 3000\n\
                CMD [\"node\", \"peko-bridge-server.mjs\", \"node\", \"./dist/server/entry.mjs\"]\n",
            hint: "add @astrojs/node (mode: 'standalone') so dist/server is emitted",
        },
        "angular" => ArtifactSpec {
            required: "dist",
            includes: &["dist"],
            // The server bundle path is dist/<project>/server/server.mjs; the
            // project name is not known here, so locate it at start.
            dockerfile: "FROM node:22-slim\nWORKDIR /app\nCOPY dist ./dist\n\
                COPY peko-bridge-server.mjs ./\nEXPOSE 3000\n\
                CMD [\"node\", \"peko-bridge-server.mjs\", \"sh\", \"-c\", \"node $(find dist -path '*/server/server.mjs' | head -1)\"]\n",
            hint: "enable SSR so dist/<app>/server/server.mjs is emitted",
        },
        // Next and the default.
        _ => ArtifactSpec {
            required: ".next/standalone",
            includes: &[".next/standalone", ".next/static", "public"],
            dockerfile: "FROM node:22-slim\nWORKDIR /app\nCOPY .next/standalone ./\n\
                COPY .next/static ./.next/static\nCOPY public ./public\n\
                COPY peko-bridge-server.mjs ./\nEXPOSE 3000\n\
                CMD [\"node\", \"peko-bridge-server.mjs\", \"node\", \"server.js\"]\n",
            hint: "set output: 'standalone' in next.config.js so the server bundle is emitted",
        },
    }
}

/// One failure mode for a server deploy.
#[derive(Debug, Error)]
pub enum DeployError {
    /// The expected build output was not found; the string explains what and
    /// how to produce it.
    #[error("{0}")]
    NoBuildOutput(String),

    /// The artifact exceeds the size cap.
    #[error("the artifact is {size} bytes, over the {max} byte limit")]
    TooLarge { size: usize, max: usize },

    /// Assembling the tarball failed.
    #[error("could not assemble the artifact: {0}")]
    Archive(#[source] std::io::Error),

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

    /// The platform forbade the deploy with a user-facing explanation, most
    /// often an account whose email is not verified.
    #[error("{0}")]
    Forbidden(String),

    /// The app is not configured for server hosting (or the request was
    /// malformed). The string is the server's explanation.
    #[error("{0}")]
    BadRequest(String),

    /// The app id is not one this account owns.
    #[error("the app was not found on your account")]
    NotFound,

    /// Server hosting is not wired up on the platform yet (a `503`).
    #[error("server hosting is not available on the platform yet")]
    NotConfigured,

    /// Requesting the upload slot failed for another reason.
    #[error("could not start the deploy (HTTP {0})")]
    Start(u16),

    /// The upload to the signed URL failed.
    #[error("could not upload the artifact (HTTP {0})")]
    Upload(u16),

    /// Signalling completion failed for another reason.
    #[error("could not complete the deploy (HTTP {0})")]
    Complete(u16),
}

/// The response from `POST /api/deploy/start`.
#[derive(Deserialize)]
struct StartResponse {
    #[serde(rename = "deploymentId")]
    deployment_id: String,
    /// The presigned S3 POST: `url` is the bucket endpoint and `fields` are the
    /// form fields (policy, signature, `Content-Type`, ...) to send verbatim.
    upload: UploadTarget,
    /// The S3 policy's max object size; the artifact must be under it.
    #[serde(rename = "maxUploadBytes")]
    max_upload_bytes: Option<u64>,
    /// The app's serving host, `<slug>.serve.pekoui.com` (deterministic per app,
    /// known immediately; serves once the deploy reaches `live`).
    host: Option<String>,
    /// The full `https://<host>` URL.
    url: Option<String>,
}

/// A presigned S3 POST target: the endpoint plus the form fields that carry the
/// policy and signature.
#[derive(Deserialize)]
struct UploadTarget {
    url: String,
    fields: std::collections::HashMap<String, String>,
}

/// The response from `GET /api/deploy/status`.
#[derive(Deserialize)]
struct StatusResponse {
    status: String,
    url: Option<String>,
    error: Option<String>,
}

/// The response from `POST /api/deploy/complete`.
#[derive(Deserialize)]
struct CompleteResponse {
    status: String,
}

/// A `{ "error" }` or `{ "message" }` explanation body.
#[derive(Deserialize)]
struct ErrorBody {
    error: Option<String>,
    message: Option<String>,
}

/// The result of a started deploy.
pub struct DeployOutcome {
    /// The platform's deployment id, for status lookups.
    pub deployment_id: String,
    /// The initial status the server reported, such as `building`.
    pub status: String,
    /// The app's serving host, `<slug>.serve.pekoui.com`, from the start reply.
    pub host: Option<String>,
    /// The full `https://<host>` URL the app will serve at.
    pub url: Option<String>,
}

/// A single deploy-status poll result.
pub struct DeployStatus {
    /// One of `building`, `releasing`, `live`, `failed`.
    pub status: String,
    /// The live URL, when reported.
    pub url: Option<String>,
    /// The failure explanation when `status` is `failed`.
    pub error: Option<String>,
}

/// Assemble the gzipped deploy artifact from a built project at `root` for the
/// given server `framework`: a `Dockerfile` plus the build output it copies.
pub fn build_artifact(root: &Path, framework: &str) -> Result<Vec<u8>, DeployError> {
    let spec = artifact_spec(framework);
    if !root.join(spec.required).exists() {
        return Err(DeployError::NoBuildOutput(format!(
            "no `{}` build output was found; {}",
            spec.required, spec.hint
        )));
    }

    let gz = GzEncoder::new(Vec::new(), Compression::default());
    let mut tar = tar::Builder::new(gz);

    // The Dockerfile is emitted in memory rather than written to the project.
    let mut header = tar::Header::new_gnu();
    header.set_size(spec.dockerfile.len() as u64);
    header.set_mode(0o644);
    tar.append_data(&mut header, "Dockerfile", spec.dockerfile.as_bytes())
        .map_err(DeployError::Archive)?;

    // The native-bridge front proxy the Dockerfile launches, likewise emitted in
    // memory so the image is self-contained (no runtime npm install).
    let mut bridge_header = tar::Header::new_gnu();
    bridge_header.set_size(BRIDGE_SERVER_JS.len() as u64);
    bridge_header.set_mode(0o644);
    tar.append_data(
        &mut bridge_header,
        BRIDGE_SERVER_NAME,
        BRIDGE_SERVER_JS.as_bytes(),
    )
    .map_err(DeployError::Archive)?;

    // Add each declared path; skip optional ones that are absent.
    for include in spec.includes {
        let path = root.join(include);
        if path.is_dir() {
            tar.append_dir_all(include, &path)
                .map_err(DeployError::Archive)?;
        } else if path.is_file() {
            tar.append_path_with_name(&path, include)
                .map_err(DeployError::Archive)?;
        }
    }

    let gz = tar.into_inner().map_err(DeployError::Archive)?;
    let bytes = gz.finish().map_err(DeployError::Archive)?;
    if bytes.len() > MAX_ARTIFACT_BYTES {
        return Err(DeployError::TooLarge {
            size: bytes.len(),
            max: MAX_ARTIFACT_BYTES,
        });
    }
    Ok(bytes)
}

/// Build the HTTP client for the deploy handshake, with a generous timeout for
/// the artifact upload.
fn http_client() -> Result<reqwest::Client, DeployError> {
    reqwest::Client::builder()
        .user_agent(concat!("peko-cli/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(DeployError::HttpClient)
}

/// Read a server explanation from an error response, falling back to a generic
/// message.
async fn error_body(response: reqwest::Response) -> String {
    response
        .json::<ErrorBody>()
        .await
        .ok()
        .and_then(|b| b.error.or(b.message))
        .unwrap_or_else(|| "the platform rejected the request".to_owned())
}

/// Run the deploy handshake for `app_id`, uploading `bytes` as the artifact.
/// `id_token` is the bearer token from `peko login`. `framework` is the SSR
/// framework (stored for display + defaults); `health_path` overrides the ALB
/// health-check path (default `/`) for a framework that does not 200 on root.
pub async fn deploy(
    base: &str,
    id_token: &str,
    app_id: &str,
    framework: &str,
    health_path: Option<&str>,
    bytes: Vec<u8>,
) -> Result<DeployOutcome, DeployError> {
    let http = http_client()?;

    // 1. Request an upload slot.
    let start = http
        .post(format!("{base}/api/deploy/start"))
        .bearer_auth(id_token)
        .json(&serde_json::json!({ "appId": app_id }))
        .send()
        .await
        .map_err(DeployError::Network)?;
    match start.status() {
        s if s.is_success() => {}
        reqwest::StatusCode::UNAUTHORIZED => return Err(DeployError::Unauthorized),
        reqwest::StatusCode::FORBIDDEN => {
            return Err(DeployError::Forbidden(error_body(start).await));
        }
        reqwest::StatusCode::NOT_FOUND => return Err(DeployError::NotFound),
        reqwest::StatusCode::SERVICE_UNAVAILABLE => return Err(DeployError::NotConfigured),
        reqwest::StatusCode::BAD_REQUEST => {
            return Err(DeployError::BadRequest(error_body(start).await));
        }
        other => return Err(DeployError::Start(other.as_u16())),
    }
    let start: StartResponse = start.json().await.map_err(DeployError::Decode)?;

    let artifact_len = bytes.len();

    // Reject an over-large artifact before the round-trip; the S3 policy caps it
    // too (a 403 below), but this gives a clearer message.
    if let Some(max) = start.max_upload_bytes
        && artifact_len as u64 > max
    {
        return Err(DeployError::TooLarge {
            size: artifact_len,
            max: max as usize,
        });
    }

    // 2. Upload the artifact via the presigned S3 POST. It goes straight to
    // storage and carries no bearer token; the signed policy is the auth. Send
    // every policy field first, then the `file` part last (S3 requires that
    // order). The multipart boundary sets Content-Type — don't set it manually;
    // the artifact's own type rides in the `Content-Type` policy field.
    let mut form = reqwest::multipart::Form::new();
    for (key, value) in &start.upload.fields {
        form = form.text(key.clone(), value.clone());
    }
    let file_part = reqwest::multipart::Part::bytes(bytes).file_name("artifact.tar.gz");
    form = form.part("file", file_part);
    let upload = http
        .post(&start.upload.url)
        .multipart(form)
        .send()
        .await
        .map_err(DeployError::Network)?;
    if !upload.status().is_success() {
        // S3 answers 403 with an `EntityTooLarge`/policy error when the artifact
        // exceeds the signed cap.
        let status = upload.status();
        if status == reqwest::StatusCode::FORBIDDEN {
            let body = upload.text().await.unwrap_or_default();
            if body.contains("EntityTooLarge") {
                return Err(DeployError::TooLarge {
                    size: artifact_len,
                    max: start.max_upload_bytes.unwrap_or(0) as usize,
                });
            }
        }
        return Err(DeployError::Upload(status.as_u16()));
    }

    // 3. Signal completion; the server-side build begins. `framework` and an
    // optional `healthPath` tell the platform how to run and health-check it.
    let mut complete_body = serde_json::json!({
        "appId": app_id,
        "deploymentId": start.deployment_id,
        "framework": framework,
    });
    if let Some(path) = health_path {
        complete_body["healthPath"] = serde_json::Value::String(path.to_owned());
    }
    let complete = http
        .post(format!("{base}/api/deploy/complete"))
        .bearer_auth(id_token)
        .json(&complete_body)
        .send()
        .await
        .map_err(DeployError::Network)?;
    match complete.status() {
        s if s.is_success() => {}
        reqwest::StatusCode::UNAUTHORIZED => return Err(DeployError::Unauthorized),
        reqwest::StatusCode::FORBIDDEN => {
            return Err(DeployError::Forbidden(error_body(complete).await));
        }
        other => return Err(DeployError::Complete(other.as_u16())),
    }
    let done: CompleteResponse = complete.json().await.map_err(DeployError::Decode)?;

    Ok(DeployOutcome {
        deployment_id: start.deployment_id,
        status: done.status,
        host: start.host,
        url: start.url,
    })
}

/// Poll `GET /api/deploy/status` once. Returns `Ok(None)` for a `404`, which
/// just means the deployment is not recorded yet — the caller should keep
/// polling.
pub async fn deploy_status(
    base: &str,
    id_token: &str,
    app_id: &str,
    deployment_id: &str,
) -> Result<Option<DeployStatus>, DeployError> {
    let http = http_client()?;
    let resp = http
        .get(format!("{base}/api/deploy/status"))
        .query(&[("appId", app_id), ("deploymentId", deployment_id)])
        .bearer_auth(id_token)
        .send()
        .await
        .map_err(DeployError::Network)?;
    match resp.status() {
        s if s.is_success() => {}
        reqwest::StatusCode::UNAUTHORIZED => return Err(DeployError::Unauthorized),
        reqwest::StatusCode::NOT_FOUND => return Ok(None),
        other => return Err(DeployError::Complete(other.as_u16())),
    }
    let parsed: StatusResponse = resp.json().await.map_err(DeployError::Decode)?;
    Ok(Some(DeployStatus {
        status: parsed.status,
        url: parsed.url,
        error: parsed.error,
    }))
}
