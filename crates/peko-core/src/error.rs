//! Error type and supporting I/O helpers for `peko_core`.
//!
//! This module defines [`PekoError`], the crate's single error type for
//! environmental failures (I/O, JSON parsing, non-UTF-8 paths), and a small
//! set of helpers that wrap `std::fs` operations to attach path context to
//! any underlying [`std::io::Error`].
//!
//! Semantic problems in user source code (syntax errors, type mismatches,
//! unresolved references) are *not* represented here. Those flow through
//! [`crate::diagnostics::DiagnosticList`] as
//! [`crate::diagnostics::PekoDiagnostic`] values.

use std::path::{Path, PathBuf};
use thiserror::Error;

/// Errors raised by `peko_core` tooling operations.
///
/// `PekoError` covers *environmental* failures: filesystem I/O, malformed
/// package metadata, and path encoding issues. It is deliberately distinct
/// from [`crate::diagnostics::PekoDiagnostic`], which represents semantic
/// findings about user source code and is collected (not propagated) during
/// compilation.
///
/// Each I/O- or parse-related variant preserves the originating path so that
/// error messages can point at the specific file that failed, and chains the
/// underlying error via [`std::error::Error::source`] for callers that want
/// to inspect it.
#[derive(Debug, Error)]
pub enum PekoError {
    /// A filesystem operation failed.
    ///
    /// The `path` field records the file or directory that was being acted on;
    /// the `source` field carries the underlying [`std::io::Error`].
    #[error("I/O error at `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// A JSON artifact (typically `Package.json`) failed to parse.
    ///
    /// The `path` field records the file whose contents could not be parsed;
    /// the `source` field carries the underlying [`serde_json::Error`].
    #[error("failed to parse JSON at `{path}`: {source}")]
    PackageParse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    /// A path could not be converted to a UTF-8 string.
    ///
    /// Raised at the boundaries where this crate needs to hand a `&str`
    /// representation of a path to a consumer (e.g. for inclusion in a
    /// diagnostic message or AST node).
    #[error("path is not valid UTF-8: `{0}`")]
    InvalidUtf8Path(PathBuf),

    /// A target descriptor string was not well-formed.
    ///
    /// See [`crate::target::PekoTarget::from_descriptor`] for the expected
    /// `os-arch` / `os-arch-console` format.
    #[error("invalid target descriptor: `{0}`")]
    InvalidTargetDescriptor(String),

    /// A Peko binary artifact (a `.pkbin` config or cache file) was unreadable
    /// or malformed.
    ///
    /// The `path` field records the file being parsed; the `detail` field
    /// carries a short description of the structural problem (missing magic
    /// tag, truncated block, etc.).
    #[error("Peko binary `{path}` is corrupt: {detail}")]
    CorruptBinary { path: PathBuf, detail: String },
}

/// Convenience alias for `Result<T, PekoError>`.
///
/// Used throughout the crate's public API on functions that can fail with
/// environmental errors.
pub type PekoResult<T> = Result<T, PekoError>;

/// Read a file to a `String`, attaching the originating path to any I/O error.
///
/// Drop-in replacement for [`std::fs::read_to_string`] that produces a
/// [`PekoError::Io`] on failure rather than a bare [`std::io::Error`].
///
/// # Errors
///
/// Returns [`PekoError::Io`] if the file does not exist, cannot be opened,
/// or contains invalid UTF-8.
///
/// # Examples
///
/// ```no_run
/// use peko_core::error::read_to_string;
///
/// let source = read_to_string("src/main.peko")?;
/// # Ok::<(), peko_core::PekoError>(())
/// ```
pub fn read_to_string(path: impl AsRef<Path>) -> PekoResult<String> {
    let path = path.as_ref();
    std::fs::read_to_string(path).map_err(|source| PekoError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Write a slice of bytes (or string) to a file, attaching the originating
/// path to any I/O error.
///
/// Drop-in replacement for [`std::fs::write`] that produces a
/// [`PekoError::Io`] on failure rather than a bare [`std::io::Error`].
/// Overwrites the file if it already exists.
///
/// # Errors
///
/// Returns [`PekoError::Io`] if the path is not writable, the parent
/// directory does not exist, or any other I/O failure occurs.
///
/// # Examples
///
/// ```no_run
/// use peko_core::error::write;
///
/// write("out/build.log", "compilation succeeded\n")?;
/// # Ok::<(), peko_core::PekoError>(())
/// ```
pub fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> PekoResult<()> {
    let path = path.as_ref();
    std::fs::write(path, contents).map_err(|source| PekoError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Recursively create a directory and all of its missing parents, attaching
/// the originating path to any I/O error.
///
/// Drop-in replacement for [`std::fs::create_dir_all`] that produces a
/// [`PekoError::Io`] on failure rather than a bare [`std::io::Error`]. Like
/// the standard-library function, this is a no-op if the directory already
/// exists.
///
/// # Errors
///
/// Returns [`PekoError::Io`] if a path component cannot be created
/// (permission denied, a non-directory exists at the target path, etc.).
///
/// # Examples
///
/// ```no_run
/// use peko_core::error::create_dir_all;
///
/// create_dir_all("out/cache/modules")?;
/// # Ok::<(), peko_core::PekoError>(())
/// ```
pub fn create_dir_all(path: impl AsRef<Path>) -> PekoResult<()> {
    let path = path.as_ref();
    std::fs::create_dir_all(path).map_err(|source| PekoError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;

    #[test]
    fn read_to_string_missing_file_returns_io_error_with_path() {
        let missing = PathBuf::from("/definitely/does/not/exist.peko");
        let err = read_to_string(&missing).expect_err("missing file must error");

        match err {
            PekoError::Io { path, source } => {
                assert_eq!(path, missing);
                assert_eq!(source.kind(), ErrorKind::NotFound);
            }
            other => panic!("expected Io variant, got {other:?}"),
        }
    }

    #[test]
    fn read_to_string_roundtrips_through_a_tempfile() {
        let dir = std::env::temp_dir();
        let path = dir.join("peko_core_error_test_roundtrip.txt");
        let payload = "peko peko";

        write(&path, payload).expect("write must succeed");
        let read_back = read_to_string(&path).expect("read must succeed");
        assert_eq!(read_back, payload);

        // Clean up; ignore failure (test should not depend on cleanup).
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn create_dir_all_is_idempotent_and_path_preserving() {
        let dir = std::env::temp_dir().join("peko_core_error_test_mkdir/nested/path");
        create_dir_all(&dir).expect("first create must succeed");
        // Calling twice must not error, std::fs::create_dir_all is no-op on existing.
        create_dir_all(&dir).expect("second create must be a no-op");
        assert!(dir.is_dir());

        // Clean up the leaf; parents are left in temp_dir which is fine.
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join("peko_core_error_test_mkdir"));
    }

    #[test]
    fn invalid_utf8_path_renders_path_in_message() {
        let path = PathBuf::from("/some/weird/path");
        let err = PekoError::InvalidUtf8Path(path.clone());
        let rendered = format!("{err}");
        assert!(rendered.contains("/some/weird/path"), "got: {rendered}");
    }

    #[test]
    fn io_error_renders_path_and_source_in_message() {
        let err = PekoError::Io {
            path: PathBuf::from("/x/y/z"),
            source: std::io::Error::new(ErrorKind::PermissionDenied, "denied"),
        };
        let rendered = format!("{err}");
        assert!(rendered.contains("/x/y/z"), "missing path: {rendered}");
        assert!(rendered.contains("denied"), "missing source: {rendered}");
    }
}
