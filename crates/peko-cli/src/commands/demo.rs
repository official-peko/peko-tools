//! `peko demo` - run the app's demo shots to verify the automation flow.
//!
//! Builds the current UI project in demo mode, launches it against the
//! framework dev server with PEKO_DEMO set, and drives its demo shots through
//! the in-page automation agent over the app bridge. No screenshots or
//! recordings are produced; this verifies that the shot scripts navigate, find
//! elements, and interact as written.
//!
//! The driver is a bridge client. It fetches the shot list and each shot's
//! steps from the pekoshots native handlers (shots.list, shots.get), then for
//! each step calls shots.dispatch, which the app relays to the page as a
//! shots:step event; the in-page agent runs the step and reports back through
//! shots.report, relayed to this driver as a shots:done event.

use std::path::Path;
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;

use peko_core::target::PekoTarget;

use crate::cli::CLIInfo;
use crate::cli::reporting::Reporter;
use crate::commands::run;
use crate::project::PekoProject;

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

pub async fn execute(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    let mut project = match PekoProject::from_current_directory() {
        Ok(project) => project,
        Err(e) => {
            reporter.error(format!("could not load project: {e}"));
            return ExitCode::FAILURE;
        }
    };

    if project.ui_project_info.is_none() {
        reporter.error("peko demo needs a UI project (an app with a [ui] table)");
        return ExitCode::FAILURE;
    }

    let release = cli_info.flags.has_flag("release");
    let ide = cli_info.flags.has_flag("ide");
    let shot_name = cli_info
        .arguments
        .get(1)
        .cloned()
        .or_else(|| cli_info.flags.get_flag("shot"));
    let from_index: usize = cli_info
        .flags
        .get_flag("from")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    // The pause between shots, so a watcher can take each one in. Seconds,
    // default 3, override with --delay. Does not gate control and never pauses
    // between the steps within a shot; it is purely a pause between shots.
    let delay = Duration::from_secs_f64(
        cli_info
            .flags
            .get_flag("delay")
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(3.0)
            .max(0.0),
    );

    let root = project.get_root().to_path_buf();
    let build_directory = root.join(if release { "build/release" } else { "build/debug" });

    // Resolve dependencies before compiling.
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

    if !run::dev_script_present(&root) {
        reporter.error(
            "peko demo needs a framework dev script (a `dev` entry in package.json, e.g. npm run dev)",
        );
        return ExitCode::FAILURE;
    }

    let target = PekoTarget::default();
    let arch_build_dir = build_directory
        .join(target.operating_system.to_string())
        .join(target.architecture.to_string());
    if let Err(e) = std::fs::create_dir_all(&arch_build_dir) {
        reporter.error(format!(
            "could not create build directory {}: {e}",
            arch_build_dir.display()
        ));
        return ExitCode::FAILURE;
    }
    let binary = arch_build_dir.join(&project.name);
    let incremental_dir = root.join(".peko/incremental");
    let dev_state = root.join(".peko/dev-route");
    let _ = std::fs::remove_file(&dev_state);
    let bridge_file = root.join(".peko/demo-bridge");
    let _ = std::fs::remove_file(&bridge_file);

    // Install web dependencies once.
    if !root.join("node_modules").is_dir() {
        reporter.status("Installing", "web dependencies (npm install)");
        match crate::proc::npm().arg("install").current_dir(&root).status() {
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
    let (mut dev_server, dev_url) = match run::start_dev_server(&root) {
        Ok(pair) => pair,
        Err(e) => {
            reporter.error(format!("could not start the dev server: {e}"));
            return ExitCode::FAILURE;
        }
    };
    reporter.success(format!("dev server ready at {dev_url}"));

    // Build the app in demo mode.
    if !run::compile_ui(
        cli_info.get_peko_root(),
        release,
        true,
        &mut project,
        target,
        &incremental_dir,
        &binary,
        reporter,
        None,
    ) {
        run::stop_dev_server(&mut dev_server);
        return ExitCode::FAILURE;
    }

    // Launch the app pointed at the dev server, in demo mode, publishing its
    // bridge coordinates to bridge_file.
    let mut app_child = match spawn_demo_app(&binary, &dev_url, &dev_state, &bridge_file) {
        Ok(child) => child,
        Err(e) => {
            reporter.error(format!("could not launch {}: {e}", binary.display()));
            run::stop_dev_server(&mut dev_server);
            return ExitCode::FAILURE;
        }
    };

    let outcome = drive(&bridge_file, shot_name, from_index, ide, delay, reporter).await;

    // Teardown: the app first, then the dev server and its process group.
    let _ = app_child.kill();
    let _ = app_child.wait();
    run::stop_dev_server(&mut dev_server);

    match outcome {
        Ok(()) => {
            reporter.success("demo run complete");
            ExitCode::SUCCESS
        }
        Err(e) => {
            reporter.error(e);
            ExitCode::FAILURE
        }
    }
}

/// Launch the built app in demo mode. PEKO_DEMO selects the demo path in the
/// pekoui host and the pekoshots runtime; PEKO_DEV_BRIDGE names the file the app
/// publishes its bridge URL and token to.
fn spawn_demo_app(
    binary: &Path,
    dev_url: &str,
    dev_state: &Path,
    bridge_file: &Path,
) -> std::io::Result<Child> {
    Command::new(binary)
        .env("PEKO_DEV_SERVER", dev_url)
        .env("PEKO_INITIAL_ROUTE", "")
        .env("PEKO_DEV_STATE", dev_state)
        .env("PEKO_DEMO", "1")
        .env("PEKO_DEV_BRIDGE", bridge_file)
        .stdin(Stdio::null())
        .spawn()
}

/// Connect to the app bridge and drive the shots.
async fn drive(
    bridge_file: &Path,
    shot_name: Option<String>,
    from_index: usize,
    ide: bool,
    delay: Duration,
    reporter: &Reporter,
) -> Result<(), String> {
    let (url, token) = wait_for_coords(bridge_file, Duration::from_secs(30)).await?;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .map_err(|e| format!("could not connect to the app bridge: {e}"))?;

    ws.send(Message::text(format!("{{\"t\":\"auth\",\"token\":\"{token}\"}}")))
        .await
        .map_err(|e| format!("bridge auth failed: {e}"))?;
    wait_ready(&mut ws).await?;

    let mut call_id: u64 = 0;

    // Fetch the shot list and pick the targets.
    let list = call(&mut ws, &mut call_id, "shots.list", "{}").await?;
    let entries = list
        .as_array()
        .ok_or_else(|| "the app returned no shots (is pekoshots attached?)".to_string())?;
    let mut names: Vec<String> = entries
        .iter()
        .filter_map(|e| e.get("name").and_then(|n| n.as_str()).map(String::from))
        .collect();
    if let Some(ref only) = shot_name {
        if !names.iter().any(|n| n == only) {
            return Err(format!("no shot named '{only}'"));
        }
        names.retain(|n| n == only);
    }
    if names.is_empty() {
        return Err("this app declares no demo shots".to_string());
    }

    reporter.info(format!(
        "driving {} shot(s), {:.1}s between shots",
        names.len(),
        delay.as_secs_f64()
    ));
    reporter.info("waiting for the in-page agent");
    probe_ready(&mut ws, &mut call_id).await?;

    let mut first_shot = true;
    for name in &names {
        // Pause between shots so a watcher can take each one in. Never before
        // the first shot, and never between the steps within a shot.
        if !first_shot {
            tokio::time::sleep(delay).await;
        }
        first_shot = false;

        let shot = call(
            &mut ws,
            &mut call_id,
            "shots.get",
            &format!("{{\"name\":{}}}", serde_json::to_string(name).unwrap()),
        )
        .await?;
        let steps: Vec<Value> = shot
            .get("steps")
            .and_then(|s| s.as_array())
            .cloned()
            .unwrap_or_default();
        // --from applies when a single shot is targeted.
        let start = if shot_name.is_some() { from_index } else { 0 };
        if ide {
            run_shot_ide(&mut ws, &mut call_id, name, &steps, reporter).await?;
        } else {
            run_shot(&mut ws, &mut call_id, name, &steps, start, reporter).await?;
        }
    }
    Ok(())
}

/// Run every step of a shot in order, printing each result. Stops the shot at
/// the first failing step.
async fn run_shot(
    ws: &mut Ws,
    call_id: &mut u64,
    name: &str,
    steps: &[Value],
    from: usize,
    reporter: &Reporter,
) -> Result<(), String> {
    reporter.info(format!("shot {name}: {} steps", steps.len()));
    for (index, step) in steps.iter().enumerate() {
        if index < from {
            continue;
        }
        let (ok, op, note) = run_one(ws, call_id, name, index, step).await?;
        if ok {
            println!("  {index:>3}  {op}  ok{note}");
        } else {
            println!("  {index:>3}  {op}  FAILED: {note}");
            return Err(format!("shot {name} failed at step {index} ({op})"));
        }
    }
    Ok(())
}

/// Step through a shot under IDE control, reading commands on stdin: run, step,
/// skip <n>, pause, stop.
async fn run_shot_ide(
    ws: &mut Ws,
    call_id: &mut u64,
    name: &str,
    steps: &[Value],
    reporter: &Reporter,
) -> Result<(), String> {
    reporter.info(format!(
        "ide: shot {name} has {} steps. commands: run, step, skip <n>, pause, stop",
        steps.len()
    ));

    // A blocking stdin reader thread feeds commands to the async loop.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        use std::io::BufRead;
        let stdin = std::io::stdin();
        for line in stdin.lock().lines().map_while(Result::ok) {
            if tx.send(line.trim().to_string()).is_err() {
                break;
            }
        }
    });

    let mut index: usize = 0;
    while let Some(cmd) = rx.recv().await {
        if cmd == "stop" {
            break;
        } else if cmd == "pause" {
            // Stepping runs synchronously, so there is nothing to interrupt.
        } else if cmd == "step" || cmd == "run" {
            let run_all = cmd == "run";
            loop {
                if index >= steps.len() {
                    println!("  (end of shot)");
                    break;
                }
                let step = &steps[index];
                let (ok, op, note) = run_one(ws, call_id, name, index, step).await?;
                if ok {
                    println!("  {index:>3}  {op}  ok{note}");
                } else {
                    println!("  {index:>3}  {op}  FAILED: {note}");
                }
                index += 1;
                if !run_all || !ok {
                    break;
                }
            }
        } else if let Some(rest) = cmd.strip_prefix("skip ") {
            match rest.trim().parse::<usize>() {
                Ok(n) => {
                    index = n.min(steps.len());
                    println!("  (at step {index})");
                }
                Err(_) => println!("  usage: skip <n>"),
            }
        } else if !cmd.is_empty() {
            println!("  commands: run, step, skip <n>, pause, stop");
        }
    }
    Ok(())
}

