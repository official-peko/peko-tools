//! `peko deploy`: publish a package to the registry (`deploy package`) or
//! deploy the app to Peko server hosting (`deploy server`).
//!
//! `deploy server` builds the project's web app, packages the Next.js
//! standalone output into a Dockerized artifact, and hands it to the platform,
//! which builds and runs it on its own infrastructure. `deploy package` packs
//! the enclosing library package into a `.pkpkg`, verifies it locally, then
//! uploads it to the registry through the platform publish handshake. Both talk
//! to the platform with the same bearer session from `peko login`; the HTTP
//! handshakes live in `crate::deploy` and `crate::registry`. A verified email
//! is required.

use std::process::ExitCode;

use peko_core::config::{Manifest, ManifestKind};

use crate::cli::CLIInfo;
use crate::cli::reporting::Reporter;
use crate::deploy::DeployError;
use crate::project::PekoProject;
use crate::registry::pack;

/// Execute the `deploy` subcommand.
pub async fn execute(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    let Some(target) = cli_info.arguments.get(1) else {
        reporter.error("`deploy` requires a target");
        reporter.help(format!(
            "run '{} help deploy' to see how this command works",
            cli_info.executable
        ));
        return ExitCode::FAILURE;
    };
    match target.as_str() {
        "app" => crate::deploy_app::run(cli_info, reporter).await,
        "server" => deploy_server(cli_info, reporter).await,
        "package" => deploy_package(cli_info, reporter).await,
        "runner-keygen" => deploy_runner_keygen(cli_info, reporter),
        "runner-pubkey" => deploy_runner_pubkey(cli_info, reporter),
        "unseal" => deploy_unseal(cli_info, reporter),
        other => {
            reporter.error(format!("unknown deploy target '{other}'"));
            reporter.help(
                "targets are 'app' (build and submit an app), 'package' (publish to the registry), and 'server' (Peko server hosting)",
            );
            ExitCode::FAILURE
        }
    }
}

/// `deploy runner-keygen`: generate the remote build worker's keypair. Writes
/// the secret identity to `--out` (default `~/.peko/runner.key`, mode 0600) and
/// prints the public recipient (`age1…`) to register with the platform. Run once
/// during Mac setup.
fn deploy_runner_keygen(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    let out = cli_info
        .flags
        .get_flag("out")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(default_runner_key_path);
    // Replacing a runner key invalidates every bundle already sealed to the old
    // one, so never clobber silently.
    if out.exists() && !cli_info.flags.has_flag("force") {
        reporter.error(format!(
            "a runner key already exists at {}",
            out.display()
        ));
        reporter.help(
            "use `peko deploy runner-pubkey` to print its public half; pass --force to replace it (this invalidates bundles sealed to the old key)",
        );
        return ExitCode::FAILURE;
    }
    let (public, secret) = crate::deploy_seal::generate_runner_key();
    if let Some(parent) = out.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&out, format!("{secret}\n")) {
        reporter.error(format!("could not write the runner key to {}: {e}", out.display()));
        return ExitCode::FAILURE;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o600));
    }
    reporter.success(format!("wrote the runner secret key to {}", out.display()));
    reporter.info("register this public key with the platform (it is not secret):");
    println!("{public}");
    ExitCode::SUCCESS
}

/// `deploy runner-pubkey`: print the public half of an existing runner key
/// (`--key`, default `~/.peko/runner.key`) to stdout. Lets a Mac re-register
/// with the platform without generating a new key, which would invalidate every
/// bundle sealed to the old one.
fn deploy_runner_pubkey(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    let path = cli_info
        .flags
        .get_flag("key")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(default_runner_key_path);
    let secret = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) => {
            reporter.error(format!(
                "could not read the runner key at {}: {e}",
                path.display()
            ));
            reporter.help("generate one with `peko deploy runner-keygen`");
            return ExitCode::FAILURE;
        }
    };
    match crate::deploy_seal::public_from_secret(&secret) {
        Ok(public) => {
            println!("{public}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            reporter.error(e);
            ExitCode::FAILURE
        }
    }
}

