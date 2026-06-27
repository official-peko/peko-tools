//! The on-disk registry cache layout.
//!
//! The cache lives under `<peko_root>/registry` and has three areas. `cache/`
//! holds verified `.pkpkg` blobs, `src/` holds unpacked frozen source trees,
//! and `index/` holds cached index files with their ETags. The source layout
//! is shared with discovery through [`peko_core::packages::registry_source_dir`].

use std::path::{Path, PathBuf};

use peko_core::packages::{registry_source_dir, registry_source_root};

/// The registry directory name under the Peko root.
pub const REGISTRY_DIR: &str = "registry";

/// The on-disk registry cache.
#[derive(Debug, Clone)]
pub struct Cache {
    peko_root: PathBuf,
}

impl Cache {
    /// Build a cache rooted at `<peko_root>/registry`.
    pub fn new(peko_root: &Path) -> Cache {
        Cache {
            peko_root: peko_root.to_path_buf(),
        }
    }

    /// The registry root directory.
    pub fn root(&self) -> PathBuf {
        self.peko_root.join(REGISTRY_DIR)
    }

    /// The verified-blob directory.
    pub fn blob_dir(&self) -> PathBuf {
        self.root().join("cache")
    }

    /// The unpacked-source root directory.
    pub fn source_root(&self) -> PathBuf {
        registry_source_root(&self.peko_root)
    }

    /// The cached-index directory.
    pub fn index_dir(&self) -> PathBuf {
        self.root().join("index")
    }

    /// The path of a version's `.pkpkg` blob.
    pub fn blob_path(&self, name: &str, version: &str) -> PathBuf {
        self.blob_dir()
            .join(name)
            .join(format!("{name}-{version}.pkpkg"))
    }

    /// The unpacked source directory of a version.
    pub fn source_dir(&self, name: &str, version: &str) -> PathBuf {
        registry_source_dir(&self.peko_root, name, version)
    }

    /// The cached index file of a package.
    pub fn index_path(&self, name: &str) -> PathBuf {
        self.index_dir().join(format!("{name}.jsonl"))
    }

    /// The stored ETag of a package's cached index.
    pub fn index_etag_path(&self, name: &str) -> PathBuf {
        self.index_dir().join(format!("{name}.etag"))
    }

    /// `true` if a version's source is already unpacked with its manifest.
    pub fn is_unpacked(&self, name: &str, version: &str) -> bool {
        self.source_dir(name, version)
            .join(peko_core::config::MANIFEST_FILE)
            .is_file()
    }
}
