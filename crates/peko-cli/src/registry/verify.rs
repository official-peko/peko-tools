//! Structural verification of a `.pkpkg` container.
//!
//! [`verify`] scans a packed container's bytes and reports what it holds: the
//! container header, the embedded `peko.toml` and its keys, and the packed
//! source tree. It collects every problem it finds rather than stopping at the
//! first, so a single pass surfaces both hard errors (a package that cannot be
//! published) and softer warnings (a missing registry-quality field). The same
//! pass runs from the `verify` command and, before an upload, from `publish`.

use std::io::{Cursor, Read};

use peko_core::config::{
    CONTAINER_HEADER_LEN, CONTAINER_VERSION, Compression, ContainerHeader, Manifest, ManifestKind,
    decode_container,
};

use super::pack;

/// How serious a single finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// A problem that makes the container invalid or unpublishable.
    Error,
    /// A problem that does not block publishing but should be addressed.
    Warning,
}

/// One problem found while verifying a container.
#[derive(Debug, Clone)]
pub struct Finding {
    pub severity: Severity,
    pub message: String,
}

/// The keys and metadata read from a container's embedded `peko.toml`.
#[derive(Debug, Clone)]
pub struct ManifestSummary {
    pub kind: ManifestKind,
    pub name: String,
    pub version: String,
    pub description: Option<String>,
    pub license: Option<String>,
    pub authors: Vec<String>,
    pub repository: Option<String>,
    pub keywords: Vec<String>,
    pub categories: Vec<String>,
    pub min_compiler: Option<String>,
    /// The `[lib].root` entry source path, present only for a package.
    pub lib_root: Option<String>,
    /// Dependency name paired with its version requirement or local path.
    pub dependencies: Vec<(String, String)>,
    pub platforms: Vec<String>,
    pub native_sources: usize,
}

/// A description of the packed source tree inside the payload.
#[derive(Debug, Clone)]
pub struct PayloadSummary {
    /// Total uncompressed size of the packed files.
    pub uncompressed_size: u64,
    /// The packed file paths, relative to the package root.
    pub files: Vec<String>,
    /// Whether the payload contains the entry file named by `[lib].root`.
    pub entry_present: bool,
}

/// The full result of verifying a container.
#[derive(Debug, Clone)]
pub struct PackageReport {
    pub file_size: usize,
    pub checksum: String,
    pub container_version: u16,
    pub compression: Compression,
    pub signed: bool,
    pub meta_len: usize,
    pub payload_len: usize,
    pub signature_len: usize,
    pub manifest: Option<ManifestSummary>,
    pub payload: Option<PayloadSummary>,
    pub findings: Vec<Finding>,
}

impl PackageReport {
    fn error(&mut self, message: impl Into<String>) {
        self.findings.push(Finding {
            severity: Severity::Error,
            message: message.into(),
        });
    }

    fn warning(&mut self, message: impl Into<String>) {
        self.findings.push(Finding {
            severity: Severity::Warning,
            message: message.into(),
        });
    }

    /// The number of error-severity findings.
    pub fn error_count(&self) -> usize {
        self.findings
            .iter()
            .filter(|f| f.severity == Severity::Error)
            .count()
    }

    /// The number of warning-severity findings.
    pub fn warning_count(&self) -> usize {
        self.findings
            .iter()
            .filter(|f| f.severity == Severity::Warning)
            .count()
    }

    /// Whether the container is valid: it parsed and has no error findings.
    pub fn is_valid(&self) -> bool {
        self.error_count() == 0
    }
}

