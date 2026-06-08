//! `peko keys`: manage per-project signing keys.
//!
//! Subcommands:
//! - `add` copies a platform's key files into `.peko/keys/<platform>/`,
//!   records their names in the key registry, and stores the associated
//!   password in the OS keychain.
//! - `list` shows the registered keys per platform and whether a password
//!   is present.
//! - `remove` deletes a platform's key files, registry entry, and stored
//!   passwords.
//!
//! Signing material is per project. Key files live under the project; the
//! passwords live in the OS keychain keyed by the project's bundle id.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::Value;

use crate::bundler::signing;
use crate::cli::CLIInfo;
use crate::cli::reporting::Reporter;
use crate::project::PekoProject;

/// Roles whose password is stored in the keychain for each platform.
fn password_roles(platform: &str) -> &'static [&'static str] {
    match platform {
        "android" => &["store", "key"],
        "ios" | "macos" => &["p12"],
        "windows" => &["pfx"],
        _ => &[],
    }
}

pub async fn execute(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    let project = match PekoProject::from_current_directory() {
        Ok(project) => project,
        Err(_) => {
            reporter.error("not inside a Peko project");
            return ExitCode::FAILURE;
        }
    };

    let Some(ui_info) = project.ui_project_info.as_ref() else {
        reporter.error("CLI projects don't have signing keys");
        return ExitCode::FAILURE;
    };
    let bundle_id = ui_info.bundle_id.clone();
    let root = project.get_root().to_path_buf();

    let Some(subcommand) = cli_info.arguments.get(1) else {
        reporter.error("`peko keys` requires a subcommand: add, list, or remove");
        reporter.help(format!("run '{} help keys' for usage", cli_info.executable));
        return ExitCode::FAILURE;
    };

    match subcommand.as_str() {
        "add" => add(cli_info, reporter, &root, &bundle_id),
        "list" => list(reporter, &root, &bundle_id),
        "remove" => remove(cli_info, reporter, &root, &bundle_id),
        other => {
            reporter.error(format!("unknown keys subcommand '{other}'"));
            reporter.help(format!("run '{} help keys' for usage", cli_info.executable));
            ExitCode::FAILURE
        }
    }
}

/// Validate and return the requested platform from `--platform`.
fn require_platform(cli_info: &CLIInfo, reporter: &Reporter) -> Option<String> {
    let Some(platform) = cli_info.flags.get_flag("platform") else {
        reporter.error("`--platform` is required (android, ios, macos, or windows)");
        return None;
    };
    match platform.as_str() {
        "android" | "ios" | "macos" | "windows" => Some(platform),
        other => {
            reporter.error(format!(
                "unknown platform '{other}'; expected android, ios, macos, or windows"
            ));
            None
        }
    }
}

/// Read a password from `--password` or `--password-file`.
fn read_password(cli_info: &CLIInfo) -> Option<String> {
    if let Some(password) = cli_info.flags.get_flag("password") {
        return Some(password);
    }
    if let Some(path) = cli_info.flags.get_flag("password-file")
        && let Ok(contents) = std::fs::read_to_string(&path)
    {
        return Some(contents.trim_end_matches(['\n', '\r']).to_string());
    }
    None
}

/// Set a registered file name for a platform role in the registry.
fn set_registry_file(registry: &mut Value, platform: &str, role: &str, filename: &str) {
    let root = registry.as_object_mut().expect("registry is an object");
    let platforms = root
        .entry("platforms")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let entry = platforms
        .as_object_mut()
        .expect("platforms is an object")
        .entry(platform)
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let entry_object = entry.as_object_mut().expect("platform entry is an object");
    let files = entry_object
        .entry("files")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    files
        .as_object_mut()
        .expect("files is an object")
        .insert(role.to_string(), Value::String(filename.to_string()));
}

/// Set the Android key alias in the registry.
fn set_registry_alias(registry: &mut Value, platform: &str, alias: &str) {
    let root = registry.as_object_mut().expect("registry is an object");
    let platforms = root
        .entry("platforms")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let entry = platforms
        .as_object_mut()
        .expect("platforms is an object")
        .entry(platform)
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    entry
        .as_object_mut()
        .expect("platform entry is an object")
        .insert("alias".to_string(), Value::String(alias.to_string()));
}

/// Copy a key file into the platform key directory and record it.
fn install_file(
    reporter: &Reporter,
    root: &Path,
    platform: &str,
    role: &str,
    source: &Path,
    registry: &mut Value,
) -> bool {
    if !source.exists() {
        reporter.error(format!("file '{}' does not exist", source.display()));
        return false;
    }
    let Some(name_os) = source.file_name() else {
        reporter.error("could not determine key file name");
        return false;
    };
    let filename = name_os.to_string_lossy().to_string();
    let dir = signing::platform_dir(root, platform);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        reporter.error(format!("could not create {}: {e}", dir.display()));
        return false;
    }
    let dest = dir.join(&filename);
    if let Err(e) = std::fs::copy(source, &dest) {
        reporter.error(format!(
            "could not copy key file to {}: {e}",
            dest.display()
        ));
        return false;
    }
    set_registry_file(registry, platform, role, &filename);
    true
}

