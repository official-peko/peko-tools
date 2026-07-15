//! Top-level CLI plumbing: argument parsing, environment validation, and
//! the Peko-root health checks.
//!
//! [`CLIInfo`] is the central value most commands receive. It holds the
//! parsed flags, positional arguments, the invoking executable name, and a
//! resolved path to the user's Peko root directory (the directory pointed at
//! by the `PEKO_ROOT_PATH` environment variable).
//!
//! Sub-modules:
//!
//! - [`data_structures`]: the [`data_structures::Flags`] type backing
//!   parsed `--flag` / `--flag=value` arguments.
//! - [`reporting`]: the unified [`reporting::Reporter`] that every command
//!   uses for user-facing output.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use merkle_hash::{Algorithm, MerkleTree};
use thiserror::Error;

pub mod data_structures;
pub mod reporting;

/// One parse-time or environment-validation error from [`CLIInfo::new`].
///
/// `CLIInfo::new` accumulates every error it finds and returns them all at
/// once via `Err(Vec<CliError>)`, so the caller can show the user every
/// problem in a single pass rather than fixing them one-at-a-time. Each
/// variant's `Display` formatting is the user-facing message that the
/// cli's reporter renders.
#[derive(Debug, Error)]
pub enum CliError {
    /// A flag was supplied with the `--flag=` or `--flag =` syntax but the
    /// value after the equals sign was missing (no following argv slot).
    #[error("syntax error when providing value for flag {0}")]
    FlagSyntax(String),

    /// A value-taking flag was written space-separated (`--flag value`) but no
    /// value followed it: it was the last token, or the next token was itself
    /// another flag.
    #[error("flag `--{0}` requires a value")]
    MissingValue(String),

    /// `PEKO_ROOT_PATH` was unset or unreadable.
    #[error(
        "could not find variable 'PEKO_ROOT_PATH' in the env. \
         Please set this variable in order to use the compiler."
    )]
    PekoRootUnset,

    /// `PEKO_ROOT_PATH` pointed at a path that does not exist.
    #[error(
        "the Peko root directory {0} pointed to by the 'PEKO_ROOT_PATH' \
         in the env does not exist"
    )]
    PekoRootMissing(PathBuf),

    /// `PEKO_ROOT_PATH` pointed at something that is not a directory.
    #[error(
        "the Peko root directory {0} pointed to by the 'PEKO_ROOT_PATH' \
         in the env is not a directory"
    )]
    PekoRootNotDirectory(PathBuf),
}

/// Parsed command-line invocation plus environment context.
pub struct CLIInfo {
    /// Named flags collected from the invocation.
    pub flags: data_structures::Flags,
    /// Positional arguments. `arguments[0]` is the subcommand.
    pub arguments: Vec<String>,
    /// The invoking executable name (argv[0]).
    pub executable: String,
    /// Path to the user's Peko root directory (`PEKO_ROOT_PATH`).
    peko_root: PathBuf,
}

