//! Envelope encryption for the deploy bundle's Apple signing material.
//!
//! A remote Mac builder must sign the release build, so it needs the signing
//! certificate, its password, and the provisioning profile — but the platform
//! that relays the bundle must never see them in the clear. The CLI seals the
//! material to the runner's public key (the `age` format: X25519 +
//! ChaCha20-Poly1305); only the Mac, holding the matching secret key, can
//! [`unseal`] it. Both ends run in the Rust CLI (`peko deploy runner-keygen`
//! and `peko deploy unseal`), so the Python build worker never touches
//! plaintext keys.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;

use age::secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};

use crate::bundler::signing::AppleSigningKey;

/// The manifest inside the sealed blob: per-platform bundle-relative file paths
/// and the certificate password.
#[derive(Serialize, Deserialize)]
struct SealedManifest {
    platforms: BTreeMap<String, SealedPlatform>,
}

/// One platform's sealed signing material, by path within the unsealed tree.
#[derive(Serialize, Deserialize)]
pub struct SealedPlatform {
    /// Path to the app-signing PKCS#12 certificate (relative to the unseal dir).
    pub p12: String,
    /// The app-signing certificate password.
    pub password: String,
    /// Path to the provisioning profile, if any.
    pub profile: Option<String>,
    /// Path to the entitlements plist, if any.
    pub entitlements: Option<String>,
    /// Path to the Mac Installer Distribution `.pkg`-signing certificate, if any
    /// (macOS only). Passed to `peko build --installer-p12`.
    #[serde(default)]
    pub installer_p12: Option<String>,
    /// The installer certificate password, if `installer_p12` is set.
    #[serde(default)]
    pub installer_password: Option<String>,
}

/// The signing material to seal for one platform: the app-signing key plus, for
/// macOS, an optional Mac Installer Distribution certificate and its password.
pub struct PlatformSigning {
    /// The app / bundle signing key (certificate, password, profile).
    pub app: AppleSigningKey,
    /// The macOS installer certificate `(p12 path, password)`, if registered.
    pub installer: Option<(std::path::PathBuf, String)>,
}

/// Generate a new runner keypair. Returns `(public, secret)` as the `age`
/// bech32 strings (`age1…` recipient, `AGE-SECRET-KEY-…` identity).
pub fn generate_runner_key() -> (String, String) {
    let identity = age::x25519::Identity::generate();
    let public = identity.to_public().to_string();
    let secret = identity.to_string().expose_secret().to_string();
    (public, secret)
}

/// The public recipient (`age1…`) for an existing secret identity
/// (`AGE-SECRET-KEY-…`), so a runner can re-register without generating a new
/// key — which would invalidate every bundle sealed to the old one.
pub fn public_from_secret(secret: &str) -> Result<String, String> {
    let identity: age::x25519::Identity = secret
        .trim()
        .parse()
        .map_err(|e| format!("invalid runner secret key: {e}"))?;
    Ok(identity.to_public().to_string())
}

/// Encrypt `plaintext` to the recipient public key (`age1…`).
fn seal(recipient: &str, plaintext: &[u8]) -> Result<Vec<u8>, String> {
    let recipient: age::x25519::Recipient = recipient
        .parse()
        .map_err(|e| format!("invalid runner public key: {e}"))?;
    let encryptor =
        age::Encryptor::with_recipients(std::iter::once(&recipient as &dyn age::Recipient))
            .map_err(|e| format!("could not build the encryptor: {e}"))?;
    let mut out = Vec::new();
    let mut writer = encryptor
        .wrap_output(&mut out)
        .map_err(|e| format!("could not start encryption: {e}"))?;
    writer
        .write_all(plaintext)
        .map_err(|e| format!("could not write ciphertext: {e}"))?;
    writer
        .finish()
        .map_err(|e| format!("could not finish encryption: {e}"))?;
    Ok(out)
}

/// Decrypt `ciphertext` with the runner secret key (`AGE-SECRET-KEY-…`).
fn open(secret: &str, ciphertext: &[u8]) -> Result<Vec<u8>, String> {
    let identity: age::x25519::Identity = secret
        .parse()
        .map_err(|e| format!("invalid runner secret key: {e}"))?;
    let decryptor = age::Decryptor::new(ciphertext)
        .map_err(|e| format!("could not read the sealed blob: {e}"))?;
    let mut reader = decryptor
        .decrypt(std::iter::once(&identity as &dyn age::Identity))
        .map_err(|e| format!("could not decrypt the sealed blob (wrong key?): {e}"))?;
    let mut out = Vec::new();
    reader
        .read_to_end(&mut out)
        .map_err(|e| format!("could not read the decrypted material: {e}"))?;
    Ok(out)
}

