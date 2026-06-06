//! `peko project`: create, view, and modify Peko projects.

use std::path::PathBuf;
use std::process::ExitCode;

use eframe::egui;
use egui::{ColorImage, TextureHandle};
use peko_core::target::OperatingSystem;
use rustyline::history::FileHistory;
use rustyline::Editor;

use crate::bundler;
use crate::cli::reporting::Reporter;
use crate::cli::CLIInfo;
use crate::project::{PekoProject, ProjectIcon, UIProjectInfo};

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

    // `new` is special since it doesn't require a project to exist yet.
    if subcommand == "new" {
        return execute_new(cli_info, reporter);
    }

    // Every other subcommand needs a loaded project.
    let project = match PekoProject::from_current_directory() {
        Ok(p) => p,
        Err(e) => {
            reporter.error(format!("could not load project: {e}"));
            reporter.help(format!(
                "run '{} project new' to create a new project here",
                cli_info.executable
            ));
            return ExitCode::FAILURE;
        }
    };

    match subcommand.as_str() {
        "modify" => execute_modify(cli_info, reporter, project),
        "set-icon" => execute_set_icon(cli_info, reporter, project),
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
        Ok(name) => name,
        Err(e) => {
            reporter.error(format!("could not read project name: {e}"));
            return ExitCode::FAILURE;
        }
    };

    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            reporter.error(format!("cannot read current directory: {e}"));
            return ExitCode::FAILURE;
        }
    };
    let project_root_folder = cwd.join(&project_name);

    let mut peko_project = PekoProject::new(
        project_name.clone(),
        project_root_folder.clone(),
        None,
        project_root_folder.join("main.peko"),
        None,
    );

    if project_is_ui {
        let bundle_id = match rl.readline("* Bundle ID        => ") {
            Ok(v) => v,
            Err(e) => {
                reporter.error(format!("could not read bundle id: {e}"));
                return ExitCode::FAILURE;
            }
        };

        let version = match rl.readline_with_initial("* Project version  => ", ("v0.1.0", "")) {
            Ok(v) => v,
            Err(e) => {
                reporter.error(format!("could not read project version: {e}"));
                return ExitCode::FAILURE;
            }
        };

        let default_icon_path = cli_info
            .get_peko_root()
            .join("Compiler/bundling/defaulticon.bin");
        let default_icon_bytes = match std::fs::read(&default_icon_path) {
            Ok(b) => b,
            Err(e) => {
                reporter.error(format!(
                    "could not read default icon at {}: {e}",
                    default_icon_path.display()
                ));
                return ExitCode::FAILURE;
            }
        };

        peko_project.ui_project_info = Some(UIProjectInfo::new(
            bundle_id,
            version,
            vec![
                OperatingSystem::Android,
                OperatingSystem::IOS,
                OperatingSystem::Linux,
                OperatingSystem::MacOS,
                OperatingSystem::Windows,
            ],
            ProjectIcon::new(default_icon_bytes, 1024),
        ));
    }

    // Clobber an existing project folder only with --force.
    if project_root_folder.exists() {
        if !cli_info.flags.has_flag("force") {
            reporter.error(format!(
                "project already exists at {}",
                project_root_folder.display()
            ));
            reporter.help(format!(
                "run '{} project new --force' to overwrite",
                cli_info.executable
            ));
            return ExitCode::FAILURE;
        }
        reporter.info(format!(
            "--force specified, removing existing {}",
            project_root_folder.display()
        ));
        let removal = if project_root_folder.is_dir() {
            std::fs::remove_dir_all(&project_root_folder)
        } else {
            std::fs::remove_file(&project_root_folder)
        };
        if let Err(e) = removal {
            reporter.error(format!("could not remove existing project folder: {e}"));
            return ExitCode::FAILURE;
        }
    }

    // Scaffold the project directory tree.
    let base_dirs = [
        project_root_folder.clone(),
        project_root_folder.join(".peko/project"),
    ];
    for dir in &base_dirs {
        if let Err(e) = std::fs::create_dir_all(dir) {
            reporter.error(format!("could not create {}: {e}", dir.display()));
            return ExitCode::FAILURE;
        }
    }

    if project_is_ui {
        let ui_dirs = [
            project_root_folder.join("assets"),
            project_root_folder.join("pages/index"),
        ];
        for dir in &ui_dirs {
            if let Err(e) = std::fs::create_dir_all(dir) {
                reporter.error(format!("could not create {}: {e}", dir.display()));
                return ExitCode::FAILURE;
            }
        }

        // Starter files for a UI project.
        let starter_files: &[(PathBuf, &[u8])] = &[
            (project_root_folder.join("root_styles.scss"), b""),
            (
                project_root_folder.join("pages/index/page.peko"),
                INDEX_PAGE_TEMPLATE.as_bytes(),
            ),
            (
                project_root_folder.join("pages/index/page_styles.scss"),
                b"",
            ),
        ];
        for (path, bytes) in starter_files {
            if let Err(e) = std::fs::write(path, bytes) {
                reporter.error(format!("could not write {}: {e}", path.display()));
                return ExitCode::FAILURE;
            }
        }

        if let Err(e) = bundler::regenerate_application_bundle_files(&peko_project) {
            reporter.error(format!("could not generate bundling config files: {e}"));
            return ExitCode::FAILURE;
        }
    }

    // Root entrypoint.
    let main_peko_path = project_root_folder.join("main.peko");
    let main_peko_bytes = if project_is_ui {
        ui_main_peko_template(&project_name).into_bytes()
    } else {
        CLI_MAIN_PEKO_TEMPLATE.as_bytes().to_vec()
    };
    if let Err(e) = std::fs::write(&main_peko_path, &main_peko_bytes) {
        reporter.error(format!("could not write {}: {e}", main_peko_path.display()));
        return ExitCode::FAILURE;
    }

    // Persist project config.
    let config_path = project_root_folder.join(".peko/project/config.pkbin");
    if let Err(e) = std::fs::write(&config_path, peko_project.to_binary()) {
        reporter.error(format!(
            "could not write project config to {}: {e}",
            config_path.display()
        ));
        return ExitCode::FAILURE;
    }

    reporter.success(format!(
        "created new project {project_name} at {}",
        project_root_folder.display()
    ));
    ExitCode::SUCCESS
}