impl CLIInfo {
    /// Parse the process's CLI invocation.
    ///
    /// `flag_prefixes` enumerates which argv-token leading substrings mark a
    /// flag. The cli passes `["-", "--"]`, matching short and long forms.
    ///
    /// `value_flags` names the invoked subcommand's value-taking flags. When
    /// one of these is written space-separated with no `=` (`--flag value`),
    /// the following argv token is consumed as its value; if no value follows
    /// (end of argv, or the next token is itself a flag) a [`CliError::MissingValue`]
    /// is recorded. Flags not listed here keep the historical behavior: a bare
    /// `--flag value` leaves `value` as a positional.
    ///
    /// On success, returns a populated [`CLIInfo`]. On failure, returns
    /// every error encountered during parsing and environment validation so
    /// the caller can show them all at once.
    pub fn new(flag_prefixes: Vec<String>, value_flags: &[&str]) -> Result<CLIInfo, Vec<CliError>> {
        let mut errors = Vec::new();
        let cli_arguments: Vec<String> = std::env::args().collect();

        let mut flags = HashMap::new();
        let mut arguments = Vec::new();

        let mut index = 0;
        while index < cli_arguments.len() {
            // Check whether the current arg starts with any of the flag
            // prefixes. If so, strip the prefix and continue past it (the
            // historical `+ 1` offset accounts for the fact that the `--`
            // long form matches the `-` short prefix first, leaving the
            // second `-` to strip).
            let mut is_flag = false;
            let mut flag = String::new();
            for flag_prefix in &flag_prefixes {
                if cli_arguments[index].starts_with(flag_prefix) {
                    is_flag = true;
                    flag = cli_arguments[index][flag_prefix.len() + 1..].to_string();
                    break;
                }
            }

            // Plain positional: collect and move on.
            if !is_flag {
                arguments.push(cli_arguments[index].clone());
                index += 1;
                continue;
            }

            // The flag's name is everything up to an `=`, or the whole token.
            let flag_name = match flag.split_once('=') {
                Some((name, _)) => name.to_owned(),
                None => flag.clone(),
            };

            // Resolve the flag's value across the supported syntaxes:
            //   --flag=value     value sits in the same token
            //   --flag= value    value sits in the next token
            //   --flag = value   value sits in the token after `=`
            //   --flag =value    value sits in the next token after `=`
            //   --flag           no value
            let flag_value = if let Some((_, after_eq)) = flag.split_once('=') {
                if after_eq.is_empty() {
                    // --flag= value  (value is the next argv slot)
                    if index + 1 >= cli_arguments.len() {
                        errors.push(CliError::FlagSyntax(flag_name.clone()));
                        None
                    } else {
                        index += 1;
                        Some(cli_arguments[index].clone())
                    }
                } else {
                    // --flag=value
                    Some(after_eq.to_owned())
                }
            } else if index + 1 < cli_arguments.len() && cli_arguments[index + 1] == "=" {
                // --flag = value
                index += 1;
                if index + 1 >= cli_arguments.len() {
                    errors.push(CliError::FlagSyntax(flag_name.clone()));
                    None
                } else {
                    index += 1;
                    Some(cli_arguments[index].clone())
                }
            } else if index + 1 < cli_arguments.len() && cli_arguments[index + 1].starts_with('=') {
                // --flag =value
                index += 1;
                let mut value = cli_arguments[index].clone();
                value.remove(0);
                Some(value)
            } else if value_flags.contains(&flag_name.as_str()) {
                // --flag value  (space-separated value for a value-taking flag).
                // Consume the next token unless it is missing or is itself a
                // flag, in which case the value is missing.
                let next_is_flag = index + 1 < cli_arguments.len()
                    && flag_prefixes
                        .iter()
                        .any(|prefix| cli_arguments[index + 1].starts_with(prefix));
                if index + 1 >= cli_arguments.len() || next_is_flag {
                    errors.push(CliError::MissingValue(flag_name.clone()));
                    None
                } else {
                    index += 1;
                    Some(cli_arguments[index].clone())
                }
            } else {
                None
            };

            flags.insert(flag_name, flag_value);
            index += 1;
        }

        // Resolve the Peko root: the PEKO_ROOT_PATH env var when set, otherwise
        // the default ~/.Peko. Existence and health are enforced per-command by
        // perform_root_checkup, so `setup` (which creates the root) can run
        // before it exists.
        let peko_root = match std::env::var("PEKO_ROOT_PATH") {
            Ok(value) if !value.is_empty() => PathBuf::from(value),
            _ => dirs::home_dir()
                .map(|home| home.join(".Peko"))
                .unwrap_or_else(|| PathBuf::from(".Peko")),
        };

        if !errors.is_empty() {
            return Err(errors);
        }

        let executable = arguments.remove(0);
        Ok(CLIInfo {
            flags: data_structures::Flags::from(flags),
            arguments,
            executable,
            peko_root,
        })
    }

    /// The path to the user's Peko root, resolved from `PEKO_ROOT_PATH`.
    pub fn get_peko_root(&self) -> &Path {
        &self.peko_root
    }

