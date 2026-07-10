//! Pure-Rust Authenticode signing of Windows PE executables.
//!
//! A signature is a PKCS#7 SignedData over an Authenticode
//! `SpcIndirectDataContent`, embedded into the PE certificate table. The
//! authenticode crate computes the byte-exact PE digest and provides the
//! `Spc*` ASN.1 types; the cms crate builds the SignedData; the RSA key and
//! certificate come from the PKCS#12 material. The system `osslsigncode`
//! remains a fallback for environments this path cannot handle.
//!
//! The output must be validated on Windows (for example `signtool verify /pa`)
//! before it is relied on for distribution.

use std::path::Path;

use authenticode::{
    DigestInfo, SpcAttributeTypeAndOptionalValue, SpcIndirectDataContent, WIN_CERT_REVISION_2_0,
    WIN_CERT_TYPE_PKCS_SIGNED_DATA, authenticode_digest,
};
use cms::builder::{SignedDataBuilder, SignerInfoBuilder};
use cms::signed_data::{EncapsulatedContentInfo, SignerIdentifier};
use der::Encode;
use der::asn1::{Any, ObjectIdentifier, OctetString};
use object::read::pe::PeFile64;
use rsa::RsaPrivateKey;
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::DecodePrivateKey;
use sha2::{Digest, Sha256};
use spki::AlgorithmIdentifierOwned;
use x509_cert::Certificate;
use x509_cert::der::Decode;

use crate::bundler::BundleResult;
use crate::bundler::signing::WindowsSigningKey;

/// OID `1.3.6.1.4.1.311.2.1.15`, SPC_PE_IMAGE_DATA.
const SPC_PE_IMAGE_DATA_OBJID: ObjectIdentifier =
    ObjectIdentifier::new_unwrap("1.3.6.1.4.1.311.2.1.15");

/// OID `1.3.6.1.4.1.311.2.1.4`, SPC_INDIRECT_DATA.
const SPC_INDIRECT_DATA_OBJID: ObjectIdentifier =
    ObjectIdentifier::new_unwrap("1.3.6.1.4.1.311.2.1.4");

/// OID `2.16.840.1.101.3.4.2.1`, SHA-256.
const SHA256_OID: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.16.840.1.101.3.4.2.1");

/// The canonical SpcPeImageData value emitted by signtool and osslsigncode:
/// `SEQUENCE { flags BIT STRING (empty), file [0] { [2] { [0] BMPString
/// "<<<Obsolete>>>" } } }`. The exact bytes are fixed by the Authenticode
/// specification, so they are embedded verbatim.
const SPC_PE_IMAGE_DATA_VALUE: &[u8] = &[
    0x30, 0x24, 0x03, 0x01, 0x00, 0xa0, 0x1f, 0xa2, 0x1d, 0x80, 0x1b, 0x00, 0x3c, 0x00, 0x3c, 0x00,
    0x3c, 0x00, 0x4f, 0x00, 0x62, 0x00, 0x73, 0x00, 0x6f, 0x00, 0x6c, 0x00, 0x65, 0x00, 0x74, 0x00,
    0x65, 0x00, 0x3e, 0x00, 0x3e, 0x00, 0x3e,
];

/// The SHA-256 algorithm identifier with absent parameters.
fn sha256_algid() -> AlgorithmIdentifierOwned {
    AlgorithmIdentifierOwned {
        oid: SHA256_OID,
        parameters: None,
    }
}

