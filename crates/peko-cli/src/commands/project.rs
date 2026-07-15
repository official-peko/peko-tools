//! `peko project`: create, view, and inspect Peko projects.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use eframe::egui;
use egui::{ColorImage, TextureHandle};
use rustyline::Editor;
use rustyline::history::FileHistory;

use crate::cli::CLIInfo;
use crate::cli::reporting::Reporter;
use crate::project::PekoProject;

// ---------------------------------------------------------------------------
// Top-level dispatcher
// ---------------------------------------------------------------------------

/// Execute the `project` subcommand.
pub async fn execute(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    let Some(subcommand) = cli_info.arguments.get(1) else {
        reporter.error("`project` requires a subcommand");
        reporter.help(format!(
            "run '{} help project' to see how this command works",
            cli_info.executable
        ));
        return ExitCode::FAILURE;
    };

    // `new` scaffolds a project, so it does not need one to already exist.
    if subcommand == "new" {
        return execute_new(cli_info, reporter);
    }

    // Every other subcommand needs a loaded project.
    let project = match PekoProject::from_current_directory() {
        Ok(p) => p,
        Err(e) => {
            reporter.error(format!("could not load project: {e}"));
            reporter.help("create a peko.toml in the project root to define a project");
            return ExitCode::FAILURE;
        }
    };

    match subcommand.as_str() {
        "add-asset" => execute_add_asset(cli_info, reporter, project),
        "remove-asset" => execute_remove_asset(cli_info, reporter, project),
        "show-info" => execute_show_info(reporter, project),
        "show-icon" => execute_show_icon(reporter, project),
        "show-assets" => execute_show_assets(cli_info, reporter, project),
        other => {
            reporter.error(format!(
                "no such subcommand '{other}' for 'project' command"
            ));
            reporter.help(format!(
                "run '{} help project' to see a list of valid subcommands",
                cli_info.executable
            ));
            ExitCode::FAILURE
        }
    }
}

// ---------------------------------------------------------------------------
// Subcommand: new
// ---------------------------------------------------------------------------

/// `peko project new`: scaffold a fresh project, prompting for its details.
fn execute_new(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    // A --name flag drives non-interactive scaffolding, so the IDE and scripts
    // create projects through flags instead of prompts.
    if cli_info.flags.has_flag("name") {
        return execute_new_noninteractive(cli_info, reporter);
    }

    println!("---------------------");
    println!(">> Create a project <<");
    println!("---------------------");

    let mut rl = match Editor::<(), FileHistory>::new() {
        Ok(rl) => rl,
        Err(e) => {
            reporter.error(format!("could not initialize prompt editor: {e}"));
            return ExitCode::FAILURE;
        }
    };

    // If the current directory is already a web-framework project (and not yet
    // a Peko project), offer to set it up in place instead of scaffolding a new
    // subdirectory. This turns a raw React/Vue/etc. project into a Peko app.
    if let Ok(current_dir) = std::env::current_dir()
        && !current_dir.join("peko.toml").is_file()
        && let Some(framework) = detect_web_framework(&current_dir)
    {
        let adopt = confirmation_prompt(
            &mut rl,
            &format!(
                "* Found an existing {framework} project here. Set up THIS directory as a Peko app (Y/n) => "
            ),
            true,
        );
        if adopt {
            return adopt_current_directory(cli_info, reporter, &mut rl, &current_dir, &framework);
        }
    }

    // Choose the project type: a UI (native app), a plain CLI program, or a
    // distributable library package.
    let project_type = match prompt_project_type() {
        Some(project_type) => project_type,
        None => {
            reporter.error("no project type was selected");
            return ExitCode::FAILURE;
        }
    };
    let project_is_ui = matches!(project_type, ProjectType::Ui);

    let project_name = match rl.readline("* Project name     => ") {
        Ok(name) => name.trim().to_owned(),
        Err(e) => {
            reporter.error(format!("could not read project name: {e}"));
            return ExitCode::FAILURE;
        }
    };
    if project_name.is_empty() {
        reporter.error("project name cannot be empty");
        return ExitCode::FAILURE;
    }

    let mut bundle_id = String::new();
    if project_is_ui {
        bundle_id = match rl.readline("* Bundle ID        => ") {
            Ok(value) => value.trim().to_owned(),
            Err(e) => {
                reporter.error(format!("could not read bundle id: {e}"));
                return ExitCode::FAILURE;
            }
        };
    }

    let version = match rl.readline_with_initial("* Project version  => ", ("0.1.0", "")) {
        Ok(value) => normalize_version(&value),
        Err(e) => {
            reporter.error(format!("could not read project version: {e}"));
            return ExitCode::FAILURE;
        }
    };

    // A UI project is a static (SSG) web app built with a web framework, taken
    // from `--framework <template>` when given, otherwise chosen from a menu.
    // The value is the matching create-vite template.
    let framework = if project_is_ui {
        match cli_info.flags.get_flag("framework") {
            Some(framework) => framework,
            None => match prompt_web_framework() {
                Some(framework) => framework,
                None => {
                    reporter.error("no web framework was selected");
                    return ExitCode::FAILURE;
                }
            },
        }
    } else {
        String::new()
    };

    let cwd = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(e) => {
            reporter.error(format!("cannot read current directory: {e}"));
            return ExitCode::FAILURE;
        }
    };

    finish_new_project(
        cli_info,
        reporter,
        project_type,
        &project_name,
        &bundle_id,
        &version,
        &framework,
        &cwd,
    )
}

/// The kind of project `project new` scaffolds.
#[derive(Clone, Copy)]
enum ProjectType {
    /// A native app with a web frontend (static SSG or server SSR).
    Ui,
    /// A plain command-line program.
    Cli,
    /// A distributable library package (`[package]` + `[lib]`).
    Package,
}

