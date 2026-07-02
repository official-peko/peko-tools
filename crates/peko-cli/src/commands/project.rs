//! `peko project`: create, view, and inspect Peko projects.

use std::path::PathBuf;
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

    let cwd = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(e) => {
            reporter.error(format!("cannot read current directory: {e}"));
            return ExitCode::FAILURE;
        }
    };
    let project_root = cwd.join(&project_name);

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

    let mut directories = vec![project_root.join("source")];
    if project_is_ui {
        directories.push(project_root.join("assets"));
        directories.push(project_root.join("source/pages/index"));
    }
    for directory in &directories {
        if let Err(e) = std::fs::create_dir_all(directory) {
            reporter.error(format!("could not create {}: {e}", directory.display()));
            return ExitCode::FAILURE;
        }
    }

    let manifest = if project_is_ui {
        UI_MANIFEST_TEMPLATE
            .replace("{name}", &project_name)
            .replace("{bundle}", &bundle_id)
            .replace("{version}", &version)
    } else {
        CLI_MANIFEST_TEMPLATE
            .replace("{name}", &project_name)
            .replace("{version}", &version)
    };

    let main_peko = if project_is_ui {
        ui_main_peko_template(&project_name)
    } else {
        CLI_MAIN_PEKO_TEMPLATE.to_owned()
    };

    let mut files: Vec<(PathBuf, Vec<u8>)> = vec![
        (project_root.join("peko.toml"), manifest.into_bytes()),
        (project_root.join("source/main.peko"), main_peko.into_bytes()),
    ];
    if project_is_ui {
        files.push((project_root.join("source/root_styles.scss"), Vec::new()));
        files.push((
            project_root.join("source/pages/index/page.peko"),
            INDEX_PAGE_TEMPLATE.as_bytes().to_vec(),
        ));
        files.push((
            project_root.join("source/pages/index/page_styles.scss"),
            Vec::new(),
        ));
    }
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

/// Normalize a typed version into bare semver, dropping a leading `v`.
fn normalize_version(input: &str) -> String {
    let trimmed = input.trim();
    trimmed.strip_prefix('v').unwrap_or(trimmed).to_owned()
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

/// The `peko.toml` scaffolded for a UI project.
const UI_MANIFEST_TEMPLATE: &str = "[project]\n\
                                    name = \"{name}\"\n\
                                    bundle = \"{bundle}\"\n\
                                    version = \"{version}\"\n\
                                    target_platforms = [\"android\", \"ios\", \"linux\", \"macos\", \"windows\"]\n\
                                    \n\
                                    [ui]\n\
                                    framework = \"native\"\n\
                                    \n\
                                    [dependencies]\n";

/// The `peko.toml` scaffolded for a CLI project.
const CLI_MANIFEST_TEMPLATE: &str = "[project]\n\
                                     name = \"{name}\"\n\
                                     version = \"{version}\"\n\
                                     \n\
                                     [dependencies]\n";

/// The `source/main.peko` scaffolded for a CLI project.
const CLI_MAIN_PEKO_TEMPLATE: &str = "import std::io;\n\
                                      \n\
                                      fn on_start() {\n\
                                      \tio::println(\"Hello World!\")\n\
                                      }\n";

/// The starter page scaffolded for a UI project.
const INDEX_PAGE_TEMPLATE: &str = "style page_styles;\n\
                                   \n\
                                   class Index from ui::Page {\n\
                                   \tconstructor() => super() {\n\
                                   \t\tthis.set_styling(ui::Styling(page_styles));\n\
                                   \t}\n\
                                   \n\
                                   \tfn render() => ui::Element {\n\
                                   \t\treturn <h1>Hello World</h1>;\n\
                                   \t}\n\
                                   }\n";

/// The `source/main.peko` scaffolded for a UI project.
fn ui_main_peko_template(project_name: &str) -> String {
    format!(
        "import {{ Index }} from pages::index;\n\
         style root_styles;\n\
         \n\
         fn OnStart() {{\n\
         \tapp := ui::App(\"{project_name}\", 800, 800);\n\
         \tapp.set_root_layout(closure(content: ui::Element) => ui::Element {{\n\
         \t\treturn content;\n\
         \t}}, ui::Styling(root_styles));\n\
         \tapp.add_page(\"/\", Index());\n\
         \tapp.run();\n\
         }}\n"
    )
}

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
