//! `peko run` - build and run the current project.
//!
//! For CLI projects this is a one-shot: compile, then exec the binary.
//!
//! For UI projects this is a one-shot compile. The interactive dev-run
//! (webview host plus live reload) is being reworked under the pekoui
//! package, so `peko run` on a UI project builds the project and reports
//! the result rather than launching a window. Use `peko build` to produce
//! a distributable bundle.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode};
use std::sync::atomic::Ordering;

use peko_core::target::PekoTarget;

use crate::cli::CLIInfo;
use crate::cli::reporting::{IndicatifSink, Reporter};
use crate::commands::devtools::{self, DevAction, DevChannel, DevEvent};
use crate::execution;
use crate::project::PekoProject;

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
        // naturally.
        run_cli_project(cli_info, project, build_directory, reporter)
    } else {
        // The terminal dev loop. `peko run --devtools` takes a different entry
        // (execute_with_devtools) handled in main before the async dispatch,
        // since its window must own the process main thread.
        run_ui_project(
            cli_info.get_peko_root(),
            release,
            project,
            build_directory,
            reporter,
            None,
        )
        .await
    }
}

/// Entry for `peko run --devtools`, called from `main` on the process main
/// thread (winit requires the window's event loop there). The dev loop runs on
/// a background thread with its own runtime and streams events to the window;
/// closing the window signals the loop to stop.
pub fn execute_with_devtools(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    reporter.info("opening devtools window");

    let peko_root = cli_info.get_peko_root().to_path_buf();
    let release = cli_info.flags.has_flag("release");
    let (dev, events, actions, shutdown) = devtools::channel();

    // The dev loop (npm, compile, watch, relaunch) runs off the main thread so
    // the window can own it.
    let background = std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(_) => return,
        };
        runtime.block_on(devtools_dev_loop(peko_root, release, dev));
    });

    // Runs until the window is closed.
    let result = devtools::run_window(events, actions);

    // Tell the loop to tear down, then wait for it.
    shutdown.store(true, Ordering::Relaxed);
    let _ = background.join();

    // Belt and suspenders: restore the terminal to a sane line mode in case a
    // child left it altered, so the shell prompt behaves normally on return.
    restore_terminal();
    result
}

/// Reset the controlling terminal to a sane cooked mode. A no-op where stdin is
/// not a terminal.
fn restore_terminal() {
    #[cfg(unix)]
    {
        let _ = Command::new("stty").arg("sane").status();
    }
    #[cfg(windows)]
    {
        // The Windows equivalent of `stty sane`: restore the console input mode
        // to the cooked defaults (line editing and echo) a child may have
        // cleared.
        use windows_sys::Win32::System::Console::{
            ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT, GetStdHandle,
            STD_INPUT_HANDLE, SetConsoleMode,
        };
        unsafe {
            let handle = GetStdHandle(STD_INPUT_HANDLE);
            if !handle.is_null() {
                SetConsoleMode(
                    handle,
                    ENABLE_PROCESSED_INPUT | ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT,
                );
            }
        }
    }
}

/// Run the UI dev loop under IDE control: no window, a newline-delimited JSON
/// stream on stdout carries dev events, and stdin carries commands. The dev
/// loop is the same one the devtools window drives, so the loop tears the app
/// window and dev server down on shutdown. An embedding IDE owns the app
/// lifecycle through this transport.
pub fn execute_with_ide(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    reporter.info("starting ide dev loop");

    let peko_root = cli_info.get_peko_root().to_path_buf();
    let release = cli_info.flags.has_flag("release");
    let (dev, events, actions, shutdown) = devtools::channel();

    // The dev loop (npm, compile, watch, relaunch) runs off the main thread so
    // the stdin reader can drive it while stdout drains events.
    let loop_shutdown = shutdown.clone();
    let background = std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(_) => {
                loop_shutdown.store(true, Ordering::Relaxed);
                return;
            }
        };
        runtime.block_on(devtools_dev_loop(peko_root, release, dev));
    });

    // Runs until stdin closes or a stop command tears the loop down.
    run_ide_pump(events, actions, shutdown.clone());

    // Tell the loop to tear down, then wait for it.
    shutdown.store(true, Ordering::Relaxed);
    let _ = background.join();

    restore_terminal();
    ExitCode::SUCCESS
}

