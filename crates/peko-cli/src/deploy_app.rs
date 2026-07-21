//! `peko deploy app`: the two-build (generation + submission) app deploy flow.
//!
//! For each target platform declared in `peko.toml`, this produces **two**
//! builds and ships them to the platform as one bundle:
//!
//! - **Build 1 — generation**: `peko build --demo` (debug, demo fixtures and
//!   pekoshots in, unsigned). The farm runs this to generate store screenshots
//!   and recordings.
//! - **Build 2 — submission**: `peko build --release` (release, demo and
//!   pekoshots stripped, signed). The store-ready binary.
//!
//! The flow: confirm the target platforms, confirm signing keys are connected,
//! build both variants per platform, organize the outputs, pack them into a
//! single compressed `.pkdeploy` bundle, and push it to the platform.
//!
//! The bundle **always** carries a clean, buildable snapshot of the app source
//! (under `source/`, with cache/deps/keys excluded) — the source of truth for
//! the platform, and what the remote Mac builder builds from.
//!
//! Non-Apple targets (Windows/Android/Linux) always build locally. Apple targets
//! build locally on a Mac host; on a non-Mac host they are handed to the remote
//! Mac builder (`remote_build`), which builds them from the packaged source with
//! headless signing — the runner itself is not yet implemented, but the bundle
//! it will consume is complete.

use std::path::Path;
use std::process::ExitCode;

use peko_core::target::OperatingSystem;
use serde::{Deserialize, Serialize};

use std::collections::BTreeMap;

use crate::bundler::signing;
use crate::cli::CLIInfo;
use crate::cli::reporting::Reporter;
use crate::commands::platform_label;
use crate::project::PekoProject;
use crate::{deploy_pack, deploy_seal};

/// Which of the two builds to produce for a platform.
#[derive(Clone, Copy)]
enum BuildKind {
    /// Debug + demo, unsigned. Drives screenshot / recording generation.
    Generation,
    /// Signed release, demo stripped. The submission binary.
    Submission,
}

impl BuildKind {
    /// The `peko build` flag name that selects this build.
    fn flag_name(self) -> &'static str {
        match self {
            BuildKind::Generation => "demo",
            BuildKind::Submission => "release",
        }
    }

    /// The build output root the bundler writes into for this build.
    fn output_root(self) -> &'static str {
        match self {
            BuildKind::Generation => "build/debug",
            BuildKind::Submission => "build/release",
        }
    }

    /// A short human label.
    fn label(self) -> &'static str {
        match self {
            BuildKind::Generation => "generation (demo)",
            BuildKind::Submission => "submission (release)",
        }
    }
}

/// One platform's place in the deploy bundle manifest.
#[derive(Serialize)]
struct PlatformArtifact {
    /// The operating system (`windows`, `android`, `linux`, `macos`, `ios`).
    os: String,
    /// Bundle-relative path to the generation (Build 1) output tree.
    generation: String,
    /// Bundle-relative path to the submission (Build 2) output tree.
    submission: String,
    /// Whether a signing key was connected for the submission build.
    signed: bool,
}

/// Instructions for the remote Mac builder: the platforms it must build from
/// the packaged app source. Present when an Apple target was requested from a
/// non-Mac host. The source itself is always in the bundle (see
/// [`DeployManifest::source`]).
#[derive(Serialize)]
struct RemoteBuild {
    /// The platforms the runner builds from source (`macos`, `ios`).
    targets: Vec<String>,
}

/// Sealed signing material info in the bundle manifest. The remote builder
/// decrypts `sealed` with its runner key (`peko deploy unseal`) to sign the
/// listed platforms' release builds.
#[derive(Serialize)]
struct SigningInfo {
    /// Bundle-relative path to the sealed blob.
    sealed: String,
    /// The platforms whose signing material is sealed (`ios`, `macos`).
    platforms: Vec<String>,
}