/// Verify a `.pkpkg` container's bytes, returning a report of its contents and
/// every problem found.
pub fn verify(bytes: &[u8]) -> PackageReport {
    let mut report = PackageReport {
        file_size: bytes.len(),
        checksum: pack::checksum(bytes),
        container_version: 0,
        compression: Compression::None,
        signed: false,
        meta_len: 0,
        payload_len: 0,
        signature_len: 0,
        manifest: None,
        payload: None,
        findings: Vec::new(),
    };

    // Header: the fixed 32-byte frame. Without it nothing else can be trusted.
    let header = match ContainerHeader::decode(bytes) {
        Ok(header) => header,
        Err(error) => {
            report.error(format!("container header is invalid: {error}"));
            return report;
        }
    };
    report.container_version = header.container_version;
    report.compression = header.compression;
    report.signed = header.signed;
    report.meta_len = header.meta_len as usize;
    report.payload_len = header.payload_len as usize;

    if header.container_version != CONTAINER_VERSION {
        report.warning(format!(
            "container format version is {}, but this tool understands version {CONTAINER_VERSION}; \
             the report may be incomplete",
            header.container_version
        ));
    }

    // Body: the manifest, payload, and optional signature framed after the header.
    let container = match decode_container(bytes) {
        Ok(container) => container,
        Err(error) => {
            report.error(format!("container body is invalid: {error}"));
            return report;
        }
    };
    report.signature_len = container.signature.map(<[u8]>::len).unwrap_or(0);

    // Signature-flag consistency.
    let body_end = CONTAINER_HEADER_LEN + report.meta_len + report.payload_len;
    if header.signed && report.signature_len == 0 {
        report.error("the header marks the container signed, but no signature trailer follows the payload");
    } else if !header.signed && bytes.len() > body_end {
        report.warning(format!(
            "{} trailing byte(s) follow the payload in an unsigned container",
            bytes.len() - body_end
        ));
    }

    // Manifest: the embedded peko.toml and its keys.
    let lib_root = verify_manifest(container.manifest, &mut report);

    // Payload: the compressed source tree.
    verify_payload(&header, container.manifest, container.payload, lib_root, &mut report);

    report
}

/// Parse and inspect the embedded manifest, recording findings and returning the
/// package entry path (`[lib].root`) when the manifest is a package.
fn verify_manifest(manifest_text: &str, report: &mut PackageReport) -> Option<String> {
    let manifest = match Manifest::parse(manifest_text, std::path::Path::new("peko.toml")) {
        Ok(manifest) => manifest,
        Err(error) => {
            report.error(format!("embedded peko.toml is invalid: {error}"));
            return None;
        }
    };

    let kind = manifest.kind();
    if kind != ManifestKind::Package {
        report.error(
            "the container holds an application manifest, not a package; only [package] manifests are publishable",
        );
    }

    let dependencies = manifest
        .dependencies()
        .iter()
        .map(|(name, dependency)| (name.clone(), describe_dependency(dependency)))
        .collect();

    let (lib_root, summary) = match &manifest {
        Manifest::Package(package) => {
            let meta = &package.package;

            // Registry-quality fields: not required to be valid, but recommended
            // so the package presents well and can be trusted.
            if meta.description.as_deref().unwrap_or("").trim().is_empty() {
                report.warning("[package].description is empty; add a short summary for the registry");
            }
            if meta.license.is_none() {
                report.warning("[package].license is missing; add an SPDX license identifier");
            }
            if meta.authors.is_empty() {
                report.warning("[package].authors is empty");
            }
            if meta.repository.is_none() {
                report.warning("[package].repository is missing; add the source repository url");
            }

            let lib_root = package.lib.root.to_string_lossy().replace('\\', "/");
            let summary = ManifestSummary {
                kind,
                name: meta.name.clone(),
                version: meta.version.to_string(),
                description: meta.description.clone(),
                license: meta.license.clone(),
                authors: meta.authors.clone(),
                repository: meta.repository.clone(),
                keywords: meta.keywords.clone(),
                categories: meta.categories.clone(),
                min_compiler: meta.peko.as_ref().map(ToString::to_string),
                lib_root: Some(lib_root.clone()),
                dependencies,
                platforms: package.platforms.supported.iter().map(|os| os.name().to_owned()).collect(),
                native_sources: package.native.as_ref().map(|n| n.sources.len()).unwrap_or(0),
            };
            (Some(lib_root), summary)
        }
        Manifest::Application(app) => {
            let summary = ManifestSummary {
                kind,
                name: app.project.name.clone(),
                version: app.project.version.to_string(),
                description: None,
                license: None,
                authors: Vec::new(),
                repository: None,
                keywords: Vec::new(),
                categories: Vec::new(),
                min_compiler: None,
                lib_root: None,
                dependencies,
                platforms: app.platforms.supported.iter().map(|os| os.name().to_owned()).collect(),
                native_sources: app.native.as_ref().map(|n| n.sources.len()).unwrap_or(0),
            };
            (None, summary)
        }
    };

    report.manifest = Some(summary);
    lib_root
}

