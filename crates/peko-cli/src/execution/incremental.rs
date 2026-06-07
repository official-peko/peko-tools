//! Incremental project compilation.
//!
//! The incremental cache lives under `<project>/.peko/incremental/` and
//! consists of two binary files plus a per-target `objects/` subtree:
//!
//! - `filemap.pkbin`: the [`ProjectIncrementalMap`]'s file graph: which
//!   tracked files exist, which top-level modules they belong to, which
//!   stylesheets they reference, which other files import them (for
//!   recheck-on-change), and which extra object files need to flow to the
//!   linker per target.
//! - `filehashes.pkbin`: md5 digests of every tracked file and tracked
//!   stylesheet. Used to detect which files have changed since the last
//!   build.
//! - `objects/<os>/<arch>/*.o`: compiled object files keyed by an encoded
//!   project-relative file id.
//!
//! [`compile_project`] is the public entry point. It walks the incremental
//! cache (creating one if absent), recompiles only the files whose hash has
//! changed (plus dependents that need rechecking), and finally invokes
//! `lld_link` to produce the executable.
//!
//! Progress reporting is via the cli's [`ProgressSink`] trait. The first
//! build of a project displays a spinner during the entrypoint codegen
//! (file count not yet known); subsequent incremental rebuilds set a total
//! up-front and tick once per file.
//!
//! [`ProgressSink`]: crate::cli::reporting::ProgressSink

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use derive_new::new;
use indexmap::IndexMap;
use itertools::Itertools;
use peko_core::diagnostics::{DiagnosticList, DiagnosticType};
use peko_core::error::{PekoError, PekoResult};
use peko_core::execution::data_structures::ExecutionModule;
use peko_core::target::PekoTarget;
use peko_llvm::codegen::PekoValueBuilder;
use peko_llvm::codegen::builders::prelude::GlobalBuilder;
use peko_llvm::codegen::context::PekoCodegenContext;
use peko_llvm::codegen::data_structures::CodegenModule;

use crate::cli::reporting::ProgressSink;
use crate::project::{self, PekoProject};

use super::parse_peko_source;

// ---------------------------------------------------------------------------
// Path and file id encoding
// ---------------------------------------------------------------------------

/// Separator placed between path components in the encoded file-id string.
const FILEID_SEPARATOR: &str = "----";

/// Encode a path as a stable, filesystem-safe string id.
///
/// The path is canonicalized first, then every native path separator is
/// replaced by [`FILEID_SEPARATOR`]. On Windows, the `\\?\` long-path
/// prefix that canonicalization adds is stripped so the encoded id begins
/// with the drive letter rather than a UNC prefix.
fn pathbuf_to_fileid(path: &Path) -> String {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mut string = canonical.display().to_string();

    // Strip the Windows `\\?\` (or `//?/`) long-path prefix.
    if let Some(stripped) = string.strip_prefix(r"\\?\") {
        string = stripped.to_owned();
    } else if let Some(stripped) = string.strip_prefix("//?/") {
        string = stripped.to_owned();
    }

    string.replace(std::path::MAIN_SEPARATOR, FILEID_SEPARATOR)
}

/// Inverse of [`pathbuf_to_fileid`]. Reconstructs an absolute path from an
/// encoded file id by swapping every [`FILEID_SEPARATOR`] back to the
/// native separator.
fn fileid_to_pathbuf(fileid: &str) -> PathBuf {
    PathBuf::from(fileid.replace(FILEID_SEPARATOR, std::path::MAIN_SEPARATOR_STR))
}

/// Compute the md5 digest of `hashable`'s utf-8 bytes.
fn md5_hash_string(hashable: &str) -> [u8; 16] {
    let mut md5_context = md5::Context::new();
    md5_context.write_all(hashable.as_bytes()).unwrap();
    md5_context.compute().into()
}

// ---------------------------------------------------------------------------
// ProjectFile: per-source-file metadata
// ---------------------------------------------------------------------------

/// Per-file metadata tracked in the incremental cache.
#[derive(new, Clone, Debug)]
pub struct ProjectFile {
    /// Encoded canonical path; see [`pathbuf_to_fileid`].
    pub file_id: String,
    /// Mangled name of the `set_globals` function for this file's module.
    pub global_set_id: String,
    /// Top-level module name the file contributes to.
    pub module_name: String,
    /// Files that import this one (must be re-checked when this file changes).
    pub rechecks: Vec<ProjectFile>,
    /// Stylesheets this file references (tracked for change detection).
    pub styles_to_watch: Vec<String>,
}

impl ProjectFile {
    /// Build a `ProjectFile` snapshot of a freshly-codegen-ed top-level
    /// module.
    pub fn from_top_level_module(
        top_level: Arc<RwLock<CodegenModule>>,
        track_rechecks: bool,
        track_styles: bool,
    ) -> ProjectFile {
        let module = top_level.read().unwrap();
        let top = module.get_top_level().unwrap();
        ProjectFile {
            file_id: pathbuf_to_fileid(&module.get_file().canonicalize().unwrap()),
            global_set_id: format!("{}::set_globals", module.name),
            module_name: module.get_name().to_owned(),
            rechecks: if track_rechecks {
                top.imported_by
                    .iter()
                    .map(|imported| {
                        ProjectFile::from_top_level_module(
                            Arc::clone(imported),
                            false,
                            track_styles,
                        )
                    })
                    .collect()
            } else {
                Vec::new()
            },
            styles_to_watch: if track_styles {
                top.imported_styles
                    .iter()
                    .map(|stylesheet| pathbuf_to_fileid(stylesheet))
                    .collect()
            } else {
                Vec::new()
            },
        }
    }

    /// Decode this file's path from its file id.
    pub fn get_path(&self) -> PathBuf {
        fileid_to_pathbuf(&self.file_id)
    }