/// The `deploy.json` manifest embedded at the root of the bundle.
#[derive(Serialize)]
struct DeployManifest {
    /// The app name.
    app: String,
    /// The platform-assigned app id, when linked.
    app_id: Option<String>,
    /// The app version being deployed.
    version: String,
    /// The CLI/toolchain version that produced the bundle.
    tool_version: String,
    /// The host operating system the bundle was built on.
    host_os: String,
    /// One entry per platform successfully built locally.
    platforms: Vec<PlatformArtifact>,
    /// Bundle-relative path to the packaged app source tree. Always present —
    /// the bundle always carries a clean, buildable source snapshot.
    source: String,
    /// `true` when path deps were vendored into `source/vendor/` and the
    /// resolved registry/gated cache + global lockfile were mirrored into
    /// `pekoroot/` for a hermetic remote build. Set for remote Apple builds.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    vendored: bool,
    /// `true` when this is an SSR (server-framework) app whose hosted frontend
    /// was deployed by this run before the native shells were built.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    server_deployed: bool,
    /// `true` when the SSG web frontend was prebuilt into `prebuilt/web/` for a
    /// remote builder to substitute via `peko build --web-dist`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    prebuilt_frontend: bool,
    /// Sealed signing material for the remote builder, when Apple signing keys
    /// were connected and a runner key was available to seal to.
    #[serde(skip_serializing_if = "Option::is_none")]
    signing: Option<SigningInfo>,
    /// Present when the bundle's source must be built for extra platforms by the
    /// remote Mac builder (Apple targets requested from a non-Mac host).
    #[serde(skip_serializing_if = "Option::is_none")]
    remote_build: Option<RemoteBuild>,
}