/// A one-line description of a dependency's source.
fn describe_dependency(dependency: &peko_core::config::Dependency) -> String {
    match dependency {
        peko_core::config::Dependency::Registry { version } => version.to_string(),
        peko_core::config::Dependency::Path { path } => {
            format!("path: {}", path.display())
        }
    }
}

/// Decompress and walk the packed source tree, recording findings and filling in
/// the payload summary.
fn verify_payload(
    header: &ContainerHeader,
    manifest_text: &str,
    payload: &[u8],
    lib_root: Option<String>,
    report: &mut PackageReport,
) {
    // Undo the container compression to recover the tar.
    let tar_bytes = match header.compression {
        Compression::None => payload.to_vec(),
        Compression::Zstd => match zstd::decode_all(Cursor::new(payload)) {
            Ok(bytes) => bytes,
            Err(error) => {
                report.error(format!("payload could not be decompressed: {error}"));
                return;
            }
        },
    };

    let mut archive = tar::Archive::new(Cursor::new(&tar_bytes));
    let entries = match archive.entries() {
        Ok(entries) => entries,
        Err(error) => {
            report.error(format!("payload is not a readable archive: {error}"));
            return;
        }
    };

    let mut files = Vec::new();
    let mut uncompressed_size = 0u64;
    let mut embedded_toml_in_payload: Option<Vec<u8>> = None;
    let mut source_files = 0usize;

    for entry in entries {
        let mut entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                report.error(format!("corrupt archive entry: {error}"));
                return;
            }
        };
        let path = match entry.path() {
            Ok(path) => path.to_string_lossy().replace('\\', "/"),
            Err(error) => {
                report.error(format!("archive entry has an unreadable path: {error}"));
                continue;
            }
        };
        uncompressed_size += entry.size();

        // The build cache and generated container must never be packed.
        if path.starts_with(".peko/") || path.starts_with("target/") {
            report.warning(format!("build artifact leaked into the package: {path}"));
        }
        if path.ends_with(".pkpkg") {
            report.warning(format!("a packed container leaked into the package: {path}"));
        }
        if path.ends_with(".peko") {
            source_files += 1;
        }
        if path == "peko.toml" {
            let mut contents = Vec::new();
            if entry.read_to_end(&mut contents).is_ok() {
                embedded_toml_in_payload = Some(contents);
            }
        }

        files.push(path);
    }

    // The manifest is embedded in the header and packed in the tree; they must
    // agree, or the header and source describe different packages.
    match embedded_toml_in_payload {
        Some(bytes) if bytes != manifest_text.as_bytes() => {
            report.error("the embedded manifest does not match the peko.toml packed in the payload");
        }
        None => report.warning("the payload does not contain a peko.toml at its root"),
        _ => {}
    }

    if files.is_empty() {
        report.error("the payload contains no files");
    }
    if source_files == 0 {
        report.warning("the payload contains no .peko source files");
    }

    // The declared entry file must actually be packed, or the package cannot be
    // compiled by whoever installs it.
    let entry_present = match &lib_root {
        Some(root) => {
            let present = files.iter().any(|path| path == root);
            if !present {
                report.error(format!(
                    "the [lib].root entry file '{root}' is not present in the payload"
                ));
            }
            present
        }
        None => false,
    };

    files.sort();
    report.payload = Some(PayloadSummary {
        uncompressed_size,
        files,
        entry_present,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use peko_core::config::{Compression, encode_container};

    /// A minimal valid package: a peko.toml plus its lib entry, packed the same
    /// way `pack` does (embedded manifest, `zstd(tar(source))` payload).
    fn build_package(manifest: &str, entry_path: &str) -> Vec<u8> {
        let mut tar_buf = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_buf);
            append(&mut builder, "peko.toml", manifest.as_bytes());
            append(&mut builder, entry_path, b"[public] fn hello() {}\n");
            builder.finish().unwrap();
        }
        let payload = zstd::encode_all(Cursor::new(tar_buf), 19).unwrap();
        encode_container(manifest, Compression::Zstd, &payload, None)
    }

    fn append<W: std::io::Write>(builder: &mut tar::Builder<W>, name: &str, bytes: &[u8]) {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, name, bytes).unwrap();
    }

    const GOOD_MANIFEST: &str = "[package]\n\
        name = \"widget\"\n\
        version = \"1.2.3\"\n\
        description = \"A widget\"\n\
        license = \"MIT\"\n\
        authors = [\"Ada\"]\n\
        repository = \"https://example.com/widget\"\n\
        \n\
        [lib]\n\
        root = \"source/lib.peko\"\n";

    #[test]
    fn accepts_a_well_formed_package() {
        let bytes = build_package(GOOD_MANIFEST, "source/lib.peko");
        let report = verify(&bytes);
        assert!(report.is_valid(), "expected valid, findings: {:?}", report.findings);
        let manifest = report.manifest.expect("manifest summary");
        assert_eq!(manifest.name, "widget");
        assert_eq!(manifest.version, "1.2.3");
        assert_eq!(manifest.kind, ManifestKind::Package);
        assert!(report.payload.unwrap().entry_present);
    }

    #[test]
    fn rejects_bad_magic() {
        let report = verify(b"not a peko container at all");
        assert!(!report.is_valid());
        assert!(report.findings.iter().any(|f| f.message.contains("header is invalid")));
    }

    #[test]
    fn rejects_truncated_container() {
        let bytes = build_package(GOOD_MANIFEST, "source/lib.peko");
        let report = verify(&bytes[..bytes.len() - 100]);
        assert!(!report.is_valid());
    }

    #[test]
    fn errors_when_entry_file_is_missing() {
        // The manifest names source/lib.peko, but the payload packs a different file.
        let bytes = build_package(GOOD_MANIFEST, "source/other.peko");
        let report = verify(&bytes);
        assert!(!report.is_valid());
        assert!(
            report.findings.iter().any(|f| f.message.contains("entry file")),
            "findings: {:?}",
            report.findings
        );
    }

    #[test]
    fn warns_on_missing_registry_fields() {
        let sparse = "[package]\nname = \"bare\"\nversion = \"0.1.0\"\n\n[lib]\nroot = \"source/lib.peko\"\n";
        let bytes = build_package(sparse, "source/lib.peko");
        let report = verify(&bytes);
        assert!(report.is_valid(), "still valid: {:?}", report.findings);
        assert!(report.warning_count() >= 3, "expected quality warnings: {:?}", report.findings);
    }

    #[test]
    fn errors_when_manifest_mismatches_payload() {
        // Embed one manifest but pack a different peko.toml in the tree.
        let mut tar_buf = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_buf);
            append(&mut builder, "peko.toml", b"[package]\nname = \"other\"\nversion = \"9.9.9\"\n\n[lib]\nroot = \"source/lib.peko\"\n");
            append(&mut builder, "source/lib.peko", b"\n");
            builder.finish().unwrap();
        }
        let payload = zstd::encode_all(Cursor::new(tar_buf), 19).unwrap();
        let bytes = encode_container(GOOD_MANIFEST, Compression::Zstd, &payload, None);

        let report = verify(&bytes);
        assert!(!report.is_valid());
        assert!(report.findings.iter().any(|f| f.message.contains("does not match")));
    }
}
