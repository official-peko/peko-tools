//! Signing key management and platform signers.
//!
//! Signing material is stored per project. Key files (keystores, PKCS#12
//! certificates, provisioning profiles) live under `<root>/.peko/keys/<platform>/`.
//! Secrets (the passwords that open those files) are stored in the operating
//! system keychain through the `keyring-core` crate, keyed by the project's bundle
//! id, so they never sit in plaintext on disk. Non-secret metadata (which
//! files are registered, the Android key alias) lives in
//! `<root>/.peko/keys/registry.json`.
//!
//! Resolution functions ([`resolve_android`], [`resolve_apple`],
//! [`resolve_windows`]) return the full material for a platform, or `None`
//! when nothing is registered. The signers ([`sign_apple_bundle`],
//! [`jarsigner_sign_aab`], [`osslsigncode_sign`]) consume that material.
//! Apple signing uses the `apple-codesign` crate directly; Android and
//! Windows shell out to `jarsigner` and `osslsigncode`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use peko_core::target::OperatingSystem;
use serde_json::Value;

use crate::bundler::{run_tool, BundleError, BundleResult};

/// Service name used for every keychain entry this module creates.
const KEYCHAIN_SERVICE: &str = "dev.peko.signing";

/// Platform identifier string used in paths, the registry, and keychain
/// account names. Returns `None` for platforms that are not signed.
pub fn platform_id(os: &OperatingSystem) -> Option<&'static str> {
    match os {
        OperatingSystem::Android => Some("android"),
        OperatingSystem::IOS => Some("ios"),
        OperatingSystem::MacOS => Some("macos"),
        OperatingSystem::Windows => Some("windows"),
        OperatingSystem::Linux | OperatingSystem::Unknown => None,
    }
}

/// The project's signing key directory (`<root>/.peko/keys`).
pub fn keys_dir(project_root: &Path) -> PathBuf {
    project_root.join(".peko/keys")
}

/// The directory holding one platform's key files.
pub fn platform_dir(project_root: &Path, platform: &str) -> PathBuf {
    keys_dir(project_root).join(platform)
}

/// Path to the registry file that records non-secret key metadata.
fn registry_path(project_root: &Path) -> PathBuf {
    keys_dir(project_root).join("registry.json")
}

// ---------------------------------------------------------------------------
// Registry (non-secret metadata)
// ---------------------------------------------------------------------------

/// Load the key registry as a JSON object, or an empty object when no
/// registry exists yet.
pub fn load_registry(project_root: &Path) -> BundleResult<Value> {
    let path = registry_path(project_root);
    if !path.exists() {
        return Ok(Value::Object(serde_json::Map::new()));
    }
    let text = std::fs::read_to_string(&path).map_err(|source| BundleError::Io {
        path: path.clone(),
        source,
    })?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|e| BundleError::Signing(format!("registry parse failed: {e}")))?;
    Ok(value)
}

/// Write the key registry back to disk, creating the keys directory if
/// needed.
pub fn save_registry(project_root: &Path, registry: &Value) -> BundleResult<()> {
    let dir = keys_dir(project_root);
    std::fs::create_dir_all(&dir).map_err(|source| BundleError::Io {
        path: dir.clone(),
        source,
    })?;
    let path = registry_path(project_root);
    let text = serde_json::to_string_pretty(registry)
        .map_err(|e| BundleError::Signing(format!("registry serialize failed: {e}")))?;
    std::fs::write(&path, text).map_err(|source| BundleError::Io { path, source })
}

/// Read one platform's registry object, or `None` when absent.
fn platform_entry<'a>(registry: &'a Value, platform: &str) -> Option<&'a Value> {
    registry.get("platforms").and_then(|p| p.get(platform))
}

/// Read a registered file name for a platform role, joined onto the
/// platform key directory.
fn registered_file(
    project_root: &Path,
    registry: &Value,
    platform: &str,
    role: &str,
) -> Option<PathBuf> {
    let entry = platform_entry(registry, platform)?;
    let name = entry.get("files")?.get(role)?.as_str()?;
    Some(platform_dir(project_root, platform).join(name))
}

// ---------------------------------------------------------------------------
// Secrets (OS keychain via keyring-core)
// ---------------------------------------------------------------------------