/// Execute `peko deploy app`.
pub async fn run(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    let project = match PekoProject::from_current_directory() {
        Ok(project) => project,
        Err(e) => {
            reporter.error(format!("could not load project: {e}"));
            return ExitCode::FAILURE;
        }
    };

    let Some(ui) = project.ui_project_info.clone() else {
        reporter.error("`deploy app` needs a UI app project (one with a [ui] table)");
        reporter.help("`deploy package` publishes a library; `deploy server` deploys an SSR app");
        return ExitCode::FAILURE;
    };
    let root = project.get_root().to_path_buf();

    let assume_yes = cli_info.flags.has_flag("yes");
    let allow_unsigned = cli_info.flags.has_flag("allow-unsigned");
    let no_upload = cli_info.flags.has_flag("no-upload");

    // --- Step 1: confirm target platforms -----------------------------------
    if ui.platforms.is_empty() {
        reporter.error("the project declares no target_platforms to deploy");
        reporter.help("list platforms under [project].target_platforms in peko.toml");
        return ExitCode::FAILURE;
    }

    // A Mac host builds Apple targets locally; other hosts hand them to the
    // remote Mac builder. `PEKO_ASSUME_NON_APPLE_HOST` forces the non-Mac path
    // so the remote-build source packaging can be exercised from a Mac.
    let host_is_mac =
        cfg!(target_os = "macos") && std::env::var_os("PEKO_ASSUME_NON_APPLE_HOST").is_none();
    let mut local_targets: Vec<OperatingSystem> = Vec::new();
    let mut remote_targets: Vec<OperatingSystem> = Vec::new();
    for os in &ui.platforms {
        if platform_label(os).is_none() {
            reporter.error("target_platforms contains an unsupported operating system");
            return ExitCode::FAILURE;
        }
        let is_apple = matches!(os, OperatingSystem::MacOS | OperatingSystem::IOS);
        if is_apple && !host_is_mac {
            remote_targets.push(*os);
        } else {
            local_targets.push(*os);
        }
    }

    reporter.status("Deploying", format!("{} {}", project.name, ui.version));
    reporter.info("target platforms:");
    for os in &local_targets {
        reporter.info(format!("  - {} (local build)", platform_label(os).unwrap()));
    }
    for os in &remote_targets {
        reporter.info(format!(
            "  - {} (needs a remote Apple builder)",
            platform_label(os).unwrap()
        ));
    }

    if !confirm(assume_yes, "Deploy for these platforms?", true) {
        reporter.info("deploy cancelled");
        return ExitCode::SUCCESS;
    }

    // Apple targets on a non-Mac host need the remote Mac builder. Accepting
    // packages the app source into the bundle so the runner can build them
    // later (the runner itself is not implemented yet); declining drops them.
    let mut remote_build_requested = false;
    if !remote_targets.is_empty() {
        let labels = remote_targets
            .iter()
            .map(|os| platform_label(os).unwrap())
            .collect::<Vec<_>>()
            .join(", ");
        reporter.warning(format!(
            "{labels}: Apple builds require a Mac host or the remote Mac builder"
        ));
        if confirm(
            assume_yes,
            "Package source for a remote Apple build?",
            false,
        ) {
            remote_build_requested = true;
            reporter.info(format!(
                "{labels}: app source will be packaged for the remote builder (the runner is not available yet)"
            ));
        } else {
            reporter.info(format!("skipping {labels} for this deploy"));
            remote_targets.clear();
        }
    }

    if local_targets.is_empty() && !remote_build_requested {
        reporter.error("no platforms to deploy");
        return ExitCode::FAILURE;
    }

    // --- Step 2: confirm signing keys ---------------------------------------
    let mut signed_status: std::collections::BTreeMap<String, bool> = Default::default();
    let mut missing_keys: Vec<&OperatingSystem> = Vec::new();
    reporter.info("signing keys:");
    for os in &local_targets {
        match signing::platform_id(os) {
            Some(platform) => {
                let connected = signing::key_connected(&root, platform);
                signed_status.insert(os.to_string(), connected);
                reporter.info(format!(
                    "  - {}: {}",
                    platform_label(os).unwrap(),
                    if connected { "connected" } else { "MISSING" }
                ));
                if !connected {
                    missing_keys.push(os);
                }
            }
            None => {
                // Linux does not sign.
                signed_status.insert(os.to_string(), false);
                reporter.info(format!("  - {}: not signed", platform_label(os).unwrap()));
            }
        }
    }

    if !missing_keys.is_empty() && !allow_unsigned {
        let labels = missing_keys
            .iter()
            .map(|os| platform_label(os).unwrap())
            .collect::<Vec<_>>()
            .join(", ");
        reporter.warning(format!("no signing key connected for: {labels}"));
        reporter.help("run `peko keys add --platform <platform>` to connect a key");
        if !confirm(
            assume_yes,
            "Continue anyway? Submission builds for those platforms will be unsigned",
            false,
        ) {
            reporter.info("deploy cancelled");
            return ExitCode::SUCCESS;
        }
    }

    // --- Step 3: two builds per local platform ------------------------------

    // An SSR (server-framework) app's native shells load its hosted origin, so
    // the frontend must be deployed and the host assigned before the native
    // apps are built (they bake `bundle::host`). Deploy the server first,
    // in-process. Skipped on a local dry run.
    let server_deployed = ui.framework == "server" && !no_upload;
    if server_deployed {
        reporter.status("Deploying", "the SSR server (frontend) first");
        if crate::commands::deploy::deploy_server(cli_info, reporter).await != ExitCode::SUCCESS {
            reporter.error("server deploy failed; aborting app deploy");
            return ExitCode::FAILURE;
        }
    }

    for os in &local_targets {
        let label = platform_label(os).unwrap();
        for kind in [BuildKind::Generation, BuildKind::Submission] {
            reporter.status("Building", format!("{label}: {}", kind.label()));
            if !run_build(cli_info, os, kind, reporter).await {
                reporter.error(format!("{label} {} build failed", kind.label()));
                return ExitCode::FAILURE;
            }
        }
    }

    // --- Step 4: organize artifacts -----------------------------------------
    let staging = root.join(".peko/deploy");
    if staging.exists() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    let mut artifacts = Vec::new();
    for os in &local_targets {
        let os_name = os.to_string();
        let gen_src = root
            .join(BuildKind::Generation.output_root())
            .join(&os_name);
        let sub_src = root
            .join(BuildKind::Submission.output_root())
            .join(&os_name);
        let gen_dst = format!("generation/{os_name}");
        let sub_dst = format!("submission/{os_name}");
        if let Err(e) = copy_tree(&gen_src, &staging.join(&gen_dst)) {
            reporter.error(format!("could not stage {os_name} generation build: {e}"));
            return ExitCode::FAILURE;
        }
        if let Err(e) = copy_tree(&sub_src, &staging.join(&sub_dst)) {
            reporter.error(format!("could not stage {os_name} submission build: {e}"));
            return ExitCode::FAILURE;
        }
        artifacts.push(PlatformArtifact {
            os: os_name.clone(),
            generation: gen_dst,
            submission: sub_dst,
            signed: *signed_status.get(&os_name).unwrap_or(&false),
        });
    }

    // Always package a clean, buildable snapshot of the app source into the
    // bundle (build cache, build output, node_modules, VCS metadata, and signing
    // keys excluded). It is the source of truth for remote Apple builds and is
    // available to the platform for any rebuild.
    reporter.status("Packaging", "app source");
    if let Err(e) = copy_source_tree(&root, &staging.join("source")) {
        reporter.error(format!("could not package the app source: {e}"));
        return ExitCode::FAILURE;
    }

    // For a remote build the Mac needs the app's dependencies without registry
    // access, entitlements, or a matching global install: vendor local path
    // deps into the packaged source (rewriting the manifest/lockfile) and mirror
    // the resolved registry/gated cache + global lockfile into `pekoroot/`.
    let vendored = remote_build_requested && !remote_targets.is_empty();
    if vendored {
        reporter.status("Packaging", "dependencies for the remote builder");
        if let Err(e) = deploy_pack::vendor_path_deps(&root, &staging.join("source")) {
            reporter.error(format!("could not vendor path dependencies: {e}"));
            return ExitCode::FAILURE;
        }
        if let Err(e) =
            deploy_pack::mirror_dependency_cache(cli_info.get_peko_root(), &root, &staging)
        {
            reporter.error(format!("could not mirror the dependency cache: {e}"));
            return ExitCode::FAILURE;
        }
    }

    // An SSG (static) app embeds its web frontend, which builds with node — which
    // the remote Mac does not have. Build it here and ship it as `prebuilt/web`;
    // the Mac substitutes it into `assets/` via `peko build --web-dist`. The
    // freshly built copy under `source/assets` is dropped to avoid shipping it
    // twice.
    let prebuilt_frontend = vendored && ui.framework == "static";
    if prebuilt_frontend {
        reporter.status("Packaging", "prebuilding the web frontend");
        if crate::commands::build::build_web_frontend(
            &project,
            cli_info.get_peko_root(),
            None,
            reporter,
        )
        .is_err()
        {
            return ExitCode::FAILURE;
        }
        let built = root.join("assets");
        if built.is_dir()
            && let Err(e) = deploy_pack::copy_dir_all(&built, &staging.join("prebuilt/web"))
        {
            reporter.error(format!("could not package the prebuilt web frontend: {e}"));
            return ExitCode::FAILURE;
        }
        let _ = std::fs::remove_dir_all(staging.join("source/assets"));
    }

    // Seal the signing material for the remote Apple targets to the runner's
    // public key, so the Mac can sign the release build but the platform relays
    // only ciphertext. The recipient comes from the platform's runner-key
    // registry (`GET /api/deploy/runner-key`); `--runner-key` / `PEKO_RUNNER_KEY`
    // override it for testing without the platform. Without signing keys or a
    // recipient, the remote release build is left unsigned.
    let mut signing = None;
    if vendored {
        let mut apple_keys: BTreeMap<String, deploy_seal::PlatformSigning> = BTreeMap::new();
        for os in &remote_targets {
            if let Some(platform) = signing::platform_id(os)
                && let Ok(Some(app)) =
                    signing::resolve_apple(cli_info, &root, &ui.bundle_id, platform)
            {
                // macOS also needs the installer cert to sign the `.pkg`.
                let installer = if *os == OperatingSystem::MacOS {
                    signing::resolve_installer(cli_info, &root, &ui.bundle_id).unwrap_or(None)
                } else {
                    None
                };
                apple_keys.insert(
                    platform.to_string(),
                    deploy_seal::PlatformSigning { app, installer },
                );
            }
        }
        if !apple_keys.is_empty() {
            let recipient = runner_public_key(cli_info, reporter).await;
            match recipient {
                Some(runner) => {
                    reporter.status(
                        "Packaging",
                        match &runner.key_id {
                            Some(id) => {
                                format!("sealing signing material to build runner \"{id}\"")
                            }
                            None => "sealing signing material for the remote builder".to_owned(),
                        },
                    );
                    match deploy_seal::seal_signing_material(&runner.recipient, &apple_keys) {
                        Ok(bytes) => {
                            if let Err(e) = std::fs::write(staging.join("signing.sealed"), &bytes) {
                                reporter.error(format!("could not write signing.sealed: {e}"));
                                return ExitCode::FAILURE;
                            }
                            signing = Some(SigningInfo {
                                sealed: "signing.sealed".to_owned(),
                                platforms: apple_keys.keys().cloned().collect(),
                            });
                        }
                        Err(e) => {
                            reporter.error(format!("could not seal the signing material: {e}"));
                            return ExitCode::FAILURE;
                        }
                    }
                }
                None => reporter.warning(
                    "signing keys are connected but no remote build runner key is available; the remote release build will be unsigned",
                ),
            }
        }
    }

    // Remote Apple targets (requested from a non-Mac host) tell the runner which
    // platforms to build from that source.
    let remote_build = if remote_build_requested && !remote_targets.is_empty() {
        Some(RemoteBuild {
            targets: remote_targets.iter().map(|os| os.to_string()).collect(),
        })
    } else {
        None
    };

    let manifest = DeployManifest {
        app: project.name.clone(),
        app_id: ui.app_id.clone(),
        version: ui.version.clone(),
        tool_version: env!("CARGO_PKG_VERSION").to_owned(),
        host_os: std::env::consts::OS.to_owned(),
        platforms: artifacts,
        source: "source".to_owned(),
        vendored,
        server_deployed,
        prebuilt_frontend,
        signing,
        remote_build,
    };
    let manifest_json = match serde_json::to_vec_pretty(&manifest) {
        Ok(bytes) => bytes,
        Err(e) => {
            reporter.error(format!("could not write the deploy manifest: {e}"));
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = std::fs::write(staging.join("deploy.json"), &manifest_json) {
        reporter.error(format!("could not write deploy.json: {e}"));
        return ExitCode::FAILURE;
    }

    // --- Step 5: bundle + compress ------------------------------------------
    reporter.status("Bundling", "compressing the deploy artifacts");
    let bundle_bytes = match pack_deploy(&staging) {
        Ok(bytes) => bytes,
        Err(e) => {
            reporter.error(format!("could not pack the deploy bundle: {e}"));
            return ExitCode::FAILURE;
        }
    };
    let bundle_dir = root.join("build/deploy");
    if let Err(e) = std::fs::create_dir_all(&bundle_dir) {
        reporter.error(format!("could not create the deploy output directory: {e}"));
        return ExitCode::FAILURE;
    }
    let bundle_path = bundle_dir.join(format!(
        "{}-{}.pkdeploy",
        sanitize(&project.name),
        ui.version
    ));
    if let Err(e) = std::fs::write(&bundle_path, &bundle_bytes) {
        reporter.error(format!("could not write the deploy bundle: {e}"));
        return ExitCode::FAILURE;
    }
    reporter.info(format!(
        "bundle: {} ({:.1} MiB)",
        bundle_path.display(),
        bundle_bytes.len() as f64 / (1024.0 * 1024.0)
    ));

    // --- Step 6: push to the platform ---------------------------------------
    if no_upload {
        reporter.success(format!(
            "built deploy bundle for {} (not uploaded)",
            project.name
        ));
        return ExitCode::SUCCESS;
    }

    match push_bundle(cli_info, &ui, bundle_bytes, reporter).await {
        PushOutcome::Uploaded => {
            reporter.success(format!("deployed {} {}", project.name, ui.version));
            ExitCode::SUCCESS
        }
        PushOutcome::Skipped => {
            // The bundle is on disk; the upload will land once the platform
            // intake is available. Not a hard failure for local testing.
            reporter.success(format!(
                "built deploy bundle for {}; upload skipped (bundle kept at {})",
                project.name,
                bundle_path.display()
            ));
            ExitCode::SUCCESS
        }
        PushOutcome::Failed(message) => {
            reporter.error(format!("deploy upload failed: {message}"));
            reporter.help(format!("the bundle is kept at {}", bundle_path.display()));
            ExitCode::FAILURE
        }
    }
}

/// The platform's `GET /api/deploy/runner-key` response: the remote build
/// runner's public half, which signing material is sealed to.
#[derive(Deserialize)]
struct RunnerKeyResponse {
    #[serde(rename = "publicKey")]
    public_key: String,
    /// Which machine the key belongs to, reported so the user can see what
    /// their signing material is being sealed to.
    #[serde(rename = "keyId")]
    key_id: Option<String>,
}

/// The remote build runner's public key and the machine it identifies.
struct RunnerKey {
    recipient: String,
    key_id: Option<String>,
}

/// Whether `value` is a well-formed age recipient — the same shape the platform
/// enforces on write. Validated before sealing so a corrupted or wrong response
/// fails here rather than producing a bundle nobody can decrypt, which would
/// only surface much later as an opaque remote build failure.
fn is_age_recipient(value: &str) -> bool {
    let Some(body) = value.strip_prefix("age1") else {
        return false;
    };
    (40..=100).contains(&body.len())
        && body
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// The remote build runner's public key, to seal the signing material to.
///
/// Normally fetched from the platform's runner-key registry, which an admin
/// registers the build Mac with; `--runner-key` / `PEKO_RUNNER_KEY` override it
/// so the flow can be exercised offline. Returns `None` (with a reason) when no
/// runner is registered or the session can't authenticate — the caller then
/// ships an unsealed bundle and the remote release build is left unsigned.
///
/// Deliberately sends no `Origin` and no App Check header: the route's
/// same-origin assertion passes only when `Origin` is absent (the CLI path),
/// and a valid bearer token already exempts the call from attestation.
async fn runner_public_key(cli_info: &CLIInfo, reporter: &Reporter) -> Option<RunnerKey> {
    if let Some(key) = cli_info
        .flags
        .get_flag("runner-key")
        .or_else(|| std::env::var("PEKO_RUNNER_KEY").ok())
    {
        if !is_age_recipient(&key) {
            reporter.error("the runner key override is not a valid age recipient (age1…)");
            return None;
        }
        return Some(RunnerKey {
            recipient: key,
            key_id: None,
        });
    }

    let Some(session) = crate::auth::Session::load() else {
        reporter.warning("not signed in, so the remote build runner key could not be fetched");
        reporter.help("run `peko login`, or pass --runner-key <age1…>");
        return None;
    };
    let id_token = match crate::auth::fresh_id_token(&session).await {
        Ok(token) => token,
        Err(e) => {
            reporter.warning(format!(
                "could not refresh the session for the runner key: {e}"
            ));
            return None;
        }
    };
    let base = crate::auth::platform_base(cli_info.flags.get_flag("base"));
    let http = reqwest::Client::builder()
        .user_agent(concat!("peko-cli/", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()?;
    let response = match http
        .get(format!("{base}/api/deploy/runner-key"))
        .bearer_auth(&id_token)
        .send()
        .await
    {
        Ok(response) => response,
        Err(e) => {
            reporter.warning(format!("could not reach the runner-key registry: {e}"));
            return None;
        }
    };
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        reporter.warning("no build runner key is registered with the platform");
        reporter.help(
            "an admin must register the build Mac's key at app.pekoui.com/admin/runner-key, or pass --runner-key <age1…>",
        );
        return None;
    }
    if !response.status().is_success() {
        let failure = crate::auth::explain_failure(response).await;
        reporter.warning(failure.message);
        if let Some(help) = failure.help {
            reporter.help(help);
        }
        return None;
    }
    let key = match response.json::<RunnerKeyResponse>().await {
        Ok(key) => key,
        Err(e) => {
            reporter.warning(format!("could not read the runner key: {e}"));
            return None;
        }
    };
    if !is_age_recipient(&key.public_key) {
        reporter
            .warning("the platform returned a malformed runner key, so nothing was sealed to it");
        return None;
    }
    Some(RunnerKey {
        recipient: key.public_key,
        key_id: key.key_id,
    })
}

/// Directory names excluded when packaging the app source for a remote build:
/// the Peko cache/keys, build output, installed dependencies, VCS metadata, and
/// framework build caches. The runner reinstalls dependencies and rebuilds.
const SOURCE_EXCLUDE_DIRS: &[&str] = &[
    ".peko",
    "build",
    "node_modules",
    "target",
    ".git",
    "dist",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".astro",
    ".output",
    ".vite",
];

/// Recursively copy the project source tree, skipping [`SOURCE_EXCLUDE_DIRS`]
/// and `.DS_Store`. Produces a clean, buildable source snapshot for the remote
/// Mac runner — no cache, no installed dependencies, no signing keys.
fn copy_source_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let from = entry.path();
        let to = dst.join(name.as_ref());
        if entry.file_type()?.is_dir() {
            if SOURCE_EXCLUDE_DIRS.contains(&name.as_ref()) {
                continue;
            }
            copy_source_tree(&from, &to)?;
        } else if name != ".DS_Store" {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Run one build in-process, reusing all build, bundle, and signing logic via
/// `build::execute` with a derived flag set (the demo/release kind and a single
/// `--platform`). Returns `true` on success. The working directory is already
/// the project root, which `execute` builds from.
async fn run_build(
    cli_info: &CLIInfo,
    os: &OperatingSystem,
    kind: BuildKind,
    reporter: &Reporter,
) -> bool {
    let mut flags = crate::cli::data_structures::Flags::default();
    flags.set_flag(kind.flag_name(), None::<String>);
    flags.set_flag("platform", Some(os.to_string()));
    let build_cli = cli_info.with_flags(flags);
    crate::commands::build::execute(&build_cli, reporter).await == ExitCode::SUCCESS
}

/// Recursively copy a directory tree. Errors if the source is missing.
fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    if !src.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("build output {} is missing", src.display()),
        ));
    }
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Pack the staging directory into a compressed `.pkdeploy` bundle
/// (`zstd(tar(staging))`).
fn pack_deploy(staging: &Path) -> Result<Vec<u8>, String> {
    let mut tar = tar::Builder::new(Vec::new());
    tar.append_dir_all(".", staging)
        .map_err(|e| format!("could not archive the deploy artifacts: {e}"))?;
    let tar_bytes = tar
        .into_inner()
        .map_err(|e| format!("could not finalize the archive: {e}"))?;
    zstd::encode_all(&tar_bytes[..], 19).map_err(|e| format!("could not compress the bundle: {e}"))
}

/// The `POST …/deploys` (start) response: a signed storage URL to PUT the
/// bundle to, and the path to call once the upload finishes.
#[derive(Deserialize)]
struct StartResponse {
    #[serde(rename = "releaseId")]
    release_id: String,
    upload: UploadTarget,
    /// Path (or absolute URL) of the completion endpoint.
    complete: String,
    #[serde(rename = "maxBytes")]
    max_bytes: Option<u64>,
}

/// The signed storage target: PUT the bundle bytes here with `content_type`.
#[derive(Deserialize)]
struct UploadTarget {
    url: String,
    #[serde(default = "default_put_method")]
    method: String,
    #[serde(rename = "contentType")]
    content_type: Option<String>,
}

fn default_put_method() -> String {
    "PUT".to_owned()
}

/// The `GET …/status` response while the platform unpacks the bundle.
#[derive(Deserialize)]
struct StatusResponse {
    status: String,
    #[serde(default)]
    error: Option<String>,
}

/// The result of attempting to push the bundle to the platform.
enum PushOutcome {
    /// The platform accepted the upload.
    Uploaded,
    /// The platform intake is not available yet; the local bundle is kept.
    Skipped,
    /// The upload failed for a reason the user should see.
    Failed(String),
}

/// Upload the deploy bundle via the platform's three-hop signed-upload flow.
///
/// A real bundle is far larger than a function body limit, so the bytes go
/// straight to storage: `POST /api/apps/{id}/deploys` (start) returns a signed
/// storage URL, the CLI `PUT`s the `.pkdeploy` there directly, then
/// `POST …/deploys/{releaseId}/complete` starts extraction, and the CLI polls
/// `GET …/status` until the draft opens. Mirrors `deploy server`.
async fn push_bundle(
    cli_info: &CLIInfo,
    ui: &crate::project::UIProjectInfo,
    bundle: Vec<u8>,
    reporter: &Reporter,
) -> PushOutcome {
    let Some(app_id) = ui.app_id.clone() else {
        reporter.warning("project is not linked to a platform app; skipping upload");
        reporter.help("run `peko link <app-id>` to link it (from your app's dashboard)");
        return PushOutcome::Skipped;
    };

    let session = match crate::auth::Session::load() {
        Some(session) => session,
        None => {
            reporter.warning("not signed in; skipping upload");
            reporter.help("run `peko login`, then re-run `peko deploy app`");
            return PushOutcome::Skipped;
        }
    };
    let id_token = match crate::auth::fresh_id_token(&session).await {
        Ok(token) => token,
        Err(e) => return PushOutcome::Failed(format!("could not refresh the session: {e}")),
    };

    let base = crate::auth::platform_base(cli_info.flags.get_flag("base"));
    let http = match reqwest::Client::builder()
        .user_agent(concat!("peko-cli/", env!("CARGO_PKG_VERSION")))
        .build()
    {
        Ok(client) => client,
        Err(e) => return PushOutcome::Failed(format!("could not build the HTTP client: {e}")),
    };

    // 1. Start: request a signed storage URL for this release.
    reporter.status("Uploading", "requesting an upload slot");
    let start_url = format!("{base}/api/apps/{app_id}/deploys");
    let start_resp = match http
        .post(&start_url)
        .bearer_auth(&id_token)
        .json(&serde_json::json!({ "version": ui.version }))
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(e) => return PushOutcome::Failed(format!("network error starting the deploy: {e}")),
    };
    if !start_resp.status().is_success() {
        let status = start_resp.status().as_u16();
        return PushOutcome::Failed(
            request_error("could not start the deploy", status, start_resp).await,
        );
    }
    let start: StartResponse = match start_resp.json().await {
        Ok(start) => start,
        Err(e) => return PushOutcome::Failed(format!("could not read the upload slot: {e}")),
    };

    if let Some(max) = start.max_bytes
        && bundle.len() as u64 > max
    {
        return PushOutcome::Failed(format!(
            "bundle is {:.1} MiB, over the platform limit of {:.1} MiB",
            bundle.len() as f64 / (1024.0 * 1024.0),
            max as f64 / (1024.0 * 1024.0)
        ));
    }

    // 2. PUT the bundle straight to storage via the signed URL (no bearer).
    reporter.status(
        "Uploading",
        format!(
            "{:.1} MiB to storage",
            bundle.len() as f64 / (1024.0 * 1024.0)
        ),
    );
    let method =
        reqwest::Method::from_bytes(start.upload.method.as_bytes()).unwrap_or(reqwest::Method::PUT);
    let mut put = http.request(method, &start.upload.url).body(bundle);
    if let Some(content_type) = &start.upload.content_type {
        put = put.header(reqwest::header::CONTENT_TYPE, content_type);
    }
    let put_resp = match put.send().await {
        Ok(resp) => resp,
        Err(e) => return PushOutcome::Failed(format!("network error uploading to storage: {e}")),
    };
    if !put_resp.status().is_success() {
        let status = put_resp.status().as_u16();
        return PushOutcome::Failed(
            request_error("upload to storage failed", status, put_resp).await,
        );
    }

    // 3. Complete: the platform verifies the object and starts unpacking.
    reporter.status("Uploading", "finalizing the upload");
    let complete_url = if start.complete.starts_with("http") {
        start.complete.clone()
    } else {
        format!("{base}{}", start.complete)
    };
    let complete_resp = match http
        .post(&complete_url)
        .bearer_auth(&id_token)
        .json(&serde_json::json!({}))
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(e) => return PushOutcome::Failed(format!("network error finalizing the upload: {e}")),
    };
    if !complete_resp.status().is_success() {
        let status = complete_resp.status().as_u16();
        return PushOutcome::Failed(
            request_error("could not finalize the deploy", status, complete_resp).await,
        );
    }

    // 4. Poll until the platform unpacks the bundle into a draft.
    reporter.status("Processing", "the platform is unpacking the bundle");
    let status_url = format!(
        "{base}/api/apps/{app_id}/deploys/{}/status",
        start.release_id
    );
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        let Ok(resp) = http.get(&status_url).bearer_auth(&id_token).send().await else {
            continue;
        };
        if !resp.status().is_success() {
            continue;
        }
        let Ok(status) = resp.json::<StatusResponse>().await else {
            continue;
        };
        match status.status.as_str() {
            "draft" => return PushOutcome::Uploaded,
            "failed" => {
                let reason = status.error.unwrap_or_else(|| "unknown error".to_owned());
                return PushOutcome::Failed(format!(
                    "the platform could not process the bundle: {reason}"
                ));
            }
            _ => {} // uploading | unpacking — keep polling
        }
    }

    // The upload landed; extraction is still running on the platform.
    reporter.warning("still unpacking on the platform; the draft will appear on your dashboard");
    PushOutcome::Uploaded
}

