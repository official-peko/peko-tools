//! Packing a project into a `.pkpkg` container and unpacking it back.
//!
//! A container's payload is `zstd(tar(source))`. The verbatim `peko.toml` is
//! embedded in the container header by [`LoadedManifest::to_container`]. The
//! tar holds the project's source tree, with the build cache and generated
//! files excluded.

use std::io::{Cursor, Write};
use std::path::Path;

use peko_core::config::{Compression, LOCKFILE_FILE, LoadedManifest, decode_container};
use sha2::{Digest, Sha256};

use super::RegistryError;

/// The zstd compression level for container payloads.
const ZSTD_LEVEL: i32 = 19;

/// Directory names excluded from a packed source tree.
const EXCLUDED_DIRS: &[&str] = &[".peko", "target"];

/// Pack the project at `loaded`'s root into a `.pkpkg` container.
pub fn pack(loaded: &LoadedManifest) -> Result<Vec<u8>, RegistryError> {
    let payload = compress_source(&loaded.root)?;
    loaded
        .to_container(Compression::Zstd, &payload, None)
        .map_err(RegistryError::Config)
}

/// Pack a prebuilt package's all-platforms `prebuilt/` tree into a single
/// distributable bundle: the verbatim `peko.toml` header plus
/// `zstd(tar(peko.toml + prebuilt/))`. This is the one file an admin uploads to
/// the registry for a gated package — it carries every platform's objects for
/// one toolchain, and unpacks (via [`unpack`]) into a resolvable prebuilt
/// package (its `peko.toml` and `prebuilt/` tree).
pub fn pack_prebuilt(
    loaded: &LoadedManifest,
    prebuilt_dir: &Path,
) -> Result<Vec<u8>, RegistryError> {
    let mut tar_buf = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_buf);
        // The manifest, so the unpacked bundle is a resolvable package.
        let manifest_path = loaded.root.join("peko.toml");
        builder
            .append_path_with_name(&manifest_path, "peko.toml")
            .map_err(|source| RegistryError::Io {
                path: manifest_path.clone(),
                source,
            })?;
        // The whole all-platforms prebuilt tree (stubs + every triple's objects
        // + prebuilt.toml).
        builder
            .append_dir_all("prebuilt", prebuilt_dir)
            .map_err(|source| RegistryError::Io {
                path: prebuilt_dir.to_path_buf(),
                source,
            })?;
        // The client npm package the library ships (`[client]`), if any, at its
        // declared path — so a consuming app wires it into its web frontend
        // exactly as it would for a from-source package (whose source tree
        // already carries it).
        if let peko_core::config::Manifest::Package(pkg) = &loaded.manifest
            && let Some(client) = &pkg.client
        {
            let client_dir = loaded.root.join(&client.root);
            if client_dir.is_dir() {
                let rel = client.root.to_string_lossy().replace('\\', "/");
                builder
                    .append_dir_all(&rel, &client_dir)
                    .map_err(|source| RegistryError::Io {
                        path: client_dir.clone(),
                        source,
                    })?;
            }
        }
        builder.finish().map_err(|source| RegistryError::Io {
            path: prebuilt_dir.to_path_buf(),
            source,
        })?;
    }
    let payload =
        zstd::encode_all(Cursor::new(tar_buf), ZSTD_LEVEL).map_err(|source| RegistryError::Io {
            path: prebuilt_dir.to_path_buf(),
            source,
        })?;
    loaded
        .to_container(Compression::Zstd, &payload, None)
        .map_err(RegistryError::Config)
}

/// Unpack a `.pkpkg` container's source tree into `dest`.
pub fn unpack(bytes: &[u8], dest: &Path) -> Result<(), RegistryError> {
    let container = decode_container(bytes).map_err(RegistryError::Container)?;
    let tar_bytes =
        zstd::decode_all(Cursor::new(container.payload)).map_err(|source| RegistryError::Io {
            path: dest.to_path_buf(),
            source,
        })?;

    std::fs::create_dir_all(dest).map_err(|source| RegistryError::Io {
        path: dest.to_path_buf(),
        source,
    })?;

    let mut archive = tar::Archive::new(Cursor::new(tar_bytes));
    archive.unpack(dest).map_err(|source| RegistryError::Io {
        path: dest.to_path_buf(),
        source,
    })
}

/// The hex SHA-256 of a blob, prefixed with the algorithm name.
pub fn checksum(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

/// Tar the project source and compress it with zstd.
fn compress_source(root: &Path) -> Result<Vec<u8>, RegistryError> {
    let mut tar_buf = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_buf);
        append_dir(&mut builder, root, root)?;
        builder.finish().map_err(|source| RegistryError::Io {
            path: root.to_path_buf(),
            source,
        })?;
    }

    zstd::encode_all(Cursor::new(tar_buf), ZSTD_LEVEL).map_err(|source| RegistryError::Io {
        path: root.to_path_buf(),
        source,
    })
}

/// Append every packable file under `dir` to the tar, keyed by its path
/// relative to `root`.
fn append_dir<W: Write>(
    builder: &mut tar::Builder<W>,
    root: &Path,
    dir: &Path,
) -> Result<(), RegistryError> {
    let entries = std::fs::read_dir(dir).map_err(|source| RegistryError::Io {
        path: dir.to_path_buf(),
        source,
    })?;

    for entry in entries {
        let entry = entry.map_err(|source| RegistryError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();

        if path.is_dir() {
            if !EXCLUDED_DIRS.contains(&name.as_ref()) {
                append_dir(builder, root, &path)?;
            }
        } else if path.is_file() && !is_excluded_file(&name) {
            let relative = path.strip_prefix(root).unwrap_or(&path);
            builder
                .append_path_with_name(&path, relative)
                .map_err(|source| RegistryError::Io {
                    path: path.clone(),
                    source,
                })?;
        }
    }

    Ok(())
}

/// `true` for files that never belong in a source bundle.
fn is_excluded_file(name: &str) -> bool {
    name == LOCKFILE_FILE || name.ends_with(".pkpkg")
}
