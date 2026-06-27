//! The registry index: one JSON line per published version.
//!
//! Resolution reads only the index. Each line carries a version's dependency
//! list, `.pkpkg` checksum, minimum compiler, platforms, and a yanked flag, so
//! a body never has to be downloaded to learn its dependencies.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::RegistryError;

/// One line of a package's index file.
///
/// The field set is the provisional CLI-side contract. It is reconciled with
/// the registry server once the platform is built.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexEntry {
    /// The package name.
    pub name: String,
    /// The exact published version.
    pub version: String,
    /// The dependency requirements, mapping name to version requirement.
    #[serde(default)]
    pub deps: BTreeMap<String, String>,
    /// The `.pkpkg` checksum download verifies against.
    pub checksum: String,
    /// The minimum compiler version the version requires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_compiler: Option<String>,
    /// The operating systems the version supports.
    #[serde(default)]
    pub platforms: Vec<String>,
    /// Whether the version has been yanked.
    #[serde(default)]
    pub yanked: bool,
}

/// Parse a JSON-lines index body into entries, skipping blank lines.
pub fn parse_index(body: &str) -> Result<Vec<IndexEntry>, RegistryError> {
    let mut entries = Vec::new();
    for (offset, line) in body.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let entry = serde_json::from_str(line).map_err(|source| RegistryError::IndexParse {
            line: offset + 1,
            source,
        })?;
        entries.push(entry);
    }
    Ok(entries)
}

/// Serialize entries into a JSON-lines body.
pub fn write_index(entries: &[IndexEntry]) -> Result<String, RegistryError> {
    let mut out = String::new();
    for entry in entries {
        let line = serde_json::to_string(entry).map_err(RegistryError::IndexSerialize)?;
        out.push_str(&line);
        out.push('\n');
    }
    Ok(out)
}
