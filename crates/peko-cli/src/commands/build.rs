//! `peko build`: build the project for one or more target platforms.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use peko_core::target::{OperatingSystem, PekoTarget};

use crate::bundler::{self, BundleError};
use crate::cli::CLIInfo;
use crate::cli::reporting::{ProgressSink, Reporter};
use crate::commands::platform_label;
use crate::execution;
use crate::project::PekoProject;

/// What went wrong for one platform.
enum PlatformFailure {
    /// The build itself failed (compile errors or tooling error).
    Build(BundleError),
    /// The build succeeded but the release signing step failed or
    /// produced no signed artifact.
    Sign(Option<BundleError>),
}

/// Execute the `build` subcommand.
pub async fn execute(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    // `--prebuild` runs the library-package prebuild flow (cross-compile every
    // platform, generate stubs + FFI headers, write prebuilt.toml, and pack the
    // all-platforms `.pkbundle`) instead of an ordinary project build.
    if cli_info.flags.has_flag("prebuild") {
        return crate::execution::prebuild::run(cli_info, reporter);
    }

    let project = match PekoProject::from_current_directory() {
        Ok(project) => project,
        Err(e) => {
            reporter.error(format!("could not load project: {e}"));
            return ExitCode::FAILURE;
        }
    };

    let release = cli_info.flags.has_flag("release");
    let build_directory = project.get_root().join(if release {
        "build/release"
    } else {
        "build/debug"
    });

    // A build following a `peko run` must discard the run's hot-reload
    // incremental cache (marked by `.peko/incremental/run`) so the distributable
    // build does not reuse run-instrumented objects. A standalone `peko clean`
    // handles explicit cleaning.
    let force_clean_marker = project.get_root().join(".peko/incremental/run");
    if force_clean_marker.exists() {
        let incremental_dir = project.get_root().join(".peko/incremental");
        let _ = std::fs::remove_dir_all(&incremental_dir);
    }

    // Resolve, download, and lock declared dependencies before compiling.
    let progress = reporter.progress();
    progress.start_phase("Resolving dependencies");
    let ensured = crate::registry::install::ensure_dependencies(
        cli_info.get_peko_root(),
        project.get_root(),
        progress,
    )
    .await;
    progress.finish_phase();
    if let Err(e) = ensured {
        reporter.error(format!("could not resolve dependencies: {e}"));
        return ExitCode::FAILURE;
    }

    if project.ui_project_info.is_none() {
        build_cli_project(cli_info, project, build_directory, reporter)
    } else {
        build_ui_project(cli_info, project, build_directory, release, reporter)
    }
}

/// Build the web frontend of a static UI project into `assets/`.
///
/// When the project carries a `package.json`, install its dependencies once
/// (when `node_modules` is absent) and run its build script. The scaffolded
/// Vite config writes into `assets/`, which the platform bundlers then embed.
/// A project without a `package.json` is left as-is, so a hand-authored
/// `assets/` still works.
pub(crate) fn build_web_frontend(
    project: &PekoProject,
    peko_root: &std::path::Path,
    web_dist: Option<&std::path::Path>,
    reporter: &Reporter,
) -> Result<(), ExitCode> {
    let root = project.get_root();
    if !root.join("package.json").is_file() {
        return Ok(());
    }

    // A remote build (a Mac with no node) supplies the frontend the dev machine
    // already built: substitute it into `assets/` and skip npm entirely.
    if let Some(web_dist) = web_dist {
        reporter.status("Building", "web app (prebuilt, from the deploy bundle)");
        if let Err(e) = replace_dir(web_dist, &root.join("assets")) {
            reporter.error(format!("could not install the prebuilt web frontend: {e}"));
            return Err(ExitCode::FAILURE);
        }
        return Ok(());
    }

    // Deliver each registry package's client npm package (`[client]`) into this
    // app's web frontend and register a file: dependency, before installing.
    let deps_changed = wire_client_dependencies(root, peko_root, reporter)?;

    if deps_changed || !root.join("node_modules").is_dir() {
        reporter.status("Installing", "web dependencies (npm install)");
        // npm writes progress to stdout; keep it off the JSON event stream.
        let mut npm = crate::proc::npm();
        crate::proc::route_stdout_to_stderr(&mut npm);
        match npm.arg("install").current_dir(root).status() {
            Ok(status) if status.success() => {}
            Ok(_) => {
                reporter.error("npm install failed");
                return Err(ExitCode::FAILURE);
            }
            Err(e) => {
                reporter.error(format!("could not run npm install: {e}"));
                return Err(ExitCode::FAILURE);
            }
        }
    }

    reporter.status("Building", "web app (npm run build)");
    let mut npm = crate::proc::npm();
    crate::proc::route_stdout_to_stderr(&mut npm);
    match npm.args(["run", "build"]).current_dir(root).status() {
        Ok(status) if status.success() => Ok(()),
        Ok(_) => {
            reporter.error("web build failed (npm run build)");
            Err(ExitCode::FAILURE)
        }
        Err(e) => {
            reporter.error(format!("could not run npm run build: {e}"));
            Err(ExitCode::FAILURE)
        }
    }
}