/// Sign a Windows PE executable in place with pure-Rust Authenticode.
///
/// Reads the PKCS#12 material for the certificate and RSA key, computes the
/// Authenticode digest, builds the SignedData, and embeds it into the PE
/// certificate table.
pub fn sign_pe(exe: &Path, key: &WindowsSigningKey) -> BundleResult<()> {
    let bytes = read(exe)?;
    let pfx_bytes = read(&key.pfx)?;

    let (cert, signing_key) = load_pkcs12(&pfx_bytes, &key.password)?;

    // The Authenticode digest over the PE, excluding the checksum field, the
    // certificate table data directory entry, and the certificate table.
    let pe = PeFile64::parse(bytes.as_slice()).map_err(|e| err(format!("parse PE: {e}")))?;
    let mut hasher = Sha256::new();
    authenticode_digest(&pe, &mut hasher).map_err(|_| err("compute the Authenticode digest"))?;
    let digest = hasher.finalize();

    // SpcIndirectDataContent: the image reference plus the PE digest.
    let indirect = SpcIndirectDataContent {
        data: SpcAttributeTypeAndOptionalValue {
            value_type: SPC_PE_IMAGE_DATA_OBJID,
            value: Any::from_der(SPC_PE_IMAGE_DATA_VALUE)
                .map_err(|e| err(format!("encode SpcPeImageData: {e}")))?,
        },
        message_digest: DigestInfo {
            digest_algorithm: sha256_algid(),
            digest: OctetString::new(digest.as_slice())
                .map_err(|e| err(format!("wrap digest: {e}")))?,
        },
    };
    let econtent = indirect
        .to_der()
        .map_err(|e| err(format!("encode SpcIndirectDataContent: {e}")))?;

    // The SignedData encapsulates the SpcIndirectDataContent under the
    // Authenticode content type.
    // The eContent is the SpcIndirectDataContent itself (an ASN.1 SEQUENCE),
    // reparsed into an Any so it is carried verbatim rather than rewrapped.
    let encap = EncapsulatedContentInfo {
        econtent_type: SPC_INDIRECT_DATA_OBJID,
        econtent: Some(Any::from_der(&econtent).map_err(|e| err(format!("wrap eContent: {e}")))?),
    };

    let sid = SignerIdentifier::IssuerAndSerialNumber(cms::cert::IssuerAndSerialNumber {
        issuer: cert.tbs_certificate.issuer.clone(),
        serial_number: cert.tbs_certificate.serial_number.clone(),
    });

    let signer = SigningKey::<Sha256>::new(signing_key);

    let signer_info = SignerInfoBuilder::new(&signer, sid, sha256_algid(), &encap, None)
        .map_err(|e| err(format!("signer info: {e:?}")))?;

    let signed = SignedDataBuilder::new(&encap)
        .add_digest_algorithm(sha256_algid())
        .map_err(|e| err(format!("digest algorithm: {e:?}")))?
        .add_certificate(cms::cert::CertificateChoices::Certificate(cert))
        .map_err(|e| err(format!("add certificate: {e:?}")))?
        .add_signer_info::<SigningKey<Sha256>, rsa::pkcs1v15::Signature>(signer_info)
        .map_err(|e| err(format!("add signer: {e:?}")))?
        .build()
        .map_err(|e| err(format!("build SignedData: {e:?}")))?;

    let pkcs7 = signed
        .to_der()
        .map_err(|e| err(format!("encode SignedData: {e}")))?;

    let signed_bytes = embed_signature(bytes, &pkcs7)?;
    write(exe, &signed_bytes)
}

/// Parse a PKCS#12 blob into its X.509 certificate and RSA private key.
fn load_pkcs12(pfx: &[u8], password: &str) -> BundleResult<(Certificate, RsaPrivateKey)> {
    let store = p12::PFX::parse(pfx).map_err(|e| err(format!("parse PKCS#12: {e}")))?;

    let cert_der = store
        .cert_bags(password)
        .map_err(|e| err(format!("read PKCS#12 certificates: {e}")))?
        .into_iter()
        .next()
        .ok_or_else(|| err("the PKCS#12 holds no certificate"))?;
    let cert =
        Certificate::from_der(&cert_der).map_err(|e| err(format!("parse certificate: {e}")))?;

    let key_der = store
        .key_bags(password)
        .map_err(|e| err(format!("read PKCS#12 key: {e}")))?
        .into_iter()
        .next()
        .ok_or_else(|| err("the PKCS#12 holds no private key"))?;
    let signing_key = RsaPrivateKey::from_pkcs8_der(&key_der)
        .map_err(|e| err(format!("parse RSA private key: {e}")))?;

    Ok((cert, signing_key))
}