    /// Read the file's current bytes and md5-hash them.
    pub fn generate_md5(&self) -> [u8; 16] {
        md5_hash_string(&std::fs::read_to_string(self.get_path()).unwrap())
    }
}

// ---------------------------------------------------------------------------
// ProjectIncrementalMap: the on-disk file graph plus change detection.
// ---------------------------------------------------------------------------

/// In-memory representation of the project's incremental cache.
#[derive(Clone, Debug)]
pub struct ProjectIncrementalMap {
    pub folder: PathBuf,
    pub track_styles: bool,
    tracked_files: Vec<String>,
    linked_files: HashMap<String, Vec<PathBuf>>,
    files: Vec<ProjectFile>,
    file_hashes: HashMap<String, Vec<u8>>,
    updated_filemap: bool,
    updated_hashmap: bool,
    global_set_ids: Vec<String>,
}

impl ProjectIncrementalMap {
    /// `true` if anything has been modified since the cache was last
    /// flushed to disk.
    pub fn updated(&self) -> bool {
        self.updated_filemap || self.updated_hashmap
    }

    /// `true` if `file_id` is currently in the tracked-files list.
    pub fn is_file_tracked(&self, file_id: &str) -> bool {
        self.tracked_files.iter().any(|tracked| tracked == file_id)
    }

    /// Build a fresh, empty incremental map rooted at `output_directory`.
    pub fn new(output_directory: impl AsRef<Path>, track_styles: bool) -> ProjectIncrementalMap {
        ProjectIncrementalMap {
            folder: output_directory.as_ref().to_path_buf(),
            track_styles,
            tracked_files: Vec::new(),
            linked_files: HashMap::new(),
            files: Vec::new(),
            file_hashes: HashMap::new(),
            updated_filemap: false,
            updated_hashmap: false,
            global_set_ids: Vec::new(),
        }
    }

    /// Load an incremental map from `incremental_dir`. Returns `None` when
    /// the two cache files are missing or corrupt (corrupt-cache failures
    /// are treated as "fall back to a clean build" rather than an error
    /// the user needs to act on).
    pub fn from_incremental_directory(
        incremental_dir: impl AsRef<Path>,
        track_styles: bool,
    ) -> Option<ProjectIncrementalMap> {
        let incremental_dir = incremental_dir.as_ref();
        let filemap_bin = incremental_dir.join("filemap.pkbin");
        let hashmap_bin = incremental_dir.join("filehashes.pkbin");

        if !filemap_bin.exists() || !hashmap_bin.exists() {
            return None;
        }

        let mut project_incremental = ProjectIncrementalMap::new(incremental_dir, track_styles);
        project_incremental
            .parse_filemap_from_binary(&filemap_bin)
            .ok()?;
        project_incremental
            .parse_filehashmap_from_binary(&hashmap_bin)
            .ok()?;

        Some(project_incremental)
    }

    /// Flush any pending changes to disk.
    pub fn write_updates(&mut self) {
        if self.updated_filemap {
            std::fs::write(self.folder.join("filemap.pkbin"), self.filemap_to_binary()).unwrap();
            self.updated_filemap = false;
        }

        if self.updated_hashmap {
            std::fs::write(
                self.folder.join("filehashes.pkbin"),
                self.file_hashmap_to_binary(),
            )
            .unwrap();
            self.updated_hashmap = false;
        }
    }

    /// Snapshot of every file currently tracked.
    pub fn get_tracked_files(&self) -> Vec<ProjectFile> {
        self.files.clone()
    }

    /// Snapshot of every `set_globals` function id currently registered.
    pub fn get_global_sets(&self) -> Vec<String> {
        self.global_set_ids.clone()
    }

    /// Append a new `set_globals` function id to the registry.
    pub fn add_global_set_function(&mut self, function_id: String) {
        self.global_set_ids.push(function_id);
        self.updated_filemap = true;
    }

    /// Remove a `set_globals` function id from the registry. No-op if the
    /// id isn't currently registered.
    pub fn remove_global_set_function(&mut self, function_id: String) {
        let Some((idx, _)) = self
            .global_set_ids
            .iter()
            .find_position(|id| **id == function_id)
        else {
            return;
        };
        self.global_set_ids.remove(idx);
        self.updated_filemap = true;
    }

    /// Walk every tracked file, dropping any whose source has been deleted
    /// from disk. Returns the dropped entries so the caller can also
    /// invalidate their rechecks.
    pub fn get_removed_files(&mut self) -> Vec<ProjectFile> {
        let mut removed_files = Vec::new();
        for file in self.files.clone() {
            if !file.get_path().exists() {
                self.remove_file(file.clone());
                removed_files.push(file);
            }
        }
        removed_files
    }

    /// `true` if any tracked file's md5 differs from its saved digest
    /// (including any of its referenced stylesheets).
    pub fn tracked_files_changed(&self) -> bool {
        for (file_id, saved_hash) in &self.file_hashes {
            let Some(current_file) = self.files.iter().find(|file| file.file_id == *file_id) else {
                continue;
            };
            if current_file.generate_md5().as_slice() != saved_hash.as_slice() {
                return true;
            }

            for style in &current_file.styles_to_watch {
                // A style that the file now watches but which has no
                // recorded hash means the file gained a new style
                // reference since it was last registered. Treat that as
                // a change so the new style gets picked up.
                let Some(saved_style_hash) = self.file_hashes.get(style) else {
                    return true;
                };

                // If the style file can't be read (e.g. it was deleted),
                // treat that as a change too rather than panicking.
                let Ok(style_source) = std::fs::read_to_string(fileid_to_pathbuf(style)) else {
                    return true;
                };

                let current_style_hash = md5_hash_string(&style_source);
                if current_style_hash.as_slice() != saved_style_hash.as_slice() {
                    return true;
                }
            }
        }
        false
    }