// ---------------------------------------------------------------------------
// Subcommand: modify
// ---------------------------------------------------------------------------

fn execute_modify(cli_info: &CLIInfo, reporter: &Reporter, mut project: PekoProject) -> ExitCode {
    let mut rl = match Editor::<(), FileHistory>::new() {
        Ok(rl) => rl,
        Err(e) => {
            reporter.error(format!("could not initialize prompt editor: {e}"));
            return ExitCode::FAILURE;
        }
    };

    // Existing UI status as the default for the prompt.
    let is_currently_ui = project.ui_project_info.is_some();
    let should_be_ui = confirmation_prompt(&mut rl, "* UI project (y/n) => ", is_currently_ui);

    let new_name =
        match rl.readline_with_initial("* Project name     => ", (project.name.as_str(), "")) {
            Ok(v) => v,
            Err(e) => {
                reporter.error(format!("could not read project name: {e}"));
                return ExitCode::FAILURE;
            }
        };
    project.name = new_name;

    if !should_be_ui {
        // CLI project, drop any existing UI info.
        project.ui_project_info = None;
    } else {
        // Capture existing UI info up front so we don't repeatedly
        // clone the option in each branch below.
        let existing = project
            .ui_project_info
            .take()
            .unwrap_or_else(|| default_ui_info_for_modify(cli_info));

        let bundle_id = match rl
            .readline_with_initial("* Bundle ID        => ", (existing.bundle_id.as_str(), ""))
        {
            Ok(v) => v,
            Err(e) => {
                reporter.error(format!("could not read bundle id: {e}"));
                return ExitCode::FAILURE;
            }
        };

        let version = match rl
            .readline_with_initial("* Project version  => ", (existing.version.as_str(), ""))
        {
            Ok(v) => v,
            Err(e) => {
                reporter.error(format!("could not read project version: {e}"));
                return ExitCode::FAILURE;
            }
        };

        println!("* Target platforms");
        let mut targets = Vec::new();
        for (os, prompt) in [
            (OperatingSystem::Android, "\t Android (y/n) => "),
            (OperatingSystem::IOS, "\t iOS (y/n)     => "),
            (OperatingSystem::Linux, "\t Linux (y/n)   => "),
            (OperatingSystem::MacOS, "\t macOS (y/n)   => "),
            (OperatingSystem::Windows, "\t Windows (y/n) => "),
        ] {
            let currently_enabled = existing
                .platforms
                .iter()
                .any(|p| std::mem::discriminant(p) == std::mem::discriminant(&os));
            if confirmation_prompt(&mut rl, prompt, currently_enabled) {
                targets.push(os);
            }
        }

        project.ui_project_info = Some(UIProjectInfo::new(
            bundle_id,
            version,
            targets,
            existing.icon,
        ));
    }

    // Persist updated config.
    let config_path = project.get_root().join(".peko/project/config.pkbin");
    if let Err(e) = std::fs::write(&config_path, project.to_binary()) {
        reporter.error(format!(
            "could not write project config to {}: {e}",
            config_path.display()
        ));
        return ExitCode::FAILURE;
    }

    reporter.success(format!("updated project {}", project.name));
    ExitCode::SUCCESS
}

