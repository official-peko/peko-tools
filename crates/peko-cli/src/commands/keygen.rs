//! `peko keys generate`: create signing material without leaving the CLI.
//!
//! Getting a project signable has historically meant following a platform's own
//! documentation with tools that are only present on one operating system —
//! `keytool` from a JDK for Android, `openssl` plus Keychain Access for Apple.
//! This module does that work from any host:
//!
//! - **Android**: a PKCS#12 keystore, generated with the JDK the toolchain
//!   already ships, so nothing has to be installed.
//! - **Apple**: an RSA key and a certificate signing request to upload to the
//!   developer portal, then the `.cer` Apple returns combined with that key into
//!   the `.p12` the bundler signs with. Both steps are pure Rust, so a Windows
//!   or Linux developer can produce Apple signing material.
//!
//! Generated material is registered in the project's key registry and its
//! passwords stored in the OS keychain, exactly as `peko keys add` would, so
//! nothing downstream needs to know how a key was obtained.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::bundler::signing;
use crate::cli::CLIInfo;
use crate::cli::reporting::Reporter;

/// The private key a CSR was generated with, kept next to the request so the
/// `.cer` Apple returns can be paired with it later.
const APPLE_KEY_FILE: &str = "apple-signing-key.pem";
/// The certificate signing request to upload to the Apple developer portal.
const APPLE_CSR_FILE: &str = "apple-signing-request.certSigningRequest";

/// Execute `peko keys generate`.
pub fn execute(cli_info: &CLIInfo, reporter: &Reporter, root: &Path, bundle_id: &str) -> ExitCode {
    let Some(platform) = cli_info.flags.get_flag("platform") else {
        reporter.error("`keys generate` needs --platform <android|apple>");
        reporter.help("android generates a keystore; apple generates a certificate request");
        return ExitCode::FAILURE;
    };
    match platform.as_str() {
        "android" => generate_android(cli_info, reporter, root, bundle_id),
        "apple" | "ios" | "macos" => generate_apple_csr(cli_info, reporter, root),
        other => {
            reporter.error(format!("cannot generate keys for '{other}'"));
            reporter.help("supported: android, apple");
            ExitCode::FAILURE
        }
    }
}