/// Pump the IDE transport: a stdin reader turns newline-delimited JSON commands
/// into dev actions, and this thread writes dev events to stdout as
/// newline-delimited JSON. Returns when the loop's event sender closes, when a
/// stop command arrives, or when stdin reaches end of file.
fn run_ide_pump(
    events: std::sync::mpsc::Receiver<DevEvent>,
    actions: std::sync::mpsc::Sender<DevAction>,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    use std::io::{BufRead, Write};
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Duration;

    // Reader thread: a command per line. A stop command or end of file sets the
    // shutdown flag and asks the loop to tear down.
    let reader_shutdown = shutdown.clone();
    let reader = std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let line = match line {
                Ok(line) => line,
                Err(_) => break,
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let value: serde_json::Value = match serde_json::from_str(trimmed) {
                Ok(value) => value,
                Err(_) => continue,
            };
            let kind = value.get("t").and_then(|v| v.as_str()).unwrap_or("");
            match kind {
                "stop" => break,
                "rebuild" => {
                    let _ = actions.send(DevAction::Rebuild);
                }
                "restart" => {
                    let _ = actions.send(DevAction::RestartApp);
                }
                "eval" => {
                    let code = value
                        .get("code")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let _ = actions.send(DevAction::Eval(code));
                }
                "complete" => {
                    let code = value
                        .get("code")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let _ = actions.send(DevAction::Complete(code));
                }
                "page" => {
                    let _ = actions.send(DevAction::PageInfo);
                }
                "resource" => {
                    let url = value
                        .get("url")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let _ = actions.send(DevAction::Resource(url));
                }
                "source" => {
                    let _ = actions.send(DevAction::ViewSource);
                }
                _ => {}
            }
        }
        reader_shutdown.store(true, Ordering::Relaxed);
        let _ = actions.send(DevAction::Shutdown);
    });

    // Writer loop: drain events to stdout until the loop closes the channel.
    loop {
        match events.recv_timeout(Duration::from_millis(150)) {
            Ok(event) => {
                let line = ide_event_json(&event);
                let stdout = std::io::stdout();
                let mut handle = stdout.lock();
                let _ = writeln!(handle, "{line}");
                let _ = handle.flush();
            }
            Err(RecvTimeoutError::Timeout) => {
                if shutdown.load(Ordering::Relaxed) {
                    // Keep draining until the loop drops its sender, so the last
                    // teardown events reach the IDE.
                    continue;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    let _ = reader.join();
}

/// Serialize a dev event as one line of the IDE transport. The tags match the
/// build/run panel's event dispatch: status, diagnostics, route, console, eval,
/// and trace.
fn ide_event_json(event: &DevEvent) -> String {
    let value = match event {
        DevEvent::Status(text) => serde_json::json!({ "t": "status", "text": text }),
        DevEvent::Diagnostics(items) => {
            let items: Vec<_> = items
                .iter()
                .map(|d| {
                    serde_json::json!({
                        "file": d.file,
                        "line": d.line,
                        "column": d.column,
                        "severity": d.severity.to_lowercase(),
                        "message": d.message,
                    })
                })
                .collect();
            serde_json::json!({ "t": "diagnostics", "items": items })
        }
        DevEvent::Route(path) => serde_json::json!({ "t": "route", "path": path }),
        DevEvent::Console(line) => {
            serde_json::json!({ "t": "console", "level": line.level, "text": line.text })
        }
        DevEvent::EvalResult { kind, ok, text } => {
            if kind == "complete" {
                // The page returns {base, names} as JSON; forward it for the
                // console's completion popup.
                let parsed: serde_json::Value =
                    serde_json::from_str(text).unwrap_or_else(|_| serde_json::json!({}));
                serde_json::json!({
                    "t": "complete",
                    "base": parsed.get("base").cloned().unwrap_or(serde_json::Value::String(String::new())),
                    "names": parsed.get("names").cloned().unwrap_or(serde_json::Value::Array(Vec::new())),
                })
            } else if kind == "page" {
                // The page returns a snapshot object as JSON; forward it for the
                // page inspector.
                let parsed: serde_json::Value =
                    serde_json::from_str(text).unwrap_or_else(|_| serde_json::json!({}));
                serde_json::json!({ "t": "page", "info": parsed })
            } else if kind == "resource" {
                // The page returns {url, mime, text} for a fetched resource.
                let parsed: serde_json::Value =
                    serde_json::from_str(text).unwrap_or_else(|_| serde_json::json!({}));
                serde_json::json!({ "t": "resource", "resource": parsed })
            } else {
                // An eval result: tag it "result" (an evaluated expression's
                // return value) so the console can style it apart from a
                // forwarded console.log line, or "error" when it threw.
                let level = if *ok { "result" } else { "error" };
                serde_json::json!({ "t": "console", "level": level, "text": text })
            }
        }
        DevEvent::Trace { dir, label, data } => {
            serde_json::json!({ "t": "trace", "dir": dir, "label": label, "data": data })
        }
    };
    value.to_string()
}

/// The dev loop under devtools: load the project, resolve dependencies, then run
/// the shared UI loop streaming events to the window.
async fn devtools_dev_loop(peko_root: PathBuf, release: bool, dev: DevChannel) -> ExitCode {
    let reporter = Reporter::new().with_progress(IndicatifSink::new());

    let project = match PekoProject::from_current_directory() {
        Ok(project) => project,
        Err(e) => {
            dev.status(format!("could not load project: {e}"));
            reporter.error(format!("could not load project: {e}"));
            return ExitCode::FAILURE;
        }
    };

    let progress = reporter.progress();
    progress.start_phase("Resolving dependencies");
    let ensured =
        crate::registry::install::ensure_dependencies(&peko_root, project.get_root(), progress)
            .await;
    progress.finish_phase();
    if let Err(e) = ensured {
        dev.status(format!("dependency resolution failed: {e}"));
        reporter.error(format!("could not resolve dependencies: {e}"));
        return ExitCode::FAILURE;
    }

    let build_directory = project.get_root().join(if release {
        "build/release"
    } else {
        "build/debug"
    });

    run_ui_project(
        &peko_root,
        release,
        project,
        build_directory,
        &reporter,
        Some(dev),
    )
    .await
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
        !cli_info.flags.has_flag("release"),
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
// UI project: live dev loop.
// ---------------------------------------------------------------------------

/// Run a UI project as a live dev loop. The framework's own dev server (npm run
/// dev) serves the web UI with hot reload, and the native window loads that dev
/// URL instead of a bundled asset server, so JS, CSS, and component edits reload
/// live with no involvement here. Changes to the `.peko` source recompile the
/// native binary incrementally and relaunch the window, restoring the route the
/// user was on. The project stays in memory across rebuilds, so the incremental
/// object cache keeps external packages compiled and only changed files rebuild.
async fn run_ui_project(
    peko_root: &Path,
    release: bool,
    mut project: PekoProject,
    build_directory: PathBuf,
    reporter: &Reporter,
    dev: Option<DevChannel>,
) -> ExitCode {
    reporter.info(format!("running {} as a UI project", project.name));

    let root = project.get_root().to_path_buf();

    // The dev loop drives the framework's own hot reload, which needs a `dev`
    // script (npm run dev). Without one there is nothing to serve live.
    if !dev_script_present(&root) {
        reporter.error(
            "peko run needs a framework dev script (a `dev` entry in package.json, e.g. npm run dev). Use `peko build` for a distributable bundle",
        );
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
    let incremental_dir = root.join(".peko/incremental");

    // The launched app writes its current route here on every route change, so a
    // rebuild can relaunch and restore it. Start clean.
    let dev_state = root.join(".peko/dev-route");
    let _ = std::fs::remove_file(&dev_state);

    // Install web dependencies once, then start the framework dev server and
    // learn the URL it serves on.
    if !root.join("node_modules").is_dir() {
        reporter.status("Installing", "web dependencies (npm install)");
        let status = crate::proc::npm().arg("install").current_dir(&root).status();
        match status {
            Ok(s) if s.success() => {}
            Ok(_) => {
                reporter.error("npm install failed");
                return ExitCode::FAILURE;
            }
            Err(e) => {
                reporter.error(format!("could not run npm install: {e}"));
                return ExitCode::FAILURE;
            }
        }
    }

    reporter.status("Starting", "framework dev server (npm run dev)");
    if let Some(dev) = &dev {
        dev.status("starting dev server...");
    }
    let (mut dev_server, dev_url) = match start_dev_server(&root) {
        Ok(pair) => pair,
        Err(e) => {
            reporter.error(format!("could not start the dev server: {e}"));
            if let Some(dev) = &dev {
                dev.status(format!("dev server error: {e}"));
            }
            return ExitCode::FAILURE;
        }
    };
    reporter.success(format!("dev server ready at {dev_url}"));
    if let Some(dev) = &dev {
        dev.status(format!("dev server ready at {dev_url}"));
    }

    // First build, then launch the window pointed at the dev server. A failed
    // build still enters the watch loop, so the next save can fix it.
    let mut app_child: Option<Child> = None;
    if compile_ui(
        peko_root,
        release,
        &mut project,
        default_target,
        &incremental_dir,
        &binary,
        reporter,
        dev.as_ref(),
    ) {
        match spawn_app(&binary, &dev_url, "", &dev_state, dev.as_ref()) {
            Ok(child) => app_child = Some(child),
            Err(e) => reporter.error(format!("could not launch {}: {e}", binary.display())),
        }
    }

    // Watch the peko source and manifest. Events arrive on a Tokio channel so
    // the loop can select against Ctrl-C.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut watcher = match notify::recommended_watcher(
        move |res: notify::Result<notify::Event>| {
            let _ = tx.send(res);
        },
    ) {
        Ok(w) => w,
        Err(e) => {
            reporter.error(format!("could not start the file watcher: {e}"));
            stop_dev_server(&mut dev_server);
            return ExitCode::FAILURE;
        }
    };
    use notify::Watcher;
    let _ = watcher.watch(&root.join("source"), notify::RecursiveMode::Recursive);
    // Also watch the directory that actually holds the entry file: a project can
    // set a custom `[project] entry` (for example src/main.peko) outside the
    // conventional source/ directory, and a save there must still relaunch.
    if let Some(entry_dir) = project.get_entrypoint().parent()
        && entry_dir != root.join("source")
    {
        let _ = watcher.watch(entry_dir, notify::RecursiveMode::Recursive);
    }
    let _ = watcher.watch(&root.join("peko.toml"), notify::RecursiveMode::NonRecursive);
    // Under devtools, also watch the dev-route file to stream navigations to the
    // window. It sits directly in .peko, so a non-recursive watch avoids the
    // churn of the incremental object cache under .peko/incremental.
    if dev.is_some() {
        let _ = watcher.watch(&root.join(".peko"), notify::RecursiveMode::NonRecursive);
    }

    // Under devtools, connect to the app's bridge so the window can drive the
    // interactive console and view source. The client reconnects across
    // relaunches; outgoing calls flow through this sender.
    let mut call_tx: Option<tokio::sync::mpsc::UnboundedSender<String>> = None;
    if let Some(channel) = &dev {
        let bridge_file = root.join(".peko/dev-bridge");
        let _ = std::fs::remove_file(&bridge_file);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(crate::commands::bridge_client::run(
            bridge_file,
            channel.sender(),
            rx,
            channel.shutdown.clone(),
        ));
        call_tx = Some(tx);
    }

    if dev.is_some() {
        reporter.info("watching peko source for changes - close the devtools window to stop");
    } else {
        reporter.info("watching peko source for changes - press Ctrl-C to stop");
    }

    loop {
        // Devtools: honor window actions and the shutdown signal before waiting.
        if let Some(channel) = &dev {
            if channel.shutdown.load(Ordering::Relaxed) {
                break;
            }
            let mut want_rebuild = false;
            let mut want_restart = false;
            while let Ok(action) = channel.actions.try_recv() {
                match action {
                    DevAction::Shutdown => channel.shutdown.store(true, Ordering::Relaxed),
                    DevAction::Rebuild => want_rebuild = true,
                    DevAction::RestartApp => want_restart = true,
                    DevAction::Eval(code) => {
                        if let Some(tx) = &call_tx {
                            let _ = tx.send(devtools_eval_call("eval", &code));
                        }
                    }
                    DevAction::Complete(code) => {
                        if let Some(tx) = &call_tx {
                            let _ = tx.send(devtools_eval_call("complete", &code));
                        }
                    }
                    DevAction::PageInfo => {
                        if let Some(tx) = &call_tx {
                            let _ = tx.send(devtools_eval_call("page", ""));
                        }
                    }
                    DevAction::Resource(url) => {
                        if let Some(tx) = &call_tx {
                            let _ = tx.send(devtools_eval_call("resource", &url));
                        }
                    }
                    DevAction::ViewSource => {
                        if let Some(tx) = &call_tx {
                            let _ = tx.send(devtools_eval_call("source", ""));
                        }
                    }
                }
            }
            if channel.shutdown.load(Ordering::Relaxed) {
                break;
            }
            if want_restart {
                let route = std::fs::read_to_string(&dev_state)
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default();
                if let Some(mut child) = app_child.take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                match spawn_app(&binary, &dev_url, &route, &dev_state, Some(channel)) {
                    Ok(child) => {
                        app_child = Some(child);
                        channel.status("app restarted");
                    }
                    Err(e) => channel.status(format!("restart failed: {e}")),
                }
            }
            if want_rebuild {
                rebuild_and_relaunch(
                    peko_root,
                    release,
                    &mut project,
                    default_target,
                    &incremental_dir,
                    &binary,
                    &dev_url,
                    &dev_state,
                    reporter,
                    Some(channel),
                    &mut app_child,
                );
            }
        }

        tokio::select! {
            _ = tokio::signal::ctrl_c(), if dev.is_none() => break,
            _ = tokio::time::sleep(std::time::Duration::from_millis(150)), if dev.is_some() => {}
            event = rx.recv() => {
                let Some(event) = event else { break };
                let event = match event {
                    Ok(event) => event,
                    Err(_) => continue,
                };

                // Under devtools, a dev-route file change is a navigation, not a
                // source edit: stream it to the window without a rebuild.
                if let Some(channel) = &dev
                    && event
                        .paths
                        .iter()
                        .any(|p| p.file_name().and_then(|n| n.to_str()) == Some("dev-route"))
                {
                    if let Ok(route) = std::fs::read_to_string(&dev_state) {
                        let route = route.trim();
                        if !route.is_empty() {
                            channel.route(route);
                        }
                    }
                    continue;
                }

                if !touches_peko_source(&event) {
                    continue;
                }
                // A save often lands as several events; let them settle so the
                // rebuild runs once.
                tokio::time::sleep(std::time::Duration::from_millis(120)).await;
                while rx.try_recv().is_ok() {}
                rebuild_and_relaunch(
                    peko_root,
                    release,
                    &mut project,
                    default_target,
                    &incremental_dir,
                    &binary,
                    &dev_url,
                    &dev_state,
                    reporter,
                    dev.as_ref(),
                    &mut app_child,
                );
            }
        }
    }

    // Tear down the window and the dev server on the way out.
    if let Some(mut child) = app_child.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    stop_dev_server(&mut dev_server);
    reporter.info("dev session stopped");
    if let Some(channel) = &dev {
        channel.status("dev session stopped");
    }
    ExitCode::SUCCESS
}

/// Build a bridge call that asks the page to evaluate an expression (kind
/// "eval") or return its source (kind "source"). The reply is ignored; the
/// result comes back as a devtools:result event through the bridge client.
fn devtools_eval_call(kind: &str, code: &str) -> String {
    serde_json::json!({
        "t": "call",
        "id": 0,
        "method": "devtools.eval",
        "params": { "id": 0, "kind": kind, "code": code },
    })
    .to_string()
}

/// Read the last route, recompile, and relaunch the window restoring it. Shared
/// by the file-watch path and the devtools Rebuild action. Compilation is
/// CPU-bound and synchronous, so it runs off the async runtime.
#[allow(clippy::too_many_arguments)]
fn rebuild_and_relaunch(
    peko_root: &Path,
    release: bool,
    project: &mut PekoProject,
    target: PekoTarget,
    incremental_dir: &Path,
    binary: &Path,
    dev_url: &str,
    dev_state: &Path,
    reporter: &Reporter,
    dev: Option<&DevChannel>,
    app_child: &mut Option<Child>,
) {
    let route = std::fs::read_to_string(dev_state)
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    reporter.status("Rebuilding", &project.name);
    if let Some(channel) = dev {
        channel.status("rebuilding...");
    }
    let built = tokio::task::block_in_place(|| {
        compile_ui(
            peko_root,
            release,
            project,
            target,
            incremental_dir,
            binary,
            reporter,
            dev,
        )
    });
    if !built {
        return;
    }
    if let Some(mut child) = app_child.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    match spawn_app(binary, dev_url, &route, dev_state, dev) {
        Ok(child) => {
            *app_child = Some(child);
            let message = if route.is_empty() {
                "reloaded".to_string()
            } else {
                format!("reloaded at {route}")
            };
            reporter.success(message.clone());
            if let Some(channel) = dev {
                channel.status(message);
                if !route.is_empty() {
                    channel.route(route);
                }
            }
        }
        Err(e) => {
            reporter.error(format!("could not relaunch: {e}"));
            if let Some(channel) = dev {
                channel.status(format!("relaunch failed: {e}"));
            }
        }
    }
}

/// Compile the UI project's peko source for the host, linking the native binary.
/// Reports diagnostics (to the terminal and, under devtools, to the window) and
/// returns whether the build succeeded.
#[allow(clippy::too_many_arguments)]
pub(crate) fn compile_ui(
    peko_root: &Path,
    release: bool,
    project: &mut PekoProject,
    target: PekoTarget,
    incremental_dir: &Path,
    binary: &Path,
    reporter: &Reporter,
    dev: Option<&DevChannel>,
) -> bool {
    let progress = reporter.progress();
    progress.start_phase(&format!("Building {}", project.name));
    let result = execution::incremental::compile_project(
        peko_root,
        project,
        target,
        incremental_dir.to_path_buf(),
        Some(binary.to_path_buf()),
        false,
        Vec::new(),
        None,
        None,
        !release,
        progress,
    );
    progress.finish_phase();

    match result {
        Ok(Some(diagnostics)) => {
            reporter.report_diagnostics(&diagnostics);
            reporter.error(format!("compilation of {} failed", project.name));
            if let Some(channel) = dev {
                channel.diagnostics(&diagnostics);
                channel.status(format!(
                    "build failed ({} diagnostics)",
                    diagnostics.get_diagnostics().len()
                ));
            }
            false
        }
        Ok(None) => {
            if let Some(channel) = dev {
                channel.clear_diagnostics();
                channel.status("built");
            }
            true
        }
        Err(e) => {
            reporter.error(format!("compilation failed: {e}"));
            if let Some(channel) = dev {
                channel.status(format!("build error: {e}"));
            }
            false
        }
    }
}

/// Launch the built app pointed at the dev server. PEKO_DEV_SERVER selects dev
/// mode in the pekoui host, PEKO_INITIAL_ROUTE restores the route the last build
/// was on, and PEKO_DEV_STATE names the file the app writes its route to.
fn spawn_app(
    binary: &Path,
    dev_url: &str,
    route: &str,
    dev_state: &Path,
    dev: Option<&DevChannel>,
) -> std::io::Result<Child> {
    let mut command = Command::new(binary);
    command
        .env("PEKO_DEV_SERVER", dev_url)
        .env("PEKO_INITIAL_ROUTE", route)
        .env("PEKO_DEV_STATE", dev_state)
        // A webview app never reads the terminal, so deny it stdin rather than
        // risk a child leaving the terminal in a strange mode after a kill.
        .stdin(std::process::Stdio::null());

    // Under devtools the app prints console lines to stdout with a marker; pipe
    // it so a reader thread can split those out. PEKO_DEVTOOLS also tells the
    // client SDK to forward the console, and PEKO_DEV_BRIDGE names the file the
    // app publishes its bridge URL and token to for the devtools bridge client.
    if dev.is_some() {
        command.env("PEKO_DEVTOOLS", "1");
        if let Some(parent) = dev_state.parent() {
            command.env("PEKO_DEV_BRIDGE", parent.join("dev-bridge"));
        }
        command.stdout(std::process::Stdio::piped());
    }

    let mut child = command.spawn()?;

    if let Some(channel) = dev
        && let Some(stdout) = child.stdout.take()
    {
        let events = channel.sender();
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                match line.strip_prefix("@@PEKO_DEVTOOLS@@ ") {
                    Some(payload) => {
                        if let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) {
                            let level = value
                                .get("level")
                                .and_then(|v| v.as_str())
                                .unwrap_or("log")
                                .to_string();
                            let text = value
                                .get("text")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let _ = events.send(devtools::DevEvent::Console(
                                devtools::DevConsoleLine { level, text },
                            ));
                        }
                    }
                    None => println!("{line}"),
                }
            }
        });
    }

    Ok(child)
}