/// Fallback default UI info used when `modify` is given a CLI project
/// being promoted to a UI project. Reads the default icon from the
/// toolchain.
fn default_ui_info_for_modify(cli_info: &CLIInfo) -> UIProjectInfo {
    let default_icon_path = cli_info
        .get_peko_root()
        .join("Compiler/bundling/defaulticon.bin");
    // Best-effort: if the default icon can't be read, fall back to an
    // empty icon. The user can re-set it later via `project set-icon`.
    let default_icon_bytes = std::fs::read(&default_icon_path).unwrap_or_default();
    UIProjectInfo::new(
        String::new(),
        "v0.1.0".to_owned(),
        vec![
            OperatingSystem::Android,
            OperatingSystem::IOS,
            OperatingSystem::Linux,
            OperatingSystem::MacOS,
            OperatingSystem::Windows,
        ],
        ProjectIcon::new(default_icon_bytes, 1024),
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
// Subcommand: set-icon
// ---------------------------------------------------------------------------

fn execute_set_icon(cli_info: &CLIInfo, reporter: &Reporter, mut project: PekoProject) -> ExitCode {
    if project.ui_project_info.is_none() {
        reporter.error("cannot set an icon on a CLI project");
        return ExitCode::FAILURE;
    }

    let Some(icon_arg) = cli_info.arguments.get(2) else {
        reporter.error(
            "`project set-icon` requires a path to an icon file (PNG, JPEG, WebP, ICO, or AVIF)",
        );
        reporter.help(format!(
            "run '{} help project' to see how this command works",
            cli_info.executable
        ));
        return ExitCode::FAILURE;
    };

    let icon_path = PathBuf::from(icon_arg);
    if !icon_path.exists() {
        reporter.error(format!(
            "icon file '{}' does not exist",
            icon_path.display()
        ));
        return ExitCode::FAILURE;
    }

    // Validate extension before doing any image-loading work.
    let extension = icon_path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match extension.as_deref() {
        Some("png" | "jpg" | "jpeg" | "webp" | "ico" | "avif") => {}
        _ => {
            reporter.error("icon file must be one of: .png, .jpg, .jpeg, .webp, .ico, .avif");
            return ExitCode::FAILURE;
        }
    };

    // Load and (if needed) resize the image.
    let file = match std::fs::File::open(&icon_path) {
        Ok(f) => f,
        Err(e) => {
            reporter.error(format!("could not open {}: {e}", icon_path.display()));
            return ExitCode::FAILURE;
        }
    };

    let format = match image::ImageFormat::from_extension(icon_path.extension().unwrap()) {
        Some(f) => f,
        None => {
            reporter.error("could not determine image format from extension");
            return ExitCode::FAILURE;
        }
    };

    let mut image_data = match image::load(std::io::BufReader::new(file), format) {
        Ok(img) => img,
        Err(_) => {
            reporter.error("icon file is not a valid image");
            return ExitCode::FAILURE;
        }
    };

    if image_data.width() != 1024 || image_data.height() != 1024 {
        reporter.warning("provided icon is not 1024x1024, resizing");
        reporter
            .help("resizing may decrease quality. To control the result, pass a 1024x1024 source");
        image_data = image_data.resize(1024, 1024, image::imageops::FilterType::Lanczos3);
    }

    // RGBA8 raw bytes for the project icon.
    let rgba_bytes = image_data.to_rgba8().into_raw();
    let icon_width = image_data.width();
    let new_icon = ProjectIcon::new(rgba_bytes, icon_width);

    // Update the project's UI info with the new icon. We already
    // verified ui_project_info.is_some() above.
    let mut ui_info = project.ui_project_info.take().unwrap();
    ui_info.icon = new_icon;
    project.ui_project_info = Some(ui_info);

    let config_path = project.get_root().join(".peko/project/config.pkbin");
    if let Err(e) = std::fs::write(&config_path, project.to_binary()) {
        reporter.error(format!(
            "could not write project config to {}: {e}",
            config_path.display()
        ));
        return ExitCode::FAILURE;
    }

    reporter.success(format!("updated icon for project {}", project.name));
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
    let image_height = if image_width == 0 {
        0
    } else {
        (image_pixels.len() / 4) / image_width
    };

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

// ---------------------------------------------------------------------------
// Shared helper: yes/no prompt
// ---------------------------------------------------------------------------

/// Ask a yes/no question, returning `true` for affirmative answers.
/// `default_yes` controls what an empty answer means and what's
/// pre-filled in the prompt.
fn confirmation_prompt(rl: &mut Editor<(), FileHistory>, prompt: &str, default_yes: bool) -> bool {
    let default_initial = if default_yes { "y" } else { "n" };

    loop {
        let answer = match rl.readline_with_initial(prompt, (default_initial, "")) {
            Ok(a) => a,
            Err(_) => return default_yes,
        };

        match answer.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => return true,
            "n" | "no" => return false,
            "" => return default_yes,
            _ => {
                println!(">> please type either yes or no");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Starter-file templates
// ---------------------------------------------------------------------------

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

const CLI_MAIN_PEKO_TEMPLATE: &str = "fn OnStart() {\n\
                                      \tconsole::println(\"Hello World!\");\n\
                                      }\n";

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