/// Deliver the client npm package (`[client]`) of every reachable registry
/// package into this app's web frontend: extract its files under
/// `.peko/client/<pkg>/` and register a `file:` dependency for it in the app's
/// package.json. This is the official replacement for hand-vendored client SDKs
/// (the old `sync-*` scripts). Runs on every build (self-healing); npm symlinks
/// the file: dep, so re-copied files are picked up without a reinstall. Returns
/// whether package.json changed (a new/changed dep needs a fresh npm install).
fn wire_client_dependencies(
    project_root: &std::path::Path,
    peko_root: &std::path::Path,
    reporter: &Reporter,
) -> Result<bool, ExitCode> {
    use peko_core::config::Manifest;

    // demo = true: a client JS import is a web-build concern independent of the
    // native demo gate, so wire every client dep (including demo-only ones like
    // pekoshots) so the frontend that imports it always resolves.
    let mut wired: Vec<(String, String)> = Vec::new();
    for root in execution::native::reachable_package_roots(peko_root, project_root, true) {
        let loaded = match Manifest::load(root.join("peko.toml")) {
            Ok(loaded) => loaded,
            Err(_) => continue,
        };
        let Manifest::Package(pkg) = &loaded.manifest else {
            continue;
        };
        let Some(client) = &pkg.client else {
            continue;
        };
        let src = loaded.root.join(&client.root);
        if !src.is_dir() {
            continue;
        }
        let dest_rel = format!(".peko/client/{}", pkg.package.name);
        if let Err(e) = replace_dir(&src, &project_root.join(&dest_rel)) {
            reporter.error(format!(
                "could not stage client package {}: {e}",
                client.name
            ));
            return Err(ExitCode::FAILURE);
        }
        wired.push((client.name.clone(), dest_rel));
    }
    if wired.is_empty() {
        return Ok(false);
    }
    let changed = match register_client_deps(&project_root.join("package.json"), &wired) {
        Ok(changed) => changed,
        Err(e) => {
            reporter.error(format!("could not register client deps in package.json: {e}"));
            return Err(ExitCode::FAILURE);
        }
    };
    reporter.status(
        "Linking",
        format!("{} client package(s) from the registry", wired.len()),
    );
    Ok(changed)
}

/// Recursively copy `src` onto `dest`, clearing `dest` first.
fn replace_dir(src: &std::path::Path, dest: &std::path::Path) -> std::io::Result<()> {
    if dest.exists() {
        std::fs::remove_dir_all(dest)?;
    }
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            replace_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Point a `file:` dependency at each `(npm name, relative path)`, preserving
/// the rest of package.json. Returns whether anything changed.
fn register_client_deps(
    package_json: &std::path::Path,
    deps: &[(String, String)],
) -> std::io::Result<bool> {
    let invalid = |m: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, m.to_string());
    let text = std::fs::read_to_string(package_json)?;
    let mut value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| invalid(&e.to_string()))?;
    let obj = value
        .as_object_mut()
        .ok_or_else(|| invalid("package.json is not an object"))?;
    let table = obj
        .entry("dependencies")
        .or_insert_with(|| serde_json::Value::Object(Default::default()))
        .as_object_mut()
        .ok_or_else(|| invalid("dependencies is not an object"))?;
    let mut changed = false;
    for (name, rel) in deps {
        let want = serde_json::Value::String(format!("file:{rel}"));
        if table.get(name) != Some(&want) {
            table.insert(name.clone(), want);
            changed = true;
        }
    }
    if changed {
        let mut out = serde_json::to_string_pretty(&value).map_err(|e| invalid(&e.to_string()))?;
        out.push('\n');
        std::fs::write(package_json, out)?;
    }
    Ok(changed)
}

/// Wipe the per-mode output build directory (e.g. `build/debug/`) so
/// stale artifacts from previous builds don't leak across runs. Used for a CLI
/// project, whose single host binary is the only output.
fn nuke_output_directory(build_directory: &std::path::Path) -> std::io::Result<()> {
    if build_directory.exists() {
        if build_directory.is_dir() {
            std::fs::remove_dir_all(build_directory)?;
        } else {
            std::fs::remove_file(build_directory)?;
        }
    }
    std::fs::create_dir_all(build_directory)
}

