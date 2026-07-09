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

use app_store_connect::UnifiedApiKey;
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
    // The project's declared, signable target platforms (drop Linux/Unknown).
    let declared: Vec<String> = ui_info
        .platforms
        .iter()
        .filter_map(|os| signing::platform_id(os))
        .map(str::to_string)
        .collect();

    let Some(subcommand) = cli_info.arguments.get(1) else {
        reporter.error("`peko keys` requires a subcommand: add, install, set-password, verify, list, or remove");
        reporter.help(format!("run '{} help keys' for usage", cli_info.executable));
        return ExitCode::FAILURE;
    };

    match subcommand.as_str() {
        "add" => add(cli_info, reporter, &root, &bundle_id),
        "install" => install(cli_info, reporter, &root),
        "set-password" => set_password(cli_info, reporter, &root, &bundle_id),
        "verify" => verify(cli_info, reporter, &root, &bundle_id, &declared),
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

/// Encode the App Store Connect API key used for notarization from its
/// three parts and install it under the macOS keys directory. The issuer
/// id, key id, and `.p8` private key are combined into a unified JSON file
/// that the notary client reads. Returns false when a partial flag set is
/// given or encoding fails, with the error already reported. Returns true
/// when a key is installed or when no notary flags are present.
fn install_notary_key(
    reporter: &Reporter,
    root: &Path,
    cli_info: &CLIInfo,
    registry: &mut Value,
) -> bool {
    let issuer = cli_info.flags.get_flag("notary-issuer");
    let key_id = cli_info.flags.get_flag("notary-key-id");
    let private_key = cli_info.flags.get_flag("notary-p8");

    let (issuer, key_id, private_key) = match (issuer, key_id, private_key) {
        (None, None, None) => return true,
        (Some(issuer), Some(key_id), Some(private_key)) => (issuer, key_id, private_key),
        _ => {
            reporter
                .error("notary key needs all of --notary-issuer, --notary-key-id, and --notary-p8");
            return false;
        }
    };

    let private_key_path = PathBuf::from(&private_key);
    if !private_key_path.exists() {
        reporter.error(format!(
            "notary private key '{}' does not exist",
            private_key_path.display()
        ));
        return false;
    }

    let dir = signing::platform_dir(root, "macos");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        reporter.error(format!("could not create {}: {e}", dir.display()));
        return false;
    }

    let unified = match UnifiedApiKey::from_ecdsa_pem_path(&issuer, &key_id, &private_key_path) {
        Ok(unified) => unified,
        Err(e) => {
            reporter.error(format!("could not read notary private key: {e}"));
            return false;
        }
    };

    let dest = dir.join("notary_key.json");
    if let Err(e) = unified.write_json_file(&dest) {
        reporter.error(format!(
            "could not write notary key to {}: {e}",
            dest.display()
        ));
        return false;
    }

    set_registry_file(registry, "macos", "notary_key", "notary_key.json");
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

    if platform == "macos" && !install_notary_key(reporter, root, cli_info, &mut registry) {
        return ExitCode::FAILURE;
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

    // Store the password(s) in the OS keychain. All of a project's signing
    // passwords live in one keychain item, so the existing set is loaded, this
    // platform's roles are added, and the item is written once.
    let password = read_password(cli_info);
    let mut secrets = signing::SigningSecrets::load(bundle_id);
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
        secrets.set("android", "store", &store_password);
        secrets.set("android", "key", &key_password);
    } else {
        let Some(password) = password else {
            reporter.error(format!(
                "{platform} keys need a password (use --password or --password-file)"
            ));
            return ExitCode::FAILURE;
        };
        let role = password_roles(&platform)[0];
        secrets.set(&platform, role, &password);
    }
    if let Err(e) = secrets.store(bundle_id) {
        reporter.error(format!("could not store password: {e}"));
        return ExitCode::FAILURE;
    }

    reporter.success(format!("registered {platform} signing key"));
    ExitCode::SUCCESS
}