/// Tar the given platforms' signing material (certificate, profile,
/// entitlements) with a manifest carrying the passwords, and seal it to the
/// recipient public key. `keys` maps a platform id (`ios`, `macos`) to its
/// resolved signing material.
pub fn seal_signing_material(
    recipient: &str,
    keys: &BTreeMap<String, PlatformSigning>,
) -> Result<Vec<u8>, String> {
    let mut tar = tar::Builder::new(Vec::new());
    let mut manifest = SealedManifest {
        platforms: BTreeMap::new(),
    };
    for (platform, material) in keys {
        let key = &material.app;
        let p12_rel = format!("{platform}/cert.p12");
        append_file(&mut tar, &p12_rel, &key.p12)?;
        let profile = match &key.profile {
            Some(path) => {
                let rel = format!("{platform}/profile.mobileprovision");
                append_file(&mut tar, &rel, path)?;
                Some(rel)
            }
            None => None,
        };
        let entitlements = match &key.entitlements {
            Some(path) => {
                let rel = format!("{platform}/entitlements.plist");
                append_file(&mut tar, &rel, path)?;
                Some(rel)
            }
            None => None,
        };
        let (installer_p12, installer_password) = match &material.installer {
            Some((path, password)) => {
                let rel = format!("{platform}/installer.p12");
                append_file(&mut tar, &rel, path)?;
                (Some(rel), Some(password.clone()))
            }
            None => (None, None),
        };
        manifest.platforms.insert(
            platform.clone(),
            SealedPlatform {
                p12: p12_rel,
                password: key.p12_password.clone(),
                profile,
                entitlements,
                installer_p12,
                installer_password,
            },
        );
    }
    let manifest_json = serde_json::to_vec_pretty(&manifest)
        .map_err(|e| format!("could not encode the signing manifest: {e}"))?;
    let mut header = tar::Header::new_gnu();
    header.set_size(manifest_json.len() as u64);
    header.set_mode(0o600);
    header.set_cksum();
    tar.append_data(&mut header, "manifest.json", &manifest_json[..])
        .map_err(|e| format!("could not add the signing manifest: {e}"))?;
    let tar_bytes = tar
        .into_inner()
        .map_err(|e| format!("could not finalize the signing archive: {e}"))?;
    seal(recipient, &tar_bytes)
}

/// Unseal a sealed blob into `out_dir` and return the extracted per-platform
/// material (with paths made absolute under `out_dir`). Used by
/// `peko deploy unseal` on the Mac; the build worker then passes the paths to
/// `peko build`'s headless signing flags.
pub fn unseal_to_dir(
    secret: &str,
    sealed: &[u8],
    out_dir: &Path,
) -> Result<BTreeMap<String, SealedPlatform>, String> {
    let tar_bytes = open(secret, sealed)?;
    std::fs::create_dir_all(out_dir)
        .map_err(|e| format!("could not create {}: {e}", out_dir.display()))?;
    let mut archive = tar::Archive::new(&tar_bytes[..]);
    archive
        .unpack(out_dir)
        .map_err(|e| format!("could not extract the signing material: {e}"))?;
    let manifest_text = std::fs::read_to_string(out_dir.join("manifest.json"))
        .map_err(|e| format!("could not read the signing manifest: {e}"))?;
    let manifest: SealedManifest = serde_json::from_str(&manifest_text)
        .map_err(|e| format!("could not parse the signing manifest: {e}"))?;
    Ok(manifest.platforms)
}

/// Append `src` to the tar under bundle-relative `name`, mode 0600.
fn append_file(tar: &mut tar::Builder<Vec<u8>>, name: &str, src: &Path) -> Result<(), String> {
    let bytes = std::fs::read(src).map_err(|e| format!("could not read {}: {e}", src.display()))?;
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o600);
    header.set_cksum();
    tar.append_data(&mut header, name, &bytes[..])
        .map_err(|e| format!("could not add {name} to the signing archive: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_unseal_round_trip() {
        let (public, secret) = generate_runner_key();
        assert!(public.starts_with("age1"));
        assert!(secret.starts_with("AGE-SECRET-KEY-"));

        let dir = tempfile::tempdir().unwrap();
        let p12 = dir.path().join("cert.p12");
        let profile = dir.path().join("app.mobileprovision");
        let installer = dir.path().join("installer.p12");
        std::fs::write(&p12, b"fake-p12-bytes-\x00\x01\x02").unwrap();
        std::fs::write(&profile, b"fake-profile").unwrap();
        std::fs::write(&installer, b"fake-installer-p12").unwrap();

        let mut keys = BTreeMap::new();
        keys.insert(
            "macos".to_owned(),
            PlatformSigning {
                app: AppleSigningKey {
                    p12: p12.clone(),
                    p12_password: "s3cr3t".to_owned(),
                    profile: Some(profile.clone()),
                    entitlements: None,
                },
                installer: Some((installer.clone(), "inst-pw".to_owned())),
            },
        );

        let sealed = seal_signing_material(&public, &keys).unwrap();
        // A wrong key cannot open it.
        let (_other_pub, other_secret) = generate_runner_key();
        let out = dir.path().join("unsealed");
        assert!(unseal_to_dir(&other_secret, &sealed, &out).is_err());

        let platforms = unseal_to_dir(&secret, &sealed, &out).unwrap();
        let macos = platforms.get("macos").unwrap();
        assert_eq!(macos.password, "s3cr3t");
        assert_eq!(
            std::fs::read(out.join(&macos.p12)).unwrap(),
            b"fake-p12-bytes-\x00\x01\x02"
        );
        assert!(macos.profile.is_some());
        // The installer certificate round-trips too.
        assert_eq!(macos.installer_password.as_deref(), Some("inst-pw"));
        assert_eq!(
            std::fs::read(out.join(macos.installer_p12.as_ref().unwrap())).unwrap(),
            b"fake-installer-p12"
        );
    }
}
