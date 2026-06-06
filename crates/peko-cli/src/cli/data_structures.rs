//! Data structures used during CLI argument parsing.
//!
//! The cli is invoked as `peko <subcommand> [args...] [--flag] [--flag=value]
//! [--flag value]`. After parsing, positional arguments end up in
//! `CLIInfo::arguments` (with `arguments[0]` being the subcommand), and named
//! flags end up in a [`Flags`] keyed by flag name.
//!
//! A flag may carry a value or not: `--release` is a bare flag,
//! `--version=v0.1.0` is a flag with a value, and `--output build/peko` is
//! a flag whose value sits in the next argv slot. The wrapping
//! `Option<String>` in [`Flags`] distinguishes "flag absent", "flag present
//! without value", and "flag present with value" (`Some(_)` vs. `None` vs.
//! the key being missing entirely).

use std::collections::HashMap;

/// Map of flag names to optional values from the command line.
///
/// A missing key means the flag was not supplied. A present key with `None`
/// means the flag was supplied as a bare switch. A present key with
/// `Some(value)` means the flag was supplied with a value via any of the
/// accepted syntaxes (`--flag=value`, `--flag value`, `--flag =value`, etc.).
#[derive(Clone, Debug, Default)]
pub struct Flags {
    flags: HashMap<String, Option<String>>,
}

impl Flags {
    /// Builds an empty `Flags`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts or overwrites a flag's value.
    ///
    /// Pass `None` for the value to record a bare switch.
    pub fn set_flag(&mut self, name: impl AsRef<str>, value: Option<impl ToString>) {
        self.flags
            .insert(name.as_ref().to_owned(), value.map(|v| v.to_string()));
    }

    /// Returns `true` if the flag was supplied on the command line, regardless
    /// of whether it carried a value.
    pub fn has_flag(&self, name: impl AsRef<str>) -> bool {
        self.flags.contains_key(name.as_ref())
    }

    /// Returns the value associated with `name`, if any.
    ///
    /// The outer `Option` reflects whether the flag was supplied; the inner
    /// `Option` reflects whether a value was supplied with it. The two cases
    /// are flattened here because callers almost always care about the value
    /// itself rather than the absent/bare distinction.
    pub fn get_flag(&self, name: impl AsRef<str>) -> Option<String> {
        self.flags.get(name.as_ref()).and_then(Clone::clone)
    }

    /// Removes a flag and returns its prior entry, if any.
    pub fn remove_flag(&mut self, name: impl AsRef<str>) -> Option<Option<String>> {
        self.flags.remove(name.as_ref())
    }
}

impl From<HashMap<String, Option<String>>> for Flags {
    fn from(flags: HashMap<String, Option<String>>) -> Self {
        Self { flags }
    }
}