/// Present a menu of project types and return the selection, or `None` if it
/// was aborted.
fn prompt_project_type() -> Option<ProjectType> {
    const CHOICES: [(&str, ProjectType); 3] = [
        ("UI project (native app with a web frontend)", ProjectType::Ui),
        ("CLI project (a command-line program)", ProjectType::Cli),
        ("Package (a library for distribution)", ProjectType::Package),
    ];
    let labels: Vec<&str> = CHOICES.iter().map(|(label, _)| *label).collect();
    let selection = dialoguer::Select::new()
        .with_prompt("Project type")
        .items(&labels)
        .default(0)
        .interact_opt()
        .ok()
        .flatten()?;
    Some(CHOICES[selection].1)
}

/// Parse the `--type <ui|cli|package>` flag.
fn parse_project_type(value: &str) -> Option<ProjectType> {
    match value.trim().to_lowercase().as_str() {
        "ui" => Some(ProjectType::Ui),
        "cli" => Some(ProjectType::Cli),
        "package" | "pkg" | "lib" | "library" => Some(ProjectType::Package),
        _ => None,
    }
}

/// `peko project new --name <name> [flags]`: scaffold without prompts. The type
/// is chosen with `--type ui|cli|package` (UI is the default, or opt out with
/// --no-ui for a CLI project); --bundle, --version, --framework, and --dir fill
/// the remaining details, with the same defaults the prompts offer.
fn execute_new_noninteractive(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    let project_name = cli_info
        .flags
        .get_flag("name")
        .unwrap_or_default()
        .trim()
        .to_owned();
    if project_name.is_empty() {
        reporter.error("project name cannot be empty");
        return ExitCode::FAILURE;
    }

    // `--type` selects the kind explicitly; without it, `--no-ui` picks a CLI
    // project and the default is a UI project (preserving prior behavior).
    let project_type = match cli_info.flags.get_flag("type") {
        Some(value) => match parse_project_type(&value) {
            Some(project_type) => project_type,
            None => {
                reporter.error(format!(
                    "unknown project type '{value}'; expected ui, cli, or package"
                ));
                return ExitCode::FAILURE;
            }
        },
        None if cli_info.flags.has_flag("no-ui") => ProjectType::Cli,
        None => ProjectType::Ui,
    };
    let project_is_ui = matches!(project_type, ProjectType::Ui);

    let bundle_id = if project_is_ui {
        cli_info.flags.get_flag("bundle").unwrap_or_default()
    } else {
        String::new()
    };

    let version = cli_info
        .flags
        .get_flag("version")
        .map(|value| normalize_version(&value))
        .unwrap_or_else(|| "0.1.0".to_owned());

    // A UI project needs a web-framework template. Default to React with
    // TypeScript, matching the first typed choice the menu offers.
    let framework = if project_is_ui {
        cli_info
            .flags
            .get_flag("framework")
            .unwrap_or_else(|| "react-ts".to_owned())
    } else {
        String::new()
    };

    let base_dir = match cli_info.flags.get_flag("dir") {
        Some(dir) => expand_home(&dir),
        None => match std::env::current_dir() {
            Ok(dir) => dir,
            Err(e) => {
                reporter.error(format!("cannot read current directory: {e}"));
                return ExitCode::FAILURE;
            }
        },
    };
    if !base_dir.is_dir() {
        reporter.error(format!(
            "target directory does not exist: {}",
            base_dir.display()
        ));
        return ExitCode::FAILURE;
    }

    finish_new_project(
        cli_info,
        reporter,
        project_type,
        &project_name,
        &bundle_id,
        &version,
        &framework,
        &base_dir,
    )
}

/// Resolve the project root under base_dir, handle an existing folder (removed
/// only with --force), and scaffold either a UI (web) tree or a plain CLI tree.
/// Shared by the interactive and non-interactive `project new` paths.
#[allow(clippy::too_many_arguments)]
fn finish_new_project(
    cli_info: &CLIInfo,
    reporter: &Reporter,
    project_type: ProjectType,
    project_name: &str,
    bundle_id: &str,
    version: &str,
    framework: &str,
    base_dir: &Path,
) -> ExitCode {
    let project_is_ui = matches!(project_type, ProjectType::Ui);
    let project_root = base_dir.join(project_name);

    if project_root.exists() {
        if !cli_info.flags.has_flag("force") {
            reporter.error(format!(
                "a folder named '{project_name}' already exists here"
            ));
            reporter.help(format!(
                "run '{} project new --force' to overwrite it",
                cli_info.executable
            ));
            return ExitCode::FAILURE;
        }
        reporter.info(format!("--force specified, removing existing '{project_name}'"));
        let removal = if project_root.is_dir() {
            std::fs::remove_dir_all(&project_root)
        } else {
            std::fs::remove_file(&project_root)
        };
        if let Err(e) = removal {
            reporter.error(format!("could not remove existing '{project_name}': {e}"));
            return ExitCode::FAILURE;
        }
    }

    // A UI project is scaffolded with the framework's own tool. A static (SSG)
    // app uses create-vite and is overlaid with the native Peko host; a server
    // (SSR) app uses the framework's own scaffolder and is deployed to the
    // platform. A CLI project is a plain source tree.
    if project_is_ui {
        if peko_core::config::ServerFramework::from_str(framework).is_some() {
            // A --name run (Studio, scripts) is non-interactive; a prompted run
            // lets the framework's tool prompt.
            let non_interactive = cli_info.flags.get_flag("name").is_some();
            return scaffold_server_project(
                reporter,
                project_name,
                bundle_id,
                version,
                framework,
                &project_root,
                non_interactive,
            );
        }
        return scaffold_ui_project(
            cli_info,
            reporter,
            project_name,
            bundle_id,
            version,
            framework,
            &project_root,
        );
    }

    // A library package is a source tree with a `[package]`/`[lib]` manifest and
    // a `source/lib.peko` entry, packed and published rather than built.
    if matches!(project_type, ProjectType::Package) {
        return scaffold_package_project(reporter, project_name, version, &project_root);
    }

    if let Err(e) = std::fs::create_dir_all(project_root.join("src")) {
        reporter.error(format!("could not create source directory: {e}"));
        return ExitCode::FAILURE;
    }

    let manifest = CLI_MANIFEST_TEMPLATE
        .replace("{name}", project_name)
        .replace("{version}", version);
    let files: Vec<(PathBuf, Vec<u8>)> = vec![
        (project_root.join("peko.toml"), manifest.into_bytes()),
        (
            project_root.join("src/main.peko"),
            CLI_MAIN_PEKO_TEMPLATE.as_bytes().to_vec(),
        ),
    ];
    for (path, bytes) in &files {
        if let Err(e) = std::fs::write(path, bytes) {
            reporter.error(format!("could not write {}: {e}", path.display()));
            return ExitCode::FAILURE;
        }
    }

    reporter.success(format!(
        "created new project {project_name} at {}",
        project_root.display()
    ));
    ExitCode::SUCCESS
}