/// Install a single key file into a platform and register it, without
/// requiring the platform's other files. Used by the IDE's drag-and-drop, which
/// adds files one at a time. `--role` names the slot (cert/p12, profile,
/// entitlements, keystore, pfx); `--alias` sets the Android key alias.
fn install(cli_info: &CLIInfo, reporter: &Reporter, root: &Path) -> ExitCode {
    let Some(platform) = require_platform(cli_info, reporter) else {
        return ExitCode::FAILURE;
    };
    let Some(role) = cli_info.flags.get_flag("role") else {
        reporter.error("`--role` is required (p12, profile, entitlements, keystore, pfx)");
        return ExitCode::FAILURE;
    };
    if !role_is_valid(&platform, &role) {
        reporter.error(format!(
            "role '{role}' is not a {platform} signing file; expected {}",
            valid_roles(&platform).join(", ")
        ));
        return ExitCode::FAILURE;
    }

    let mut registry = match signing::load_registry(root) {
        Ok(value) => value,
        Err(e) => {
            reporter.error(format!("could not read key registry: {e}"));
            return ExitCode::FAILURE;
        }
    };

    // Two sources: a file on disk (--file), or the file's bytes as base64 on
    // stdin (--filename names the destination). The IDE uses the latter so a
    // dropped file's bytes cross the bridge without a filesystem path and
    // decoding happens here, preserving binary fidelity.
    match (cli_info.flags.get_flag("file"), cli_info.flags.get_flag("filename")) {
        (Some(file), _) => {
            if !install_file(reporter, root, &platform, &role, &PathBuf::from(file), &mut registry)
            {
                return ExitCode::FAILURE;
            }
        }
        (None, Some(filename)) => {
            if !install_stdin_base64(reporter, root, &platform, &role, &filename, &mut registry) {
                return ExitCode::FAILURE;
            }
        }
        (None, None) => {
            reporter.error("`--file <path>` or `--filename <name>` (with base64 on stdin) is required");
            return ExitCode::FAILURE;
        }
    }
    if platform == "android"
        && let Some(alias) = cli_info.flags.get_flag("alias")
    {
        set_registry_alias(&mut registry, &platform, &alias);
    }
    if let Err(e) = signing::save_registry(root, &registry) {
        reporter.error(format!("could not write key registry: {e}"));
        return ExitCode::FAILURE;
    }
    reporter.success(format!("installed {platform} {role}"));
    ExitCode::SUCCESS
}

/// Read base64 from stdin, decode it, write it into the platform key directory
/// under `filename`, and register it under `role`. Byte fidelity is guaranteed
/// by decoding here rather than in the webview.
fn install_stdin_base64(
    reporter: &Reporter,
    root: &Path,
    platform: &str,
    role: &str,
    filename: &str,
    registry: &mut Value,
) -> bool {
    use base64::Engine as _;
    use std::io::Read as _;

    // Reject a path in the file name; it must be a bare name.
    if filename.contains('/') || filename.contains('\\') || filename.is_empty() {
        reporter.error("--filename must be a bare file name");
        return false;
    }

    let mut encoded = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut encoded) {
        reporter.error(format!("could not read key bytes from stdin: {e}"));
        return false;
    }
    let bytes = match base64::engine::general_purpose::STANDARD.decode(encoded.trim()) {
        Ok(bytes) => bytes,
        Err(e) => {
            reporter.error(format!("key bytes are not valid base64: {e}"));
            return false;
        }
    };

    let dir = signing::platform_dir(root, platform);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        reporter.error(format!("could not create {}: {e}", dir.display()));
        return false;
    }
    let dest = dir.join(filename);
    if let Err(e) = std::fs::write(&dest, &bytes) {
        reporter.error(format!("could not write key file to {}: {e}", dest.display()));
        return false;
    }
    set_registry_file(registry, platform, role, filename);
    true
}

