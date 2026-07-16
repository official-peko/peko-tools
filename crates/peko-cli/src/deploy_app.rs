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
//! Non-Apple targets (Windows/Android/Linux) always build locally. Apple
//! targets build locally on a Mac host; on a non-Mac host the app source is
//! packaged into the bundle (under `source/`, cache/deps/keys excluded) for the
//! remote Mac builder to build with headless signing — the runner itself is not
//! yet implemented, but the bundle it will consume is complete.

use std::path::Path;
use std::process::{Command, ExitCode};

use peko_core::target::OperatingSystem;
use serde::Serialize;

use crate::bundler::signing;
use crate::cli::CLIInfo;
use crate::cli::reporting::Reporter;
use crate::commands::platform_label;
use crate::project::PekoProject;

/// Which of the two builds to produce for a platform.
#[derive(Clone, Copy)]
enum BuildKind {
    /// Debug + demo, unsigned. Drives screenshot / recording generation.
    Generation,
    /// Signed release, demo stripped. The submission binary.
    Submission,
}

impl BuildKind {
    /// The `peko build` flag that selects this build.
    fn flag(self) -> &'static str {
        match self {
            BuildKind::Generation => "--demo",
            BuildKind::Submission => "--release",
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
/// non-Mac host.
#[derive(Serialize)]
struct RemoteBuild {
    /// The platforms the runner builds from source (`macos`, `ios`).
    targets: Vec<String>,
    /// Bundle-relative path to the packaged project source tree.
    source: String,
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
    /// Present when the bundle carries app source for a remote Apple build.
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
    let Ok(exe) = std::env::current_exe() else {
        reporter.error("could not locate the peko executable to run the builds");
        return ExitCode::FAILURE;
    };

    for os in &local_targets {
        let label = platform_label(os).unwrap();
        for kind in [BuildKind::Generation, BuildKind::Submission] {
            reporter.status("Building", format!("{label}: {}", kind.label()));
            if let Err(e) = run_build(&exe, &root, os, kind) {
                reporter.error(format!("{label} {} build failed: {e}", kind.label()));
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

    // Package the app source for remote Apple targets so the Mac runner can
    // build them (Build 1 + Build 2) with headless signing. The build cache,
    // build output, node_modules, VCS metadata, and signing keys are excluded.
    let remote_build = if remote_build_requested && !remote_targets.is_empty() {
        reporter.status("Packaging", "app source for the remote Apple builder");
        if let Err(e) = copy_source_tree(&root, &staging.join("source")) {
            reporter.error(format!("could not package the app source: {e}"));
            return ExitCode::FAILURE;
        }
        Some(RemoteBuild {
            targets: remote_targets.iter().map(|os| os.to_string()).collect(),
            source: "source".to_owned(),
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

    match push_bundle(cli_info, &ui, &project.name, bundle_bytes, reporter).await {
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

/// Run one `peko build` as a subprocess, reusing all build, bundle, and signing
/// logic. Streams the build's output to the terminal.
fn run_build(exe: &Path, root: &Path, os: &OperatingSystem, kind: BuildKind) -> Result<(), String> {
    let status = Command::new(exe)
        .arg("build")
        .arg(kind.flag())
        .arg("--platform")
        .arg(os.to_string())
        .current_dir(root)
        .status()
        .map_err(|e| format!("could not launch peko build: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("peko build exited with {status}"))
    }
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

/// The result of attempting to push the bundle to the platform.
enum PushOutcome {
    /// The platform accepted the upload.
    Uploaded,
    /// The platform intake is not available yet; the local bundle is kept.
    Skipped,
    /// The upload failed for a reason the user should see.
    Failed(String),
}

/// Upload the deploy bundle to the platform app-deploy intake.
///
/// Contract (platform finalizes the shape): `POST {base}/api/apps/{app_id}/
/// deploys` with a bearer session and the bundle as multipart `bundle`. This is
/// coordination work like the gated-download intake; until it is live, a 404 /
/// 501 is treated as "not available yet" and the local bundle is kept.
async fn push_bundle(
    cli_info: &CLIInfo,
    ui: &crate::project::UIProjectInfo,
    app_name: &str,
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

    reporter.status("Uploading", "deploy bundle to the platform");
    let filename = format!("{}-{}.pkdeploy", sanitize(app_name), ui.version);
    let part = reqwest::multipart::Part::bytes(bundle).file_name(filename);
    let form = reqwest::multipart::Form::new()
        .text("version", ui.version.clone())
        .part("bundle", part);

    let url = format!("{base}/api/apps/{app_id}/deploys");
    let response = match http
        .post(&url)
        .bearer_auth(&id_token)
        .multipart(form)
        .send()
        .await
    {
        Ok(response) => response,
        Err(e) => return PushOutcome::Failed(format!("network error: {e}")),
    };

    match response.status().as_u16() {
        200..=202 => PushOutcome::Uploaded,
        401 => PushOutcome::Failed("not authorized (sign in as the app owner)".to_owned()),
        403 => PushOutcome::Failed("forbidden (you are not the app owner)".to_owned()),
        // The intake is not live yet — keep the local bundle.
        404 | 501 => {
            reporter.warning("the platform deploy intake is not available yet");
            PushOutcome::Skipped
        }
        other => PushOutcome::Failed(format!("unexpected status {other}")),
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
