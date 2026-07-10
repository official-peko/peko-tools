//! The on-disk layout of a Peko install under the root directory.

use std::path::{Path, PathBuf};

use super::{Result, SetupError};

/// Paths within the `~/.Peko` install root.
#[derive(Debug, Clone)]
pub struct Layout {
    root: PathBuf,
}

impl Layout {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn compiler(&self) -> PathBuf {
        self.root.join("Compiler")
    }

    pub fn bin_peko(&self) -> PathBuf {
        self.compiler().join("bin").join("peko")
    }

    pub fn toolchains(&self) -> PathBuf {
        self.compiler().join("toolchains")
    }

    pub fn packages(&self) -> PathBuf {
        self.root.join("Packages")
    }

    pub fn manifest(&self) -> PathBuf {
        self.root.join("versions.json")
    }

    pub fn env_file(&self) -> PathBuf {
        self.root.join("env")
    }

    /// A toolchain directory by its slash-separated relative path, e.g.
    /// "linux/arm" or "macos/arm64".
    pub fn toolchain_dir(&self, relative: &str) -> PathBuf {
        let mut path = self.toolchains();
        for part in relative.split('/') {
            path.push(part);
        }
        path
    }

    /// Whether a versions manifest is present (a prior install exists).
    pub fn is_installed(&self) -> bool {
        self.manifest().is_file()
    }

    /// Create the root and its top-level directories.
    pub fn create_base(&self) -> Result<()> {
        for dir in [
            self.root.clone(),
            self.compiler(),
            self.bin_peko(),
            self.toolchains(),
            self.packages(),
        ] {
            std::fs::create_dir_all(&dir)
                .map_err(|e| SetupError::io(format!("create {}", dir.display()), e))?;
        }
        Ok(())
    }
}
