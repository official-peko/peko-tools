//! Builders that pack a [`HostPackage`] or [`HostPackageVersion`] into its
//! on-disk distribution format.
//!
//! A `HostPackage` becomes a `.pkpkg` binary, a custom container holding
//! the package metadata (`PACKAGEINFO` / `PACKAGEREADME` / `PACKAGELICENSE`)
//! plus one zip-encoded payload per version (`PACKAGEVERSIONS`). A
//! `HostPackageVersion` on its own becomes a `.zip` containing every
//! `.peko` / `.o` / `.a` / `.lib` file under its version directory.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Seek, Write};

use peko_core::packages::{HostPackage, HostPackageVersion};
use tempfile::NamedTempFile;
use zip::write::{ExtendedFileOptions, FileOptions};
use zip::{CompressionMethod, ZipWriter};

use super::ziputil;

/// Extract a zip archive's contents into a directory, stripping the
/// common-root folder if every entry shares one.
///
/// **Note**: this macro will be removed once the bundler pass migrates its
/// single live call site (`bundler/android.rs`) to direct `ZipArchive`
/// usage. It's kept here for the duration of the cli refactor so the
/// intermediate state still builds.
#[macro_export]
macro_rules! extract_zip {
    ($zip_archive_content:expr, $output_path:expr) => {
        zip::ZipArchive::new($zip_archive_content)
            .unwrap()
            .extract_unwrapped_root_dir($output_path, zip::read::root_dir_common_filter)
            .unwrap()
    };
}

/// Builds a binary representation of a package component into a temp file
/// that is automatically deleted when the returned handle is dropped.
///
/// Callers should `seek(SeekFrom::Start(0))` before reading, the returned
/// handle's file position is not guaranteed to be at the start of the
/// file.
pub trait PackageComponentBinaryBuilder {
    /// Build the binary. `version` selects a single version to include
    /// when supported by the implementation; pass `None` to include all
    /// versions.
    fn build_binary(&self, version: Option<String>) -> io::Result<NamedTempFile>;
}

// ---------------------------------------------------------------------------
// HostPackageVersion to zip
// ---------------------------------------------------------------------------

impl PackageComponentBinaryBuilder for HostPackageVersion {
    fn build_binary(&self, _version: Option<String>) -> io::Result<NamedTempFile> {
        let folder_name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| io::Error::other("HostPackageVersion path has no valid file name"))?;

        // Use a known suffix so on-disk inspection (debugging, system
        // probes) is friendlier, and so the file can be reopened by tools
        // expecting a `.zip` extension if the consumer never imports it.
        let mut tmp_zip = tempfile::Builder::new()
            .prefix(folder_name)
            .suffix(".zip")
            .tempfile()?;

        // Open a fresh File handle for the zip writer so its file pointer
        // is independent of the NamedTempFile's. The temp file itself is
        // kept alive by `tmp_zip` and deleted when it's dropped.
        let mut zip = ZipWriter::new(tmp_zip.reopen()?);

        let file_options: FileOptions<ExtendedFileOptions> = FileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .unix_permissions(0o644);

        // Only `.o`, `.a`, `.lib`, and `.peko` files (plus a couple of
        // top-level metadata files) survive into the package binary.
        let allowed_extensions: &[&str] = &["o", "a", "lib", "peko"];

        for entry in fs::read_dir(&self.path)? {
            let entry = entry?;
            let path = entry.path();
            let entry_name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .ok_or_else(|| io::Error::other("directory entry has no name"))?;

            if path.is_file()
                && (matches!(entry_name.as_str(), "deps.json" | "README.md")
                    || path.extension().and_then(|e| e.to_str()) == Some("peko"))
            {
                // Top-level metadata files land directly in the zip root.
                zip.start_file(entry_name.as_str(), file_options.clone())
                    .map_err(io::Error::other)?;
                let mut file = File::open(&path)?;
                io::copy(&mut file, &mut zip)?;
            } else if path.is_dir()
                && ziputil::dir_contains_extension(&path, Some(allowed_extensions))?
            {
                // Subdirectories are only included if they contain at
                // least one file whose extension we care about.
                ziputil::zip_add_folder(&mut zip, &path, None, Some(allowed_extensions))?;
            }
        }