/// Dispatch one step to the page and wait for its result. Returns whether it
/// succeeded, the op name, and a short note (a detail summary or the error).
async fn run_one(
    ws: &mut Ws,
    call_id: &mut u64,
    name: &str,
    index: usize,
    step: &Value,
) -> Result<(bool, String, String), String> {
    let step_id = format!("{name}#{index}");
    let params = format!(
        "{{\"id\":{},\"step\":{}}}",
        serde_json::to_string(&step_id).unwrap(),
        step
    );
    send_call(ws, call_id, "shots.dispatch", &params).await?;
    let data = wait_event(ws, "shots:done", &step_id, Duration::from_secs(30)).await?;

    let ok = data.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    let op = data
        .get("op")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();
    let note = if ok {
        match data.get("detail") {
            Some(d) if !is_empty_object(d) => format!(" {d}"),
            _ => String::new(),
        }
    } else {
        data.get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("failed")
            .to_string()
    };
    Ok((ok, op, note))
}

fn is_empty_object(value: &Value) -> bool {
    value.as_object().map(|o| o.is_empty()).unwrap_or(false)
}

/// Probe the in-page agent until it answers, so we do not dispatch shot steps
/// before the page has loaded and wired itself.
async fn probe_ready(ws: &mut Ws, call_id: &mut u64) -> Result<(), String> {
    for _ in 0..60 {
        send_call(
            ws,
            call_id,
            "shots.dispatch",
            "{\"id\":\"__probe__\",\"step\":{\"op\":\"ping\"}}",
        )
        .await?;
        if let Ok(data) = wait_event(ws, "shots:done", "__probe__", Duration::from_millis(500)).await
        {
            // Report what the in-page agent says it loaded, so a run that shows
            // no cursor can be diagnosed from the terminal.
            if let Some(detail) = data.get("detail") {
                println!("  agent: {detail}");
            }
            return Ok(());
        }
    }
    Err("the demo agent never came up (does the app import @peko/pekoshots?)".to_string())
}

