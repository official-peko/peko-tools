//! Compilation target description for `peko_core`.
//!
//! This module defines the data structures that describe *what* Pekoscript
//! source is being compiled for: the target operating system, CPU
//! architecture, and a console/non-console flag. Code generation (in the
//! separate `peko_llvm` crate) consumes these to emit appropriate triples
//! and platform-specific behavior.

#[cfg(test)]
mod tests;

use std::fmt;
use std::str::FromStr;

use crate::error::{PekoError, PekoResult};

/// Operating system families that Pekoscript can target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperatingSystem {
    MacOS,
    Windows,
    Linux,
    Android,
    IOS,
    Unknown,
}

impl OperatingSystem {
    /// Parses an operating system from its lowercase canonical name.
    ///
    /// Unknown names map to [`OperatingSystem::Unknown`]; this conversion is
    /// infallible by design so that downstream consumers can decide what to
    /// do about unrecognized targets rather than having to handle a parse
    /// error.
    ///
    /// # Examples
    ///
    /// ```
    /// use peko_core::target::OperatingSystem;
    ///
    /// assert_eq!(OperatingSystem::from_name("linux"), OperatingSystem::Linux);
    /// assert_eq!(OperatingSystem::from_name("plan9"), OperatingSystem::Unknown);
    /// ```
    #[must_use]
    pub fn from_name(name: &str) -> Self {
        match name {
            "macos" => Self::MacOS,
            "windows" => Self::Windows,
            "linux" => Self::Linux,
            "android" => Self::Android,
            "ios" => Self::IOS,
            _ => Self::Unknown,
        }
    }

    /// Returns the canonical lowercase name of this operating system.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::MacOS => "macos",
            Self::Windows => "windows",
            Self::Linux => "linux",
            Self::Android => "android",
            Self::IOS => "ios",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for OperatingSystem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// CPU architectures that Pekoscript can target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Architecture {
    Arm,
    X86_64,
    Unknown,
}

impl Architecture {
    /// Parses an architecture from its lowercase canonical name.
    ///
    /// Unknown names map to [`Architecture::Unknown`]; like
    /// [`OperatingSystem::from_name`], this is infallible by design.
    ///
    /// # Examples
    ///
    /// ```
    /// use peko_core::target::Architecture;
    ///
    /// assert_eq!(Architecture::from_name("x86_64"), Architecture::X86_64);
    /// assert_eq!(Architecture::from_name("riscv"), Architecture::Unknown);
    /// ```
    #[must_use]
    pub fn from_name(name: &str) -> Self {
        match name {
            "arm" => Self::Arm,
            "x86_64" => Self::X86_64,
            _ => Self::Unknown,
        }
    }

    /// Returns the canonical lowercase name of this architecture.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Arm => "arm",
            Self::X86_64 => "x86_64",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for Architecture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Full description of a Pekoscript compilation target.
///
/// A `PekoTarget` is the triple of operating system, CPU architecture, and a
/// flag indicating whether the program is a console application (relevant
/// primarily on Windows, where it controls subsystem selection).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PekoTarget {
    pub operating_system: OperatingSystem,
    pub architecture: Architecture,
    pub console: bool,
}

impl PekoTarget {
    /// Constructs a target from its components.
    #[must_use]
    pub fn new(
        operating_system: OperatingSystem,
        architecture: Architecture,
        console: bool,
    ) -> Self {
        Self {
            operating_system,
            architecture,
            console,
        }
    }

