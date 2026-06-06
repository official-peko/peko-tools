//! Zip-archive helpers used by the packager when building distributable
//! `.pkg` packages.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use zip::write::{ExtendedFileOptions, FileOptions};
use zip::{CompressionMethod, ZipWriter};

/// Recursively add a folder and its contents to an open zip archive.
///
/// The folder is placed at `<base_in_zip>/<folder_name>/` inside the zip,
/// or `<folder_name>/` when `base_in_zip` is `None` or empty. When
/// `allowed_extensions` is `Some(_)`, only files whose extension matches
/// (case-insensitively) are included; directories are always recursed.
pub fn zip_add_folder<W: Write + io::Seek>(
    zip: &mut ZipWriter<W>,
    folder_path: impl AsRef<Path>,
    base_in_zip: Option<&str>,
    allowed_extensions: Option<&[&str]>,
) -> io::Result<()> {
    let folder_path = folder_path.as_ref();

    if !folder_path.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a directory", folder_path.display()),
        ));
    }

    let folder_name = folder_path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Folder has no name"))?
        .to_string_lossy();

    let zip_prefix = match base_in_zip {
        Some(base) if !base.is_empty() => {
            format!("{}/{}/", base.trim_end_matches('/'), folder_name)
        }
        _ => format!("{}/", folder_name),
    };

    let dir_options: FileOptions<ExtendedFileOptions> = FileOptions::default()
        .compression_method(CompressionMethod::Stored)
        .unix_permissions(0o755);

    let file_options: FileOptions<ExtendedFileOptions> = FileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o644);

    zip.add_directory(&zip_prefix, dir_options.clone())
        .map_err(io::Error::other)?;

    add_dir_contents(
        zip,
        folder_path,
        &zip_prefix,
        &dir_options,
        &file_options,
        allowed_extensions,
    )?;

    Ok(())
}

/// `true` if `path`'s extension is in `allowed_extensions` (case-insensitive
/// comparison). Files with no extension are rejected when a filter is set.
fn extension_allowed(path: &Path, allowed_extensions: Option<&[&str]>) -> bool {
    match allowed_extensions {
        None => true,
        Some(exts) => path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| exts.iter().any(|allowed| allowed.eq_ignore_ascii_case(e)))
            .unwrap_or(false),
    }
}

/// Recurse into a directory, mirroring its entries into the zip under
/// `zip_prefix`. Subdirectories are followed unconditionally; files are
/// included only when [`extension_allowed`] passes.
fn add_dir_contents<W: Write + io::Seek>(
    zip: &mut ZipWriter<W>,
    dir: &Path,
    zip_prefix: &str,
    dir_options: &FileOptions<ExtendedFileOptions>,
    file_options: &FileOptions<ExtendedFileOptions>,
    allowed_extensions: Option<&[&str]>,
) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path: PathBuf = entry.path();
        let entry_name = path
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Entry has no name"))?
            .to_string_lossy();

        let zip_path = format!("{zip_prefix}{entry_name}");

        if path.is_dir() {
            let zip_dir_path = format!("{zip_path}/");
            zip.add_directory(&zip_dir_path, dir_options.clone())
                .map_err(io::Error::other)?;

            add_dir_contents(
                zip,
                &path,
                &zip_dir_path,
                dir_options,
                file_options,
                allowed_extensions,
            )?;
        } else if path.is_file() && extension_allowed(&path, allowed_extensions) {
            zip.start_file(&zip_path, file_options.clone())
                .map_err(io::Error::other)?;

            // Stream the file into the zip rather than slurping it into
            // memory first, keeps memory bounded regardless of file size.
            let mut f = File::open(&path)?;
            io::copy(&mut f, zip)?;
        }
    }

    Ok(())
}

/// Walks `dir` recursively and returns `true` as soon as any file with an
/// allowed extension is found.
///
/// Used by the bundler to decide whether a project's optional resource
/// folders (assets, fonts, etc.) actually contain anything worth including
/// in the final package before adding them.
pub fn dir_contains_extension(dir: &Path, allowed_extensions: Option<&[&str]>) -> io::Result<bool> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_file() && extension_allowed(&path, allowed_extensions) {
            return Ok(true);
        } else if path.is_dir() && dir_contains_extension(&path, allowed_extensions)? {
            return Ok(true);
        }
    }
    Ok(false)
}