    /// Check each tracked file's md5 against its saved digest, updating the
    /// saved digest in place and returning the list of files that changed.
    /// Identifies both source changes and stylesheet changes.
    pub fn get_changed_files(&mut self) -> Vec<ProjectFile> {
        let mut changed_files = Vec::new();
        for (file_id, saved_hash) in self.file_hashes.clone() {
            let Some(current_file) = self.files.iter().find(|file| file.file_id == file_id) else {
                continue;
            };
            let current_hash = current_file.generate_md5();

            let mut marked_changed = false;
            if current_hash.as_slice() != saved_hash.as_slice() {
                marked_changed = true;
                changed_files.push(current_file.clone());
                self.file_hashes.insert(file_id, current_hash.to_vec());
                self.updated_hashmap = true;
            }

            for style in &current_file.styles_to_watch.clone() {
                // If the style file can't be read (deleted), record an
                // empty hash and mark the file changed so the linker
                // drops the stale state on the next pass.
                let style_source =
                    std::fs::read_to_string(fileid_to_pathbuf(style)).unwrap_or_default();
                let current_style_hash = md5_hash_string(&style_source);

                // A style with no recorded hash is newly watched: insert
                // its hash and mark the owning file changed. Using `.get`
                // (not indexing) avoids a panic when styles_to_watch has
                // drifted ahead of file_hashes, which happens whenever a
                // file gains a new style reference between builds.
                let style_differs = match self.file_hashes.get(style) {
                    Some(saved) => current_style_hash.as_slice() != saved.as_slice(),
                    None => true,
                };

                if style_differs {
                    if !marked_changed {
                        changed_files.push(current_file.clone());
                        marked_changed = true;
                    }
                    self.file_hashes
                        .insert(style.clone(), current_style_hash.to_vec());
                    self.updated_hashmap = true;
                }
            }
        }
        changed_files
    }

    /// Register a new file and its hash in the cache.
    pub fn add_file(&mut self, file: ProjectFile) {
        self.file_hashes
            .insert(file.file_id.clone(), file.generate_md5().to_vec());

        for style in &file.styles_to_watch {
            // Record the style's hash. If the style file can't be read,
            // store the hash of an empty string so the entry exists; the
            // next incremental pass will pick up the real content (or
            // treat it as changed if still missing).
            let style_source =
                std::fs::read_to_string(fileid_to_pathbuf(style)).unwrap_or_default();
            self.file_hashes
                .insert(style.clone(), md5_hash_string(&style_source).to_vec());
        }

        self.tracked_files.push(file.file_id.clone());
        self.files.push(file);

        self.updated_filemap = true;
        self.updated_hashmap = true;
    }

    /// Drop a file's entry from the cache plus any associated style
    /// hashes. No-op if the file isn't currently tracked.
    pub fn remove_file(&mut self, file: ProjectFile) {
        let Some((file_tree_index, _)) = self
            .files
            .iter()
            .find_position(|file1| file1.file_id == file.file_id)
        else {
            return;
        };
        self.files.remove(file_tree_index);

        self.file_hashes.remove(&file.file_id);
        for style in &file.styles_to_watch {
            self.file_hashes.remove(style);
        }

        let Some((tracked_index, _)) = self
            .tracked_files
            .iter()
            .find_position(|file_id| **file_id == file.file_id)
        else {
            return;
        };
        self.tracked_files.remove(tracked_index);
    }

    /// Read the file-hash map from a `.pkbin`.
    pub fn parse_filehashmap_from_binary(&mut self, binary_path: &Path) -> PekoResult<()> {
        let raw = std::fs::read(binary_path).map_err(|source| PekoError::Io {
            path: binary_path.to_path_buf(),
            source,
        })?;
        let mut reader = project::PekoBinaryReader::new(raw);

        let corrupt = |detail: &str| PekoError::CorruptBinary {
            path: binary_path.to_path_buf(),
            detail: detail.to_owned(),
        };

        if !reader.parse_magic("PEKOHASHMAP") || !reader.parse_nullspace() {
            return Err(corrupt("couldn't find PEKOHASHMAP tag"));
        }
        if !reader.parse_magic("FILES") {
            return Err(corrupt("couldn't find FILES tag"));
        }

        let file_count = reader
            .parse_u32()
            .ok_or_else(|| corrupt("truncated file count"))?;

        for _ in 0..file_count {
            let filename_length = reader
                .parse_u32()
                .ok_or_else(|| corrupt("truncated filename length"))?;
            let filename = reader
                .parse_string(filename_length)
                .ok_or_else(|| corrupt("truncated filename"))?;

            let hash = reader
                .parse_bytes(16)
                .ok_or_else(|| corrupt(&format!("hash truncated for {filename}")))?;

            self.file_hashes.insert(filename, hash);
        }

        Ok(())
    }

    /// Serialize the file-hash map to a binary blob.
    pub fn file_hashmap_to_binary(&self) -> Vec<u8> {
        let mut final_binary = Vec::new();

        final_binary.extend("PEKOHASHMAP".bytes());
        final_binary.extend([0; 8]);

        final_binary.extend("FILES".bytes());
        final_binary.extend((self.files.len() as u32).to_be_bytes());

        for (file_id, file_hash) in &self.file_hashes {
            final_binary.extend((file_id.len() as u32).to_be_bytes());
            final_binary.extend(file_id.bytes());
            final_binary.extend(file_hash);
        }

        final_binary
    }