/// Scaffold a UI (static web) project: run the web framework's own scaffolder
/// (create-vite), then overlay the Peko host manifest, the entry, and a small
/// Vite tweak so the build output lands in `assets/` with relative asset URLs.
fn scaffold_ui_project(
    cli_info: &CLIInfo,
    reporter: &Reporter,
    project_name: &str,
    bundle_id: &str,
    version: &str,
    framework: &str,
    project_root: &Path,
) -> ExitCode {
    // The web framework is scaffolded by its own tool, so npm must be present.
    // npm is a batch script on Windows, so it launches as npm.cmd there.
    let npm_present = crate::proc::npm()
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !npm_present {
        reporter.error("npm is required to scaffold a UI (web) project, but it was not found");
        reporter.help("install Node.js (which bundles npm) from https://nodejs.org, then re-run");
        return ExitCode::FAILURE;
    }

    let scaffold_dir = project_root.parent().unwrap_or_else(|| Path::new("."));

    reporter.status(
        "Scaffolding",
        format!("{framework} web app with create-vite"),
    );
    let create = crate::proc::npm()
        .args([
            "create",
            "vite@latest",
            project_name,
            "--",
            "--template",
            framework,
        ])
        .current_dir(scaffold_dir)
        .stdin(std::process::Stdio::null())
        .status();
    match create {
        Ok(status) if status.success() => {}
        Ok(_) => {
            reporter.error(format!(
                "create-vite did not scaffold the '{framework}' template successfully"
            ));
            reporter.help("check that the framework name is a valid create-vite template");
            return ExitCode::FAILURE;
        }
        Err(e) => {
            reporter.error(format!("could not run create-vite through npm: {e}"));
            return ExitCode::FAILURE;
        }
    }

    if !overlay_peko_host(cli_info, reporter, project_name, bundle_id, version, project_root) {
        return ExitCode::FAILURE;
    }

    reporter.success(format!(
        "created new UI project {project_name} at {}",
        project_root.display()
    ));
    reporter.info(format!(
        "next: cd {project_name} && npm install, then `peko build`"
    ));
    ExitCode::SUCCESS
}

/// The manifest for a server (SSR) app: `[project]` plus a `[ui]` that names the
/// SSR framework. No entry, no native target platforms — it is deployed, not
/// bundled.
const SERVER_MANIFEST_TEMPLATE: &str = "[project]\n\
    name = \"{name}\"\n\
    bundle = \"{bundle}\"\n\
    version = \"{version}\"\n\
    \n\
    # A server (SSR) app deployed to Peko hosting. Link it to a platform app with\n\
    # `peko link <app-id>`, then deploy with `peko deploy server`.\n\
    [ui]\n\
    framework = \"{framework}\"\n";

/// One SSR framework's scaffolder: the command that creates the app and the
/// self-hosting configuration the user must ensure before deploying.
struct ServerScaffolder {
    /// The framework id, matching `ServerFramework::as_str`.
    id: &'static str,
    /// A human-readable name.
    display: &'static str,
    /// `true` to launch via npx, `false` via `npm create`.
    npx: bool,
    /// The command arguments, with `{name}` replaced by the project name.
    args: &'static [&'static str],
    /// Extra arguments appended when scaffolding non-interactively (from Studio
    /// or a script), so the create tool uses defaults instead of prompting.
    /// Best-effort — the tools evolve.
    yes_args: &'static [&'static str],
    /// The one self-hosting step the framework needs for a Node deploy (or a
    /// note that none is required).
    hosting: &'static str,
}

/// The scaffolder for each server framework. The create tools evolve, so these
/// invocations are intentionally minimal and run interactively — the framework's
/// own tool owns its prompts.
const SERVER_SCAFFOLDERS: &[ServerScaffolder] = &[
    ServerScaffolder {
        id: "next",
        display: "Next.js",
        npx: false,
        args: &["create", "next-app@latest", "{name}"],
        yes_args: &["--yes"],
        hosting: "output: 'standalone' (set automatically)",
    },
    ServerScaffolder {
        id: "nuxt",
        display: "Nuxt",
        npx: false,
        args: &["create", "nuxt@latest", "{name}"],
        yes_args: &["--packageManager", "npm", "--no-gitInit"],
        hosting: "none needed — `nuxt build` emits the Nitro node server (.output)",
    },
    ServerScaffolder {
        id: "sveltekit",
        display: "SvelteKit",
        npx: true,
        args: &["sv", "create", "{name}"],
        yes_args: &["--template", "minimal", "--types", "ts", "--no-add-ons", "--no-install"],
        hosting: "install @sveltejs/adapter-node and use it in svelte.config.js",
    },
    ServerScaffolder {
        id: "remix",
        display: "Remix / React Router",
        npx: false,
        args: &["create", "react-router@latest", "{name}"],
        yes_args: &["--yes"],
        hosting: "none needed — the framework build emits build/server",
    },
    ServerScaffolder {
        id: "astro",
        display: "Astro",
        npx: false,
        args: &["create", "astro@latest", "{name}"],
        yes_args: &["--template", "minimal", "--no-install", "--no-git", "--skip-houston"],
        hosting: "add @astrojs/node (output: 'server') in astro.config.mjs",
    },
    ServerScaffolder {
        id: "angular",
        display: "Angular",
        npx: true,
        args: &["@angular/cli@latest", "new", "{name}", "--ssr"],
        yes_args: &["--defaults", "--skip-git", "--skip-install"],
        hosting: "none needed — created with --ssr",
    },
];