/// Remove only the given platforms' output subdirectories under `build/<mode>`
/// (e.g. `build/debug/windows`), leaving other platforms' outputs untouched. A
/// UI build cleans per platform rather than wiping the whole mode directory so a
/// multi-platform `peko deploy` — which invokes `peko build --platform X` once
/// per platform into the same tree — accumulates every platform's output for the
/// bundle instead of each build erasing the last.
fn clean_platform_outputs(
    build_directory: &std::path::Path,
    platforms: &[OperatingSystem],
) -> std::io::Result<()> {
    std::fs::create_dir_all(build_directory)?;
    for platform in platforms {
        let platform_dir = build_directory.join(platform.to_string());
        if platform_dir.is_dir() {
            std::fs::remove_dir_all(&platform_dir)?;
        } else if platform_dir.exists() {
            std::fs::remove_file(&platform_dir)?;
        }
    }
    Ok(())
}

/// Build a "CLI" project (no `ui_project_info`): a single host binary
/// for the host's default target.
fn build_cli_project(
    cli_info: &CLIInfo,
    mut project: PekoProject,
    build_directory: PathBuf,
    reporter: &Reporter,
) -> ExitCode {
    let start = Instant::now();

    reporter.status("Building", format!("{} (CLI project)", project.name));

    if let Err(e) = nuke_output_directory(&build_directory) {
        reporter.error(format!(
            "could not prepare build directory {}: {e}",
            build_directory.display()
        ));
        return ExitCode::FAILURE;
    }

    let default_target = PekoTarget::default();
    let arch_build_dir = build_directory
        .join(default_target.operating_system.to_string())
        .join(default_target.architecture.to_string());

    if let Err(e) = std::fs::create_dir_all(&arch_build_dir) {
        reporter.error(format!(
            "could not create build directory {}: {e}",
            arch_build_dir.display()
        ));
        return ExitCode::FAILURE;
    }

    let binary = arch_build_dir.join(&project.name);
    let incremental_dir = project.get_root().join(".peko/incremental");

    let progress = reporter.progress();
    progress.start_phase(&format!("Building {}", project.name));

    let compile_result = execution::incremental::compile_project(
        cli_info.get_peko_root(),
        &mut project,
        default_target,
        incremental_dir,
        Some(binary.clone()),
        false,
        Vec::new(),
        None,
        None,
        !cli_info.flags.has_flag("release"),
        cli_info.flags.has_flag("demo"),
        progress,
    );

    progress.finish_phase();

    let diagnostics = match compile_result {
        Ok(diag) => diag,
        Err(e) => {
            reporter.error(format!("compilation failed: {e}"));
            return ExitCode::FAILURE;
        }
    };

    if let Some(diagnostics) = diagnostics {
        reporter.report_diagnostics(&diagnostics);
        report_build_failed(reporter, &project.name, diagnostics.get_error_count());
        return ExitCode::FAILURE;
    }

    reporter.success(format!(
        "{} in {:.2?} → {}",
        project.name,
        start.elapsed(),
        binary.display()
    ));
    ExitCode::SUCCESS
}