    /// Read the file map from a `.pkbin`.
    pub fn parse_filemap_from_binary(&mut self, binary_path: &Path) -> PekoResult<()> {
        let raw = std::fs::read(binary_path).map_err(|source| PekoError::Io {
            path: binary_path.to_path_buf(),
            source,
        })?;
        let mut reader = project::PekoBinaryReader::new(raw);

        let corrupt = |detail: &str| PekoError::CorruptBinary {
            path: binary_path.to_path_buf(),
            detail: detail.to_owned(),
        };

        if !reader.parse_magic("PEKOFILEMAP") || !reader.parse_nullspace() {
            return Err(corrupt("couldn't find PEKOFILEMAP tag"));
        }
        if !reader.parse_magic("LINKEDFILES") {
            return Err(corrupt("couldn't find LINKEDFILES tag"));
        }

        let platform_count = reader
            .parse_u8()
            .ok_or_else(|| corrupt("truncated platform count"))?;

        for _ in 0..platform_count {
            let platform_name_length = reader
                .parse_u32()
                .ok_or_else(|| corrupt("truncated platform name length"))?;
            let platform_name = reader
                .parse_string(platform_name_length)
                .ok_or_else(|| corrupt("truncated platform name"))?;

            let file_count = reader
                .parse_u32()
                .ok_or_else(|| corrupt("truncated LINKEDFILES file count"))?;

            let mut linked_files = Vec::new();
            for _ in 0..file_count {
                let filename_length = reader
                    .parse_u32()
                    .ok_or_else(|| corrupt("truncated linked filename length"))?;
                let filename = reader
                    .parse_string(filename_length)
                    .ok_or_else(|| corrupt("truncated linked filename"))?;
                linked_files.push(PathBuf::from(filename));
            }
            self.linked_files.insert(platform_name, linked_files);
        }

        if !reader.parse_nullspace() || !reader.parse_magic("FILES") {
            return Err(corrupt("couldn't find FILES tag"));
        }

        let file_count = reader
            .parse_u32()
            .ok_or_else(|| corrupt("truncated FILES count"))?;

        for _ in 0..file_count {
            let filename_length = reader
                .parse_u32()
                .ok_or_else(|| corrupt("truncated filename length"))?;
            let filename = reader
                .parse_string(filename_length)
                .ok_or_else(|| corrupt("truncated filename"))?;

            let modname_length = reader
                .parse_u32()
                .ok_or_else(|| corrupt("truncated modname length"))?;
            let modname = reader
                .parse_string(modname_length)
                .ok_or_else(|| corrupt("truncated modname"))?;

            let style_count = reader
                .parse_u32()
                .ok_or_else(|| corrupt("truncated style count"))?;

            let mut style_ids = Vec::new();
            for _ in 0..style_count {
                let style_id_length = reader
                    .parse_u32()
                    .ok_or_else(|| corrupt("truncated style id length"))?;
                style_ids.push(
                    reader
                        .parse_string(style_id_length)
                        .ok_or_else(|| corrupt("truncated style id"))?,
                );
            }

            self.files.push(ProjectFile::new(
                filename.clone(),
                format!("{modname}::set_globals"),
                modname,
                Vec::new(),
                style_ids,
            ));
            self.tracked_files.push(filename);
        }

        if !reader.parse_nullspace() || !reader.parse_magic("FILECOMPILATION") {
            return Err(corrupt("couldn't find FILECOMPILATION tree"));
        }

        let node_count = reader
            .parse_u32()
            .ok_or_else(|| corrupt("truncated FILECOMPILATION node count"))?;

        for _ in 0..node_count {
            let file_id_length = reader
                .parse_u32()
                .ok_or_else(|| corrupt("truncated file id length"))?;
            let file_id = reader
                .parse_string(file_id_length)
                .ok_or_else(|| corrupt("truncated file id"))?;
            let rechecks_count = reader
                .parse_u32()
                .ok_or_else(|| corrupt("truncated rechecks count"))?;

            let files_reference = self.files.clone();
            let Some(file) = self.files.iter_mut().find(|file| file.file_id == file_id) else {
                // File id no longer present; skip rechecks for it.
                for _ in 0..rechecks_count {
                    let recheck_id_length = reader
                        .parse_u32()
                        .ok_or_else(|| corrupt("truncated recheck id length"))?;
                    let _ = reader
                        .parse_string(recheck_id_length)
                        .ok_or_else(|| corrupt("truncated recheck id"))?;
                }
                continue;
            };

            for _ in 0..rechecks_count {
                let recheck_id_length = reader
                    .parse_u32()
                    .ok_or_else(|| corrupt("truncated recheck id length"))?;
                let recheck_file_id = reader
                    .parse_string(recheck_id_length)
                    .ok_or_else(|| corrupt("truncated recheck id"))?;
                let Some(recheck_file) = files_reference
                    .iter()
                    .find(|file| file.file_id == recheck_file_id)
                else {
                    continue;
                };
                file.rechecks.push(recheck_file.clone());
            }
        }

        if !reader.parse_nullspace() || !reader.parse_magic("GLOBALSETS") {
            return Err(corrupt("couldn't find GLOBALSETS tree"));
        }

        let globalset_count = reader
            .parse_u32()
            .ok_or_else(|| corrupt("truncated GLOBALSETS count"))?;

        for _ in 0..globalset_count {
            let global_length = reader
                .parse_u32()
                .ok_or_else(|| corrupt("truncated global set length"))?;
            self.global_set_ids.push(
                reader
                    .parse_string(global_length)
                    .ok_or_else(|| corrupt("truncated global set id"))?,
            );
        }

        Ok(())
    }

    /// Append `file_to_link` to the per-platform linker file list. No-op
    /// if the file is already registered for that platform.
    pub fn add_linked_file(&mut self, platform: String, file_to_link: PathBuf) {
        let entry = self.linked_files.entry(platform).or_default();
        if entry.iter().any(|f| f == &file_to_link) {
            return;
        }
        entry.push(file_to_link);
        self.updated_filemap = true;
    }