/// Generate an Android upload keystore with the bundled JDK's `keytool`.
///
/// Uses PKCS#12 rather than the legacy JKS format: it is the JDK default, and
/// the bundler reads the keystore back through the same format.
fn generate_android(
    cli_info: &CLIInfo,
    reporter: &Reporter,
    root: &Path,
    bundle_id: &str,
) -> ExitCode {
    let alias = cli_info
        .flags
        .get_flag("alias")
        .unwrap_or_else(|| "upload".to_owned());
    let validity = cli_info
        .flags
        .get_flag("validity")
        .unwrap_or_else(|| "10000".to_owned());

    // Play requires an upload key valid well beyond a normal release cadence;
    // a key that expires makes future updates unpublishable under the same
    // listing, so the default is deliberately long.
    let Some(password) = read_new_password(cli_info, reporter) else {
        return ExitCode::FAILURE;
    };

    let dname = cli_info.flags.get_flag("dname").unwrap_or_else(|| {
        // keytool requires a distinguished name; the bundle id is the only
        // identifier the project reliably has, and it is not user-visible in
        // the published app.
        format!("CN={bundle_id}, OU=Unknown, O=Unknown, L=Unknown, S=Unknown, C=US")
    });

    let dir = signing::platform_dir(root, "android");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        reporter.error(format!("could not create {}: {e}", dir.display()));
        return ExitCode::FAILURE;
    }
    let out = cli_info
        .flags
        .get_flag("out")
        .map(PathBuf::from)
        .unwrap_or_else(|| dir.join("upload.keystore"));

    if out.exists() {
        // Overwriting a keystore is unrecoverable: Play ties the listing to this
        // key, and a replacement cannot publish updates to the same app.
        reporter.error(format!("{} already exists", out.display()));
        reporter.help("delete it first if you are certain, or pass --out with another path");
        return ExitCode::FAILURE;
    }

    let keytool = crate::bundler::java_tool(cli_info.get_peko_root(), "keytool");
    if !keytool.exists() {
        reporter.error("the bundled JDK is missing, so keytool is unavailable");
        reporter.help("run `peko setup` to install the toolchain");
        return ExitCode::FAILURE;
    }

    reporter.status(
        "Generating",
        format!("Android keystore at {}", out.display()),
    );
    let mut command = std::process::Command::new(&keytool);
    command
        .arg("-genkeypair")
        .arg("-keystore")
        .arg(&out)
        .arg("-storetype")
        .arg("PKCS12")
        .arg("-alias")
        .arg(&alias)
        .arg("-keyalg")
        .arg("RSA")
        .arg("-keysize")
        .arg("2048")
        .arg("-validity")
        .arg(&validity)
        .arg("-dname")
        .arg(&dname)
        .arg("-storepass")
        .arg(&password)
        .arg("-keypass")
        .arg(&password);
    crate::proc::hide_window(&mut command);
    crate::proc::route_stdout_to_stderr(&mut command);

    match command.status() {
        Err(e) => {
            reporter.error(format!("could not run keytool: {e}"));
            return ExitCode::FAILURE;
        }
        Ok(status) if !status.success() => {
            reporter.error("keytool failed to generate the keystore");
            return ExitCode::FAILURE;
        }
        Ok(_) => {}
    }

    // Register the keystore and store both passwords, so the project is
    // immediately signable rather than requiring a separate `keys add`.
    if !register(reporter, root, "android", "keystore", &out) {
        return ExitCode::FAILURE;
    }
    let mut secrets = signing::SigningSecrets::load(bundle_id);
    secrets.set("android", "store", &password);
    secrets.set("android", "key", &password);
    if let Err(e) = secrets.store(bundle_id) {
        reporter.error(format!("could not store the keystore password: {e}"));
        return ExitCode::FAILURE;
    }

    reporter.success(format!("generated and registered {}", out.display()));
    reporter.info(format!("alias: {alias}"));
    reporter.warning("back this keystore up: Play updates cannot be signed with a different key");
    ExitCode::SUCCESS
}