/// Send a bridge call, without waiting for its reply.
async fn send_call(
    ws: &mut Ws,
    call_id: &mut u64,
    method: &str,
    params: &str,
) -> Result<(), String> {
    let id = *call_id;
    *call_id += 1;
    let message = format!("{{\"t\":\"call\",\"id\":{id},\"method\":\"{method}\",\"params\":{params}}}");
    ws.send(Message::text(message))
        .await
        .map_err(|e| format!("bridge send failed: {e}"))
}

/// Send a bridge call and wait for its reply, returning the result value.
async fn call(
    ws: &mut Ws,
    call_id: &mut u64,
    method: &str,
    params: &str,
) -> Result<Value, String> {
    let id = *call_id;
    send_call(ws, call_id, method, params).await?;

    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        let value = match next_json(ws, Duration::from_secs(30)).await? {
            Some(value) => value,
            None => continue,
        };
        if value.get("t").and_then(|t| t.as_str()) == Some("reply")
            && value.get("id").and_then(|i| i.as_u64()) == Some(id)
        {
            if value.get("ok").and_then(|o| o.as_bool()).unwrap_or(false) {
                return Ok(value.get("result").cloned().unwrap_or(Value::Null));
            }
            return Err(format!(
                "call {method} failed: {}",
                value.get("error").map(|e| e.to_string()).unwrap_or_default()
            ));
        }
    }
    Err(format!("timed out waiting for a reply to {method}"))
}