fn server_scaffolder(id: &str) -> Option<&'static ServerScaffolder> {
    SERVER_SCAFFOLDERS.iter().find(|s| s.id == id)
}

/// Scaffold a server (SSR) project: run the framework's own create tool
/// interactively, then write a server manifest that links it to Peko hosting.
/// The app is deployed with `peko deploy server`, not built locally.
fn scaffold_server_project(
    reporter: &Reporter,
    project_name: &str,
    bundle_id: &str,
    version: &str,
    framework: &str,
    project_root: &Path,
    non_interactive: bool,
) -> ExitCode {
    let Some(scaffolder) = server_scaffolder(framework) else {
        reporter.error(format!("no scaffolder for server framework '{framework}'"));
        return ExitCode::FAILURE;
    };

    // npm must be present (npx ships with it) to run the framework's scaffolder.
    let npm_present = crate::proc::npm()
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !npm_present {
        reporter.error(format!(
            "Node.js is required to scaffold a {} project, but npm was not found",
            scaffolder.display
        ));
        reporter.help("install Node.js (which bundles npm and npx) from https://nodejs.org");
        return ExitCode::FAILURE;
    }

    let scaffold_dir = project_root.parent().unwrap_or_else(|| Path::new("."));
    let mut args: Vec<String> = scaffolder
        .args
        .iter()
        .map(|a| a.replace("{name}", project_name))
        .collect();
    // Non-interactive callers (Studio, scripts) have no TTY for the tool's
    // prompts, so pass its "use defaults" flags.
    if non_interactive {
        args.extend(scaffolder.yes_args.iter().map(|a| (*a).to_owned()));
    }

    reporter.status(
        "Scaffolding",
        format!("{} app (runs the framework's own scaffolder)", scaffolder.display),
    );
    // Run interactively (inherited stdio) so the framework's prompts work.
    let mut command = if scaffolder.npx {
        crate::proc::npx()
    } else {
        crate::proc::npm()
    };
    match command.args(&args).current_dir(scaffold_dir).status() {
        Ok(status) if status.success() => {}
        Ok(_) => {
            reporter.error(format!(
                "{} scaffolding did not complete successfully",
                scaffolder.display
            ));
            return ExitCode::FAILURE;
        }
        Err(e) => {
            reporter.error(format!("could not run the {} scaffolder: {e}", scaffolder.display));
            return ExitCode::FAILURE;
        }
    }

    if !project_root.is_dir() {
        reporter.error(format!(
            "the scaffolder did not create {}",
            project_root.display()
        ));
        return ExitCode::FAILURE;
    }

    // Overlay the server manifest that links the app to Peko hosting.
    let manifest = SERVER_MANIFEST_TEMPLATE
        .replace("{name}", project_name)
        .replace("{bundle}", bundle_id)
        .replace("{version}", version)
        .replace("{framework}", framework);
    if let Err(e) = std::fs::write(project_root.join("peko.toml"), manifest) {
        reporter.error(format!("could not write peko.toml: {e}"));
        return ExitCode::FAILURE;
    }

    reporter.success(format!(
        "created new {} server app {project_name} at {}",
        scaffolder.display,
        project_root.display()
    ));

    // Configure the framework for a self-hosted Node deploy. Next only needs
    // `output: 'standalone'`, which we set automatically; the others need a
    // package/config step the CLI leaves to the user.
    if framework == "next" {
        match enable_next_standalone(project_root) {
            Ok(true) => reporter.info("enabled standalone output in next.config for server hosting"),
            Ok(false) => reporter.warning(
                "could not set `output: 'standalone'` automatically — add it to next.config before deploying",
            ),
            Err(e) => reporter.warning(format!(
                "could not update next.config ({e}) — add `output: 'standalone'` before deploying"
            )),
        }
    } else {
        reporter.info(format!("self-hosting: {}", scaffolder.hosting));
    }

    reporter.info(format!(
        "next: cd {project_name}, then `peko link <app-id>` and `peko deploy server`"
    ));
    ExitCode::SUCCESS
}

/// Enable Next.js `output: 'standalone'` in the scaffolded next.config so the
/// deploy can package the standalone server. Returns whether it is now set
/// (already present, or injected). Handles the create-next-app TS/JS configs.
fn enable_next_standalone(project_root: &Path) -> std::io::Result<bool> {
    for name in [
        "next.config.ts",
        "next.config.mjs",
        "next.config.js",
        "next.config.cjs",
    ] {
        let path = project_root.join(name);
        if !path.is_file() {
            continue;
        }
        let source = std::fs::read_to_string(&path)?;
        if source.contains("output:") {
            // Respect an existing output setting rather than duplicating it.
            return Ok(true);
        }
        if let Some(patched) = inject_output_standalone(&source) {
            std::fs::write(&path, patched)?;
            return Ok(true);
        }
        return Ok(false);
    }
    // No config file found: write a minimal one that sets standalone output.
    std::fs::write(
        project_root.join("next.config.mjs"),
        "/** @type {import('next').NextConfig} */\n\
         const nextConfig = { output: 'standalone' };\nexport default nextConfig;\n",
    )?;
    Ok(true)
}

/// Insert `output: "standalone"` at the start of the Next config object,
/// matching the object opened by `= {` or `export default {`.
fn inject_output_standalone(source: &str) -> Option<String> {
    let brace = ["= {", "export default {"]
        .iter()
        .filter_map(|pattern| source.find(pattern).map(|i| i + pattern.len() - 1))
        .min()?;
    let mut out = String::with_capacity(source.len() + 32);
    out.push_str(&source[..brace]);
    out.push_str("{\n  output: \"standalone\",\n");
    out.push_str(&source[brace + 1..]);
    Some(out)
}

/// The framework's own source folder to place `main.peko` in, so the native
/// host lives beside the app code — an existing `src/` (create-vite) or `app/`,
/// else the project root.
fn ui_source_dir(project_root: &Path) -> &'static str {
    if project_root.join("src").is_dir() {
        "src"
    } else if project_root.join("app").is_dir() {
        "app"
    } else {
        ""
    }
}