/// Generate an RSA key and a certificate signing request for Apple.
///
/// The request is uploaded at developer.apple.com, which returns a `.cer`;
/// `peko keys p12` then pairs that certificate with the key kept here. Doing it
/// this way means no Mac and no Keychain Access are needed to obtain Apple
/// signing material.
fn generate_apple_csr(cli_info: &CLIInfo, reporter: &Reporter, root: &Path) -> ExitCode {
    use rsa::pkcs8::{EncodePrivateKey, LineEnding};

    let Some(email) = cli_info.flags.get_flag("email") else {
        reporter.error("`keys generate --platform apple` needs --email <your Apple ID>");
        return ExitCode::FAILURE;
    };
    let common_name = cli_info
        .flags
        .get_flag("name")
        .unwrap_or_else(|| email.clone());
    let country = cli_info
        .flags
        .get_flag("country")
        .unwrap_or_else(|| "US".to_owned());

    let dir = signing::platform_dir(root, "apple");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        reporter.error(format!("could not create {}: {e}", dir.display()));
        return ExitCode::FAILURE;
    }
    let key_path = dir.join(APPLE_KEY_FILE);
    let csr_path = dir.join(APPLE_CSR_FILE);

    if key_path.exists() {
        // The key is the half Apple never sees; regenerating it silently would
        // orphan any certificate already issued against the old request.
        reporter.error(format!("{} already exists", key_path.display()));
        reporter.help("delete it to start over, but any certificate issued from the old request will no longer pair");
        return ExitCode::FAILURE;
    }

    reporter.status("Generating", "a 2048-bit RSA key");
    let mut rng = rand::thread_rng();
    let key = match rsa::RsaPrivateKey::new(&mut rng, 2048) {
        Ok(key) => key,
        Err(e) => {
            reporter.error(format!("could not generate a key: {e}"));
            return ExitCode::FAILURE;
        }
    };

    reporter.status("Generating", "the certificate signing request");
    let csr_pem = match build_csr(&key, &common_name, &email, &country) {
        Ok(pem) => pem,
        Err(e) => {
            reporter.error(format!("could not build the signing request: {e}"));
            return ExitCode::FAILURE;
        }
    };

    let key_pem = match key.to_pkcs8_pem(LineEnding::LF) {
        Ok(pem) => pem,
        Err(e) => {
            reporter.error(format!("could not encode the key: {e}"));
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = std::fs::write(&key_path, key_pem.as_bytes()) {
        reporter.error(format!("could not write {}: {e}", key_path.display()));
        return ExitCode::FAILURE;
    }
    restrict(&key_path);
    if let Err(e) = std::fs::write(&csr_path, csr_pem.as_bytes()) {
        reporter.error(format!("could not write {}: {e}", csr_path.display()));
        return ExitCode::FAILURE;
    }

    reporter.success(format!("wrote {}", csr_path.display()));
    reporter.info(format!(
        "the private key stays here: {}",
        key_path.display()
    ));
    reporter.info("next: upload the request at developer.apple.com > Certificates,");
    reporter.info("download the .cer it issues, then run:");
    reporter.info("  peko keys p12 --platform <ios|macos> --cer <downloaded.cer>");
    ExitCode::SUCCESS
}

/// Build a PKCS#10 certificate signing request, PEM-encoded.
fn build_csr(
    key: &rsa::RsaPrivateKey,
    common_name: &str,
    email: &str,
    country: &str,
) -> Result<String, String> {
    use der::{Encode, asn1::SetOfVec};
    use rsa::pkcs1v15::SigningKey;
    use rsa::signature::{SignatureEncoding, Signer};
    use sha2::Sha256;
    use x509_cert::attr::AttributeTypeAndValue;
    use x509_cert::der::asn1::{Ia5String, PrintableString, Utf8StringRef};
    use x509_cert::name::{RdnSequence, RelativeDistinguishedName};
    use x509_cert::request::{CertReq, CertReqInfo};
    use x509_cert::spki::SubjectPublicKeyInfoOwned;

    // Apple matches the request to the account by the email address in the
    // subject, so all three components are required rather than decorative.
    let rdn = |oid: der::asn1::ObjectIdentifier,
               value: der::Any|
     -> Result<RelativeDistinguishedName, String> {
        let atv = AttributeTypeAndValue { oid, value };
        let mut set = SetOfVec::new();
        set.insert(atv).map_err(|e| e.to_string())?;
        Ok(RelativeDistinguishedName::from(set))
    };

    const OID_CN: &str = "2.5.4.3";
    const OID_EMAIL: &str = "1.2.840.113549.1.9.1";
    const OID_C: &str = "2.5.4.6";

    let cn_value = Utf8StringRef::new(common_name).map_err(|e| e.to_string())?;
    let email_value = Ia5String::new(email).map_err(|e| e.to_string())?;
    let country_value = PrintableString::new(country).map_err(|e| e.to_string())?;

    // Apple matches a request to the account by the email address, so all three
    // components are required rather than decorative.
    let rdns = vec![
        rdn(
            OID_CN.parse().map_err(|_| "bad CN oid".to_string())?,
            der::Any::from(cn_value),
        )?,
        rdn(
            OID_EMAIL.parse().map_err(|_| "bad email oid".to_string())?,
            der::Any::from(&email_value),
        )?,
        rdn(
            OID_C.parse().map_err(|_| "bad country oid".to_string())?,
            der::Any::from(&country_value),
        )?,
    ];

    let public_key = {
        use rsa::pkcs8::EncodePublicKey;
        let der_bytes = rsa::RsaPublicKey::from(key)
            .to_public_key_der()
            .map_err(|e| e.to_string())?;
        SubjectPublicKeyInfoOwned::try_from(der_bytes.as_bytes()).map_err(|e| e.to_string())?
    };

    let info = CertReqInfo {
        version: x509_cert::request::Version::V1,
        subject: RdnSequence::from(rdns),
        public_key,
        attributes: Default::default(),
    };

    let signed_der = info.to_der().map_err(|e| e.to_string())?;
    let signing_key = SigningKey::<Sha256>::new(key.clone());
    let signature = signing_key.sign(&signed_der);

    let request = CertReq {
        info,
        algorithm: x509_cert::spki::AlgorithmIdentifierOwned {
            // sha256WithRSAEncryption
            oid: "1.2.840.113549.1.1.11"
                .parse()
                .map_err(|_| "bad signature oid".to_string())?,
            parameters: Some(der::Any::null()),
        },
        signature: der::asn1::BitString::from_bytes(&signature.to_bytes())
            .map_err(|e| e.to_string())?,
    };

    let der_bytes = request.to_der().map_err(|e| e.to_string())?;
    Ok(pem_block("CERTIFICATE REQUEST", &der_bytes))
}

/// Wrap DER bytes in a PEM block with 64-character lines.
fn pem_block(label: &str, der_bytes: &[u8]) -> String {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(der_bytes);
    let mut out = format!("-----BEGIN {label}-----\n");
    for chunk in encoded.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

/// Record a generated file in the project's key registry, exactly as
/// `peko keys add` would, so the bundler finds it without a second command.
fn register(reporter: &Reporter, root: &Path, platform: &str, role: &str, path: &Path) -> bool {
    let mut registry = match signing::load_registry(root) {
        Ok(registry) => registry,
        Err(e) => {
            reporter.error(format!("could not read the key registry: {e}"));
            return false;
        }
    };
    if !super::keys::install_file(reporter, root, platform, role, path, &mut registry) {
        return false;
    }
    if let Err(e) = signing::save_registry(root, &registry) {
        reporter.error(format!("could not write the key registry: {e}"));
        return false;
    }
    true
}

/// Tighten permissions on a private key so it is not world-readable.
fn restrict(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Read the password for new key material. Flags only: a prompt here would
/// block the IDE, which drives this command as a subprocess.
fn read_new_password(cli_info: &CLIInfo, reporter: &Reporter) -> Option<String> {
    match super::keys::read_named_password(cli_info, "") {
        Some(password) if !password.is_empty() => Some(password),
        Some(_) => {
            reporter.error("the password is empty");
            None
        }
        None => {
            reporter.error("a password is required");
            reporter.help("pass --password-file <path> (preferred) or --password <value>");
            None
        }
    }
}

/// `peko keys p12`: pair the certificate Apple issued with the key kept from
/// the signing request, producing the PKCS#12 the bundler signs with.
///
/// Apple returns a bare `.cer` — the certificate only. On a Mac, Keychain
/// Access pairs it with the private key it generated and exports a `.p12`. This
/// does the same pairing without a Mac, using the key written by
/// `keys generate --platform apple`.
pub fn assemble_p12(
    cli_info: &CLIInfo,
    reporter: &Reporter,
    root: &Path,
    bundle_id: &str,
) -> ExitCode {
    let Some(platform) = cli_info.flags.get_flag("platform") else {
        reporter.error("`keys p12` needs --platform <ios|macos>");
        return ExitCode::FAILURE;
    };
    if platform != "ios" && platform != "macos" {
        reporter.error("`keys p12` applies to ios and macos");
        return ExitCode::FAILURE;
    }
    let Some(cer) = cli_info.flags.get_flag("cer") else {
        reporter.error("`keys p12` needs --cer <certificate downloaded from Apple>");
        return ExitCode::FAILURE;
    };
    // The installer role takes a separate certificate, so it needs its own p12.
    let role = if cli_info.flags.has_flag("installer") {
        "installer-p12"
    } else {
        "p12"
    };

    let key_path = signing::platform_dir(root, "apple").join(APPLE_KEY_FILE);
    if !key_path.exists() {
        reporter.error("no signing key was found for this project");
        reporter.help("run `peko keys generate --platform apple` first; the certificate must pair with the key that produced the request");
        return ExitCode::FAILURE;
    }
    let Some(password) = read_new_password(cli_info, reporter) else {
        return ExitCode::FAILURE;
    };

    let cert_der = match read_certificate(Path::new(&cer)) {
        Ok(der) => der,
        Err(e) => {
            reporter.error(format!("could not read {cer}: {e}"));
            return ExitCode::FAILURE;
        }
    };
    let key_der = match read_private_key(&key_path) {
        Ok(der) => der,
        Err(e) => {
            reporter.error(format!("could not read the signing key: {e}"));
            return ExitCode::FAILURE;
        }
    };

    // A certificate that was not issued for this key produces a p12 that fails
    // only at signing time, so the pairing is checked here.
    if let Err(e) = check_pair(&cert_der, &key_der) {
        reporter.error(e);
        reporter.help("this certificate was issued for a different signing request");
        return ExitCode::FAILURE;
    }

    let name = format!("{bundle_id} {platform}");
    let Some(pfx) = p12::PFX::new(&cert_der, &key_der, None, &password, &name) else {
        reporter.error("could not build the PKCS#12 container");
        return ExitCode::FAILURE;
    };

    let dir = signing::platform_dir(root, &platform);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        reporter.error(format!("could not create {}: {e}", dir.display()));
        return ExitCode::FAILURE;
    }
    let file_name = if role == "installer-p12" {
        "installer.p12"
    } else {
        "signing.p12"
    };
    let out = cli_info
        .flags
        .get_flag("out")
        .map(PathBuf::from)
        .unwrap_or_else(|| dir.join(file_name));
    if let Err(e) = std::fs::write(&out, pfx.to_der()) {
        reporter.error(format!("could not write {}: {e}", out.display()));
        return ExitCode::FAILURE;
    }
    restrict(&out);

    if !register(reporter, root, &platform, role, &out) {
        return ExitCode::FAILURE;
    }
    let mut secrets = signing::SigningSecrets::load(bundle_id);
    secrets.set(&platform, role, &password);
    if let Err(e) = secrets.store(bundle_id) {
        reporter.error(format!("could not store the certificate password: {e}"));
        return ExitCode::FAILURE;
    }

    reporter.success(format!("wrote and registered {}", out.display()));
    reporter.info("run `peko keys verify` to confirm it signs");
    ExitCode::SUCCESS
}

/// Read a certificate as DER, accepting either the DER Apple serves or a PEM
/// wrapper, since browsers and mail clients hand back either.
fn read_certificate(path: &Path) -> Result<Vec<u8>, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    if bytes.starts_with(b"-----BEGIN") {
        let text = String::from_utf8_lossy(&bytes);
        return decode_pem(&text, "CERTIFICATE");
    }
    Ok(bytes)
}

/// Read the stored PKCS#8 private key as DER.
fn read_private_key(path: &Path) -> Result<Vec<u8>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    decode_pem(&text, "PRIVATE KEY")
}