/// Wait for an event with the given name whose data.id matches, up to timeout.
async fn wait_event(
    ws: &mut Ws,
    name: &str,
    id: &str,
    timeout: Duration,
) -> Result<Value, String> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        let value = match next_json(ws, timeout).await? {
            Some(value) => value,
            None => continue,
        };
        if value.get("t").and_then(|t| t.as_str()) == Some("event")
            && value.get("name").and_then(|n| n.as_str()) == Some(name)
            && let Some(data) = value.get("data")
                && data.get("id").and_then(|i| i.as_str()) == Some(id) {
                    return Ok(data.clone());
                }
    }
    Err(format!("timed out waiting for {name}"))
}

/// Wait for the bridge ready message after auth.
async fn wait_ready(ws: &mut Ws) -> Result<(), String> {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        let value = match next_json(ws, Duration::from_secs(10)).await? {
            Some(value) => value,
            None => continue,
        };
        if value.get("t").and_then(|t| t.as_str()) == Some("ready") {
            return Ok(());
        }
    }
    Err("the app bridge did not become ready".to_string())
}

/// Read the next JSON message, or None on a read timeout or an unparseable
/// frame. Errs only when the socket closes or errors.
async fn next_json(ws: &mut Ws, timeout: Duration) -> Result<Option<Value>, String> {
    match tokio::time::timeout(timeout, ws.next()).await {
        Err(_) => Ok(None),
        Ok(None) => Err("the app bridge closed".to_string()),
        Ok(Some(Err(e))) => Err(format!("bridge error: {e}")),
        Ok(Some(Ok(message))) => {
            if message.is_close() {
                return Err("the app bridge closed".to_string());
            }
            match message.to_text() {
                Ok(text) => Ok(serde_json::from_str(text).ok()),
                Err(_) => Ok(None),
            }
        }
    }
}

/// Poll the dev-bridge file until the app publishes its coordinates.
async fn wait_for_coords(bridge_file: &Path, timeout: Duration) -> Result<(String, String), String> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Ok(text) = std::fs::read_to_string(bridge_file)
            && let Ok(value) = serde_json::from_str::<Value>(&text)
                && let (Some(url), Some(token)) = (
                    value.get("url").and_then(|v| v.as_str()),
                    value.get("token").and_then(|v| v.as_str()),
                ) {
                    return Ok((url.to_string(), token.to_string()));
                }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Err("the app did not publish its bridge (did it launch?)".to_string())
}