        // Finalize the zip and flush to the temp file's backing storage.
        zip.finish().map_err(io::Error::other)?;
        tmp_zip.as_file_mut().sync_all()?;

        Ok(tmp_zip)
    }
}

// ---------------------------------------------------------------------------
// HostPackage to .pkpkg
// ---------------------------------------------------------------------------

impl PackageComponentBinaryBuilder for HostPackage {
    fn build_binary(&self, version_to_zip: Option<String>) -> io::Result<NamedTempFile> {
        let mut pkg_file = tempfile::Builder::new()
            .prefix(&self.info.name)
            .suffix(".pkpkg")
            .tempfile()?;

        // ----- Header: "PEKOPACKAGE" + versioned-tag flag + 8-byte nullspace
        pkg_file.write_all(b"PEKOPACKAGE")?;
        pkg_file.write_all(&[version_to_zip.is_some() as u8])?;
        pkg_file.write_all(&[0; 8])?;

        // ----- Package info block
        pkg_file.write_all(b"PACKAGEINFO")?;
        write_length_prefixed(&mut pkg_file, self.info.name.as_bytes())?;
        write_length_prefixed(&mut pkg_file, self.info.label.as_bytes())?;
        write_length_prefixed(&mut pkg_file, self.info.description.as_bytes())?;
        write_length_prefixed(&mut pkg_file, self.info.latest.as_bytes())?;

        pkg_file.write_all(&(self.info.versions.len() as u32).to_be_bytes())?;
        for version in &self.info.versions {
            write_length_prefixed(&mut pkg_file, version.as_bytes())?;
        }
        pkg_file.write_all(&[0; 8])?;

        // ----- Package readme block
        pkg_file.write_all(b"PACKAGEREADME")?;
        write_optional_text(&mut pkg_file, self.readme.as_deref())?;
        pkg_file.write_all(&[0; 8])?;

        // ----- Package license block
        pkg_file.write_all(b"PACKAGELICENSE")?;
        write_optional_text(&mut pkg_file, self.license.as_deref())?;
        pkg_file.write_all(&[0; 8])?;

        // ----- Versions block: one entry per version, each carrying its
        //       own readme and a zip payload.
        pkg_file.write_all(b"PACKAGEVERSIONS")?;
        pkg_file.write_all(&(self.version_folders.len() as u32).to_be_bytes())?;

        // If `version_to_zip` is set, narrow the iteration to just that
        // entry; otherwise iterate every version.
        let single_version: Option<HashMap<String, HostPackageVersion>> =
            version_to_zip.as_ref().map(|version| {
                HashMap::from([(version.clone(), self.version_folders[version].clone())])
            });
        let versions = single_version.as_ref().unwrap_or(&self.version_folders);

        for (version, version_info) in versions {
            write_length_prefixed(&mut pkg_file, version.as_bytes())?;
            write_optional_text(&mut pkg_file, version_info.readme.as_deref())?;

            // Each version's source tree is embedded as its own zip blob.
            let mut version_zip = version_info.build_binary(None)?;
            version_zip.seek(io::SeekFrom::Start(0))?;
            let len = version_zip.as_file().metadata()?.len();
            pkg_file.write_all(&len.to_be_bytes())?;
            io::copy(&mut version_zip, &mut pkg_file)?;
            // `version_zip` (NamedTempFile) is dropped here, deleting the
            // per-version temp zip from disk.
        }
        pkg_file.as_file_mut().sync_all()?;

        Ok(pkg_file)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Write `bytes` prefixed by its `u32` length in big-endian.
fn write_length_prefixed<W: Write>(writer: &mut W, bytes: &[u8]) -> io::Result<()> {
    writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
    writer.write_all(bytes)?;
    Ok(())
}

/// Write an optional string field. When `text` is `None`, writes only a
/// zero `u32` length.
fn write_optional_text<W: Write>(writer: &mut W, text: Option<&str>) -> io::Result<()> {
    match text {
        Some(text) => write_length_prefixed(writer, text.as_bytes()),
        None => writer.write_all(&0u32.to_be_bytes()),
    }
}