    /// Verify the root and its `.roothash` digest match the on-disk state.
    ///
    /// Returns `false` if:
    /// 1. The root or its base folders don't exist.
    /// 2. The root hash file (`.roothash`) doesn't exist.
    /// 3. The recorded hash doesn't match the recomputed hash.
    ///
    /// Used by `check` to detect manual tampering or corruption of the
    /// installed compiler / packages directories.
    pub fn perform_deep_root_checkup(&self) -> bool {
        if !self.peko_root.exists() {
            return false;
        }
        let Some(full_root_hash) = self.compute_root_hash() else {
            return false;
        };
        let hash_path = self.peko_root.join(".roothash");
        if !hash_path.exists() {
            return false;
        }
        match std::fs::read(&hash_path) {
            Ok(recorded) => recorded == full_root_hash,
            Err(_) => false,
        }
    }

    /// Cheap existence check on the root and its two primary subfolders.
    ///
    /// Faster than [`perform_deep_root_checkup`] but doesn't verify file
    /// contents; only that `Compiler/` and `Packages/` exist under the
    /// root.
    ///
    /// [`perform_deep_root_checkup`]: CLIInfo::perform_deep_root_checkup
    pub fn perform_root_checkup(&self) -> bool {
        self.peko_root.exists()
            && self.peko_root.join("Compiler").exists()
            && self.peko_root.join("Packages").exists()
    }

    /// Recompute and persist the root's merkle hash to `.roothash`.
    ///
    /// Returns `true` on success. Returns `false` if the root or its base
    /// folders don't exist, if either subtree fails to hash, or if the
    /// resulting hash file can't be written.
    pub fn create_root_hash(&self) -> bool {
        if !self.perform_root_checkup() {
            return false;
        }
        let Some(full_root_hash) = self.compute_root_hash() else {
            return false;
        };
        std::fs::write(self.peko_root.join(".roothash"), full_root_hash).is_ok()
    }

    /// Compute a merkle hash over the contents of the Peko root, **excluding the
    /// large vendored directories under `Compiler/`**: `toolchains`, `llvm18`,
    /// `java`, and `bin`. These hold the platform SDKs, the bundled LLVM/JDK, and
    /// the compiler binaries — several gigabytes of files that aren't
    /// user-editable state. Hashing them dominated `peko check` (walking ~7 GB)
    /// and invalidated the cached hash on every toolchain install. What remains
    /// (`Compiler/include`, `Compiler/bundling`, and `Packages/`) is small, so the
    /// health check stays fast while still covering the state a user can change.
    ///
    /// Returns None if either path is missing or contains non-UTF-8 bytes
    /// (the merkle library requires `&str`).
    fn compute_root_hash(&self) -> Option<Vec<u8>> {
        // Vendored SDKs and binaries: large, not user-editable, skipped from the
        // hash so `peko check` doesn't walk gigabytes of files.
        const SKIP_COMPILER_CHILDREN: [&str; 4] = ["toolchains", "llvm18", "java", "bin"];
        let compiler_path = self.peko_root.join("Compiler");
        let packages_path = self.peko_root.join("Packages");

        // Collect Compiler's immediate children, sorted by name so the resulting
        // hash is order-independent. Skip the large vendored directories.
        let mut compiler_children: Vec<PathBuf> = std::fs::read_dir(&compiler_path)
            .ok()?
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|name| !SKIP_COMPILER_CHILDREN.contains(&name))
                    .unwrap_or(true)
            })
            .collect();
        compiler_children.sort();

        // Hash each surviving child individually. For files this is a
        // single-file merkle tree (which `merkle_hash` handles fine); for
        // directories it's a recursive tree.
        let mut combined: Vec<u8> = Vec::new();
        for child in &compiler_children {
            let tree = MerkleTree::builder(child.to_str()?)
                .algorithm(Algorithm::Blake3)
                .hash_names(false)
                .build()
                .ok()?;
            combined.extend_from_slice(&tree.root.item.hash);
        }

        // Append the Packages tree hash. Packages doesn't need filtering.
        let packages_tree = MerkleTree::builder(packages_path.to_str()?)
            .algorithm(Algorithm::Blake3)
            .hash_names(false)
            .build()
            .ok()?;
        combined.extend_from_slice(&packages_tree.root.item.hash);

        Some(combined)
    }
}
