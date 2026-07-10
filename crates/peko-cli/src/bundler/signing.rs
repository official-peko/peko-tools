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

use apple_codesign::AppleCertificate;
use peko_core::target::OperatingSystem;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use x509_certificate::CapturedX509Certificate;

use crate::bundler::{BundleError, BundleResult, run_tool};

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

/// The legacy per-role keychain accounts. Older versions stored one item per
/// `<bundle_id>:<platform>:<role>`. These are folded into the consolidated item
/// on first load and then removed.
const LEGACY_ROLES: &[(&str, &str)] = &[
    ("android", "store"),
    ("android", "key"),
    ("ios", "p12"),
    ("macos", "p12"),
    ("windows", "pfx"),
];

/// Every signing password for one project, held in a single keychain item and
/// keyed by `<platform>:<role>`. Storing one item per project means a build or
/// a `keys` command reads the keychain once, so the operating system authorizes
/// access at most once rather than once per platform and role.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SigningSecrets {
    #[serde(flatten)]
    entries: std::collections::BTreeMap<String, String>,
}

impl SigningSecrets {
    /// Load the project's secrets from the keychain. A missing item yields an
    /// empty set. Legacy per-role items are folded into one consolidated item
    /// on first load. Reads the consolidated item once.
    pub fn load(bundle_id: &str) -> SigningSecrets {
        crate::keychain::ensure_store();
        if let Some(raw) = crate::keychain::get(KEYCHAIN_SERVICE, bundle_id)
            && let Ok(secrets) = serde_json::from_str::<SigningSecrets>(&raw)
        {
            return secrets;
        }
        SigningSecrets::migrate_legacy(bundle_id)
    }

    /// Fold any legacy per-role items into one consolidated item, remove the
    /// legacy items, and return the consolidated set.
    fn migrate_legacy(bundle_id: &str) -> SigningSecrets {
        let mut secrets = SigningSecrets::default();
        for (platform, role) in LEGACY_ROLES {
            let account = legacy_account(bundle_id, platform, role);
            if let Some(password) = crate::keychain::get(KEYCHAIN_SERVICE, &account) {
                secrets.set(platform, role, &password);
            }
        }
        if !secrets.is_empty() {
            let _ = secrets.store(bundle_id);
            for (platform, role) in LEGACY_ROLES {
                let account = legacy_account(bundle_id, platform, role);
                let _ = crate::keychain::delete(KEYCHAIN_SERVICE, &account);
            }
        }
        secrets
    }

    /// Look up one platform role's password.
    pub fn get(&self, platform: &str, role: &str) -> Option<String> {
        self.entries.get(&secret_key(platform, role)).cloned()
    }

    /// Insert or replace one platform role's password.
    pub fn set(&mut self, platform: &str, role: &str, password: &str) {
        self.entries
            .insert(secret_key(platform, role), password.to_owned());
    }

    /// Remove every role stored for a platform. Returns `true` when a role was
    /// removed.
    pub fn remove_platform(&mut self, platform: &str) -> bool {
        let prefix = format!("{platform}:");
        let before = self.entries.len();
        self.entries.retain(|key, _| !key.starts_with(&prefix));
        self.entries.len() != before
    }

    /// `true` when no password is stored.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Persist the secrets to the keychain, replacing the item. An empty set
    /// deletes the item. Writes once.
    pub fn store(&self, bundle_id: &str) -> BundleResult<()> {
        crate::keychain::ensure_store();
        if self.entries.is_empty() {
            return crate::keychain::delete(KEYCHAIN_SERVICE, bundle_id)
                .map_err(|e| BundleError::Signing(format!("keychain delete failed: {e}")));
        }
        let raw = serde_json::to_string(self).map_err(|e| {
            BundleError::Signing(format!("could not serialize signing secrets: {e}"))
        })?;
        crate::keychain::set(KEYCHAIN_SERVICE, bundle_id, &raw)
            .map_err(|e| BundleError::Signing(format!("keychain write failed: {e}")))
    }
}

/// The keychain lookup key for a platform role.
fn secret_key(platform: &str, role: &str) -> String {
    format!("{platform}:{role}")
}