/// Pull the base64 body out of a PEM block and decode it.
fn decode_pem(text: &str, label: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let start = text
        .find(&begin)
        .ok_or_else(|| format!("no {label} block found"))?
        + begin.len();
    let stop = text
        .find(&end)
        .ok_or_else(|| format!("unterminated {label} block"))?;
    let body: String = text[start..stop]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    base64::engine::general_purpose::STANDARD
        .decode(body)
        .map_err(|e| format!("malformed base64 in the {label} block: {e}"))
}

/// Confirm a certificate's public key is the one belonging to the private key.
///
/// Without this, a mismatched pair yields a well-formed `.p12` that only fails
/// when a build tries to sign with it — far from the mistake that caused it.
fn check_pair(cert_der: &[u8], key_der: &[u8]) -> Result<(), String> {
    use rsa::pkcs8::DecodePrivateKey;
    use x509_cert::der::Decode;

    let certificate = x509_cert::Certificate::from_der(cert_der)
        .map_err(|e| format!("not a certificate: {e}"))?;
    let key = rsa::RsaPrivateKey::from_pkcs8_der(key_der)
        .map_err(|e| format!("not a usable private key: {e}"))?;

    let cert_spki = certificate
        .tbs_certificate
        .subject_public_key_info
        .subject_public_key
        .raw_bytes();
    let expected = {
        use rsa::pkcs1::EncodeRsaPublicKey;
        rsa::RsaPublicKey::from(&key)
            .to_pkcs1_der()
            .map_err(|e| e.to_string())?
    };
    if cert_spki != expected.as_bytes() {
        return Err("the certificate does not match the stored signing key".to_string());
    }
    Ok(())
}