    /// Bulk version of [`add_linked_file`](Self::add_linked_file).
    pub fn add_linked_files(&mut self, platform: String, linked_files: Vec<PathBuf>) {
        for file in linked_files {
            self.add_linked_file(platform.clone(), file);
        }
    }

    /// Serialize the file map to a binary blob.
    pub fn filemap_to_binary(&self) -> Vec<u8> {
        let mut final_binary = Vec::new();

        final_binary.extend("PEKOFILEMAP".bytes());
        final_binary.extend([0; 8]);

        final_binary.extend("LINKEDFILES".bytes());
        final_binary.push(self.linked_files.len() as u8);

        for (platform_name, files) in &self.linked_files {
            final_binary.extend((platform_name.len() as u32).to_be_bytes());
            final_binary.extend(platform_name.bytes());
            final_binary.extend((files.len() as u32).to_be_bytes());

            for file in files {
                let file_string = file.to_str().unwrap().to_owned();
                final_binary.extend((file_string.len() as u32).to_be_bytes());
                final_binary.extend(file_string.bytes());
            }
        }
        final_binary.extend([0; 8]);

        final_binary.extend("FILES".bytes());
        final_binary.extend((self.files.len() as u32).to_be_bytes());

        for file in &self.files {
            final_binary.extend((file.file_id.len() as u32).to_be_bytes());
            final_binary.extend(file.file_id.bytes());

            final_binary.extend((file.module_name.len() as u32).to_be_bytes());
            final_binary.extend(file.module_name.bytes());

            final_binary.extend((file.styles_to_watch.len() as u32).to_be_bytes());
            for style in &file.styles_to_watch {
                final_binary.extend((style.len() as u32).to_be_bytes());
                final_binary.extend(style.bytes());
            }
        }
        final_binary.extend([0; 8]);

        final_binary.extend("FILECOMPILATION".bytes());
        final_binary.extend((self.files.len() as u32).to_be_bytes());

        for file in &self.files {
            final_binary.extend((file.file_id.len() as u32).to_be_bytes());
            final_binary.extend(file.file_id.bytes());

            final_binary.extend((file.rechecks.len() as u32).to_be_bytes());
            for recheck in &file.rechecks {
                final_binary.extend((recheck.file_id.len() as u32).to_be_bytes());
                final_binary.extend(recheck.file_id.bytes());
            }
        }
        final_binary.extend([0; 8]);

        final_binary.extend("GLOBALSETS".bytes());
        final_binary.extend((self.global_set_ids.len() as u32).to_be_bytes());

        for global_set_id in &self.global_set_ids {
            final_binary.extend((global_set_id.len() as u32).to_be_bytes());
            final_binary.extend(global_set_id.bytes());
        }

        final_binary
    }
}

// ---------------------------------------------------------------------------
// Orchestrators
// ---------------------------------------------------------------------------

/// First-build path: parse the entrypoint, codegen the whole project as
/// one pass, emit object files, populate the incremental map.
///
/// File count isn't known until codegen completes, so this function emits
/// a single spinner-style progress message rather than per-file ticks.
fn initialize_incremental_for_target(
    peko_root: &Path,
    project: &mut PekoProject,
    incremental_directory: PathBuf,
    target: PekoTarget,
    main_file: PathBuf,
    compilation_root: PathBuf,
    compiled_styles_dir: Option<PathBuf>,
    asset_debug_dir: Option<PathBuf>,
    preloaded_modules: Option<IndexMap<String, Arc<RwLock<CodegenModule>>>>,
    progress: &dyn ProgressSink,
) -> PekoResult<(PekoCodegenContext, DiagnosticList)> {
    progress.message("Compiling entrypoint");

    let (asts, mut diagnostics) = parse_peko_source(
        main_file.clone(),
        std::fs::read_to_string(&main_file).unwrap(),
    );

    let mut codegen_context = PekoCodegenContext::new(
        target.clone(),
        main_file.clone(),
        false,
        compilation_root.clone(),
    );
    codegen_context.compiled_styles_folder = compiled_styles_dir.clone();
    codegen_context.asset_debug_folder = asset_debug_dir;
    match &project.ui_project_info {
        Some(ui_info) => codegen_context.application_id = Some(ui_info.bundle_id.clone()),
        None => {}
    }

    if let Some(preloaded) = preloaded_modules {
        codegen_context.module_context.load_modules(preloaded);
    }

    super::load_external_modules!(codegen_context, peko_root, Some(&compilation_root));
    codegen_context.windowsgui = !target.console;

    for ast in asts {
        ast.build_value(&mut codegen_context);
    }

    diagnostics.extend(codegen_context.diagnostics.clone());

    if diagnostics.has_errors() {
        return Ok((codegen_context, diagnostics));
    }

    let globals_set = codegen_context.create_global_set_module();

    let objects_directory = incremental_directory
        .join("objects")
        .join(target.operating_system.to_string())
        .join(target.architecture.to_string());
    std::fs::create_dir_all(&objects_directory).unwrap();
    globals_set
        .read()
        .unwrap()
        .get_top_level()
        .unwrap()
        .output_binary(target.clone(), objects_directory.join("__globals_set.o"));
    globals_set
        .read()
        .unwrap()
        .get_top_level()
        .unwrap()
        .emit_ir(objects_directory.join("__globals_set.ir"));

    // Fast path: incremental info already exists (called from a top-level
    // build with a pre-existing cache). Output binaries for each module
    // and update the linker file list, without rebuilding the map.
    if project.incremental_info.is_some() {
        let modules = codegen_context.module_context.top_level_modules.clone();
        for (modname, top_level_module) in modules {
            if modname == "extern" {
                continue;
            }

            let project_file = ProjectFile::from_top_level_module(
                Arc::clone(&top_level_module),
                true,
                compiled_styles_dir.is_none(),
            );
            if !diagnostics.has_errors() {
                codegen_context.init_module_globals(&top_level_module);
                top_level_module
                    .read()
                    .unwrap()
                    .get_top_level()
                    .unwrap()
                    .output_binary(
                        target.clone(),
                        objects_directory.join(format!("{}.o", project_file.file_id)),
                    );
                top_level_module
                    .read()
                    .unwrap()
                    .get_top_level()
                    .unwrap()
                    .emit_ir(objects_directory.join(format!("{}.ir", project_file.file_id)));
            }
        }

        project
            .incremental_info
            .as_mut()
            .unwrap()
            .add_linked_files(target.to_triple(), codegen_context.files_to_link.clone());
        project.incremental_info.as_mut().unwrap().write_updates();

        return Ok((codegen_context, diagnostics));
    }

    // Slow path: build a fresh incremental map from the codegen output.
    let mut incremental_map =
        ProjectIncrementalMap::new(&incremental_directory, compiled_styles_dir.is_none());
    for (modname, top_level) in &codegen_context.module_context.top_level_modules {
        if modname == "extern" {
            continue;
        }
        incremental_map.add_global_set_function(
            top_level
                .read()
                .unwrap()
                .get_top_level()
                .unwrap()
                .globals_info
                .globals_set_name
                .to_string(false),
        );
    }

    let modules = codegen_context.module_context.top_level_modules.clone();
    for (modname, top_level_module) in modules {
        if modname == "extern" {
            continue;
        }

        let project_file = ProjectFile::from_top_level_module(
            Arc::clone(&top_level_module),
            true,
            compiled_styles_dir.is_none(),
        );

        if !diagnostics.has_errors() {
            codegen_context.init_module_globals(&top_level_module);
            top_level_module
                .read()
                .unwrap()
                .get_top_level()
                .unwrap()
                .emit_ir(objects_directory.join(format!("{}.ir", project_file.file_id)));
            top_level_module
                .read()
                .unwrap()
                .get_top_level()
                .unwrap()
                .output_binary(
                    target.clone(),
                    objects_directory.join(format!("{}.o", project_file.file_id)),
                );
        }

        incremental_map.add_file(project_file);
    }

    incremental_map.add_linked_files(target.to_triple(), codegen_context.files_to_link.clone());
    incremental_map.write_updates();

    for error in codegen_context.diagnostics.get_diagnostics() {
        diagnostics.report_diagnostic(error.clone());
    }

    project.incremental_info = Some(incremental_map);

    Ok((codegen_context, diagnostics))
}