/// The legacy per-role keychain account for a project, platform, and role.
fn legacy_account(bundle_id: &str, platform: &str, role: &str) -> String {
    format!("{bundle_id}:{platform}:{role}")
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
    let secrets = SigningSecrets::load(bundle_id);
    let (Some(store_password), Some(key_password)) = (
        secrets.get("android", "store"),
        secrets.get("android", "key"),
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
    let Some(p12_password) = SigningSecrets::load(bundle_id).get(platform, "p12") else {
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
    let Some(password) = SigningSecrets::load(bundle_id).get("windows", "pfx") else {
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
// Verification
//
// Checks that the registered material for a platform actually satisfies that
// platform's signing requirements: the files are present, the passwords open
// them, the certificates are the right kind (an Apple code-signing certificate
// for macOS/iOS, a code-signing certificate for Windows), and nothing has
// expired. Runs on any host: PKCS#12 parsing and certificate inspection use the
// pure-Rust apple-codesign crate; a JKS keystore is verified with `keytool`
// when a JDK is present and otherwise reported as present-but-unverified.
// ---------------------------------------------------------------------------

/// One check within a platform's signing verification.
#[derive(Debug, Serialize)]
pub struct KeyCheck {
    /// What is being checked, e.g. "certificate", "provisioning profile".
    pub role: String,
    /// The registered file name, when the check concerns a file.
    pub file: Option<String>,
    /// The material (file and any secret) exists.
    pub present: bool,
    /// The material is present and satisfies the platform's requirements.
    pub ok: bool,
    /// The material is present but could not be fully verified on this host
    /// (e.g. a JKS keystore with no JDK available); treated as a soft pass.
    pub unverified: bool,
    /// A human message: the certificate identity and validity, or the reason
    /// the material does not satisfy the requirement.
    pub detail: String,
}

/// The verification result for one platform.
#[derive(Debug, Serialize)]
pub struct PlatformReport {
    pub platform: String,
    /// "missing" (required material absent), "invalid" (present but a check
    /// failed), "unverified" (present, a soft pass), or "valid".
    pub state: String,
    pub checks: Vec<KeyCheck>,
}

fn file_name_of(path: &Path) -> Option<String> {
    path.file_name().map(|n| n.to_string_lossy().to_string())
}

/// Open a PKCS#12 container, returning the leaf certificate or a message that
/// distinguishes a wrong password from a malformed file.
fn open_pkcs12(bytes: &[u8], password: &str) -> Result<CapturedX509Certificate, String> {
    match apple_codesign::cryptography::parse_pfx_data(bytes, password) {
        Ok((certificate, _key)) => Ok(certificate),
        Err(e) => {
            let debug = format!("{e:?}");
            if debug.contains("BadPassword") {
                Err("wrong password".to_string())
            } else {
                Err(format!("not a valid PKCS#12 certificate ({e})"))
            }
        }
    }
}

/// Check a certificate's validity window against the current time. Returns a
/// short "valid until <date>" message, or the reason it is out of window.
fn cert_validity(cert: &CapturedX509Certificate) -> Result<String, String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let not_before = cert.validity_not_before().timestamp();
    let not_after = cert.validity_not_after().timestamp();
    if now < not_before {
        return Err(format!("not valid until {}", cert.validity_not_before()));
    }
    if now > not_after {
        return Err(format!("expired on {}", cert.validity_not_after()));
    }
    Ok(format!("valid until {}", cert.validity_not_after()))
}

/// Whether an Apple certificate profile is usable for signing an application on
/// the given platform, with a message naming the profile or the mismatch.
fn apple_profile_ok(cert: &CapturedX509Certificate, platform: &str) -> Result<String, String> {
    use apple_codesign::CertificateProfile as P;
    let Some(profile) = cert.apple_guess_profile() else {
        return Err("not an Apple code-signing certificate".to_string());
    };
    let acceptable = match platform {
        // A macOS .app is signed by a Developer ID (distributed outside the App
        // Store), an Apple Distribution (App Store), or an Apple Development
        // certificate.
        "macos" => matches!(
            profile,
            P::DeveloperIdApplication | P::AppleDistribution | P::AppleDevelopment
        ),
        // iOS uses Apple Distribution or Apple Development, paired with a
        // provisioning profile.
        "ios" => matches!(profile, P::AppleDistribution | P::AppleDevelopment),
        _ => false,
    };
    if acceptable {
        Ok(format!("{profile:?}"))
    } else {
        match profile {
            P::MacInstallerDistribution | P::DeveloperIdInstaller => Err(
                "installer-signing certificate; an application-signing certificate is required"
                    .to_string(),
            ),
            other => Err(format!(
                "{other:?} is not usable for {platform} app signing"
            )),
        }
    }
}

/// Civil date (year, month, day) from a Unix day count. Howard Hinnant's
/// algorithm; used to format "today" without a date-library dependency.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn today_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, m, d) = civil_from_days(secs / 86400);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Read a provisioning profile's ExpirationDate. Returns Some((expired, date))
/// when a date is found, None when the profile parses but names no expiration.
fn profile_expiration(path: &Path) -> Result<Option<(bool, String)>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read profile: {e}"))?;
    let text = String::from_utf8_lossy(&bytes);
    let start = text
        .find("<?xml")
        .ok_or("not a valid provisioning profile")?;
    let end_rel = text[start..]
        .find("</plist>")
        .ok_or("not a valid provisioning profile")?;
    let plist = &text[start..start + end_rel];
    let Some(key_pos) = plist.find("<key>ExpirationDate</key>") else {
        return Ok(None);
    };
    let after = &plist[key_pos..];
    let Some(date_start) = after.find("<date>") else {
        return Ok(None);
    };
    let Some(date_end) = after[date_start..].find("</date>") else {
        return Ok(None);
    };
    let date = &after[date_start + 6..date_start + date_end];
    // ISO 8601 UTC strings order chronologically, so a lexical compare of the
    // YYYY-MM-DD prefix against today decides expiration.
    let day = date.get(0..10).unwrap_or(date);
    Ok(Some((day < today_iso().as_str(), date.to_string())))
}

/// Verify a JKS keystore's store password and alias with `keytool`. Ok(true)
/// when the alias opens, Ok(false) when it does not, Err when keytool is not
/// available (so the caller can report the material as unverified).
fn verify_jks_with_keytool(keystore: &Path, store_password: &str, alias: &str) -> Result<bool, ()> {
    let status = Command::new("keytool")
        .arg("-list")
        .arg("-keystore")
        .arg(keystore)
        .arg("-storepass")
        .arg(store_password)
        .arg("-alias")
        .arg(alias)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match status {
        Ok(status) => Ok(status.success()),
        Err(_) => Err(()),
    }
}

fn verify_apple(
    root: &Path,
    registry: &Value,
    secrets: &SigningSecrets,
    platform: &str,
    checks: &mut Vec<KeyCheck>,
) {
    let cert_file = registered_file(root, registry, platform, "p12");
    let password = secrets.get(platform, "p12");
    let mut check = KeyCheck {
        role: "certificate".to_string(),
        file: cert_file.as_deref().and_then(file_name_of),
        present: false,
        ok: false,
        unverified: false,
        detail: String::new(),
    };
    match (cert_file, password) {
        (None, _) => check.detail = "no signing certificate registered".to_string(),
        (Some(_), None) => check.detail = "certificate password not set".to_string(),
        (Some(path), Some(password)) => {
            check.present = true;
            match std::fs::read(&path) {
                Err(e) => check.detail = format!("cannot read certificate: {e}"),
                Ok(bytes) => match open_pkcs12(&bytes, &password) {
                    Err(message) => check.detail = message,
                    Ok(cert) => {
                        let cn = cert.subject_common_name().unwrap_or_default();
                        let team = cert
                            .apple_team_id()
                            .map(|t| format!(" [team {t}]"))
                            .unwrap_or_default();
                        match cert_validity(&cert).and_then(|valid| {
                            apple_profile_ok(&cert, platform).map(|profile| (valid, profile))
                        }) {
                            Err(message) => check.detail = format!("{cn}: {message}"),
                            Ok((valid, profile)) => {
                                check.ok = true;
                                check.detail = format!("{cn}{team} - {profile}, {valid}");
                            }
                        }
                    }
                },
            }
        }
    }
    checks.push(check);

    if platform == "ios" {
        let profile_file = registered_file(root, registry, "ios", "profile");
        let mut check = KeyCheck {
            role: "provisioning profile".to_string(),
            file: profile_file.as_deref().and_then(file_name_of),
            present: profile_file.is_some(),
            ok: false,
            unverified: false,
            detail: String::new(),
        };
        match profile_file {
            None => check.detail = "no provisioning profile registered".to_string(),
            Some(path) => match profile_expiration(&path) {
                Err(message) => check.detail = message,
                Ok(None) => {
                    check.ok = true;
                    check.detail = "profile parsed".to_string();
                }
                Ok(Some((expired, when))) => {
                    if expired {
                        check.detail = format!("profile expired on {when}");
                    } else {
                        check.ok = true;
                        check.detail = format!("valid until {when}");
                    }
                }
            },
        }
        checks.push(check);
    }
}

fn verify_windows(
    root: &Path,
    registry: &Value,
    secrets: &SigningSecrets,
    checks: &mut Vec<KeyCheck>,
) {
    let cert_file = registered_file(root, registry, "windows", "pfx");
    let password = secrets.get("windows", "pfx");
    let mut check = KeyCheck {
        role: "certificate".to_string(),
        file: cert_file.as_deref().and_then(file_name_of),
        present: false,
        ok: false,
        unverified: false,
        detail: String::new(),
    };
    match (cert_file, password) {
        (None, _) => check.detail = "no signing certificate registered".to_string(),
        (Some(_), None) => check.detail = "certificate password not set".to_string(),
        (Some(path), Some(password)) => {
            check.present = true;
            match std::fs::read(&path) {
                Err(e) => check.detail = format!("cannot read certificate: {e}"),
                Ok(bytes) => match open_pkcs12(&bytes, &password) {
                    Err(message) => check.detail = message,
                    Ok(cert) => {
                        let cn = cert.subject_common_name().unwrap_or_default();
                        match cert_validity(&cert) {
                            Err(message) => check.detail = format!("{cn}: {message}"),
                            Ok(valid) => {
                                let purposes = cert.apple_extended_key_usage_purposes();
                                let has_code_signing = purposes.iter().any(|p| {
                                    matches!(
                                        p,
                                        apple_codesign::ExtendedKeyUsagePurpose::CodeSigning
                                    )
                                });
                                if has_code_signing {
                                    check.ok = true;
                                    check.detail =
                                        format!("{cn} - code-signing certificate, {valid}");
                                } else if purposes.is_empty() {
                                    check.ok = true;
                                    check.unverified = true;
                                    check.detail = format!(
                                        "{cn} - {valid} (key usage could not be determined)"
                                    );
                                } else {
                                    check.detail = format!(
                                        "{cn}: not a code-signing certificate (no Code Signing extended key usage)"
                                    );
                                }
                            }
                        }
                    }
                },
            }
        }
    }
    checks.push(check);
}

fn verify_android(
    root: &Path,
    registry: &Value,
    secrets: &SigningSecrets,
    checks: &mut Vec<KeyCheck>,
) {
    let keystore = registered_file(root, registry, "android", "keystore");
    let alias = platform_entry(registry, "android")
        .and_then(|e| e.get("alias"))
        .and_then(|a| a.as_str())
        .unwrap_or("upload")
        .to_string();
    let store_password = secrets.get("android", "store");
    let mut check = KeyCheck {
        role: "keystore".to_string(),
        file: keystore.as_deref().and_then(file_name_of),
        present: false,
        ok: false,
        unverified: false,
        detail: String::new(),
    };
    match (keystore, store_password) {
        (None, _) => check.detail = "no keystore registered".to_string(),
        (Some(_), None) => check.detail = "keystore password not set".to_string(),
        (Some(path), Some(store_password)) => {
            check.present = true;
            match std::fs::read(&path) {
                Err(e) => check.detail = format!("cannot read keystore: {e}"),
                Ok(bytes) if bytes.first() == Some(&0x30) => {
                    // PKCS#12 keystore.
                    match open_pkcs12(&bytes, &store_password) {
                        Err(message) => check.detail = format!("keystore: {message}"),
                        Ok(cert) => {
                            let cn = cert.subject_common_name().unwrap_or_default();
                            match cert_validity(&cert) {
                                Err(message) => check.detail = format!("{cn}: {message}"),
                                Ok(valid) => {
                                    check.ok = true;
                                    check.detail = format!("PKCS#12 keystore - {cn}, {valid}");
                                }
                            }
                        }
                    }
                }
                Ok(bytes) if bytes.len() >= 4 && bytes[0..4] == [0xFE, 0xED, 0xFE, 0xED] => {
                    // JKS keystore: verify with keytool when a JDK is present.
                    match verify_jks_with_keytool(&path, &store_password, &alias) {
                        Ok(true) => {
                            check.ok = true;
                            check.detail =
                                format!("JKS keystore - alias '{alias}' opens with the password");
                        }
                        Ok(false) => {
                            check.detail =
                                format!("alias '{alias}' not found or the store password is wrong");
                        }
                        Err(()) => {
                            check.ok = true;
                            check.unverified = true;
                            check.detail = format!(
                                "JKS keystore present; install a JDK (keytool) to verify alias '{alias}'"
                            );
                        }
                    }
                }
                Ok(_) => {
                    check.detail =
                        "unrecognized keystore format (expected JKS or PKCS#12)".to_string()
                }
            }
        }
    }
    checks.push(check);
}

/// Verify all registered signing material for a platform.
pub fn verify_platform(
    project_root: &Path,
    secrets: &SigningSecrets,
    platform: &str,
) -> PlatformReport {
    let registry =
        load_registry(project_root).unwrap_or_else(|_| Value::Object(Default::default()));
    let mut checks = Vec::new();
    match platform {
        "macos" | "ios" => verify_apple(project_root, &registry, secrets, platform, &mut checks),
        "windows" => verify_windows(project_root, &registry, secrets, &mut checks),
        "android" => verify_android(project_root, &registry, secrets, &mut checks),
        _ => {}
    }

    let state = if checks.is_empty() || checks.iter().any(|c| !c.present) {
        "missing"
    } else if checks.iter().any(|c| !c.ok) {
        "invalid"
    } else if checks.iter().any(|c| c.unverified) {
        "unverified"
    } else {
        "valid"
    };
    PlatformReport {
        platform: platform.to_string(),
        state: state.to_string(),
        checks,
    }
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

/// Sign a path in place with the given Apple key on a dedicated thread.
/// When `entitlements_xml` is `Some`, it is applied to the main executable.
/// When `runtime_flag` is true, the main executable carries the hardened
/// runtime flag. When `timestamp` is true, the signature includes a secure
/// timestamp from Apple's timestamp service. The timestamp request runs its
/// own runtime, so the work runs on a dedicated thread that has no ambient
/// runtime. The inner runtime then starts and drops in a plain blocking
/// context.
fn sign_path_isolated(
    target: &Path,
    key: &AppleSigningKey,
    entitlements_xml: Option<&str>,
    runtime_flag: bool,
    timestamp: bool,
) -> BundleResult<()> {
    std::thread::scope(|scope| {
        let handle = scope.spawn(move || -> BundleResult<()> {
            use apple_codesign::cryptography::PrivateKey;
            use apple_codesign::{
                CodeSignatureFlags, SettingsScope, SigningSettings, UnifiedSigner,
            };

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

            // Register the Apple CA chain (WWDR intermediate and Apple root)
            // from the crate's bundled Apple certificates so the signature
            // carries a full chain. The signer otherwise embeds only the
            // leaf, and Apple does not recognize the result as a submission
            // certificate.
            settings.chain_apple_certificates();

            // Embed the team identifier carried by the Apple signing
            // certificate.
            settings.set_team_id_from_signing_certificate();

            if let Some(xml) = entitlements_xml {
                settings
                    .set_entitlements_xml(SettingsScope::Main, xml)
                    .map_err(|e| BundleError::Signing(format!("entitlements setup failed: {e}")))?;
            }

            if runtime_flag {
                settings.set_code_signature_flags(SettingsScope::Main, CodeSignatureFlags::RUNTIME);
            }

            if timestamp {
                settings
                    .set_time_stamp_url("http://timestamp.apple.com/ts01")
                    .map_err(|e| BundleError::Signing(format!("timestamp setup failed: {e}")))?;
            }

            let signer = UnifiedSigner::new(settings);
            signer
                .sign_path_in_place(target)
                .map_err(|e| BundleError::Signing(format!("signing failed: {e}")))
        });

        match handle.join() {
            Ok(result) => result,
            Err(_) => Err(BundleError::Signing("signing thread panicked".to_string())),
        }
    })
}

/// Sign a Mach-O bundle (a `.app` directory) in place with the given Apple
/// key. When `entitlements_xml` is `Some`, it is applied to the main
/// executable. When `hardened_runtime` is true, the main executable is
/// signed with the hardened runtime flag and a secure timestamp from
/// Apple's timestamp service, which notarization requires.
pub fn sign_apple_bundle(
    app_dir: &Path,
    key: &AppleSigningKey,
    entitlements_xml: Option<&str>,
    hardened_runtime: bool,
) -> BundleResult<()> {
    sign_path_isolated(
        app_dir,
        key,
        entitlements_xml,
        hardened_runtime,
        hardened_runtime,
    )
}

/// Sign a disk image in place with the given Apple key and a secure
/// timestamp. The image carries no hardened runtime flag and no
/// entitlements. The timestamp is included because notarization requires
/// it on the image signature.
pub fn sign_dmg(dmg: &Path, key: &AppleSigningKey) -> BundleResult<()> {
    sign_path_isolated(dmg, key, None, false, true)
}

/// Apple notarization credentials: an App Store Connect API key encoded as
/// a single JSON file by `rcodesign encode-app-store-connect-api-key`.
pub struct NotaryCredentials {
    pub api_key_json: PathBuf,
}

/// Resolve notarization credentials for a platform from the registry, or
/// `None` when no notary key is registered or the file is missing.
pub fn resolve_notary(project_root: &Path, platform: &str) -> Option<NotaryCredentials> {
    let registry = load_registry(project_root).ok()?;
    let api_key_json = registered_file(project_root, &registry, platform, "notary_key")?;
    if !api_key_json.exists() {
        return None;
    }
    Some(NotaryCredentials { api_key_json })
}

/// Notarize a signed bundle with Apple's notary service and staple the
/// ticket onto it, using the `apple-codesign` crate in process. The
/// credentials are an App Store Connect API key JSON file. The bundle is
/// submitted, the call waits for a terminal result, and the ticket is
/// stapled onto the bundle when notarization succeeds. This needs no
/// external tools and runs on any host the crate compiles on.
pub fn notarize_and_staple(app: &Path, creds: &NotaryCredentials) -> BundleResult<()> {
    use apple_codesign::notarization::Notarizer;
    use apple_codesign::stapling::Stapler;

    // apple-codesign drives the notary API with its own tokio runtime. The
    // CLI runs inside a tokio runtime, so the notary work runs on a
    // dedicated thread that has no ambient runtime. The inner runtime then
    // starts and drops in a plain blocking context.
    std::thread::scope(|scope| {
        let handle = scope.spawn(|| -> BundleResult<()> {
            let notarizer = Notarizer::from_api_key(&creds.api_key_json)
                .map_err(|e| BundleError::Signing(format!("notary key load failed: {e}")))?;

            notarizer
                .notarize_path(app, Some(std::time::Duration::from_secs(600)))
                .map_err(|e| BundleError::Signing(format!("notarization failed: {e}")))?;

            let stapler = Stapler::new()
                .map_err(|e| BundleError::Signing(format!("stapler init failed: {e}")))?;
            stapler
                .staple_path(app)
                .map_err(|e| BundleError::Signing(format!("staple failed: {e}")))?;

            Ok(())
        });

        match handle.join() {
            Ok(result) => result,
            Err(_) => Err(BundleError::Signing(
                "notarization thread panicked".to_string(),
            )),
        }
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_and_remove_by_platform() {
        let mut secrets = SigningSecrets::default();
        assert!(secrets.is_empty());

        secrets.set("android", "store", "a");
        secrets.set("android", "key", "b");
        secrets.set("macos", "p12", "c");
        assert!(!secrets.is_empty());
        assert_eq!(secrets.get("android", "store").as_deref(), Some("a"));
        assert_eq!(secrets.get("macos", "p12").as_deref(), Some("c"));
        assert_eq!(secrets.get("ios", "p12"), None);

        // Removing one platform leaves the others in place.
        assert!(secrets.remove_platform("android"));
        assert_eq!(secrets.get("android", "store"), None);
        assert_eq!(secrets.get("android", "key"), None);
        assert_eq!(secrets.get("macos", "p12").as_deref(), Some("c"));

        // Removing a platform with nothing stored reports no change.
        assert!(!secrets.remove_platform("windows"));
    }

    #[test]
    fn serializes_as_one_flat_blob() {
        let mut secrets = SigningSecrets::default();
        secrets.set("ios", "p12", "secret");
        secrets.set("android", "store", "pw");

        let json = serde_json::to_string(&secrets).unwrap();
        // The stored item is a flat object keyed by platform:role, with no
        // wrapper field, so one keychain item holds every password.
        assert!(!json.contains("entries"));
        assert!(json.contains("\"ios:p12\":\"secret\""));
        assert!(json.contains("\"android:store\":\"pw\""));

        let restored: SigningSecrets = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.get("ios", "p12").as_deref(), Some("secret"));
        assert_eq!(restored.get("android", "store").as_deref(), Some("pw"));
    }
}
