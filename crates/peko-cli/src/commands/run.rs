//! `peko run` - build and run the current project.
//!
//! For CLI projects this is a one-shot: compile, then exec the binary.
//!
//! For UI projects this enters a hot-reload loop:
//!
//! 1. Compile the project once and start the `appserver` subprocess.
//! 2. Compile the standalone `apprender` (a webview host that points at
//!    the appserver) and start it.
//! 3. Wait for both subprocesses to call back over a TCP listener
//!    (`127.0.0.1:7357`) to signal they're ready.
//! 4. In a polling loop:
//!    - Detect SCSS source changes, recompile to CSS, and ping the app's
//!      style-update listener at `127.0.0.1:7358`.
//!    - Detect Pekoscript source changes, rebuild the appserver to a
//!      new binary (alternating `appserver0`/`appserver1` so the old
//!      one can keep running), and swap it in.
//! 5. Exit when the apprender exits.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use peko_core::target::PekoTarget;

use crate::cli::CLIInfo;
use crate::cli::reporting::Reporter;
use crate::execution::{self, incremental::ProjectIncrementalMap};
use crate::project::PekoProject;
use crate::toolchain::resolve_for_target;

/// Execute the `run` subcommand.
pub async fn execute(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
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
        // CLI projects exec a child and wait; Ctrl-C is delivered by
        // the terminal to the whole process group, so the child sees
        // SIGINT and exits, and the parent's `.status()` returns
        // naturally. No need to install our own handler for that case.
        return run_cli_project(cli_info, project, build_directory, reporter);
    }

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_for_handler = Arc::clone(&shutdown);
    if let Err(e) = ctrlc::set_handler(move || {
        shutdown_for_handler.store(true, Ordering::SeqCst);
    }) {
        // Not fatal. The run loop will still work, the user just
        // won't be able to Ctrl-C cleanly.
        reporter.warning(format!(
            "could not install Ctrl-C handler: {e}; force-quit may leave subprocesses behind"
        ));
    }

    run_ui_project(cli_info, project, reporter, shutdown)
}

// ---------------------------------------------------------------------------
// CLI project: one-shot compile and exec.
// ---------------------------------------------------------------------------

fn run_cli_project(
    cli_info: &CLIInfo,
    mut project: PekoProject,
    build_directory: PathBuf,
    reporter: &Reporter,
) -> ExitCode {
    reporter.info(format!("running {} as a CLI project", project.name));

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
        None,
        None,
        progress,
    );

    progress.finish_phase();

    let diagnostics = match compile_result {
        Ok((_, diag)) => diag,
        Err(e) => {
            reporter.error(format!("compilation failed: {e}"));
            return ExitCode::FAILURE;
        }
    };

    if let Some(diagnostics) = diagnostics {
        reporter.report_diagnostics(&diagnostics);
        reporter.error(format!("compilation of {} failed", project.name));
        return ExitCode::FAILURE;
    }

    reporter.status("Running", &project.name);

    let exit_status = match Command::new(&binary).status() {
        Ok(status) => status,
        Err(e) => {
            reporter.error(format!("could not launch {}: {e}", binary.display()));
            return ExitCode::FAILURE;
        }
    };

    if exit_status.success() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

// ---------------------------------------------------------------------------
// UI project: hot reload loop.
// ---------------------------------------------------------------------------

