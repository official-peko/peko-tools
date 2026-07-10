//! The install actions that run subprocesses or move files into place: placing
//! the CLI, installing the release toolchains, installing the global packages,
//! and certifying the finished install.

use std::cell::Cell;
use std::path::Path;

use crate::cli::reporting::Reporter;

use super::extract::{self, ArchiveFormat};
use super::github::{GithubClient, Release};
use super::layout::Layout;
use super::{Result, SetupError, download};

/// The toolchain payloads hosted on peko-sdk-dist, with their destination under
/// Compiler/toolchains. Apple SDKs come from xcrun and Windows from xwin, so
/// they are not here.
pub const RELEASE_TOOLCHAINS: &[(&str, &str)] = &[
    ("android.tar.zst", "android"),
    ("linux-arm.tar.zst", "linux/arm"),
    ("linux-x86_64.tar.zst", "linux/x86_64"),
];

/// The packages installed globally so every project can import them.
pub const GLOBAL_PACKAGES: &[&str] = &["std", "pekoui"];

/// The shared GTK and WebKit headers the linux toolchains reference through a
/// sibling `../gtk` path. Ships once and serves every linux arch.
pub const LINUX_SUPPORT_ASSET: &str = "linux-gtk.tar.zst";

/// The OpenSSL archives the apple toolchains link. The linux and android
/// payloads carry their own; the xcrun-based apple toolchains do not, so these
/// arrive separately: per-arch for macOS and a shared fat archive for iOS.
pub const APPLE_SUPPORT_ASSET: &str = "apple-support.tar.zst";

/// The peko binary file name for the current platform.
fn peko_binary() -> &'static str {
    if cfg!(windows) { "peko.exe" } else { "peko" }
}

/// Download `url` and extract it into `dest`, inferring the archive format.
/// Download progress is reported through `reporter` under `label`, throttled to
/// once per megabyte so JSON consumers are not flooded.
pub(super) async fn install_archive(
    github: &GithubClient,
    url: &str,
    dest: &Path,
    reporter: &Reporter,
    label: &str,
) -> Result<()> {
    let name = url.rsplit('/').next().unwrap_or(url);
    let format = ArchiveFormat::from_asset_name(name)
        .ok_or_else(|| SetupError::Extract(format!("unknown archive format: {name}")))?;
    let last_mib = Cell::new(u64::MAX);
    let archive =
        download::download_to_temp(github.http(), url, |downloaded, total| {
            let mib = downloaded / 1_048_576;
            let done = total.is_some_and(|t| downloaded >= t);
            if mib != last_mib.get() || done {
                last_mib.set(mib);
                reporter.download_progress(label, downloaded, total);
            }
        })
        .await?;
    extract::extract(archive.path(), format, dest)
}

/// Place the running peko executable at Compiler/bin/peko/peko.
pub fn place_self(layout: &Layout) -> Result<()> {
    let exe =
        std::env::current_exe().map_err(|e| SetupError::io("locate current executable", e))?;
    let bin = layout.bin_peko();
    std::fs::create_dir_all(&bin)
        .map_err(|e| SetupError::io(format!("create {}", bin.display()), e))?;
    let dest = bin.join(peko_binary());
    if exe == dest {
        return Ok(());
    }
    std::fs::copy(&exe, &dest)
        .map_err(|e| SetupError::io(format!("copy {}", dest.display()), e))?;
    make_executable(&dest)
}

/// Install the linux and android toolchain payloads from the SDK release into
/// Compiler/toolchains, returning the list of installed toolchain ids.
pub async fn install_release_toolchains(
    github: &GithubClient,
    layout: &Layout,
    sdk: &Release,
    reporter: &Reporter,
) -> Result<Vec<String>> {
    let mut installed = Vec::new();
    let staging = layout.root().join(".setup-staging-tc");
    for (asset_name, dest_rel) in RELEASE_TOOLCHAINS {
        let asset = sdk
            .find_asset_named(asset_name)
            .ok_or_else(|| SetupError::AssetNotFound(asset_name.to_string()))?;
        let _ = std::fs::remove_dir_all(&staging);
        // The archives carry their destination prefix (android/, linux/arm/...).
        install_archive(github, &asset.browser_download_url, &staging, reporter, dest_rel).await?;
        extract::atomic_replace_dir(&staging.join(dest_rel), &layout.toolchain_dir(dest_rel))?;
        installed.push((*dest_rel).to_string());
    }
    let _ = std::fs::remove_dir_all(&staging);
    Ok(installed)
}

/// Install a shared support archive from the SDK release into the toolchains
/// directory. The archive carries its own path prefix under toolchains (for
/// example `linux/gtk` or `macos/arm64/openssl_libs`), so it extracts as a
/// merge alongside the per-arch toolchains without disturbing them.
pub async fn install_support(
    github: &GithubClient,
    layout: &Layout,
    sdk: &Release,
    asset_name: &str,
    reporter: &Reporter,
    label: &str,
) -> Result<()> {
    let asset = sdk
        .find_asset_named(asset_name)
        .ok_or_else(|| SetupError::AssetNotFound(asset_name.to_string()))?;
    let toolchains = layout.compiler().join("toolchains");
    std::fs::create_dir_all(&toolchains)
        .map_err(|e| SetupError::io(format!("create {}", toolchains.display()), e))?;
    install_archive(github, &asset.browser_download_url, &toolchains, reporter, label).await
}

/// Whether every global package has a cached source under the registry. Setup
/// reinstalls when one is missing, so a deleted registry heals on the next run
/// even when no release changed.
pub fn global_packages_present(layout: &Layout) -> bool {
    let src = layout.root().join("registry").join("src");
    GLOBAL_PACKAGES.iter().all(|package| {
        std::fs::read_dir(src.join(package))
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(false)
    })
}

/// Install the global packages by running `peko add <pkg> --global`.
pub fn install_packages(layout: &Layout) -> Result<()> {
    let peko = layout.bin_peko().join(peko_binary());
    for package in GLOBAL_PACKAGES {
        let output = std::process::Command::new(&peko)
            .args(["add", package, "--global"])
            .env("PEKO_ROOT_PATH", layout.root())
            .output()
            .map_err(|e| SetupError::io(format!("run peko add {package}"), e))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(SetupError::Process(format!(
                "peko add {package} --global failed: {}",
                stderr.trim()
            )));
        }
    }
    Ok(())
}

/// Re-hash the install so `peko check` reports it healthy.
pub fn certify(layout: &Layout) -> Result<()> {
    let peko = layout.bin_peko().join(peko_binary());
    let output = std::process::Command::new(&peko)
        .args(["check", "--rehash"])
        .env("PEKO_ROOT_PATH", layout.root())
        .output()
        .map_err(|e| SetupError::io("run peko check --rehash", e))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SetupError::Process(format!(
            "peko check --rehash failed: {}",
            stderr.trim()
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .map_err(|e| SetupError::io(format!("stat {}", path.display()), e))?
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms)
        .map_err(|e| SetupError::io(format!("chmod {}", path.display()), e))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}