/// `deploy unseal`: on the Mac build worker, decrypt the deploy bundle's sealed
/// signing material with the runner key. Reads `--sealed <blob>` and
/// `--key <secret-file>` (default `~/.peko/runner.key`), extracts into `--out`
/// (default `<sealed-dir>/signing`), and prints the per-platform paths + password
/// as JSON for the build worker to pass to `peko build`'s headless flags.
fn deploy_unseal(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    let Some(sealed_path) = cli_info.flags.get_flag("sealed").map(std::path::PathBuf::from) else {
        reporter.error("`deploy unseal` needs --sealed <blob>");
        return ExitCode::FAILURE;
    };
    let key_path = cli_info
        .flags
        .get_flag("key")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(default_runner_key_path);
    let out = cli_info
        .flags
        .get_flag("out")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            sealed_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join("signing")
        });

    let secret = match std::fs::read_to_string(&key_path) {
        Ok(text) => text.trim().to_owned(),
        Err(e) => {
            reporter.error(format!("could not read the runner key at {}: {e}", key_path.display()));
            return ExitCode::FAILURE;
        }
    };
    let sealed = match std::fs::read(&sealed_path) {
        Ok(bytes) => bytes,
        Err(e) => {
            reporter.error(format!("could not read {}: {e}", sealed_path.display()));
            return ExitCode::FAILURE;
        }
    };
    let platforms = match crate::deploy_seal::unseal_to_dir(&secret, &sealed, &out) {
        Ok(platforms) => platforms,
        Err(e) => {
            reporter.error(format!("could not unseal the signing material: {e}"));
            return ExitCode::FAILURE;
        }
    };
    // Emit absolute paths so the build worker can hand them to `peko build`.
    let resolved: std::collections::BTreeMap<String, serde_json::Value> = platforms
        .into_iter()
        .map(|(platform, material)| {
            let abs = |rel: &str| out.join(rel).to_string_lossy().into_owned();
            (
                platform,
                serde_json::json!({
                    "p12": abs(&material.p12),
                    "password": material.password,
                    "profile": material.profile.as_deref().map(abs),
                    "entitlements": material.entitlements.as_deref().map(abs),
                    "installer_p12": material.installer_p12.as_deref().map(abs),
                    "installer_password": material.installer_password,
                }),
            )
        })
        .collect();
    reporter.success(format!("unsealed signing material into {}", out.display()));
    println!("{}", serde_json::to_string_pretty(&resolved).unwrap_or_default());
    ExitCode::SUCCESS
}

/// The default runner key path, `~/.peko/runner.key` (the poller's config dir,
/// distinct from `PEKO_ROOT_PATH`).
fn default_runner_key_path() -> std::path::PathBuf {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    home.join(".peko").join("runner.key")
}