/// Build a UI project: runs a type-check pass, then loops the project's
/// declared target platforms invoking each platform's bundler.
fn build_ui_project(
    cli_info: &CLIInfo,
    mut project: PekoProject,
    build_directory: PathBuf,
    release: bool,
    reporter: &Reporter,
) -> ExitCode {
    let start = Instant::now();

    reporter.status("Building", format!("{} (UI project)", project.name));

    // Resolve the platforms to build first, so the output clean can remove only
    // their directories. `peko deploy` builds each platform in its own
    // `peko build --platform X` invocation into the shared build/<mode> tree;
    // wiping the whole tree per platform would delete the previous platforms'
    // outputs and leave nothing for the deploy bundle to collect.
    let platforms = match cli_info.flags.get_flag("platform") {
        Some(requested) => match parse_platform(&requested) {
            Some(os) => vec![os],
            None => {
                reporter.error(format!(
                    "unknown platform '{requested}'; expected android, ios, linux, macos, or windows"
                ));
                return ExitCode::FAILURE;
            }
        },
        None => project.ui_project_info.as_ref().unwrap().platforms.clone(),
    };

    // Validate the platform list up front so an unsupported OS is caught before
    // any work.
    for platform in &platforms {
        if platform_label(platform).is_none() {
            reporter.error("project's target_platforms contains an unsupported operating system");
            return ExitCode::FAILURE;
        }
    }

    // Clean only the directories for the platforms this build produces, leaving
    // any other platforms' outputs in build/<mode> in place.
    if let Err(e) = clean_platform_outputs(&build_directory, &platforms) {
        reporter.error(format!(
            "could not prepare build directory {}: {e}",
            build_directory.display()
        ));
        return ExitCode::FAILURE;
    }

    // For a static (SSG) web app, build the web frontend into assets/ before
    // bundling, so the platform bundlers embed the compiled dist.
    if project
        .ui_project_info
        .as_ref()
        .is_some_and(|ui| ui.framework == "static")
        && let Err(code) = build_web_frontend(
            &project,
            cli_info.get_peko_root(),
            cli_info
                .flags
                .get_flag("web-dist")
                .as_deref()
                .map(std::path::Path::new),
            reporter,
        )
    {
        return code;
    }

    // Generate the bundling config templates if they don't exist yet
    // (first build) or if --regenconfig was passed.
    let configfiles_dir = project.get_root().join(".peko/bundling/configfiles");
    if (!configfiles_dir.exists() || cli_info.flags.has_flag("regenconfig"))
        && let Err(e) = bundler::regenerate_application_bundle_files(&project)
    {
        reporter.error(format!("could not generate bundling config files: {e}"));
        return ExitCode::FAILURE;
    }

    // Type-check the project's entrypoint up front so we can report
    // semantic errors before kicking off the per-platform builds.
    let test_outcome = match execution::test(
        cli_info.get_peko_root(),
        PekoTarget::default(),
        project.get_entrypoint().to_path_buf(),
        project.get_root().to_path_buf(),
    ) {
        Ok(outcome) => outcome,
        Err(e) => {
            reporter.error(format!("type-check failed: {e}"));
            return ExitCode::FAILURE;
        }
    };

    if test_outcome.diagnostics.get_error_count() > 0 {
        reporter.report_diagnostics(&test_outcome.diagnostics);
        report_build_failed(
            reporter,
            &project.name,
            test_outcome.diagnostics.get_error_count(),
        );
        return ExitCode::FAILURE;
    }

    // For each platform (resolved and validated above): bundle, then sign if
    // --release.
    let progress = reporter.progress();
    progress.start_phase(&format!("Building {}", project.name));
    // Don't set_total upfront, the bundlers call into
    // incremental::compile_project which adds its own units (rechecks,
    // recompiles, link) via add_to_total. Letting the bar discover its
    // length as work is queued gives readings that climb like "3/12" then
    // "8/22" and onward, instead of overflowing past a fixed initial total.
    // We also count each platform's outer "start" as one unit by
    // add_to_total'ing a single unit per platform before its bundle runs.
    progress.add_to_total(platforms.len() as u64);

    let mut failures: Vec<(OperatingSystem, PlatformFailure)> = Vec::new();

    for platform in &platforms {
        let label = platform_label(platform).unwrap_or("unknown");
        progress.tick(&format!("Building for {label}"));

        let bundle_result = run_bundler(
            cli_info,
            &mut project,
            &build_directory,
            platform,
            release,
            progress,
        );

        if let Err(e) = bundle_result {
            failures.push((*platform, PlatformFailure::Build(e)));
            continue;
        }

        if release {
            match run_signer(cli_info, &project, &build_directory, platform, reporter) {
                Ok(true) => {}
                Ok(false) => {
                    failures.push((*platform, PlatformFailure::Sign(None)));
                }
                Err(e) => {
                    failures.push((*platform, PlatformFailure::Sign(Some(e))));
                }
            }
        }
    }

    progress.finish_phase();

    // Report aggregate outcome.
    if failures.is_empty() {
        reporter.success(format!("{} in {:.2?}", project.name, start.elapsed()));
        return ExitCode::SUCCESS;
    }

    for (platform, failure) in &failures {
        let label = platform_label(platform).unwrap_or("unknown");
        match failure {
            PlatformFailure::Build(BundleError::CompileDiagnostics(diagnostics)) => {
                reporter.error(format!("build for {label} failed with compile errors:"));
                reporter.report_diagnostics(diagnostics);
            }
            PlatformFailure::Build(other) => {
                reporter.error(format!("build for {label} failed: {other}"));
            }
            PlatformFailure::Sign(None) => {
                reporter.error(format!(
                    "signing for {label} failed: no signing key registered (run 'peko keys add' to register one)"
                ));
            }
            PlatformFailure::Sign(Some(e)) => {
                reporter.error(format!("signing for {label} failed: {e}"));
            }
        }
    }

    reporter.error(format!("build for {} failed", project.name));
    ExitCode::FAILURE
}