    /// Parses a target description of the form `os-arch` or `os-arch-console`.
    ///
    /// Unknown operating system and architecture names map to `Unknown`
    /// rather than failing (see [`OperatingSystem::from_name`] and
    /// [`Architecture::from_name`]). The presence of any third dash-separated
    /// component enables the console flag.
    ///
    /// # Errors
    ///
    /// Returns [`PekoError::InvalidTargetDescriptor`] when the input does not
    /// contain at least one `-` separator (i.e. when the `os-arch` minimum
    /// form is not satisfied). Unknown operating system or architecture
    /// *names* within an otherwise well-formed descriptor are not errors,
    /// they map to the `Unknown` variants.
    ///
    /// # Examples
    ///
    /// ```
    /// use peko_core::target::{Architecture, OperatingSystem, PekoTarget};
    ///
    /// let t = PekoTarget::from_descriptor("linux-x86_64")?;
    /// assert_eq!(t.operating_system, OperatingSystem::Linux);
    /// assert_eq!(t.architecture, Architecture::X86_64);
    /// assert!(!t.console);
    /// # Ok::<(), peko_core::PekoError>(())
    /// ```
    pub fn from_descriptor(descriptor: &str) -> PekoResult<Self> {
        let parts: Vec<&str> = descriptor.split('-').collect();
        let [os, arch, rest @ ..] = parts.as_slice() else {
            return Err(PekoError::InvalidTargetDescriptor(descriptor.to_owned()));
        };

        Ok(Self::new(
            OperatingSystem::from_name(os),
            Architecture::from_name(arch),
            !rest.is_empty(),
        ))
    }

    /// Renders this target as an LLVM-style target triple.
    ///
    /// The format is architecture-specific and roughly follows the
    /// conventions used by LLVM. Returns an empty string for
    /// [`OperatingSystem::Unknown`] beyond the architecture prefix.
    #[must_use]
    pub fn to_triple(&self) -> String {
        // Android is hardcoded to aarch64; ignore the configured architecture.
        if matches!(self.operating_system, OperatingSystem::Android) {
            return "aarch64-unknown-linux-android19".to_owned();
        }

        let arch_prefix = match self.architecture {
            Architecture::Arm => "arm64-",
            Architecture::X86_64 => "x86_64-",
            Architecture::Unknown => "unknown-",
        };

        let os_suffix = match self.operating_system {
            OperatingSystem::IOS => "apple-ios16.4.0",
            OperatingSystem::Linux => "pc-linux-gnu",
            OperatingSystem::MacOS => "apple-darwin20.6.0",
            OperatingSystem::Windows => "pc-win32",
            OperatingSystem::Unknown => "",
            // Android handled above.
            OperatingSystem::Android => unreachable!(),
        };

        format!("{arch_prefix}{os_suffix}")
    }
}

impl Default for PekoTarget {
    /// Builds a target matching the host system.
    ///
    /// Unrecognized host operating systems map to [`OperatingSystem::Unknown`]
    /// rather than panicking. Unrecognized architectures likewise map to
    /// [`Architecture::Unknown`].
    fn default() -> Self {
        let operating_system = match std::env::consts::OS {
            "linux" => OperatingSystem::Linux,
            "macos" => OperatingSystem::MacOS,
            "windows" => OperatingSystem::Windows,
            "android" => OperatingSystem::Android,
            "ios" => OperatingSystem::IOS,
            _ => OperatingSystem::Unknown,
        };

        // Treat 32-bit x86 as X86_64; treat 64-bit ARM variants uniformly as Arm.
        let architecture = match std::env::consts::ARCH {
            "x86" | "x86_64" => Architecture::X86_64,
            "arm" | "aarch64" | "loongarch64" => Architecture::Arm,
            _ => Architecture::Unknown,
        };

        Self {
            operating_system,
            architecture,
            console: true,
        }
    }
}

impl fmt::Display for PekoTarget {
    /// Renders the target as `os-arch` or `os-arch-console`, the inverse of
    /// [`PekoTarget::from_descriptor`].
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.operating_system, self.architecture)?;
        if self.console {
            f.write_str("-console")?;
        }
        Ok(())
    }
}

impl FromStr for PekoTarget {
    type Err = PekoError;

    fn from_str(s: &str) -> PekoResult<Self> {
        Self::from_descriptor(s)
    }
}
