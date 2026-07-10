//! Apple SDK provisioning through xcrun. macOS host only: Apple's licensing
//! restricts building for macOS/iOS to a macOS machine, so on other hosts these
//! are no-ops and Apple targets are unavailable.

use super::layout::Layout;
use super::{Result, SetupError};

/// The resolved Apple SDK paths, recorded in the versions manifest.
#[derive(Debug, Clone)]
pub struct AppleSdkPaths {
    pub macos: String,
    pub ios_device: String,
    pub ios_sim: String,
}

#[cfg(target_os = "macos")]
pub fn detect() -> Result<AppleSdkPaths> {
    Ok(AppleSdkPaths {
        macos: sdk_path("macosx")?,
        ios_device: sdk_path("iphoneos")?,
        ios_sim: sdk_path("iphonesimulator")?,
    })
}

#[cfg(target_os = "macos")]
fn sdk_path(sdk: &str) -> Result<String> {
    let output = std::process::Command::new("xcrun")
        .args(["--sdk", sdk, "--show-sdk-path"])
        .output()
        .map_err(|e| SetupError::io("run xcrun", e))?;
    if !output.status.success() {
        return Err(SetupError::Xcrun(format!(
            "xcrun --sdk {sdk} exited with {}",
            output.status
        )));
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        return Err(SetupError::Xcrun(format!("no sdk path reported for {sdk}")));
    }
    Ok(path)
}

/// Symlink the resolved SDKs into the macOS and iOS toolchain directories, where
/// the toolchain descriptors reference them by name (MacOSX.sdk / iPhoneOS.sdk).
#[cfg(target_os = "macos")]
pub fn symlink(layout: &Layout, paths: &AppleSdkPaths) -> Result<()> {
    use std::os::unix::fs::symlink as unix_symlink;

    let links = [
        (
            layout.toolchain_dir("macos/arm64").join("MacOSX.sdk"),
            paths.macos.as_str(),
        ),
        (
            layout.toolchain_dir("macos/x86_64").join("MacOSX.sdk"),
            paths.macos.as_str(),
        ),
        (
            layout.toolchain_dir("ios/arm64").join("iPhoneOS.sdk"),
            paths.ios_device.as_str(),
        ),
        (
            layout.toolchain_dir("ios/x86_64").join("iPhoneOS.sdk"),
            paths.ios_sim.as_str(),
        ),
    ];

    for (link, target) in links {
        if let Some(parent) = link.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| SetupError::io(format!("create {}", parent.display()), e))?;
        }
        if link.symlink_metadata().is_ok() {
            std::fs::remove_file(&link)
                .map_err(|e| SetupError::io(format!("remove {}", link.display()), e))?;
        }
        unix_symlink(target, &link)
            .map_err(|e| SetupError::io(format!("symlink {}", link.display()), e))?;
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn detect() -> Result<AppleSdkPaths> {
    Err(SetupError::Xcrun(
        "apple sdk detection runs only on macos".to_string(),
    ))
}

#[cfg(not(target_os = "macos"))]
pub fn symlink(_layout: &Layout, _paths: &AppleSdkPaths) -> Result<()> {
    Ok(())
}