/// Set this process's default credential store on the first keychain call.
/// The store is chosen by host operating system. macOS and iOS use the
/// Apple keychain, Windows uses the Credential Manager, and Linux uses the
/// Secret Service over D-Bus. On any other host no store is set and
/// keychain operations report an error.
fn ensure_store() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        #[cfg(target_os = "macos")]
        if let Ok(store) = apple_native_keyring_store::keychain::Store::new() {
            keyring_core::set_default_store(store);
        }
        #[cfg(target_os = "windows")]
        if let Ok(store) = windows_native_keyring_store::Store::new() {
            keyring_core::set_default_store(store);
        }
        #[cfg(target_os = "linux")]
        if let Ok(store) = dbus_secret_service_keyring_store::Store::new() {
            keyring_core::set_default_store(store);
        }
    });
}

/// Build the keychain account name for a project, platform, and role.
fn keychain_account(bundle_id: &str, platform: &str, role: &str) -> String {
    format!("{bundle_id}:{platform}:{role}")
}

/// Store a password in the OS keychain.
pub fn store_password(
    bundle_id: &str,
    platform: &str,
    role: &str,
    password: &str,
) -> BundleResult<()> {
    ensure_store();
    let account = keychain_account(bundle_id, platform, role);
    let entry = keyring_core::Entry::new(KEYCHAIN_SERVICE, &account)
        .map_err(|e| BundleError::Signing(format!("keychain open failed: {e}")))?;
    entry
        .set_password(password)
        .map_err(|e| BundleError::Signing(format!("keychain write failed: {e}")))
}

/// Retrieve a password from the OS keychain, or `None` when not set.
pub fn get_password(bundle_id: &str, platform: &str, role: &str) -> Option<String> {
    ensure_store();
    let account = keychain_account(bundle_id, platform, role);
    let entry = keyring_core::Entry::new(KEYCHAIN_SERVICE, &account).ok()?;
    entry.get_password().ok()
}

/// Remove a password from the OS keychain. Missing entries are ignored.
pub fn delete_password(bundle_id: &str, platform: &str, role: &str) -> BundleResult<()> {
    ensure_store();
    let account = keychain_account(bundle_id, platform, role);
    let entry = keyring_core::Entry::new(KEYCHAIN_SERVICE, &account)
        .map_err(|e| BundleError::Signing(format!("keychain open failed: {e}")))?;
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring_core::Error::NoEntry) => Ok(()),
        Err(e) => Err(BundleError::Signing(format!("keychain delete failed: {e}"))),
    }
}

// ---------------------------------------------------------------------------
// Resolved key material
// ---------------------------------------------------------------------------

/// Android upload key: a Java keystore plus its passwords and alias.
pub struct AndroidSigningKey {
    pub keystore: PathBuf,
    pub alias: String,
    pub store_password: String,
    pub key_password: String,
}

/// Apple signing material: a PKCS#12 certificate, its password, and an
/// optional provisioning profile and entitlements file. iOS requires the
/// profile; macOS Developer ID signing does not.
pub struct AppleSigningKey {
    pub p12: PathBuf,
    pub p12_password: String,
    pub profile: Option<PathBuf>,
    pub entitlements: Option<PathBuf>,
}

/// Windows signing material: a PKCS#12 certificate and its password.
pub struct WindowsSigningKey {
    pub pfx: PathBuf,
    pub password: String,
}

/// Resolve Android signing material for the project, or `None` when no
/// Android key is registered or a required password is missing.
pub fn resolve_android(
    project_root: &Path,
    bundle_id: &str,
) -> BundleResult<Option<AndroidSigningKey>> {
    let registry = load_registry(project_root)?;
    let Some(keystore) = registered_file(project_root, &registry, "android", "keystore") else {
        return Ok(None);
    };
    if !keystore.exists() {
        return Ok(None);
    }
    let alias = platform_entry(&registry, "android")
        .and_then(|e| e.get("alias"))
        .and_then(|a| a.as_str())
        .unwrap_or("upload")
        .to_string();
    let (Some(store_password), Some(key_password)) = (
        get_password(bundle_id, "android", "store"),
        get_password(bundle_id, "android", "key"),
    ) else {
        return Ok(None);
    };
    Ok(Some(AndroidSigningKey {
        keystore,
        alias,
        store_password,
        key_password,
    }))
}