/// Recompile a single component (one project file): parse, codegen, emit
/// its object file. The caller (`compile_project`) handles incremental-map
/// bookkeeping and progress ticks.
fn compile_component(
    peko_root: &Path,
    target: PekoTarget,
    component_file: ProjectFile,
    compilation_root: PathBuf,
    objects_directory: PathBuf,
    compiled_styles_dir: Option<PathBuf>,
    asset_debug_dir: Option<PathBuf>,
    preloaded_modules: Option<IndexMap<String, Arc<RwLock<CodegenModule>>>>,
    bundle_id: Option<String>,
) -> PekoResult<(PekoCodegenContext, DiagnosticList)> {
    let (asts, mut diagnostics) = parse_peko_source(
        component_file.get_path(),
        std::fs::read_to_string(component_file.get_path()).unwrap(),
    );

    let mut codegen_context = PekoCodegenContext::new(
        target.clone(),
        component_file.get_path(),
        true,
        compilation_root.clone(),
    );
    codegen_context.application_id = bundle_id;
    codegen_context.compiled_styles_folder = compiled_styles_dir;
    codegen_context.asset_debug_folder = asset_debug_dir;
    if let Some(preloaded) = preloaded_modules {
        codegen_context.module_context.load_modules(preloaded);
    }

    super::load_external_modules!(codegen_context, peko_root, Some(&compilation_root));
    codegen_context.windowsgui = !target.console;

    // The "Big 3" import asts (runtime / standard / console) live at the
    // front of the parsed list

    let this_module = Arc::new(RwLock::new(CodegenModule::new_top_level(
        component_file.module_name.clone(),
        component_file.get_path(),
        None,
        codegen_context.llvm_context,
    )));
    codegen_context
        .module_context
        .top_level_modules
        .insert(component_file.module_name.clone(), this_module.clone());

    codegen_context
        .module_context
        .move_to_module(this_module.clone(), false, false);

    for ast in asts.iter() {
        ast.build_value(&mut codegen_context);
    }

    codegen_context.init_module_globals(&this_module);

    if !codegen_context.diagnostics.has_errors() && !diagnostics.has_errors() {
        this_module
            .read()
            .unwrap()
            .get_top_level()
            .unwrap()
            .output_binary(
                target,
                objects_directory.join(format!("{}.o", component_file.file_id)),
            );
    }

    for error in codegen_context.diagnostics.get_diagnostics() {
        diagnostics.report_diagnostic(error.clone());
    }

    Ok((codegen_context, diagnostics))
}