/// Overlay the Peko host onto a web-framework project rooted at `project_root`:
/// the manifest that marks it a static UI project and depends on pekoui, the
/// one-line Peko entry, the Vite output config, and the @peko/client
/// dependency. Shared by scaffolding a new project and adopting an existing
/// one. Returns false when a hard error was reported.
fn overlay_peko_host(
    cli_info: &CLIInfo,
    reporter: &Reporter,
    project_name: &str,
    bundle_id: &str,
    version: &str,
    project_root: &Path,
) -> bool {
    // Overlay the Peko host: a manifest that marks this a static UI project and
    // depends on the installed pekoui package, plus the one-line entry.
    let pekoui_version = latest_pekoui_version(cli_info.get_peko_root());
    let pekoui_path = cli_info
        .get_peko_root()
        .join(format!("registry/src/pekoui/pekoui-{pekoui_version}"));
    // Place main.peko in the framework's own source folder so the native host
    // lives alongside the app code, and mark it as the entry in the manifest.
    let source_dir = ui_source_dir(project_root);
    let entry = if source_dir.is_empty() {
        "main.peko".to_owned()
    } else {
        format!("{source_dir}/main.peko")
    };
    let manifest = UI_MANIFEST_TEMPLATE
        .replace("{name}", project_name)
        .replace("{bundle}", bundle_id)
        .replace("{version}", version)
        .replace("{entry}", &entry)
        .replace("{pekoui_version}", &pekoui_version);

    if !source_dir.is_empty()
        && let Err(e) = std::fs::create_dir_all(project_root.join(source_dir))
    {
        reporter.error(format!("could not create source directory: {e}"));
        return false;
    }
    let overlay: Vec<(PathBuf, &[u8])> = vec![
        (project_root.join("peko.toml"), manifest.as_bytes()),
        (project_root.join(&entry), UI_MAIN_PEKO_TEMPLATE.as_bytes()),
    ];
    for (path, bytes) in &overlay {
        if let Err(e) = std::fs::write(path, bytes) {
            reporter.error(format!("could not write {}: {e}", path.display()));
            return false;
        }
    }

    // Point Vite at `assets/` with relative URLs so the built app is served
    // by the pekoui asset server under its /_assets/ prefix.
    if let Err(e) = patch_vite_config(project_root) {
        reporter.warning(format!(
            "set up, but could not adjust the Vite config automatically: {e}. \
             Set `base: './'` and `build.outDir: 'assets'` in vite.config manually."
        ));
    }

    // Add the @peko/client SDK as a local dependency so the web app can import
    // it and call native handlers. It resolves against the installed pekoui
    // package. npm install (run by the build) picks it up.
    if let Err(e) = add_client_dependency(project_root, &pekoui_path) {
        reporter.warning(format!(
            "set up, but could not add the @peko/client dependency automatically: {e}. \
             Add \"@peko/client\": \"file:{}/client\" to package.json dependencies manually.",
            pekoui_path.display()
        ));
    }

    // Import @peko/client from the web entry so the client SDK connects to the
    // bridge on load. The dependency alone is inert: nothing runs until the
    // module is imported.
    if let Err(e) = inject_client_import(project_root) {
        reporter.warning(format!(
            "set up, but could not import @peko/client into the web entry automatically: {e}. \
             Add `import '@peko/client'` to the top of your web entry (for example src/main.tsx)."
        ));
    }

    true
}

/// Detects a compatible web framework in `dir` by reading its package.json.
/// Returns a short framework label (react, vue, svelte, solid, preact, or
/// vite) when a known framework dependency or the Vite build tool is present.
fn detect_web_framework(dir: &Path) -> Option<String> {
    let source = std::fs::read_to_string(dir.join("package.json")).ok()?;
    let package: serde_json::Value = serde_json::from_str(&source).ok()?;

    let mut names: Vec<String> = Vec::new();
    for table in ["dependencies", "devDependencies"] {
        if let Some(map) = package.get(table).and_then(|value| value.as_object()) {
            names.extend(map.keys().cloned());
        }
    }
    let has = |name: &str| names.iter().any(|entry| entry == name);

    if has("react") || has("react-dom") {
        Some("react".to_string())
    } else if has("vue") {
        Some("vue".to_string())
    } else if has("svelte") {
        Some("svelte".to_string())
    } else if has("solid-js") {
        Some("solid".to_string())
    } else if has("preact") {
        Some("preact".to_string())
    } else if has("vite") {
        Some("vite".to_string())
    } else {
        None
    }
}

/// Reads a top-level string field from the package.json in `dir`.
fn package_json_field(dir: &Path, field: &str) -> Option<String> {
    let source = std::fs::read_to_string(dir.join("package.json")).ok()?;
    let package: serde_json::Value = serde_json::from_str(&source).ok()?;
    package
        .get(field)
        .and_then(|value| value.as_str())
        .map(str::to_owned)
}

