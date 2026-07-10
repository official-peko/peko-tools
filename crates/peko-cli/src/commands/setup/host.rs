//! Host operating system and architecture detection for setup.

use super::{Result, SetupError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Macos,
    Linux,
    Windows,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    Arm64,
    X64,
}

/// The resolved host: what platform setup is running on.
#[derive(Debug, Clone, Copy)]
pub struct Host {
    pub os: Os,
    pub arch: Arch,
    /// The Rust target triple, matching the peko binary asset names.
    pub triple: &'static str,
}

impl Host {
    /// Detect the running host, or fail on an unsupported platform.
    pub fn detect() -> Result<Self> {
        let os = match std::env::consts::OS {
            "macos" => Os::Macos,
            "linux" => Os::Linux,
            "windows" => Os::Windows,
            other => return Err(SetupError::UnsupportedHost(format!("os {other}"))),
        };
        let arch = match std::env::consts::ARCH {
            "aarch64" => Arch::Arm64,
            "x86_64" => Arch::X64,
            other => return Err(SetupError::UnsupportedHost(format!("arch {other}"))),
        };
        let triple = match (os, arch) {
            (Os::Macos, Arch::Arm64) => "aarch64-apple-darwin",
            (Os::Macos, Arch::X64) => "x86_64-apple-darwin",
            (Os::Linux, Arch::Arm64) => "aarch64-unknown-linux-gnu",
            (Os::Linux, Arch::X64) => "x86_64-unknown-linux-gnu",
            (Os::Windows, Arch::X64) => "x86_64-pc-windows-msvc",
            (Os::Windows, Arch::Arm64) => {
                return Err(SetupError::UnsupportedHost(
                    "windows on arm64 is not supported".to_string(),
                ));
            }
        };
        Ok(Self { os, arch, triple })
    }

    /// The platform token used in the toolchain layout (macos/linux/windows).
    pub fn os_token(self) -> &'static str {
        match self.os {
            Os::Macos => "macos",
            Os::Linux => "linux",
            Os::Windows => "windows",
        }
    }

    /// The architecture token used in the toolchain layout (arm/x86_64).
    pub fn arch_token(self) -> &'static str {
        match self.arch {
            Arch::Arm64 => "arm",
            Arch::X64 => "x86_64",
        }
    }
}