/// Format a failed HTTP response with the platform's error detail (bounded).
async fn request_error(context: &str, status: u16, response: reqwest::Response) -> String {
    let body = response.text().await.unwrap_or_default();
    let trimmed = body.trim();
    // Every authenticated route sits behind the platform's legal gate, which
    // answers before the handler runs. It shares status 403 with an unverified
    // email, so it is recognized by `code` and explained plainly — otherwise it
    // reads as an unrelated permission failure.
    if serde_json::from_str::<serde_json::Value>(trimmed)
        .ok()
        .and_then(|body| body.get("code")?.as_str().map(str::to_owned))
        .as_deref()
        == Some("legal_required")
    {
        return format!(
            "{context}: the Peko terms have been updated. Sign in at {} and accept them, then re-run this command.",
            crate::auth::platform_base(None)
        );
    }
    if trimmed.is_empty() {
        format!("{context} (HTTP {status})")
    } else {
        format!(
            "{context} (HTTP {status}): {}",
            trimmed.chars().take(500).collect::<String>()
        )
    }
}

/// Make a name safe for a file name (alphanumerics, `-`, `_`, `.`).
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Prompt the user for a yes/no confirmation. Returns `default` when `--yes` is
/// set or the terminal is not interactive.
fn confirm(assume_yes: bool, prompt: &str, default: bool) -> bool {
    if assume_yes {
        return true;
    }
    dialoguer::Confirm::new()
        .with_prompt(prompt)
        .default(default)
        .interact()
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_well_formed_age_recipients() {
        // The key actually registered for the build Mac.
        assert!(is_age_recipient(
            "age1s0a0rh236mx3qzws8zvvft90w6lmpye5l2drp6w8sj0tgajdmpzs5u96kv"
        ));
        // A freshly generated one, whatever it happens to be.
        let (public, secret) = crate::deploy_seal::generate_runner_key();
        assert!(is_age_recipient(&public));
        // The secret half must never be accepted as a recipient.
        assert!(!is_age_recipient(&secret));
        assert!(!is_age_recipient(""));
        assert!(!is_age_recipient("age1"));
        assert!(!is_age_recipient("age1short"));
        // Uppercase and separators are outside the bech32 charset.
        assert!(!is_age_recipient(
            "age1S0A0RH236MX3QZWS8ZVVFT90W6LMPYE5L2DRP6W8SJ0TGAJDMPZS5U96KV"
        ));
        assert!(!is_age_recipient(
            "age1s0a0rh236mx3qzws8zvvft90w6lmpye5l2drp6w8sj0tgajdmpzs5u96k-"
        ));
        // Right shape, wrong prefix.
        assert!(!is_age_recipient(
            "AGE-SECRET-KEY-1QQPQYQ5QVQGQGPQYQ5QVQGQGPQYQ5QVQGQGPQYQ5QVQGQGPQ"
        ));
    }
}