fn add(cli_info: &CLIInfo, reporter: &Reporter, root: &Path, bundle_id: &str) -> ExitCode {
    let Some(platform) = require_platform(cli_info, reporter) else {
        return ExitCode::FAILURE;
    };

    let mut registry = match signing::load_registry(root) {
        Ok(value) => value,
        Err(e) => {
            reporter.error(format!("could not read key registry: {e}"));
            return ExitCode::FAILURE;
        }
    };

    // Required and optional files differ per platform.
    let required: &[(&str, &str)] = match platform.as_str() {
        "android" => &[("keystore", "keystore")],
        "ios" => &[("cert", "p12"), ("profile", "profile")],
        "macos" => &[("cert", "p12")],
        "windows" => &[("pfx", "pfx")],
        _ => &[],
    };
    let optional: &[(&str, &str)] = match platform.as_str() {
        "ios" | "macos" => &[("entitlements", "entitlements")],
        _ => &[],
    };

    for (flag, role) in required {
        let Some(path) = cli_info.flags.get_flag(flag) else {
            reporter.error(format!("`--{flag}` is required for {platform} keys"));
            return ExitCode::FAILURE;
        };
        if !install_file(
            reporter,
            root,
            &platform,
            role,
            &PathBuf::from(path),
            &mut registry,
        ) {
            return ExitCode::FAILURE;
        }
    }
    for (flag, role) in optional {
        if let Some(path) = cli_info.flags.get_flag(flag)
            && !install_file(
                reporter,
                root,
                &platform,
                role,
                &PathBuf::from(path),
                &mut registry,
            )
        {
            return ExitCode::FAILURE;
        }
    }

    if platform == "android" {
        let alias = cli_info
            .flags
            .get_flag("alias")
            .unwrap_or_else(|| "upload".to_string());
        set_registry_alias(&mut registry, &platform, &alias);
    }

    if let Err(e) = signing::save_registry(root, &registry) {
        reporter.error(format!("could not write key registry: {e}"));
        return ExitCode::FAILURE;
    }

    // Store the password(s) in the OS keychain.
    let password = read_password(cli_info);
    if platform == "android" {
        let store_password = cli_info
            .flags
            .get_flag("store-password")
            .or_else(|| password.clone());
        let key_password = cli_info
            .flags
            .get_flag("key-password")
            .or_else(|| password.clone());
        let (Some(store_password), Some(key_password)) = (store_password, key_password) else {
            reporter.error(
                "android keys need a password (use --password, or --store-password and --key-password)",
            );
            return ExitCode::FAILURE;
        };
        if let Err(e) = signing::store_password(bundle_id, "android", "store", &store_password) {
            reporter.error(format!("could not store password: {e}"));
            return ExitCode::FAILURE;
        }
        if let Err(e) = signing::store_password(bundle_id, "android", "key", &key_password) {
            reporter.error(format!("could not store password: {e}"));
            return ExitCode::FAILURE;
        }
    } else {
        let Some(password) = password else {
            reporter.error(format!(
                "{platform} keys need a password (use --password or --password-file)"
            ));
            return ExitCode::FAILURE;
        };
        let role = password_roles(&platform)[0];
        if let Err(e) = signing::store_password(bundle_id, &platform, role, &password) {
            reporter.error(format!("could not store password: {e}"));
            return ExitCode::FAILURE;
        }
    }

    reporter.success(format!("registered {platform} signing key"));
    ExitCode::SUCCESS
}

fn list(reporter: &Reporter, root: &Path, bundle_id: &str) -> ExitCode {
    let registry = match signing::load_registry(root) {
        Ok(value) => value,
        Err(e) => {
            reporter.error(format!("could not read key registry: {e}"));
            return ExitCode::FAILURE;
        }
    };

    let mut any = false;
    for platform in ["android", "ios", "macos", "windows"] {
        let entry = registry.get("platforms").and_then(|p| p.get(platform));
        let Some(entry) = entry else {
            continue;
        };
        any = true;

        let mut files: Vec<String> = Vec::new();
        if let Some(map) = entry.get("files").and_then(|f| f.as_object()) {
            for (role, name) in map {
                files.push(format!("{role}={}", name.as_str().unwrap_or("")));
            }
        }
        if let Some(alias) = entry.get("alias").and_then(|a| a.as_str()) {
            files.push(format!("alias={alias}"));
        }

        let password_present = password_roles(platform)
            .iter()
            .all(|role| signing::get_password(bundle_id, platform, role).is_some());
        let password_state = if password_present {
            "password set"
        } else {
            "password missing"
        };

        reporter.info(format!(
            "{platform}: {} ({password_state})",
            files.join(", ")
        ));
    }

    if !any {
        reporter.info("no signing keys registered");
    }
    ExitCode::SUCCESS
}

fn remove(cli_info: &CLIInfo, reporter: &Reporter, root: &Path, bundle_id: &str) -> ExitCode {
    let Some(platform) = require_platform(cli_info, reporter) else {
        return ExitCode::FAILURE;
    };

    // Delete the platform key directory.
    let dir = signing::platform_dir(root, &platform);
    if dir.exists()
        && let Err(e) = std::fs::remove_dir_all(&dir)
    {
        reporter.error(format!("could not remove {}: {e}", dir.display()));
        return ExitCode::FAILURE;
    }

    // Drop the registry entry.
    let mut registry = match signing::load_registry(root) {
        Ok(value) => value,
        Err(e) => {
            reporter.error(format!("could not read key registry: {e}"));
            return ExitCode::FAILURE;
        }
    };
    if let Some(platforms) = registry
        .get_mut("platforms")
        .and_then(|p| p.as_object_mut())
    {
        platforms.remove(&platform);
    }
    if let Err(e) = signing::save_registry(root, &registry) {
        reporter.error(format!("could not write key registry: {e}"));
        return ExitCode::FAILURE;
    }

    // Drop the stored passwords.
    for role in password_roles(&platform) {
        if let Err(e) = signing::delete_password(bundle_id, &platform, role) {
            reporter.error(format!("could not remove stored password: {e}"));
            return ExitCode::FAILURE;
        }
    }

    reporter.success(format!("removed {platform} signing key"));
    ExitCode::SUCCESS
}