/// Store one signing password in the OS keychain. The value comes from
/// `--password`, `--password-file`, or (preferred, so it never reaches the
/// process table) standard input.
fn set_password(cli_info: &CLIInfo, reporter: &Reporter, _root: &Path, bundle_id: &str) -> ExitCode {
    let Some(platform) = require_platform(cli_info, reporter) else {
        return ExitCode::FAILURE;
    };
    let Some(role) = cli_info.flags.get_flag("role") else {
        reporter.error("`--role` is required (store, key, p12, pfx)");
        return ExitCode::FAILURE;
    };
    if !password_roles(&platform).contains(&role.as_str()) {
        reporter.error(format!(
            "role '{role}' has no password on {platform}; expected {}",
            password_roles(&platform).join(", ")
        ));
        return ExitCode::FAILURE;
    }
    let password = read_password(cli_info).or_else(|| {
        use std::io::Read;
        let mut buffer = String::new();
        std::io::stdin().read_to_string(&mut buffer).ok()?;
        let trimmed = buffer.trim_end_matches(['\n', '\r']);
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    });
    let Some(password) = password else {
        reporter.error("a password is required (use --password, --password-file, or stdin)");
        return ExitCode::FAILURE;
    };
    let mut secrets = signing::SigningSecrets::load(bundle_id);
    secrets.set(&platform, &role, &password);
    if let Err(e) = secrets.store(bundle_id) {
        reporter.error(format!("could not store password: {e}"));
        return ExitCode::FAILURE;
    }
    reporter.success(format!("set {platform} {role} password"));
    ExitCode::SUCCESS
}

/// Verify that registered signing material satisfies each platform's
/// requirements (files present, passwords open them, certificates are the right
/// kind and unexpired). With `--json`, prints the machine-readable reports the
/// IDE reads; otherwise a human summary. Exits non-zero when a platform is
/// missing material or fails a check.
fn verify(
    cli_info: &CLIInfo,
    reporter: &Reporter,
    root: &Path,
    bundle_id: &str,
    declared: &[String],
) -> ExitCode {
    let platforms: Vec<String> = match cli_info.flags.get_flag("platform") {
        Some(p) => vec![p],
        None if !declared.is_empty() => declared.to_vec(),
        None => vec![
            "android".into(),
            "ios".into(),
            "macos".into(),
            "windows".into(),
        ],
    };
    // Read the keychain once for the whole command. Every platform's
    // verification shares this snapshot, so the operating system authorizes
    // keychain access a single time rather than once per platform.
    let secrets = signing::SigningSecrets::load(bundle_id);
    let reports: Vec<signing::PlatformReport> = platforms
        .iter()
        .filter(|p| matches!(p.as_str(), "android" | "ios" | "macos" | "windows"))
        .map(|p| signing::verify_platform(root, &secrets, p))
        .collect();

    if cli_info.flags.has_flag("json") {
        println!(
            "{}",
            serde_json::to_string(&reports).unwrap_or_else(|_| "[]".to_string())
        );
        return ExitCode::SUCCESS;
    }

    let mut all_ok = true;
    for report in &reports {
        reporter.info(format!("{}: {}", report.platform, report.state));
        for check in &report.checks {
            let mark = if check.ok { "ok" } else { "FAIL" };
            reporter.info(format!("  [{mark}] {}: {}", check.role, check.detail));
        }
        if report.state == "invalid" || report.state == "missing" {
            all_ok = false;
        }
    }
    if all_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// The registry roles that are files for a platform.
fn valid_roles(platform: &str) -> &'static [&'static str] {
    match platform {
        "android" => &["keystore"],
        "ios" => &["p12", "profile", "entitlements"],
        "macos" => &["p12", "entitlements"],
        "windows" => &["pfx"],
        _ => &[],
    }
}

fn role_is_valid(platform: &str, role: &str) -> bool {
    valid_roles(platform).contains(&role)
}

fn list(reporter: &Reporter, root: &Path, bundle_id: &str) -> ExitCode {
    let registry = match signing::load_registry(root) {
        Ok(value) => value,
        Err(e) => {
            reporter.error(format!("could not read key registry: {e}"));
            return ExitCode::FAILURE;
        }
    };

    // One keychain read serves the whole listing.
    let secrets = signing::SigningSecrets::load(bundle_id);

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
            .all(|role| secrets.get(platform, role).is_some());
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

    // Drop the stored passwords for this platform, writing the item once.
    let mut secrets = signing::SigningSecrets::load(bundle_id);
    if secrets.remove_platform(&platform)
        && let Err(e) = secrets.store(bundle_id)
    {
        reporter.error(format!("could not remove stored password: {e}"));
        return ExitCode::FAILURE;
    }

    reporter.success(format!("removed {platform} signing key"));
    ExitCode::SUCCESS
}