fn run_ui_project(
    cli_info: &CLIInfo,
    mut project: PekoProject,
    reporter: &Reporter,
    shutdown: Arc<AtomicBool>,
) -> ExitCode {
    reporter.info(format!(
        "running {} as a UI project with hot reload",
        project.name
    ));

    // ---- Per-run scratch directory layout -------------------------------
    let incremental_run_dir = project.get_root().join(".peko/incremental/run");
    let incremental_info =
        ProjectIncrementalMap::from_incremental_directory(&incremental_run_dir, false);
    project.incremental_info = incremental_info.clone();

    // If on-disk incremental data was unreadable but the directory
    // exists, scrub it so the next build starts fresh.
    if incremental_info.is_none()
        && incremental_run_dir.exists()
        && let Err(e) = std::fs::remove_dir_all(&incremental_run_dir)
    {
        reporter.warning(format!(
            "could not remove stale incremental dir {}: {e}",
            incremental_run_dir.display()
        ));
    }

    let project_style_directory = incremental_run_dir.join("styles");
    if let Err(e) = std::fs::create_dir_all(&project_style_directory) {
        reporter.error(format!(
            "could not create style directory {}: {e}",
            project_style_directory.display()
        ));
        return ExitCode::FAILURE;
    }

    // Debug runs serve assets straight from the project's asset directory,
    // so edits to assets appear without a rebuild. The directory is passed
    // to the codegen context as the asset debug folder.
    let asset_debug_directory = project.assets_dir();

    let default_target = PekoTarget::default();

    // The appserver executable alternates between two filenames so we
    // can rebuild the new one while the old one is still running and
    // holding a lock on its binary.
    let mut appserver_uid = false;
    let appserver_executable = incremental_run_dir.join(format!(
        "appserver{}",
        if appserver_uid { "0" } else { "1" }
    ));

    // ---- Initial compile of the project entrypoint ----------------------
    let progress = reporter.progress();
    progress.start_phase(&format!("Building {} (initial)", project.name));

    let (preloaded_modules, preloaded_libs) = match execution::load_required_packages(
        cli_info.get_peko_root(),
        default_target,
        Some(project_style_directory.clone()),
        Some(asset_debug_directory.clone()),
    ) {
        Ok(tuple) => tuple,
        Err(e) => {
            progress.finish_phase();
            reporter.error(format!("could not load required packages: {e}"));
            return ExitCode::FAILURE;
        }
    };
    let preloaded_modules = Some(preloaded_modules);

    let initial_compile = execution::incremental::compile_project(
        cli_info.get_peko_root(),
        &mut project,
        default_target,
        incremental_run_dir.clone(),
        Some(appserver_executable.clone()),
        false,
        preloaded_libs.clone(),
        Some(project_style_directory.clone()),
        Some(asset_debug_directory.clone()),
        preloaded_modules.clone(),
        None,
        progress,
    );

    let (imported_styles, diagnostics) = match initial_compile {
        Ok(tuple) => tuple,
        Err(e) => {
            progress.finish_phase();
            reporter.error(format!("initial compile failed: {e}"));
            return ExitCode::FAILURE;
        }
    };

    if let Some(d) = &diagnostics
        && d.get_error_count() > 0
    {
        progress.finish_phase();
        reporter.report_diagnostics(d);
        reporter.error("initial run failed due to compile errors");
        return ExitCode::FAILURE;
    }

    // ---- Compile all the SCSS styles once -------------------------------
    // (maps style_file to (current_source_text, output_basename))
    let mut styles_to_watch: HashMap<PathBuf, (String, String)> = HashMap::new();
    if let Err(code) = compile_styles_initial(
        &imported_styles,
        &project_style_directory,
        &mut styles_to_watch,
        reporter,
    ) {
        progress.finish_phase();
        return code;
    }

    progress.finish_phase();

    // ---- Compile the standalone apprender (webview host) ----------------
    let apprender_object = incremental_run_dir.join("apprender.o");
    let hotreloadapp_peko_path = incremental_run_dir.join("hotreloadapp.peko");

    let reload_template = cli_info
        .get_peko_root()
        .join("Compiler/bundling/reload.peko");
    let reload_source = match std::fs::read_to_string(&reload_template) {
        Ok(s) => s,
        Err(e) => {
            reporter.error(format!(
                "could not read hot-reload template at {}: {e}",
                reload_template.display()
            ));
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = std::fs::write(&hotreloadapp_peko_path, reload_source.as_bytes()) {
        reporter.error(format!("could not write hotreloadapp.peko: {e}"));
        return ExitCode::FAILURE;
    }

    let resolved = match resolve_for_target(cli_info.get_peko_root(), &default_target) {
        Ok(resolved) => resolved,
        Err(e) => {
            reporter.error(format!("could not resolve toolchain: {e}"));
            return ExitCode::FAILURE;
        }
    };

    let progress = reporter.progress();
    progress.start_phase("Building apprender");

    let apprender_outcome = match execution::compile(
        cli_info.get_peko_root(),
        default_target,
        hotreloadapp_peko_path.clone(),
        project.get_root().to_path_buf(),
        apprender_object.clone(),
    ) {
        Ok(outcome) => outcome,
        Err(e) => {
            progress.finish_phase();
            reporter.error(format!("apprender compile failed: {e}"));
            return ExitCode::FAILURE;
        }
    };

    reporter.report_diagnostics(&apprender_outcome.diagnostics);
    if apprender_outcome.diagnostics.get_error_count() > 0 {
        progress.finish_phase();
        return ExitCode::FAILURE;
    }

    let apprender_executable = incremental_run_dir.join("apprender");
    peko_llvm::linker::lld_link(
        default_target,
        apprender_object,
        apprender_outcome.codegen_context.files_to_link.clone(),
        &resolved.toolchain,
        &resolved.dir,
        Some(apprender_executable.clone()),
        false,
        None,
    );

    progress.finish_phase();

    // ---- Bring up the listener, then the renderer and the appserver -----
    // The update listener at 127.0.0.1:7357 is used both as the
    // renderer's "ready" signal and the appserver's "ready" signal.
    // Set non-blocking so wait_for_ready_or_shutdown can poll without
    // pinning a thread; the helper interleaves accept attempts with
    // checks of the shutdown flag so Ctrl-C is honored within ~100ms
    // even while we're waiting on a subprocess to come up.
    let update_listener = match std::net::TcpListener::bind("127.0.0.1:7357") {
        Ok(l) => l,
        Err(e) => {
            reporter.error(format!(
                "could not bind hot-reload listener on 127.0.0.1:7357: {e}"
            ));
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = update_listener.set_nonblocking(true) {
        reporter.error(format!(
            "could not set hot-reload listener non-blocking: {e}"
        ));
        return ExitCode::FAILURE;
    }

    let mut apprenderer = match Command::new(&apprender_executable).spawn() {
        Ok(child) => child,
        Err(e) => {
            reporter.error(format!(
                "could not launch apprender at {}: {e}",
                apprender_executable.display()
            ));
            return ExitCode::FAILURE;
        }
    };

    // Wait for apprender ready signal, interruptible by Ctrl-C.
    match wait_for_ready_or_shutdown(&update_listener, &shutdown) {
        WaitOutcome::Ready => {}
        WaitOutcome::Shutdown => {
            let _ = apprenderer.kill();
            let _ = apprenderer.wait();
            return ExitCode::SUCCESS;
        }
        WaitOutcome::Error(e) => {
            reporter.error(format!("hot-reload listener accept failed: {e}"));
            let _ = apprenderer.kill();
            return ExitCode::FAILURE;
        }
    }

    reporter.status("Ready", "hot reload watching for changes");

    let mut current_appserver = match Command::new(&appserver_executable).spawn() {
        Ok(child) => child,
        Err(e) => {
            reporter.error(format!(
                "could not launch appserver at {}: {e}",
                appserver_executable.display()
            ));
            let _ = apprenderer.kill();
            return ExitCode::FAILURE;
        }
    };

    // Wait for appserver ready signal, interruptible by Ctrl-C.
    match wait_for_ready_or_shutdown(&update_listener, &shutdown) {
        WaitOutcome::Ready => {}
        WaitOutcome::Shutdown => {
            let _ = current_appserver.kill();
            let _ = current_appserver.wait();
            let _ = apprenderer.kill();
            let _ = apprenderer.wait();
            return ExitCode::SUCCESS;
        }
        WaitOutcome::Error(e) => {
            reporter.error(format!("hot-reload listener accept failed: {e}"));
            let _ = apprenderer.kill();
            let _ = current_appserver.kill();
            return ExitCode::FAILURE;
        }
    }

    // ---- Hot-reload polling loop ----------------------------------------
    //
    // Sleeps 100ms between iterations so the loop doesn't spin and burn
    // a CPU core polling the filesystem. 100ms is short enough that
    // change-to-reload latency feels instantaneous to a human, and long
    // enough that idle cost stays close to zero.
    //
    // Ctrl-C is honored within ~100ms throughout: the listener is in
    // non-blocking mode, and `wait_for_ready_or_shutdown` interleaves
    // accept attempts with shutdown-flag checks. No path in this loop
    // blocks for longer than one poll interval without checking
    // shutdown.
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

    loop {
        // Honor a Ctrl-C signal. The ctrlc handler installed in
        // `execute` sets this flag; we observe it on each polling
        // iteration and break out to run the cleanup path below.
        if shutdown.load(Ordering::SeqCst) {
            if !cli_info.flags.has_flag("quiet") {
                reporter.info("shutdown requested, cleaning up");
            }
            break;
        }

        // Renderer exiting ends the session.
        match apprenderer.try_wait() {
            Ok(Some(_)) | Err(_) => break,
            Ok(None) => {}
        }

        // Detect SCSS source changes by content comparison. The clone
        // is required so the loop body can mutate `styles_to_watch`
        // without aliasing the iterator.
        let watched_snapshot = styles_to_watch.clone();
        for (style_file, (style_contents, style_name)) in &watched_snapshot {
            handle_style_change(
                style_file,
                style_contents,
                style_name,
                &project_style_directory,
                &mut styles_to_watch,
                cli_info,
                reporter,
            );
        }

        // Detect Pekoscript source changes via the project's
        // incremental file map. If nothing changed, we still fall
        // through to the sleep at the end of the iteration.
        let pekoscript_changed = project
            .incremental_info
            .as_ref()
            .map(|info| info.tracked_files_changed())
            .unwrap_or(false);

        if pekoscript_changed {
            if !cli_info.flags.has_flag("quiet") {
                reporter.info("change detected, rebuilding...");
            }

            // Swap to the other appserver slot so the old one stays
            // executable while the new one builds.
            appserver_uid = !appserver_uid;
            let new_appserver_executable = incremental_run_dir.join(format!(
                "appserver{}",
                if appserver_uid { "0" } else { "1" }
            ));

            let progress = reporter.progress();
            progress.start_phase("Rebuilding appserver");

            let rebuild = execution::incremental::compile_project(
                cli_info.get_peko_root(),
                &mut project,
                default_target,
                incremental_run_dir.clone(),
                Some(new_appserver_executable.clone()),
                false,
                preloaded_libs.clone(),
                Some(project_style_directory.clone()),
                Some(asset_debug_directory.clone()),
                preloaded_modules.clone(),
                None,
                progress,
            );

            match rebuild {
                Err(e) => {
                    progress.finish_phase();
                    reporter.error(format!("rebuild failed: {e}"));
                }
                Ok((imported_styles, Some(diagnostics))) if diagnostics.get_error_count() > 0 => {
                    progress.finish_phase();
                    reporter.report_diagnostics(&diagnostics);
                    reporter.error("rebuild failed");
                    // Even though the rebuild failed, the
                    // imported_styles list is still useful, pick up
                    // any new SCSS files so we can hot-reload styles
                    // independent of the broken Pekoscript build.
                    let _ = imported_styles;
                }
                Ok((imported_styles, _)) => {
                    // Pick up any new SCSS files that got imported in
                    // the rebuild.
                    for (style_file, style_name) in &imported_styles {
                        if styles_to_watch.contains_key(style_file) {
                            continue;
                        }
                        compile_and_register_style(
                            style_file,
                            style_name,
                            &project_style_directory,
                            &mut styles_to_watch,
                            reporter,
                        );
                    }

                    // Hot-swap appserver subprocesses. The new
                    // appserver pings the listener when it's ready;
                    // we wait for that before killing the old one so
                    // the UI never sees a gap. Wait is interruptible
                    // by Ctrl-C. If shutdown is set we abandon the
                    // new appserver and the outer loop's shutdown
                    // check breaks us out cleanly on the next pass.
                    match Command::new(&new_appserver_executable).spawn() {
                        Ok(mut new_appserver) => {
                            match wait_for_ready_or_shutdown(&update_listener, &shutdown) {
                                WaitOutcome::Ready => {}
                                WaitOutcome::Shutdown => {
                                    // Don't swap; let the outer loop
                                    // tear down current_appserver as
                                    // usual. Kill the partially-
                                    // initialized new one so it
                                    // doesn't outlive us.
                                    let _ = new_appserver.kill();
                                    let _ = new_appserver.wait();
                                    progress.finish_phase();
                                    continue;
                                }
                                WaitOutcome::Error(e) => {
                                    reporter.warning(format!(
                                        "hot-reload accept after rebuild failed: {e}"
                                    ));
                                }
                            }

                            if let Err(e) = current_appserver.kill() {
                                reporter.warning(format!("could not kill previous appserver: {e}"));
                            }
                            current_appserver = new_appserver;

                            progress.finish_phase();

                            if !cli_info.flags.has_flag("quiet") {
                                reporter.status("Reloaded", "update sent to app");
                            }
                        }
                        Err(e) => {
                            progress.finish_phase();
                            reporter.error(format!(
                                "could not launch new appserver at {}: {e}",
                                new_appserver_executable.display()
                            ));
                        }
                    }
                }
            }
        }

        std::thread::sleep(POLL_INTERVAL);
    }

    // Best-effort cleanup of both spawned subprocesses on exit. The
    // appserver is the inner process; the apprenderer is the webview
    // host. Either may already have exited under us (e.g. the loop
    // broke because apprenderer's `try_wait` returned), in which case
    // these kills no-op.
    let _ = current_appserver.kill();
    let _ = apprenderer.kill();
    // Wait briefly for the OS to reap them so we don't leave zombies.
    let _ = current_appserver.wait();
    let _ = apprenderer.wait();
    ExitCode::SUCCESS
}

/// Compile every SCSS file in `imported_styles` once, write the CSS
/// output into `project_style_directory`, and seed `styles_to_watch`
/// with the file content for change detection.
fn compile_styles_initial(
    imported_styles: &HashMap<PathBuf, String>,
    project_style_directory: &Path,
    styles_to_watch: &mut HashMap<PathBuf, (String, String)>,
    reporter: &Reporter,
) -> Result<(), ExitCode> {
    for (style_file, style_name) in imported_styles {
        compile_and_register_style(
            style_file,
            style_name,
            project_style_directory,
            styles_to_watch,
            reporter,
        );
    }
    Ok(())
}

/// Compile one SCSS file to CSS, write the output to
/// `<project_style_directory>/<style_name>`, and record the source
/// text under `styles_to_watch` for change detection.
fn compile_and_register_style(
    style_file: &Path,
    style_name: &str,
    project_style_directory: &Path,
    styles_to_watch: &mut HashMap<PathBuf, (String, String)>,
    reporter: &Reporter,
) {
    let source = match std::fs::read_to_string(style_file) {
        Ok(s) => s,
        Err(e) => {
            reporter.warning(format!(
                "could not read style {}: {e}",
                style_file.display()
            ));
            return;
        }
    };

    styles_to_watch.insert(
        style_file.to_path_buf(),
        (source.clone(), style_name.to_owned()),
    );

    let css_text = match grass::from_string(source, &grass::Options::default()) {
        Ok(css) => css,
        Err(e) => {
            reporter.warning(format!(
                "SCSS compilation failed for {}: {e}",
                style_file.display()
            ));
            String::new()
        }
    };
    let output_path = project_style_directory.join(style_name);
    if let Err(e) = std::fs::write(&output_path, css_text.as_bytes()) {
        reporter.warning(format!(
            "could not write compiled style {}: {e}",
            output_path.display()
        ));
    }
}

/// React to a possible change in a single SCSS source file. If the
/// on-disk content differs from what's recorded in `styles_to_watch`,
/// recompile the file and ping the app's style-update listener.
fn handle_style_change(
    style_file: &Path,
    recorded_contents: &str,
    style_name: &str,
    project_style_directory: &Path,
    styles_to_watch: &mut HashMap<PathBuf, (String, String)>,
    cli_info: &CLIInfo,
    reporter: &Reporter,
) {
    let new_contents = match std::fs::read_to_string(style_file) {
        Ok(s) => s,
        Err(_) => return,
    };
    if new_contents == recorded_contents {
        return;
    }

    if !cli_info.flags.has_flag("quiet") {
        reporter.info(format!(
            "stylesheet {} changed, recompiling",
            style_file.display()
        ));
    }

    styles_to_watch.insert(
        style_file.to_path_buf(),
        (new_contents.clone(), style_name.to_owned()),
    );

    let css_text = match grass::from_string(new_contents, &grass::Options::default()) {
        Ok(css) => css,
        Err(e) => {
            reporter.warning(format!(
                "SCSS compilation failed for {}: {e}",
                style_file.display()
            ));
            String::new()
        }
    };
    let output_path = project_style_directory.join(style_name);
    if let Err(e) = std::fs::write(&output_path, css_text.as_bytes()) {
        reporter.warning(format!(
            "could not write compiled style {}: {e}",
            output_path.display()
        ));
        return;
    }

    // Ping the running app's style-update listener at port 7358.
    match std::net::TcpStream::connect("127.0.0.1:7358") {
        Ok(mut stream) => {
            if let Err(e) = stream.write_all(b"message") {
                reporter.warning(format!("style update ping write failed: {e}"));
            }
            let _ = stream.flush();
            let _ = stream.shutdown(std::net::Shutdown::Both);
            if !cli_info.flags.has_flag("quiet") {
                reporter.status("Reloaded", "update sent to app");
            }
        }
        Err(e) => {
            reporter.warning(format!(
                "could not connect to running app's style-update listener (127.0.0.1:7358): {e}"
            ));
        }
    }
}

/// Outcome of a `wait_for_ready_or_shutdown` call.
enum WaitOutcome {
    /// A subprocess connected to the listener and signalled ready.
    Ready,
    /// The shutdown flag was set while waiting. The caller should
    /// abandon whatever it was waiting for and tear down.
    Shutdown,
    /// The listener returned a non-WouldBlock error. Treated as fatal
    /// to the wait; the caller decides what to do.
    Error(std::io::Error),
}

/// Wait for the next connection on `listener`, but interleave the wait
/// with polls of `shutdown`. The listener is expected to be in
/// non-blocking mode (set via `set_nonblocking(true)` once at
/// creation). Polls every 100ms; returns immediately when either a
/// connection lands or shutdown is set.
fn wait_for_ready_or_shutdown(
    listener: &std::net::TcpListener,
    shutdown: &Arc<AtomicBool>,
) -> WaitOutcome {
    const POLL: std::time::Duration = std::time::Duration::from_millis(100);
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return WaitOutcome::Shutdown;
        }
        match listener.accept() {
            Ok(_) => return WaitOutcome::Ready,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(POLL);
            }
            Err(e) => return WaitOutcome::Error(e),
        }
    }
}