/// `deploy package`: pack the enclosing library package into a `.pkpkg`, verify
/// it locally, and upload it to the registry. Authentication is the session
/// from `peko login`. The server reads the package metadata from the embedded
/// `peko.toml`, validates it, and queues the version for admin review.
async fn deploy_package(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    let cwd = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(e) => {
            reporter.error(format!("cannot read current directory: {e}"));
            return ExitCode::FAILURE;
        }
    };

    let loaded = match Manifest::discover(&cwd) {
        Ok(loaded) => loaded,
        Err(e) => {
            reporter.error(format!("could not load a peko.toml here: {e}"));
            return ExitCode::FAILURE;
        }
    };

    if loaded.manifest.kind() != ManifestKind::Package {
        reporter.error("only packages can be published");
        reporter.help("a publishable package defines a [package] and [lib] table");
        return ExitCode::FAILURE;
    }

    let name = loaded.manifest.name().to_owned();
    let version = loaded.manifest.version().to_string();

    let progress = reporter.progress();
    progress.start_phase(&format!("Packing {name} {version}"));
    let bytes = match pack::pack(&loaded) {
        Ok(bytes) => bytes,
        Err(e) => {
            progress.finish_phase();
            reporter.error(format!("could not pack package: {e}"));
            return ExitCode::FAILURE;
        }
    };
    progress.finish_phase();

    // Verify the packed container before anything leaves the machine. A package
    // that fails verification must never be uploaded.
    let report = crate::registry::verify::verify(&bytes);
    for finding in &report.findings {
        match finding.severity {
            crate::registry::verify::Severity::Error => reporter.error(&finding.message),
            crate::registry::verify::Severity::Warning => reporter.warning(&finding.message),
        }
    }
    if !report.is_valid() {
        reporter.error(format!(
            "refusing to publish: the package failed verification with {} error(s)",
            report.error_count()
        ));
        reporter.help(format!("run '{} verify' for the full report", cli_info.executable));
        return ExitCode::FAILURE;
    }

    // Publishing requires a session from `peko login`.
    let Some(session) = crate::auth::Session::load() else {
        reporter.error("not logged in");
        reporter.help(format!(
            "run '{} login' to authenticate before publishing",
            cli_info.executable
        ));
        return ExitCode::FAILURE;
    };
    let base = crate::auth::platform_base(cli_info.flags.get_flag("base"));
    let id_token = match crate::auth::fresh_id_token(&session).await {
        Ok(token) => token,
        Err(crate::auth::AuthError::Unauthorized) => {
            reporter.error("session expired or revoked");
            reporter.help(format!(
                "run '{} login' to authenticate again",
                cli_info.executable
            ));
            return ExitCode::FAILURE;
        }
        Err(e) => {
            reporter.error(format!("could not authenticate: {e}"));
            return ExitCode::FAILURE;
        }
    };

    let progress = reporter.progress();
    progress.start_phase(&format!("Uploading {name} {version}"));
    let outcome = crate::registry::publish::publish(&base, &id_token, &bytes).await;
    progress.finish_phase();

    match outcome {
        Ok(done) => {
            reporter.success(format!(
                "published {} {} ({} bytes)",
                done.name,
                done.version,
                bytes.len()
            ));
            if done.status == "pending" {
                reporter.info(
                    "the version is pending admin review and appears on the public index once approved",
                );
            } else {
                reporter.info(format!("status: {}", done.status));
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            use crate::registry::publish::PublishError;
            match e {
                // A 403 carries the server's own explanation (most often an
                // unverified email). Email verification is a browser/email flow
                // and cannot be done from the CLI, so show the message and stop.
                PublishError::Forbidden(message) => {
                    reporter.error(message);
                    reporter.help(
                        "email verification is a browser step and cannot be done from the CLI; \
                         verify it from your account page on the Peko web app, then publish again",
                    );
                }
                // A 401 now comes from the platform's verification gate; treat
                // it as an invalid session and point at re-login.
                PublishError::Unauthorized => {
                    reporter.error("the platform could not verify this session");
                    reporter.help(format!(
                        "run '{} login' to authenticate again",
                        cli_info.executable
                    ));
                }
                other => reporter.error(format!("publish failed: {other}")),
            }
            ExitCode::FAILURE
        }
    }
}

/// Build, package, and deploy the current project to server hosting.
/// Emit a terminal `result` event for a deploy, in JSON mode only.
///
/// The streamed status lines already describe progress, but a host driving a
/// deploy (the IDE panel) needs the outcome as data rather than prose: whether
/// it succeeded, whether it is still building, and the resulting URL. Emitting
/// one final event keeps that contract explicit instead of leaving the host to
/// match on the wording of the last success line.
fn emit_deploy_result(reporter: &Reporter, event: serde_json::Value) {
    reporter.emit_json(event);
}