/// Resolve Apple signing material for the project on `platform` ("ios" or
/// "macos"), or `None` when not registered or the password is missing.
pub fn resolve_apple(
    project_root: &Path,
    bundle_id: &str,
    platform: &str,
) -> BundleResult<Option<AppleSigningKey>> {
    let registry = load_registry(project_root)?;
    let Some(p12) = registered_file(project_root, &registry, platform, "p12") else {
        return Ok(None);
    };
    if !p12.exists() {
        return Ok(None);
    }
    let Some(p12_password) = get_password(bundle_id, platform, "p12") else {
        return Ok(None);
    };
    let profile =
        registered_file(project_root, &registry, platform, "profile").filter(|path| path.exists());
    let entitlements = registered_file(project_root, &registry, platform, "entitlements")
        .filter(|path| path.exists());
    Ok(Some(AppleSigningKey {
        p12,
        p12_password,
        profile,
        entitlements,
    }))
}

/// Resolve Windows signing material for the project, or `None` when not
/// registered or the password is missing.
pub fn resolve_windows(
    project_root: &Path,
    bundle_id: &str,
) -> BundleResult<Option<WindowsSigningKey>> {
    let registry = load_registry(project_root)?;
    let Some(pfx) = registered_file(project_root, &registry, "windows", "pfx") else {
        return Ok(None);
    };
    if !pfx.exists() {
        return Ok(None);
    }
    let Some(password) = get_password(bundle_id, "windows", "pfx") else {
        return Ok(None);
    };
    Ok(Some(WindowsSigningKey { pfx, password }))
}

// ---------------------------------------------------------------------------
// Optional-platform signing outcome
// ---------------------------------------------------------------------------

/// Result of attempting to sign an optional platform (macOS, Windows).
/// These platforms never fail a release build; a missing key or missing
/// tool leaves the artifact unsigned.
pub enum OptionalSignOutcome {
    /// The artifact was signed.
    Signed,
    /// No signing key is registered for the platform.
    NoKey,
    /// A signing key is registered but the external signing tool is not
    /// available on the system.
    ToolUnavailable,
}

// ---------------------------------------------------------------------------
// Apple signing (apple-codesign crate, used as a library)
// ---------------------------------------------------------------------------

/// Ad-hoc sign a Mach-O bundle (a `.app` directory) in place, with no
/// certificate. Used for iOS simulator bundles, which must carry a
/// signature to launch but read their capability entitlements from a
/// Mach-O section rather than from the signature.
pub fn adhoc_sign_apple_bundle(app_dir: &Path) -> BundleResult<()> {
    use apple_codesign::{SigningSettings, UnifiedSigner};

    // Default settings with no signing key produce an ad-hoc signature.
    let settings = SigningSettings::default();
    UnifiedSigner::new(settings)
        .sign_path_in_place(app_dir)
        .map_err(|e| BundleError::Signing(format!("ad-hoc signing failed: {e}")))
}

/// Sign a Mach-O bundle (a `.app` directory) in place with the given Apple
/// key. When `entitlements_xml` is `Some`, it is applied to the main
/// executable. This drives the `apple-codesign` crate directly.
pub fn sign_apple_bundle(
    app_dir: &Path,
    key: &AppleSigningKey,
    entitlements_xml: Option<&str>,
) -> BundleResult<()> {
    use apple_codesign::cryptography::PrivateKey;
    use apple_codesign::{SettingsScope, SigningSettings, UnifiedSigner};

    let p12_bytes = std::fs::read(&key.p12).map_err(|source| BundleError::Io {
        path: key.p12.clone(),
        source,
    })?;

    // Load the certificate and private key from the PKCS#12 container.
    let (certificate, private_key) =
        apple_codesign::cryptography::parse_pfx_data(&p12_bytes, &key.p12_password)
            .map_err(|e| BundleError::Signing(format!("p12 load failed: {e}")))?;

    let mut settings = SigningSettings::default();
    settings.set_signing_key(private_key.as_key_info_signer(), certificate);

    if let Some(xml) = entitlements_xml {
        settings
            .set_entitlements_xml(SettingsScope::Main, xml)
            .map_err(|e| BundleError::Signing(format!("entitlements setup failed: {e}")))?;
    }

    let signer = UnifiedSigner::new(settings);
    signer
        .sign_path_in_place(app_dir)
        .map_err(|e| BundleError::Signing(format!("bundle signing failed: {e}")))
}