/// Set up the current directory, an existing web-framework project, as a Peko
/// app in place. Prompts for the app details, defaulting from package.json,
/// then overlays the Peko host without scaffolding a new framework tree.
fn adopt_current_directory(
    cli_info: &CLIInfo,
    reporter: &Reporter,
    rl: &mut Editor<(), FileHistory>,
    project_dir: &Path,
    framework: &str,
) -> ExitCode {
    let default_name = package_json_field(project_dir, "name")
        .map(|name| name.rsplit('/').next().unwrap_or("app").to_owned())
        .or_else(|| {
            project_dir
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "app".to_string());

    let project_name = match rl.readline_with_initial("* Project name     => ", (&default_name, "")) {
        Ok(value) => value.trim().to_owned(),
        Err(e) => {
            reporter.error(format!("could not read project name: {e}"));
            return ExitCode::FAILURE;
        }
    };
    if project_name.is_empty() {
        reporter.error("project name cannot be empty");
        return ExitCode::FAILURE;
    }

    let slug: String = project_name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_lowercase();
    let default_bundle = format!(
        "com.example.{}",
        if slug.is_empty() { "app" } else { &slug }
    );
    let bundle_id = match rl.readline_with_initial("* Bundle ID        => ", (&default_bundle, "")) {
        Ok(value) => value.trim().to_owned(),
        Err(e) => {
            reporter.error(format!("could not read bundle id: {e}"));
            return ExitCode::FAILURE;
        }
    };

    let default_version = package_json_field(project_dir, "version")
        .filter(|version| !version.is_empty())
        .unwrap_or_else(|| "0.1.0".to_string());
    let version = match rl.readline_with_initial("* Project version  => ", (&default_version, "")) {
        Ok(value) => normalize_version(&value),
        Err(e) => {
            reporter.error(format!("could not read project version: {e}"));
            return ExitCode::FAILURE;
        }
    };

    reporter.status(
        "Setting up",
        format!("the existing {framework} project as a Peko app"),
    );
    if !overlay_peko_host(cli_info, reporter, &project_name, &bundle_id, &version, project_dir) {
        return ExitCode::FAILURE;
    }

    reporter.success(format!(
        "set up the existing {framework} project as a Peko app"
    ));
    reporter.info("next: npm install, then `peko build`".to_string());
    ExitCode::SUCCESS
}

/// Add `@peko/client` to the scaffolded package.json as a local `file:`
/// dependency pointing at the installed pekoui package's client directory, so
/// `import { peko } from '@peko/client'` resolves after npm install.
fn add_client_dependency(project_root: &Path, pekoui_path: &Path) -> std::io::Result<()> {
    let package_path = project_root.join("package.json");
    let source = std::fs::read_to_string(&package_path)?;
    let mut package: serde_json::Value = serde_json::from_str(&source)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    let dependency = format!("file:{}", config_path_string(&pekoui_path.join("client")));
    let object = package
        .as_object_mut()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "package.json is not an object"))?;
    let dependencies = object
        .entry("dependencies")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if let Some(dependencies) = dependencies.as_object_mut() {
        dependencies.insert("@peko/client".to_string(), serde_json::Value::String(dependency));
    }

    let mut rendered = serde_json::to_string_pretty(&package)?;
    rendered.push('\n');
    std::fs::write(&package_path, rendered)
}

/// The newest pekoui version installed under `registry/src/pekoui`, so a
/// scaffolded project depends on the current package rather than a pinned one.
/// Matching the globally installed pekoui also keeps the project and the global
/// auto-import on one version, so pekoui compiles once. Falls back to a known
/// version when none is installed.
fn latest_pekoui_version(peko_root: &Path) -> String {
    let dir = peko_root.join("registry/src/pekoui");
    let mut best: Option<semver::Version> = None;
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(rest) = name.to_string_lossy().strip_prefix("pekoui-").map(str::to_owned)
            else {
                continue;
            };
            if let Ok(version) = semver::Version::parse(&rest)
                && best.as_ref().is_none_or(|current| version > *current)
            {
                best = Some(version);
            }
        }
    }
    best.map(|version| version.to_string())
        .unwrap_or_else(|| "0.1.0".to_string())
}

/// Prepend a side-effect import of `@peko/client` to the web entry so the
/// client SDK's auto-connect block runs on load and opens the bridge. The entry
/// is the module script named in index.html. A no-op when the import is already
/// present. A side-effect import is framework agnostic.
fn inject_client_import(project_root: &Path) -> std::io::Result<()> {
    let html = std::fs::read_to_string(project_root.join("index.html"))?;
    let entry_rel = html
        .split("type=\"module\"")
        .nth(1)
        .and_then(|rest| rest.split("src=\"").nth(1))
        .and_then(|rest| rest.split('"').next())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no module script in index.html",
            )
        })?
        .trim_start_matches('/');
    let entry = project_root.join(entry_rel);
    let source = std::fs::read_to_string(&entry)?;
    if source.contains("@peko/client") {
        return Ok(());
    }
    std::fs::write(&entry, format!("import '@peko/client'\n{source}"))
}

/// Inject `base` and `build.outDir` into the Vite config create-vite emitted,
/// so a `vite build` writes into `assets/` with relative asset URLs.
fn patch_vite_config(project_root: &Path) -> std::io::Result<()> {
    let candidates = ["vite.config.js", "vite.config.ts", "vite.config.mjs"];
    let config_path = candidates
        .iter()
        .map(|name| project_root.join(name))
        .find(|path| path.is_file());
    let Some(config_path) = config_path else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no vite.config file found",
        ));
    };

    let source = std::fs::read_to_string(&config_path)?;
    if source.contains("outDir") {
        return Ok(());
    }
    // base and outDir put the built app where the asset server serves it. The
    // dedupe resolves the framework runtime from the project root so a
    // symlinked local @peko/client adapter imports the app's own copy.
    let injected = source.replacen(
        "defineConfig({",
        "defineConfig({\n  base: './',\n  build: { outDir: 'assets', emptyOutDir: true },\n  resolve: { dedupe: ['react', 'react-dom', 'vue'] },",
        1,
    );
    std::fs::write(&config_path, injected)
}

/// Normalize a typed version into bare semver, dropping a leading `v`.
fn normalize_version(input: &str) -> String {
    let trimmed = input.trim();
    trimmed.strip_prefix('v').unwrap_or(trimmed).to_owned()
}

/// Present an arrow-key menu of web frameworks and return the selected
/// create-vite template name, or `None` if the selection was aborted.
fn prompt_web_framework() -> Option<String> {
    // (menu label, framework id). The first group are create-vite templates for
    // a static app in a native window; the second are server (SSR) frameworks
    // the platform hosts, whose ids match ServerFramework.
    const CHOICES: [(&str, &str); 15] = [
        ("React (static)", "react"),
        ("React + TypeScript (static)", "react-ts"),
        ("Vue (static)", "vue"),
        ("Vue + TypeScript (static)", "vue-ts"),
        ("Svelte (static)", "svelte"),
        ("Svelte + TypeScript (static)", "svelte-ts"),
        ("Solid (static)", "solid"),
        ("Preact (static)", "preact"),
        ("Vanilla JS (static)", "vanilla"),
        ("Next.js (server)", "next"),
        ("Nuxt (server)", "nuxt"),
        ("SvelteKit (server)", "sveltekit"),
        ("Remix / React Router (server)", "remix"),
        ("Astro (server)", "astro"),
        ("Angular (server)", "angular"),
    ];
    let labels: Vec<&str> = CHOICES.iter().map(|(label, _)| *label).collect();
    let selection = dialoguer::Select::new()
        .with_prompt("Web framework")
        .items(&labels)
        .default(0)
        .interact_opt()
        .ok()
        .flatten()?;
    Some(CHOICES[selection].1.to_owned())
}