/// Compile every changed component of `project` for `target`, then link
/// the result into an executable (or shared library when `link_shared`).
///
/// The returned tuple's first element is the imported-styles map for the
/// caller's bundling phase; the second is a `Some(diagnostics)` when an
/// error halted the build (and the caller should not proceed to link),
/// `None` on a clean link.
///
/// Progress reporting: on incremental rebuilds, the function calls
/// `progress.set_total(...)` with `recompiles + rechecks + 1 (link)` and
/// ticks per file. On first build (no incremental info yet), no total is
/// set (the caller should leave the bar in spinner mode for the duration
/// of the entrypoint codegen).
///
/// `asset_debug_dir` is forwarded to the codegen context's
/// [`PekoCodegenContext::asset_debug_folder`]. Pass `Some(dir)` for debug
/// runs that serve assets from `dir` on disk, or `None` for normal builds
/// that serve assets from the bundle.
///
/// `entitlements` is forwarded to the linker. Pass `Some(path)` to embed
/// the entitlements plist as a Mach-O section at link time (used for iOS
/// simulator bundles), or `None` for targets that do not embed
/// entitlements at link time.
#[allow(clippy::too_many_arguments)]
pub fn compile_project(
    peko_root: &Path,
    project: &mut PekoProject,
    target: PekoTarget,
    incremental_directory: PathBuf,
    binary_output: Option<PathBuf>,
    link_shared: bool,
    mut linked_files: Vec<PathBuf>,
    compiled_styles_dir: Option<PathBuf>,
    asset_debug_dir: Option<PathBuf>,
    preloaded_modules: Option<IndexMap<String, Arc<RwLock<CodegenModule>>>>,
    entitlements: Option<PathBuf>,
    progress: &dyn ProgressSink,
) -> PekoResult<(HashMap<PathBuf, String>, Option<DiagnosticList>)> {
    let mut file_diagnostics = DiagnosticList::new();
    let project_root = project.get_root().to_path_buf();
    let mut imported_styles = HashMap::new();

    let objects_directory = incremental_directory
        .join("objects")
        .join(target.operating_system.to_string())
        .join(target.architecture.to_string());

    if project.incremental_info.is_none() || !objects_directory.exists() {
        // First build: spinner mode (file count not yet known).
        let entry_file = project.get_entrypoint().to_path_buf();

        let (context, diagnostics) = initialize_incremental_for_target(
            peko_root,
            project,
            incremental_directory.clone(),
            target.clone(),
            entry_file,
            project_root.clone(),
            compiled_styles_dir,
            asset_debug_dir,
            preloaded_modules.clone(),
            progress,
        )?;
        imported_styles.extend(context.imported_styles.clone());

        if diagnostics.has_errors() {
            return Ok((imported_styles, Some(diagnostics)));
        }
        file_diagnostics.extend(diagnostics);
    } else {
        // Incremental rebuild: known file counts up-front.
        let mut files_to_recompile = HashMap::new();
        let mut files_to_recheck = HashMap::new();
        let mut removed_files = false;

        // A removed file invalidates every module that imported it (they
        // need to be recompiled so dangling symbol references don't linger).
        for removed_file in project
            .incremental_info
            .as_mut()
            .unwrap()
            .get_removed_files()
        {
            removed_files = true;
            project
                .incremental_info
                .as_mut()
                .unwrap()
                .remove_global_set_function(removed_file.global_set_id);
            for recheck in removed_file.rechecks {
                files_to_recompile.insert(recheck.file_id.clone(), (recheck.clone(), None));
            }
        }

        // A changed file is recompiled; its dependents are only rechecked
        // (re-simulated) so any signature breakage surfaces without forcing
        // a full rebuild.
        for file in project
            .incremental_info
            .as_mut()
            .unwrap()
            .get_changed_files()
        {
            for recheck in &file.rechecks {
                if !files_to_recompile.contains_key(&recheck.file_id) {
                    files_to_recheck.insert(recheck.file_id.clone(), recheck.clone());
                }
            }
            files_to_recompile.insert(file.file_id.clone(), (file.clone(), None));
        }

        // Initial known file count: rechecks + recompiles + 1 for link.
        // Use add_to_total (not set_total) so this composes with whatever
        // outer phase is driving the bar, bundlers call this in a loop
        // and we want each call to extend the bar's length, not reset it.
        progress.add_to_total((files_to_recheck.len() + files_to_recompile.len() + 1) as u64);

        // Rechecks first: if any fails, abort before recompilation.
        let mut rechecks_failed = false;
        for (_, file_recheck) in &files_to_recheck {
            progress.tick(&format!(
                "Type-checking {}",
                file_recheck.get_path().display()
            ));

            let outcome = super::test(
                peko_root,
                target.clone(),
                file_recheck.get_path(),
                project_root.clone(),
            )?;

            if outcome.diagnostics.has_errors() {
                rechecks_failed = true;
            }

            if !outcome.diagnostics.get_diagnostics().is_empty() {
                file_diagnostics.extend(outcome.diagnostics);
            }
        }

        if !rechecks_failed {
            let mut new_files_added = false;
            while !files_to_recompile.is_empty() {
                let files_to_recompile_iter = files_to_recompile.clone();
                files_to_recompile.clear();

                for (_, (recompile, global_sets_name)) in files_to_recompile_iter {
                    progress.tick(&format!("Compiling {}", recompile.get_path().display()));

                    let (context, diagnostics) = compile_component(
                        peko_root,
                        target.clone(),
                        recompile.clone(),
                        project_root.clone(),
                        objects_directory.clone(),
                        compiled_styles_dir.clone(),
                        asset_debug_dir.clone(),
                        preloaded_modules.clone(),
                        match &project.ui_project_info {
                            Some(ui_info) => Some(ui_info.bundle_id.clone()),
                            None => None,
                        },
                    )?;
                    imported_styles.extend(context.imported_styles.clone());

                    // Walk the module's "imported_by" list to extend this
                    // file's rechecks list with any newly-discovered
                    // importers.
                    for file in &mut project.incremental_info.as_mut().unwrap().files {
                        if file.file_id != recompile.file_id {
                            continue;
                        }
                        for imported_by in &context.module_context.top_level_modules
                            [&recompile.module_name]
                            .read()
                            .unwrap()
                            .get_top_level()
                            .as_ref()
                            .unwrap()
                            .imported_by
                        {
                            let imported_file = ProjectFile::from_top_level_module(
                                imported_by.clone(),
                                true,
                                compiled_styles_dir.is_none(),
                            );
                            if file
                                .rechecks
                                .iter()
                                .any(|recheck| recheck.file_id == imported_file.file_id)
                            {
                                continue;
                            }
                            file.rechecks.push(imported_file);
                        }
                    }

                    project
                        .incremental_info
                        .as_mut()
                        .unwrap()
                        .add_linked_files(target.to_triple(), context.files_to_link.clone());

                    if diagnostics.has_errors() || has_warnings(&diagnostics) {
                        file_diagnostics.extend(diagnostics);
                    }

                    if let Some(global_sets_id) = global_sets_name {
                        project
                            .incremental_info
                            .as_mut()
                            .unwrap()
                            .add_file(recompile.clone());
                        project
                            .incremental_info
                            .as_mut()
                            .unwrap()
                            .add_global_set_function(global_sets_id);
                    }

                    // Discover newly-imported modules and queue them for
                    // recompilation. Revise the bar total upward to include
                    // them.
                    let mut newly_queued: u64 = 0;
                    for (modname, top_level_module) in &context.module_context.top_level_modules {
                        if modname == "extern" {
                            continue;
                        }

                        let module_project_file = ProjectFile::from_top_level_module(
                            Arc::clone(top_level_module),
                            true,
                            compiled_styles_dir.is_none(),
                        );
                        if !project
                            .incremental_info
                            .as_ref()
                            .unwrap()
                            .is_file_tracked(&module_project_file.file_id)
                        {
                            new_files_added = true;
                            files_to_recompile.insert(
                                module_project_file.file_id.clone(),
                                (
                                    module_project_file,
                                    Some(
                                        top_level_module
                                            .read()
                                            .unwrap()
                                            .get_top_level()
                                            .unwrap()
                                            .globals_info
                                            .globals_set_name
                                            .to_string(false),
                                    ),
                                ),
                            );
                            newly_queued += 1;
                        }
                    }
                    if newly_queued > 0 {
                        progress.add_to_total(newly_queued);
                    }
                }
            }

            // If the file set changed, rebuild the globals_set object so
            // it picks up new / removed global initializers.
            if new_files_added || removed_files {
                let mut codegen_context = PekoCodegenContext::new(
                    target.clone(),
                    PathBuf::new(),
                    true,
                    project_root.clone(),
                );
                match &project.ui_project_info {
                    Some(ui_info) => {
                        codegen_context.application_id = Some(ui_info.bundle_id.clone())
                    }
                    None => {}
                }
                if let Some(preloaded) = preloaded_modules {
                    codegen_context.module_context.load_modules(preloaded);
                }

                let globals_set = codegen_context.init_all_globals_specified(
                    project.incremental_info.as_ref().unwrap().get_global_sets(),
                );

                globals_set
                    .read()
                    .unwrap()
                    .get_top_level()
                    .unwrap()
                    .output_binary(target.clone(), objects_directory.join("__globals_set.o"));
            }
        }
    }

    if file_diagnostics.has_errors() {
        return Ok((imported_styles, Some(file_diagnostics)));
    }

    // Gather every object file the linker needs.
    if project
        .incremental_info
        .as_ref()
        .unwrap()
        .linked_files
        .contains_key(&target.to_triple())
    {
        linked_files.extend(
            project.incremental_info.as_ref().unwrap().linked_files[&target.to_triple()].clone(),
        );
    }
    for file in project
        .incremental_info
        .as_ref()
        .unwrap()
        .get_tracked_files()
    {
        linked_files.push(objects_directory.join(format!("{}.o", file.file_id)));
    }

    let Some(sysroot) = resolve_sysroot(peko_root, &target) else {
        return Ok((imported_styles, None));
    };

    progress.tick("Linking");
    peko_llvm::linker::lld_link(
        target,
        objects_directory.join("__globals_set.o"),
        linked_files,
        sysroot,
        binary_output,
        link_shared,
        entitlements,
    );

    project.incremental_info.as_mut().unwrap().write_updates();

    Ok((imported_styles, None))
}