/// Extract the entitlements plist from a provisioning profile.
///
/// A `.mobileprovision` is a CMS message whose payload is an XML plist
/// holding an `Entitlements` dictionary. The plist text is located inside
/// the CMS bytes and the `Entitlements` dictionary is returned as a
/// standalone plist XML string, or `None` when it cannot be found.
pub fn entitlements_from_profile(profile: &Path) -> BundleResult<Option<String>> {
    let bytes = std::fs::read(profile).map_err(|source| BundleError::Io {
        path: profile.to_path_buf(),
        source,
    })?;

    let text = String::from_utf8_lossy(&bytes);
    let Some(plist_start) = text.find("<?xml") else {
        return Ok(None);
    };
    let Some(plist_end_rel) = text[plist_start..].find("</plist>") else {
        return Ok(None);
    };
    let plist = &text[plist_start..plist_start + plist_end_rel + "</plist>".len()];

    // Find the Entitlements dictionary inside the plist.
    let Some(key_pos) = plist.find("<key>Entitlements</key>") else {
        return Ok(None);
    };
    let after_key = &plist[key_pos..];
    let Some(dict_start_rel) = after_key.find("<dict>") else {
        return Ok(None);
    };
    let dict_start = key_pos + dict_start_rel;
    let Some(dict_end_rel) = plist[dict_start..].find("</dict>") else {
        return Ok(None);
    };
    let dict_end = dict_start + dict_end_rel + "</dict>".len();
    let dict = &plist[dict_start..dict_end];

    let standalone = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n{dict}\n</plist>\n"
    );
    Ok(Some(standalone))
}

// ---------------------------------------------------------------------------
// Android signing (jarsigner)
// ---------------------------------------------------------------------------

/// Sign a file (an `.aab` or a `.apk`) with `jarsigner`, producing a v1
/// (JAR) signature. `jarsigner` is the path to the shipped JDK tool.
pub fn jarsigner_sign(
    jarsigner: &Path,
    file: &Path,
    keystore: &Path,
    store_password: &str,
    key_password: &str,
    alias: &str,
) -> BundleResult<()> {
    run_tool(
        "jarsigner",
        Command::new(jarsigner)
            .arg("-keystore")
            .arg(keystore)
            .arg("-storepass")
            .arg(store_password)
            .arg("-keypass")
            .arg(key_password)
            .arg("-sigalg")
            .arg("SHA256withRSA")
            .arg("-digestalg")
            .arg("SHA-256")
            .arg(file)
            .arg(alias),
    )
}

/// Sign an Android app bundle with `jarsigner` using the upload key.
/// `jarsigner` is the path to the shipped JDK tool.
pub fn jarsigner_sign_aab(
    aab: &Path,
    key: &AndroidSigningKey,
    jarsigner: &Path,
) -> BundleResult<()> {
    jarsigner_sign(
        jarsigner,
        aab,
        &key.keystore,
        &key.store_password,
        &key.key_password,
        &key.alias,
    )
}

// ---------------------------------------------------------------------------
// Windows signing (osslsigncode)
// ---------------------------------------------------------------------------

/// Report whether `osslsigncode` is available on the system PATH.
pub fn osslsigncode_available() -> bool {
    Command::new("osslsigncode")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

/// Sign a Windows executable with the system `osslsigncode`, writing the
/// signed output back over the input via a temporary file.
pub fn osslsigncode_sign(exe: &Path, key: &WindowsSigningKey) -> BundleResult<()> {
    let signed = exe.with_extension("signed.exe");
    run_tool(
        "osslsigncode",
        Command::new("osslsigncode")
            .arg("sign")
            .arg("-pkcs12")
            .arg(&key.pfx)
            .arg("-pass")
            .arg(&key.password)
            .arg("-h")
            .arg("sha256")
            .arg("-in")
            .arg(exe)
            .arg("-out")
            .arg(&signed),
    )?;
    std::fs::rename(&signed, exe).map_err(|source| BundleError::Io {
        path: exe.to_path_buf(),
        source,
    })
}
