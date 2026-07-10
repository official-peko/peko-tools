//! The registry index: one JSON line per published version.
//!
//! Resolution reads only the index. Each line carries a version's dependency
//! list, `.pkpkg` checksum, minimum compiler, platforms, and a yanked flag, so
//! a body never has to be downloaded to learn its dependencies.

use std::collections::BTreeMap;

use serde::de::{SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use super::RegistryError;

/// One line of a package's index file.
///
/// The field set is the provisional CLI-side contract. It is reconciled with
/// the registry server once the platform is built.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexEntry {
    /// The package name.
    pub name: String,
    /// The exact published version. The registry serves this as `vers`.
    #[serde(alias = "vers")]
    pub version: String,
    /// The dependency requirements, mapping name to version requirement. The
    /// registry serves this as an array of `{name, req}`; both shapes are read.
    #[serde(default, deserialize_with = "deserialize_deps")]
    pub deps: BTreeMap<String, String>,
    /// The `.pkpkg` checksum download verifies against, normalized to
    /// `sha256:<hex>`. The registry serves the bare hex as `cksum`.
    #[serde(alias = "cksum", deserialize_with = "deserialize_checksum")]
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

/// Normalize a checksum to the canonical `sha256:<hex>` form. The registry
/// serves a bare hex digest; a value that already carries an algorithm prefix
/// is kept as is, so pack::checksum comparisons line up.
fn deserialize_checksum<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let raw = String::deserialize(deserializer)?;
    Ok(if raw.contains(':') {
        raw
    } else {
        format!("sha256:{raw}")
    })
}

/// Read a dependency set from either a `{name: req}` map or the registry's array
/// of `{name, req}` objects (empty as `[]`), yielding a name-to-requirement map.
fn deserialize_deps<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, String>, D::Error> {
    struct DepsVisitor;

    impl<'de> Visitor<'de> for DepsVisitor {
        type Value = BTreeMap<String, String>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a map of name to requirement or an array of {name, req}")
        }

        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut access: A,
        ) -> Result<Self::Value, A::Error> {
            let mut out = BTreeMap::new();
            while let Some((name, req)) = access.next_entry::<String, String>()? {
                out.insert(name, req);
            }
            Ok(out)
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
            #[derive(Deserialize)]
            struct Dep {
                name: String,
                req: String,
            }
            let mut out = BTreeMap::new();
            while let Some(dep) = access.next_element::<Dep>()? {
                out.insert(dep.name, dep.req);
            }
            Ok(out)
        }
    }

    deserializer.deserialize_any(DepsVisitor)
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