pub(crate) async fn deploy_server(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    let project = match PekoProject::from_current_directory() {
        Ok(project) => project,
        Err(e) => {
            reporter.error(format!("could not load a peko.toml here: {e}"));
            return ExitCode::FAILURE;
        }
    };

    // Only a server-framework app can be hosted this way.
    let is_server = project
        .ui_project_info
        .as_ref()
        .is_some_and(|ui| ui.framework == "server");
    if !is_server {
        reporter.error("`deploy server` needs a server app");
        reporter.help("set `framework = \"server\"` under [ui] in peko.toml");
        return ExitCode::FAILURE;
    }

    // The platform app id links this project to a hosted app. `--app-id`
    // overrides the manifest value for a one-off.
    let app_id = match cli_info
        .flags
        .get_flag("app-id")
        .or_else(|| project.app_id.clone())
    {
        Some(id) if !id.is_empty() => id,
        _ => {
            reporter.error("this project is not linked to a platform app");
            reporter.help(
                "set the app id under [project].app_id in peko.toml (from your app's dashboard), \
                 or pass --app-id <id>",
            );
            return ExitCode::FAILURE;
        }
    };

    // Deploying requires a session from `peko login`.
    let Some(session) = crate::auth::Session::load() else {
        reporter.error("not logged in");
        reporter.help(format!(
            "run '{} login' to authenticate before deploying",
            cli_info.executable
        ));
        return ExitCode::FAILURE;
    };
    let base = crate::auth::platform_base(cli_info.flags.get_flag("base"));

    // Build the web app (npm run build → the Next.js standalone output). The
    // server frontend is always built fresh here (this runs on the dev host,
    // which has node), never from a prebuilt dist.
    if let Err(code) = crate::commands::build::build_web_frontend(
        &project,
        cli_info.get_peko_root(),
        None,
        reporter,
    ) {
        return code;
    }

    // Package the built output into the deploy artifact, per the SSR framework
    // (defaults to Next when unset).
    let server_framework = project
        .ui_project_info
        .as_ref()
        .and_then(|ui| ui.server_framework.clone())
        .unwrap_or_else(|| "next".to_owned());
    let progress = reporter.progress();
    progress.start_phase(&format!("Packaging the app ({server_framework})"));
    let artifact = match crate::deploy::build_artifact(project.get_root(), &server_framework) {
        Ok(bytes) => bytes,
        Err(e) => {
            progress.finish_phase();
            reporter.error(format!("could not package the app: {e}"));
            return ExitCode::FAILURE;
        }
    };
    progress.finish_phase();

    // A fresh bearer token per deploy, refreshed the same way as publish.
    let id_token = match crate::auth::fresh_id_token(&session).await {
        Ok(token) => token,
        Err(crate::auth::AuthError::Unauthorized) => {
            reporter.error("session expired or revoked");
            reporter.help(format!(
                "run '{} login' to authenticate again",
                cli_info.executable
            ));
            return ExitCode::FAILURE;
        }
        Err(e) => {
            reporter.error(format!("could not authenticate: {e}"));
            return ExitCode::FAILURE;
        }
    };

    // The ALB health-checks `/` by default; every scaffolded SSR framework
    // serves it. `--health-path` overrides for an app whose root does not 200.
    let health_path = cli_info.flags.get_flag("health-path");

    let size = artifact.len();
    let progress = reporter.progress();
    progress.start_phase(&format!("Uploading {} ({size} bytes)", project.name));
    let outcome = crate::deploy::deploy(
        &base,
        &id_token,
        &app_id,
        &server_framework,
        health_path.as_deref(),
        artifact,
    )
    .await;
    progress.finish_phase();

    match outcome {
        Ok(done) => {
            // Persist the serving host so the next build bakes bundle::host and
            // the produced app loads from https://<host>.
            if let Some(host) = &done.host {
                let manifest_path = project.get_root().join("peko.toml");
                match peko_core::config::Manifest::write_host(&manifest_path, host) {
                    Ok(()) => reporter
                        .info(format!("linked host {host} in peko.toml (baked into the next build)")),
                    Err(e) => reporter
                        .warning(format!("deployed, but could not write the host to peko.toml: {e}")),
                }
            }
            let url = done
                .url
                .clone()
                .or_else(|| done.host.as_ref().map(|h| format!("https://{h}")));
            match &url {
                Some(url) => reporter.success(format!("deploying to {url} ({size} bytes uploaded)")),
                None => reporter.success(format!("deploy started ({size} bytes uploaded)")),
            }

            // Unless asked not to, wait for the build to go live.
            if cli_info.flags.has_flag("no-wait") {
                reporter.info("not waiting; track progress on your dashboard at https://app.pekoui.com");
                emit_deploy_result(
                    reporter,
                    serde_json::json!({
                        "type": "result", "ok": true, "kind": "server",
                        "state": "building", "appId": app_id, "url": url,
                    }),
                );
                return ExitCode::SUCCESS;
            }
            reporter.info("building and releasing (this can take a few minutes)...");
            match wait_for_live(&base, &id_token, &app_id, &done.deployment_id, reporter).await {
                LiveResult::Live(live_url) => {
                    let shown = live_url.or(url).unwrap_or_default();
                    reporter.success(format!("live at {shown}"));
                    emit_deploy_result(
                        reporter,
                        serde_json::json!({
                            "type": "result", "ok": true, "kind": "server",
                            "state": "live", "appId": app_id, "url": shown,
                        }),
                    );
                    ExitCode::SUCCESS
                }
                LiveResult::Failed(err) => {
                    reporter.error(format!("deploy failed: {err}"));
                    emit_deploy_result(
                        reporter,
                        serde_json::json!({
                            "type": "result", "ok": false, "kind": "server",
                            "state": "failed", "appId": app_id, "error": err,
                        }),
                    );
                    ExitCode::FAILURE
                }
                LiveResult::Timeout => {
                    reporter.warning(
                        "still building after the wait window; check your dashboard for the result",
                    );
                    if let Some(url) = &url {
                        reporter.info(format!("it will serve at {url} once live"));
                    }
                    // Still building is not a failure, but it is not a finished
                    // deploy either; the host should keep showing it as pending
                    // rather than report success.
                    emit_deploy_result(
                        reporter,
                        serde_json::json!({
                            "type": "result", "ok": true, "kind": "server",
                            "state": "building", "appId": app_id, "url": url,
                        }),
                    );
                    ExitCode::SUCCESS
                }
            }
        }
        Err(e) => {
            match e {
                // A 403 carries the server's explanation, usually an unverified
                // email — a browser/email step the CLI cannot do.
                DeployError::Forbidden(message) => {
                    reporter.error(message);
                    reporter.help(
                        "email verification is a browser step and cannot be done from the CLI; \
                         verify it from your account page on the Peko web app, then deploy again",
                    );
                }
                DeployError::Unauthorized => {
                    reporter.error("the platform could not verify this session");
                    reporter.help(format!(
                        "run '{} login' to authenticate again",
                        cli_info.executable
                    ));
                }
                DeployError::NotConfigured => {
                    reporter.error("server hosting is not available on the platform yet");
                    reporter
                        .help("try again once the platform's hosting backend is enabled");
                }
                DeployError::NotFound => {
                    reporter.error("that app was not found on your account");
                    reporter.help(
                        "check [project].app_id and that you are logged in as the app's owner",
                    );
                }
                DeployError::BadRequest(message) => reporter.error(message),
                other => reporter.error(format!("deploy failed: {other}")),
            }
            ExitCode::FAILURE
        }
    }
}

