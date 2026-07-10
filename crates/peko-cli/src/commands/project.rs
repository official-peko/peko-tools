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

    let project_is_ui = confirmation_prompt(&mut rl, "* UI project (Y/n) => ", true);

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
        project_is_ui,
        &project_name,
        &bundle_id,
        &version,
        &framework,
        &cwd,
    )
}

/// `peko project new --name <name> [flags]`: scaffold without prompts. UI is the
/// default (opt out with --no-ui); --bundle, --version, --framework, and --dir
/// fill the remaining details, with the same defaults the prompts offer.
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

    let project_is_ui = !cli_info.flags.has_flag("no-ui");

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
        Some(dir) => PathBuf::from(dir),
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
        project_is_ui,
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
    project_is_ui: bool,
    project_name: &str,
    bundle_id: &str,
    version: &str,
    framework: &str,
    base_dir: &Path,
) -> ExitCode {
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

    // A UI (web) project is scaffolded with the web framework's own tool
    // (create-vite), then overlaid with the Peko host. A CLI project is a
    // plain source tree.
    if project_is_ui {
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
    let pekoui_path = cli_info
        .get_peko_root()
        .join("registry/src/pekoui/pekoui-0.1.0");
    let manifest = UI_MANIFEST_TEMPLATE
        .replace("{name}", project_name)
        .replace("{bundle}", bundle_id)
        .replace("{version}", version)
        .replace("{pekoui_path}", &pekoui_path.display().to_string());

    if let Err(e) = std::fs::create_dir_all(project_root.join("src")) {
        reporter.error(format!("could not create source directory: {e}"));
        return false;
    }
    let overlay: Vec<(PathBuf, &[u8])> = vec![
        (project_root.join("peko.toml"), manifest.as_bytes()),
        (
            project_root.join("src/main.peko"),
            UI_MAIN_PEKO_TEMPLATE.as_bytes(),
        ),
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

    let dependency = format!("file:{}", pekoui_path.join("client").display());
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
    // (menu label, create-vite template name)
    const CHOICES: [(&str, &str); 9] = [
        ("React", "react"),
        ("React + TypeScript", "react-ts"),
        ("Vue", "vue"),
        ("Vue + TypeScript", "vue-ts"),
        ("Svelte", "svelte"),
        ("Svelte + TypeScript", "svelte-ts"),
        ("Solid", "solid"),
        ("Preact", "preact"),
        ("Vanilla JS", "vanilla"),
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

/// The `peko.toml` scaffolded for a UI (static web) project.
const UI_MANIFEST_TEMPLATE: &str = "[project]\n\
                                    name = \"{name}\"\n\
                                    bundle = \"{bundle}\"\n\
                                    version = \"{version}\"\n\
                                    entry = \"src/main.peko\"\n\
                                    target_platforms = [\"android\", \"ios\", \"linux\", \"macos\", \"windows\"]\n\
                                    \n\
                                    [ui]\n\
                                    framework = \"static\"\n\
                                    \n\
                                    [dependencies]\n\
                                    pekoui = { path = \"{pekoui_path}\" }\n";

/// The `src/main.peko` scaffolded for a UI project: a one-line host that
/// serves the built web app in a native webview.
const UI_MAIN_PEKO_TEMPLATE: &str = "import pekoui as ui;\n\
                                     \n\
                                     fn on_start() {\n\
                                     \tui::app::from_bundle().run()\n\
                                     }\n";

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