/// Whether a filesystem event touches the peko source: a `.peko` file or the
/// project manifest. Web assets are the dev server's concern and are ignored.
fn touches_peko_source(event: &notify::Event) -> bool {
    event.paths.iter().any(|path| {
        path.extension().and_then(|e| e.to_str()) == Some("peko")
            || path.file_name().and_then(|n| n.to_str()) == Some("peko.toml")
    })
}

/// Whether the project's package.json declares a `dev` script.
pub(crate) fn dev_script_present(root: &std::path::Path) -> bool {
    let Ok(text) = std::fs::read_to_string(root.join("package.json")) else {
        return false;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    json.get("scripts")
        .and_then(|s| s.get("dev"))
        .and_then(|d| d.as_str())
        .is_some()
}

/// Start `npm run dev` and read back the local URL it serves on. The child's
/// output is forwarded so the framework's logs stay visible, and its handle is
/// returned so the caller can stop it. Waits up to 30 seconds for the URL.
/// Stop the dev server together with the process group it leads, so the bundler
/// it spawns (for example Vite) does not outlive it. The group gets a terminate
/// signal first so the bundler can restore the terminal, then the server is
/// reaped.
pub(crate) fn stop_dev_server(child: &mut Child) {
    #[cfg(unix)]
    {
        // The server leads its own group (see start_dev_server), so a negative
        // pid signals every process in it.
        let pid = child.id();
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(format!("-{pid}"))
            .status();
    }
    #[cfg(windows)]
    {
        // Windows has no POSIX process groups. taskkill with /T ends the server
        // and every process it spawned (for example Vite) as a tree, so the
        // bundler does not outlive it.
        let _ = Command::new("taskkill")
            .args(["/F", "/T", "/PID"])
            .arg(child.id().to_string())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

pub(crate) fn start_dev_server(root: &std::path::Path) -> Result<(Child, String), String> {
    use std::io::{BufRead, BufReader};

    let mut command = crate::proc::npm();
    command
        .arg("run")
        .arg("dev")
        .current_dir(root)
        // No stdin: a dev server (Vite) reads the terminal for its interactive
        // shortcuts, which puts it in raw mode. Since the dev loop stops the
        // server by killing it, it never restores the terminal, leaving typing
        // broken until the shell resets. With no stdin it never grabs it.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // Lead a new process group so the whole tree (npm and the bundler it
    // spawns, for example Vite) can be signalled together at teardown. Without
    // this, killing npm orphans the bundler.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("could not run npm run dev: {e}"))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "npm run dev produced no output".to_string())?;
    let stderr = child.stderr.take();

    let (url_tx, url_rx) = std::sync::mpsc::channel::<String>();

    // Forward stdout, and report the first local URL it prints.
    let tx = url_tx.clone();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            if let Some(url) = extract_local_url(&line) {
                let _ = tx.send(url);
            }
            println!("{line}");
        }
    });
    // Forward stderr too (Vite prints some startup notices there).
    if let Some(stderr) = stderr {
        let tx = url_tx.clone();
        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                if let Some(url) = extract_local_url(&line) {
                    let _ = tx.send(url);
                }
                eprintln!("{line}");
            }
        });
    }

    match url_rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(url) => Ok((child, url)),
        Err(_) => {
            stop_dev_server(&mut child);
            Err("timed out waiting for the dev server URL".to_string())
        }
    }
}

/// Extract a localhost dev URL from a line of dev-server output, ignoring ANSI
/// color codes. Matches the first `http://localhost:<port>` or
/// `http://127.0.0.1:<port>` and trims a trailing slash.
fn extract_local_url(line: &str) -> Option<String> {
    let clean = strip_ansi(line);
    let start = clean.find("http://localhost").or_else(|| clean.find("http://127.0.0.1"))?;
    let rest = &clean[start..];
    let end = rest
        .find(|c: char| c.is_whitespace())
        .unwrap_or(rest.len());
    let url = rest[..end].trim_end_matches('/');
    if url.contains(':') && url.rsplit(':').next().is_some_and(|p| p.parse::<u16>().is_ok()) {
        Some(url.to_string())
    } else {
        None
    }
}

/// Remove ANSI escape sequences (CSI ... final byte) from a string.
fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // Skip an escape sequence: the introducer, then up to a letter.
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}