/// The terminal outcome of waiting for a deploy to go live.
enum LiveResult {
    /// Reached `live`; the optional URL is the reported live URL.
    Live(Option<String>),
    /// Reached `failed`; the string is the server's explanation.
    Failed(String),
    /// Did not settle within the wait window.
    Timeout,
}

/// Poll the deploy status until it goes live or fails, or the wait window
/// elapses (~10 minutes: CodeBuild plus Fargate startup). Reports each status
/// transition. Transient errors and a not-yet-recorded deployment keep polling.
async fn wait_for_live(
    base: &str,
    id_token: &str,
    app_id: &str,
    deployment_id: &str,
    reporter: &Reporter,
) -> LiveResult {
    let mut last = String::new();
    for _ in 0..150 {
        tokio::time::sleep(std::time::Duration::from_secs(4)).await;
        // A not-yet-recorded deployment (Ok(None)) or a transient error keeps
        // polling; only a real status advances the outcome.
        if let Ok(Some(status)) =
            crate::deploy::deploy_status(base, id_token, app_id, deployment_id).await
        {
            if status.status == "live" {
                return LiveResult::Live(status.url);
            }
            if status.status == "failed" {
                return LiveResult::Failed(
                    status.error.unwrap_or_else(|| "the build failed".to_owned()),
                );
            }
            if status.status != last {
                reporter.info(format!("status: {}", status.status));
                last = status.status;
            }
        }
    }
    LiveResult::Timeout
}