/// Append a `WIN_CERTIFICATE` wrapping `pkcs7` to `bytes` and point the PE
/// certificate-table data directory at it, then refresh the header checksum.
fn embed_signature(mut bytes: Vec<u8>, pkcs7: &[u8]) -> BundleResult<Vec<u8>> {
    let pe = pe_layout(&bytes)?;

    // A signed image must not already carry a certificate table.
    // The WIN_CERTIFICATE header is 8 bytes: dwLength (u32), wRevision (u16),
    // wCertificateType (u16), followed by the certificate bytes.
    let cert_len = 8 + pkcs7.len();
    let mut win_cert = Vec::with_capacity(cert_len);
    win_cert.extend_from_slice(&(cert_len as u32).to_le_bytes());
    win_cert.extend_from_slice(&WIN_CERT_REVISION_2_0.to_le_bytes());
    win_cert.extend_from_slice(&WIN_CERT_TYPE_PKCS_SIGNED_DATA.to_le_bytes());
    win_cert.extend_from_slice(pkcs7);

    // The certificate table starts on an 8-byte boundary from the file start.
    while !bytes.len().is_multiple_of(8) {
        bytes.push(0);
    }
    let cert_offset = bytes.len();
    bytes.extend_from_slice(&win_cert);
    // The table itself is padded to an 8-byte multiple.
    while !bytes.len().is_multiple_of(8) {
        bytes.push(0);
    }
    let cert_dir_size = (bytes.len() - cert_offset) as u32;

    // IMAGE_DIRECTORY_ENTRY_SECURITY is entry index 4. Its VirtualAddress field
    // holds a file offset (not an RVA) for the certificate table.
    let dir_entry = pe.security_dir_offset;
    bytes[dir_entry..dir_entry + 4].copy_from_slice(&(cert_offset as u32).to_le_bytes());
    bytes[dir_entry + 4..dir_entry + 8].copy_from_slice(&cert_dir_size.to_le_bytes());

    // Zero the old checksum and recompute it over the whole image.
    bytes[pe.checksum_offset..pe.checksum_offset + 4].copy_from_slice(&[0, 0, 0, 0]);
    let checksum = pe_checksum(&bytes, pe.checksum_offset);
    bytes[pe.checksum_offset..pe.checksum_offset + 4].copy_from_slice(&checksum.to_le_bytes());

    Ok(bytes)
}

/// The header offsets a signature update needs.
struct PeLayout {
    checksum_offset: usize,
    security_dir_offset: usize,
}

/// Locate the optional-header checksum field and the security data-directory
/// entry for a PE32+ image.
fn pe_layout(bytes: &[u8]) -> BundleResult<PeLayout> {
    if bytes.len() < 0x40 || &bytes[0..2] != b"MZ" {
        return Err(err("not a PE image"));
    }
    let pe_off = u32::from_le_bytes(bytes[0x3c..0x40].try_into().unwrap()) as usize;
    if bytes.len() < pe_off + 24 || &bytes[pe_off..pe_off + 4] != b"PE\0\0" {
        return Err(err("missing PE signature"));
    }
    let opt_header = pe_off + 24;
    let magic = u16::from_le_bytes(bytes[opt_header..opt_header + 2].try_into().unwrap());
    if magic != 0x20b {
        return Err(err("only PE32+ (64-bit) images are supported"));
    }
    // In the PE32+ optional header, CheckSum is at offset 64, and the data
    // directory begins at offset 112. Each entry is 8 bytes; SECURITY is index
    // 4, so 112 + 4 * 8 = 144.
    let checksum_offset = opt_header + 64;
    let security_dir_offset = opt_header + 144;
    if bytes.len() < security_dir_offset + 8 {
        return Err(err("truncated PE optional header"));
    }
    Ok(PeLayout {
        checksum_offset,
        security_dir_offset,
    })
}

/// Compute the PE image checksum. The 16-bit ones-complement sum skips the
/// 4-byte checksum field, then the file length is added.
fn pe_checksum(bytes: &[u8], checksum_offset: usize) -> u32 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < bytes.len() {
        if i == checksum_offset {
            i += 4;
            continue;
        }
        let word = u16::from_le_bytes([bytes[i], bytes[i + 1]]) as u32;
        sum += word;
        sum = (sum & 0xffff) + (sum >> 16);
        i += 2;
    }
    if i < bytes.len() {
        sum += bytes[i] as u32;
        sum = (sum & 0xffff) + (sum >> 16);
    }
    sum = (sum & 0xffff) + (sum >> 16);
    (sum & 0xffff) + bytes.len() as u32
}

fn read(path: &Path) -> BundleResult<Vec<u8>> {
    std::fs::read(path).map_err(|source| crate::bundler::BundleError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn write(path: &Path, bytes: &[u8]) -> BundleResult<()> {
    std::fs::write(path, bytes).map_err(|source| crate::bundler::BundleError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn err(message: impl Into<String>) -> crate::bundler::BundleError {
    crate::bundler::BundleError::Signing(message.into())
}
