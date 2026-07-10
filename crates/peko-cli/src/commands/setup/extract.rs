//! Archive extraction for the formats setup downloads: zip (Windows peko), and
//! tar.xz (peko binaries and the toolchain descriptors). tar.zst (the SDK and
//! toolchain payloads) is added alongside as those steps land.

use std::fs::File;
use std::io::{BufReader, Cursor};
use std::path::Path;

use super::{Result, SetupError};

#[derive(Debug, Clone, Copy)]
pub enum ArchiveFormat {
    Zip,
    TarXz,
    TarZst,
}

impl ArchiveFormat {
    /// Infer the format from an asset file name.
    pub fn from_asset_name(name: &str) -> Option<Self> {
        if name.ends_with(".zip") {
            Some(Self::Zip)
        } else if name.ends_with(".tar.xz") {
            Some(Self::TarXz)
        } else if name.ends_with(".tar.zst") {
            Some(Self::TarZst)
        } else {
            None
        }
    }
}

/// Extract `archive` into `dest`, creating `dest` if needed.
pub fn extract(archive: &Path, format: ArchiveFormat, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)
        .map_err(|e| SetupError::io(format!("create {}", dest.display()), e))?;
    match format {
        ArchiveFormat::Zip => extract_zip(archive, dest),
        ArchiveFormat::TarXz => extract_tar_xz(archive, dest),
        ArchiveFormat::TarZst => extract_tar_zst(archive, dest),
    }
}

/// Move `staged` onto `target`, replacing any existing directory there.
pub fn atomic_replace_dir(staged: &Path, target: &Path) -> Result<()> {
    if target.exists() {
        std::fs::remove_dir_all(target)
            .map_err(|e| SetupError::io(format!("remove {}", target.display()), e))?;
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| SetupError::io(format!("create {}", parent.display()), e))?;
    }
    std::fs::rename(staged, target)
        .map_err(|e| SetupError::io(format!("move into {}", target.display()), e))?;
    Ok(())
}

fn extract_zip(archive: &Path, dest: &Path) -> Result<()> {
    let file =
        File::open(archive).map_err(|e| SetupError::io(format!("open {}", archive.display()), e))?;
    let mut zip = zip::ZipArchive::new(BufReader::new(file))
        .map_err(|e| SetupError::Extract(format!("open zip: {e}")))?;
    zip.extract(dest)
        .map_err(|e| SetupError::Extract(format!("extract zip: {e}")))?;
    Ok(())
}

fn extract_tar_xz(archive: &Path, dest: &Path) -> Result<()> {
    let file =
        File::open(archive).map_err(|e| SetupError::io(format!("open {}", archive.display()), e))?;
    let mut reader = BufReader::new(file);
    let mut tar_bytes = Vec::new();
    lzma_rs::xz_decompress(&mut reader, &mut tar_bytes)
        .map_err(|e| SetupError::Extract(format!("decompress xz: {e}")))?;
    let mut tar = tar::Archive::new(Cursor::new(tar_bytes));
    tar.unpack(dest)
        .map_err(|e| SetupError::io(format!("unpack tar into {}", dest.display()), e))?;
    Ok(())
}

fn extract_tar_zst(archive: &Path, dest: &Path) -> Result<()> {
    let file =
        File::open(archive).map_err(|e| SetupError::io(format!("open {}", archive.display()), e))?;
    let decoder = zstd::stream::read::Decoder::new(BufReader::new(file))
        .map_err(|e| SetupError::Extract(format!("open zstd: {e}")))?;
    let mut tar = tar::Archive::new(decoder);
    tar.unpack(dest)
        .map_err(|e| SetupError::io(format!("unpack tar into {}", dest.display()), e))?;
    Ok(())
}