/// Resolve the sysroot directory under `peko_root/Compiler/toolchains/` for
/// `target`. Returns `None` when the target's OS/architecture combination
/// isn't supported (e.g. the i686 architecture, no current toolchain).
fn resolve_sysroot(peko_root: &Path, target: &PekoTarget) -> Option<PathBuf> {
    use peko_core::target::{Architecture, OperatingSystem};

    let toolchains = peko_root.join("Compiler/toolchains");
    Some(match target.operating_system {
        OperatingSystem::Android => toolchains.join("android"),
        OperatingSystem::Windows => toolchains.join("windows"),
        OperatingSystem::IOS => match target.architecture {
            Architecture::Arm => toolchains.join("ios/arm64"),
            Architecture::X86_64 => toolchains.join("ios/x86_64"),
            _ => return None,
        },
        OperatingSystem::Linux => match target.architecture {
            Architecture::Arm => toolchains.join("linux/arm"),
            Architecture::X86_64 => toolchains.join("linux/x86_64"),
            _ => return None,
        },
        OperatingSystem::MacOS => match target.architecture {
            Architecture::Arm => toolchains.join("macos/arm64"),
            Architecture::X86_64 => toolchains.join("macos/x86_64"),
            _ => return None,
        },
        _ => return None,
    })
}

/// `DiagnosticList::has_warnings()` substitute. (The current peko_core
/// surface exposes `has_errors()`; this scans for warnings directly.)
fn has_warnings(diagnostics: &DiagnosticList) -> bool {
    diagnostics
        .get_diagnostics()
        .iter()
        .any(|diag| matches!(diag.diagnostic_type, DiagnosticType::Warning))
}