/// Ask a yes/no question, returning `true` for affirmative answers.
fn confirmation_prompt(rl: &mut Editor<(), FileHistory>, prompt: &str, default_yes: bool) -> bool {
    let default_initial = if default_yes { "y" } else { "n" };
    loop {
        let answer = match rl.readline_with_initial(prompt, (default_initial, "")) {
            Ok(answer) => answer,
            Err(_) => return default_yes,
        };
        match answer.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => return true,
            "n" | "no" => return false,
            "" => return default_yes,
            _ => println!(">> please type either yes or no"),
        }
    }
}

/// Render a filesystem path for a manifest or config file using forward
/// slashes. A path built by joining a forward-slash literal onto a Windows base
/// otherwise mixes separators (`C:\Users\me\.Peko\registry/src/...`). Backslashes
/// are also escape characters in a TOML double-quoted string, so a raw Windows
/// path is fragile there. Forward slashes avoid both problems and are accepted
/// on every platform when the path is read back.
fn config_path_string(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

/// Expand a leading `~` (or `~/...`, `~\...`) in a directory argument to the
/// user's home directory. A `~` a UI passes through unexpanded then resolves to
/// the home directory instead of a literal `~` path that does not exist.
fn expand_home(dir: &str) -> PathBuf {
    let rest = if dir == "~" {
        Some("")
    } else {
        dir.strip_prefix("~/").or_else(|| dir.strip_prefix("~\\"))
    };
    match rest {
        Some(rest) => match dirs::home_dir() {
            Some(home) if rest.is_empty() => home,
            Some(home) => home.join(rest),
            None => PathBuf::from(dir),
        },
        None => PathBuf::from(dir),
    }
}

/// The `peko.toml` scaffolded for a UI (static web) project.
const UI_MANIFEST_TEMPLATE: &str = "[project]\n\
                                    name = \"{name}\"\n\
                                    bundle = \"{bundle}\"\n\
                                    version = \"{version}\"\n\
                                    entry = \"{entry}\"\n\
                                    target_platforms = [\"android\", \"ios\", \"linux\", \"macos\", \"windows\"]\n\
                                    \n\
                                    [ui]\n\
                                    framework = \"static\"\n\
                                    \n\
                                    [dependencies]\n\
                                    pekoui = \"{pekoui_version}\"\n";

/// The `src/main.peko` scaffolded for a UI project: a one-line host that
/// serves the built web app in a native webview.
const UI_MAIN_PEKO_TEMPLATE: &str = "import pekoui as ui;\n\
                                     \n\
                                     fn on_start() {\n\
                                     \tui::app::from_bundle().run()\n\
                                     }\n";

/// Scaffold a library package: a `[package]`/`[lib]` manifest plus an empty
/// `source/lib.peko` entry and README. Folded in from the former `pkg new`.
fn scaffold_package_project(
    reporter: &Reporter,
    project_name: &str,
    version: &str,
    project_root: &Path,
) -> ExitCode {
    if let Err(e) = std::fs::create_dir_all(project_root.join("source")) {
        reporter.error(format!("could not create source directory: {e}"));
        return ExitCode::FAILURE;
    }

    let manifest = PACKAGE_MANIFEST_TEMPLATE
        .replace("{name}", project_name)
        .replace("{version}", version);
    let files: &[(&str, &[u8])] = &[
        ("peko.toml", manifest.as_bytes()),
        ("README.md", b""),
        ("source/lib.peko", b""),
    ];
    for (relative, bytes) in files {
        let path = project_root.join(relative);
        if let Err(e) = std::fs::write(&path, bytes) {
            reporter.error(format!("could not write {}: {e}", path.display()));
            return ExitCode::FAILURE;
        }
    }

    reporter.success(format!(
        "created new package {project_name} at {}",
        project_root.display()
    ));
    ExitCode::SUCCESS
}

/// The `peko.toml` scaffolded for a library package.
const PACKAGE_MANIFEST_TEMPLATE: &str = "[package]\n\
                                         name = \"{name}\"\n\
                                         version = \"{version}\"\n\
                                         description = \"\"\n\
                                         \n\
                                         [lib]\n\
                                         root = \"source/lib.peko\"\n\
                                         \n\
                                         [dependencies]\n";

/// The `peko.toml` scaffolded for a CLI project.
const CLI_MANIFEST_TEMPLATE: &str = "[project]\n\
                                     name = \"{name}\"\n\
                                     version = \"{version}\"\n\
                                     entry = \"src/main.peko\"\n\
                                     \n\
                                     [dependencies]\n";

/// The `src/main.peko` scaffolded for a CLI project.
const CLI_MAIN_PEKO_TEMPLATE: &str = "import std::io;\n\
                                      \n\
                                      fn on_start() {\n\
                                      \tio::println(\"Hello World!\")\n\
                                      }\n";

// ---------------------------------------------------------------------------
// Subcommand: add-asset
// ---------------------------------------------------------------------------

fn execute_add_asset(cli_info: &CLIInfo, reporter: &Reporter, project: PekoProject) -> ExitCode {
    let assets_dir = project.assets_dir();

    if project.ui_project_info.is_none() {
        reporter.error("CLI projects don't have assets");
        return ExitCode::FAILURE;
    }

    let Some(asset_arg) = cli_info.arguments.get(2) else {
        reporter.error("`project add-asset` requires a path to an asset file");
        reporter.help(format!(
            "run '{} help project' to see how this command works",
            cli_info.executable
        ));
        return ExitCode::FAILURE;
    };

    let asset_path = PathBuf::from(asset_arg);
    if !asset_path.exists() {
        reporter.error(format!(
            "asset file '{}' does not exist",
            asset_path.display()
        ));
        return ExitCode::FAILURE;
    }

    let Some(asset_file_name) = asset_path.file_name() else {
        reporter.error("couldn't get asset file name");
        return ExitCode::FAILURE;
    };

    let asset_output = assets_dir.join(asset_file_name);
    if asset_output.exists() && !cli_info.flags.has_flag("force") {
        reporter.error(format!(
            "asset {} is already in project",
            asset_file_name.display()
        ));
        reporter.info("if you still wish to add this asset, rerun with --force to replace");
        return ExitCode::FAILURE;
    }

    if let Err(e) = std::fs::create_dir_all(&assets_dir) {
        reporter.error(format!(
            "could not create assets directory {}: {e}",
            assets_dir.display()
        ));
        return ExitCode::FAILURE;
    }

    if let Err(e) = std::fs::copy(&asset_path, &asset_output) {
        reporter.error(format!(
            "could not write asset file to {}: {e}",
            asset_output.display()
        ));
        return ExitCode::FAILURE;
    }

    reporter.success(format!(
        "added asset '{}'",
        asset_file_name.to_string_lossy()
    ));
    ExitCode::SUCCESS
}

// ---------------------------------------------------------------------------
// Subcommand: remove-asset
// ---------------------------------------------------------------------------

fn execute_remove_asset(cli_info: &CLIInfo, reporter: &Reporter, project: PekoProject) -> ExitCode {
    let assets_dir = project.assets_dir();

    if project.ui_project_info.is_none() {
        reporter.error("CLI projects don't have assets");
        return ExitCode::FAILURE;
    }

    let Some(asset_arg) = cli_info.arguments.get(2) else {
        reporter.error("`project remove-asset` requires an asset file name");
        reporter.help(format!(
            "run '{} help project' to see how this command works",
            cli_info.executable
        ));
        return ExitCode::FAILURE;
    };

    // The argument is the asset name relative to the assets directory.
    // Forward-slash separated names address files in subdirectories.
    let asset_output = assets_dir.join(asset_arg);
    if !asset_output.exists() {
        reporter.error(format!("asset {asset_arg} doesn't exist in project"));
        return ExitCode::FAILURE;
    }

    if let Err(e) = std::fs::remove_file(&asset_output) {
        reporter.error(format!(
            "could not remove asset file {}: {e}",
            asset_output.display()
        ));
        return ExitCode::FAILURE;
    }

    reporter.success(format!("removed asset '{asset_arg}'"));
    ExitCode::SUCCESS
}

// ---------------------------------------------------------------------------
// Subcommand: show-info
// ---------------------------------------------------------------------------

fn execute_show_info(reporter: &Reporter, project: PekoProject) -> ExitCode {
    let kind = if project.ui_project_info.is_some() {
        "UI"
    } else {
        "CLI"
    };
    println!("---- {kind} Project Info ----");

    let Some(ui_info) = project.ui_project_info.as_ref() else {
        println!("> {}", project.name);
        println!("--------------------------");
        return ExitCode::SUCCESS;
    };

    println!(
        "> {}@{} ({})",
        project.name, ui_info.version, ui_info.bundle_id
    );

    let platforms = ui_info
        .platforms
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    println!("Target platforms: {platforms}");
    println!("--------------------------");

    // Silence the unused-reporter warning while keeping the symmetric
    // signature for future use.
    let _ = reporter;
    ExitCode::SUCCESS
}

// ---------------------------------------------------------------------------
// Subcommand: show-icon
// ---------------------------------------------------------------------------

fn execute_show_icon(reporter: &Reporter, project: PekoProject) -> ExitCode {
    let Some(ui_info) = project.ui_project_info.as_ref() else {
        reporter.error("CLI projects don't have icons");
        return ExitCode::FAILURE;
    };

    let image_pixels = ui_info.icon.get_rgba_pixels();
    let image_width = ui_info.icon.width as usize;
    let image_height = (image_pixels.len() / 4)
        .checked_div(image_width)
        .unwrap_or(0);

    let result = eframe::run_native(
        "App Icon",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size((500.0, 500.0))
                .with_resizable(false),
            ..Default::default()
        },
        Box::new(move |_cc| {
            Ok(Box::new(IconRenderApp {
                texture: None,
                image: ColorImage::from_rgba_unmultiplied(
                    [image_width, image_height],
                    image_pixels.as_slice(),
                ),
            }) as Box<dyn eframe::App>)
        }),
    );

    if let Err(e) = result {
        reporter.error(format!("could not display icon window: {e}"));
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

/// eframe app used by `show-icon` to render the icon in a popup window.
#[derive(Default)]
struct IconRenderApp {
    texture: Option<TextureHandle>,
    image: ColorImage,
}

impl eframe::App for IconRenderApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            if self.texture.is_none() {
                self.texture = Some(ctx.load_texture(
                    "appicon",
                    self.image.clone(),
                    egui::TextureOptions::default(),
                ));
            }
            if let Some(texture) = &self.texture {
                let img = egui::Image::from_texture(texture)
                    .max_width(500.0)
                    .max_height(500.0);
                ui.add(img);
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Subcommand: show-assets
// ---------------------------------------------------------------------------

fn execute_show_assets(_cli_info: &CLIInfo, reporter: &Reporter, project: PekoProject) -> ExitCode {
    if project.ui_project_info.is_none() {
        reporter.error("CLI projects don't have an assets folder");
        return ExitCode::FAILURE;
    }

    let assets_path = project.assets_dir();
    if let Err(e) = std::fs::create_dir_all(&assets_path) {
        reporter.error(format!(
            "could not create assets directory {}: {e}",
            assets_path.display()
        ));
        return ExitCode::FAILURE;
    }
    if let Err(e) = open::that(&assets_path) {
        reporter.error(format!("could not open {}: {e}", assets_path.display()));
        return ExitCode::FAILURE;
    }

    reporter.info("the assets folder is the project's asset set; files added or removed here are picked up on the next build");

    ExitCode::SUCCESS
}