/// Dispatch to the correct platform bundler.
fn run_bundler(
    cli_info: &CLIInfo,
    project: &mut PekoProject,
    build_directory: &std::path::Path,
    platform: &OperatingSystem,
    release: bool,
    progress: &dyn ProgressSink,
) -> Result<(), BundleError> {
    match platform {
        OperatingSystem::Android => {
            bundler::android::bundle(cli_info, project, build_directory.join("android"), progress)
        }
        OperatingSystem::IOS => bundler::ios::bundle(
            cli_info,
            project,
            build_directory.join("ios"),
            release,
            progress,
        ),
        OperatingSystem::Linux => {
            bundler::linux::bundle(cli_info, project, build_directory.join("linux"), progress)
        }
        OperatingSystem::MacOS => {
            bundler::macos::bundle(cli_info, project, build_directory.join("macos"), progress)
        }
        OperatingSystem::Windows => {
            bundler::windows::bundle(cli_info, project, build_directory.join("windows"), progress)
        }
        OperatingSystem::Unknown => {
            // Filtered earlier in build_ui_project; defensive.
            unreachable!("OperatingSystem::Unknown filtered before bundler dispatch")
        }
    }
}

/// Dispatch to the correct platform signer. Returns:
/// - `Ok(true)` when the platform is in an acceptable state: signed, or
///   an optional platform left unsigned because no key is registered, or
///   a platform that needs no signing (Linux).
/// - `Ok(false)` when a required platform (iOS, Android) has no registered
///   signing key.
/// - `Err(...)` on tool, IO, or signing failures.
///
/// iOS and Android signing are required for release. macOS and Windows
/// signing are optional; a missing key leaves the artifact unsigned and
/// emits a warning.
fn run_signer(
    cli_info: &CLIInfo,
    project: &PekoProject,
    build_directory: &std::path::Path,
    platform: &OperatingSystem,
    reporter: &Reporter,
) -> Result<bool, BundleError> {
    let label = platform_label(platform).unwrap_or("unknown");
    match platform {
        OperatingSystem::Android => {
            bundler::android::sign(cli_info, project, build_directory.join("android"))
        }
        OperatingSystem::IOS => bundler::ios::sign(cli_info, project, build_directory.join("ios")),
        OperatingSystem::MacOS => {
            match bundler::macos::sign(cli_info, project, build_directory.join("macos"), reporter)?
            {
                bundler::signing::OptionalSignOutcome::Signed => {}
                bundler::signing::OptionalSignOutcome::NoKey => {
                    reporter.warning(format!(
                        "{label}: no signing key registered, leaving the app unsigned"
                    ));
                }
                bundler::signing::OptionalSignOutcome::ToolUnavailable => {
                    reporter.warning(format!(
                        "{label}: signing tool unavailable, leaving the app unsigned"
                    ));
                }
            }
            Ok(true)
        }
        OperatingSystem::Windows => {
            match bundler::windows::sign(cli_info, project, build_directory.join("windows"))? {
                bundler::signing::OptionalSignOutcome::Signed => {}
                bundler::signing::OptionalSignOutcome::NoKey => {
                    reporter.warning(format!(
                        "{label}: no signing key registered, leaving the executable unsigned"
                    ));
                }
                bundler::signing::OptionalSignOutcome::ToolUnavailable => {
                    reporter.warning(format!(
                        "{label}: osslsigncode not found on the system, leaving the executable unsigned"
                    ));
                }
            }
            Ok(true)
        }
        // Linux AppImages need no signing.
        OperatingSystem::Linux => Ok(true),
        OperatingSystem::Unknown => {
            unreachable!("OperatingSystem::Unknown filtered before signer dispatch")
        }
    }
}

/// Parse a `--platform` value into an [`OperatingSystem`]. Accepts the
/// canonical lowercase platform names, case-insensitively.
fn parse_platform(value: &str) -> Option<OperatingSystem> {
    match value.to_lowercase().as_str() {
        "android" => Some(OperatingSystem::Android),
        "ios" => Some(OperatingSystem::IOS),
        "linux" => Some(OperatingSystem::Linux),
        "macos" => Some(OperatingSystem::MacOS),
        "windows" => Some(OperatingSystem::Windows),
        _ => None,
    }
}

/// Print a "Build for X failed with N error(s)" line.
fn report_build_failed(reporter: &Reporter, project_name: &str, error_count: usize) {
    let plural = if error_count == 1 { "" } else { "s" };
    reporter.error(format!(
        "build for {project_name} failed with {error_count} error{plural}"
    ));
}
