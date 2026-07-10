//! `peko setup`: install or update the Peko development environment.
//!
//! This ports the peko-setup installer into the CLI. It lays out `~/.Peko`,
//! downloads the Compiler SDK and the linux/android toolchains from the public
//! releases, provides the Apple SDKs through xcrun (macOS host only) and the
//! Windows toolchain through xwin, installs the `std` and `pekoui` packages
//! globally, lays in the toolchain descriptors, records a versions manifest,
//! and configures PATH.
//!
//! It runs interactively for a human, or emits newline-delimited JSON progress
//! events (`--json`) that Peko Studio renders as a setup screen.

use std::path::PathBuf;
use std::process::ExitCode;

use thiserror::Error;

use crate::cli::CLIInfo;
use crate::cli::reporting::Reporter;
use crate::toolchain::version::{self, InstallManifest};

pub mod apple;
pub mod download;
pub mod extract;
pub mod github;
pub mod host;
pub mod install;
pub mod layout;
pub mod pathenv;
pub mod xwin;

use github::{Channel, GithubClient};
use host::{Host, Os};
use layout::Layout;

/// The GitHub repositories setup pulls from.
pub const TOOLS_REPO: &str = "official-peko/peko-tools";
pub const SDK_REPO: &str = "official-peko/peko-sdk-dist";

/// A setup step failed.
#[derive(Debug, Error)]
pub enum SetupError {
    #[error("unsupported host: {0}")]
    UnsupportedHost(String),

    #[error("network error: {0}")]
    Network(String),

    #[error("{url} returned HTTP {status}")]
    HttpStatus { status: u16, url: String },

    #[error("{context}: {source}")]
    Io {
        context: String,
        source: std::io::Error,
    },

    #[error("could not extract {0}")]
    Extract(String),

    #[error("{0}")]
    Process(String),

    #[error("asset not found: {0}")]
    AssetNotFound(String),

    #[error("release not found: {0}")]
    ReleaseNotFound(String),

    #[error("xcrun: {0}")]
    Xcrun(String),

    #[error("could not configure PATH: {0}")]
    PathConfig(String),
}

impl SetupError {
    /// Wrap an IO error with the operation that produced it.
    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, SetupError>;

/// Execute the `setup` subcommand.
pub async fn execute(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    match run(cli_info, reporter).await {
        Ok(message) => {
            reporter.success(message);
            ExitCode::SUCCESS
        }
        Err(e) => {
            reporter.error(format!("setup failed: {e}"));
            ExitCode::FAILURE
        }
    }
}

/// The install/update flow. Each phase reports progress; the whole thing is
/// resumable because every step is idempotent against the versions manifest.
async fn run(cli_info: &CLIInfo, reporter: &Reporter) -> Result<String> {
    let host = Host::detect()?;
    reporter.status(
        "Setup",
        format!(
            "installing the Peko environment for {} {} ({})",
            host.os_token(),
            host.arch_token(),
            host.triple
        ),
    );

    let layout = Layout::new(cli_info.get_peko_root().to_path_buf());
    if layout.is_installed() {
        reporter.info("an existing install was found; refreshing components");
    }
    layout.create_base()?;
    reporter.info(format!("prepared {}", layout.root().display()));

    let github = GithubClient::new()?;
    let sdk_channel = Channel::from_flag(cli_info.flags.get_flag("sdk-version"));
    let peko_channel = Channel::from_flag(cli_info.flags.get_flag("peko-version"));
    let force = cli_info.flags.has_flag("force");

    // Resolve both releases up front so update mode can compare tags and skip
    // the large downloads for components that have not changed.
    let sdk = github.resolve(SDK_REPO, &sdk_channel).await?;
    let tools = github.resolve(TOOLS_REPO, &peko_channel).await?;

    let existing = if layout.is_installed() {
        InstallManifest::load(layout.root()).ok()
    } else {
        None
    };
    let sdk_changed = force || existing.as_ref().is_none_or(|e| e.sdk.tag != sdk.tag_name);
    let tools_changed = force || existing.as_ref().is_none_or(|e| e.peko_tools.tag != tools.tag_name);

    // 1. The Compiler SDK (toolchains excluded) from peko-sdk-dist.
    if sdk_changed {
        reporter.status("SDK", format!("installing the Compiler {}", sdk.version()));
        let compiler_asset = sdk
            .find_asset_named("Compiler.tar.zst")
            .ok_or_else(|| SetupError::AssetNotFound("Compiler.tar.zst".to_string()))?;
        let staging = layout.root().join(".setup-staging");
        let _ = std::fs::remove_dir_all(&staging);
        install::install_archive(
            &github,
            &compiler_asset.browser_download_url,
            &staging,
            reporter,
            "Compiler",
        )
        .await?;
        extract::atomic_replace_dir(&staging.join("Compiler"), &layout.compiler())?;
        let _ = std::fs::remove_dir_all(&staging);
        reporter.info("Compiler installed");
    } else {
        reporter.info(format!("Compiler {} already up to date", sdk.version()));
    }

    // 2. Place (or refresh) the running CLI so later steps can invoke it.
    install::place_self(&layout)?;

    // 3. The linux and android toolchain payloads from peko-sdk-dist.
    let mut installed: Vec<String> = install::RELEASE_TOOLCHAINS
        .iter()
        .map(|(_, dest)| (*dest).to_string())
        .collect();
    if sdk_changed {
        reporter.status("Toolchains", "installing linux and android toolchains");
        install::install_release_toolchains(&github, &layout, &sdk, reporter).await?;
        // The shared GTK and WebKit headers the linux toolchains reference.
        reporter.status("Toolchains", "installing the linux GTK and WebKit headers");
        install::install_support(
            &github,
            &layout,
            &sdk,
            install::LINUX_SUPPORT_ASSET,
            reporter,
            "linux-gtk",
        )
        .await?;
    }

    // 4. Apple SDKs via xcrun, on a macOS host only (a cheap re-link each run).
    //    The OpenSSL archives the apple toolchains link ship separately from the
    //    xcrun SDKs, so they install with the rest of the SDK release.
    let apple_sdks = if host.os == Os::Macos {
        reporter.status("Toolchains", "linking Apple SDKs via xcrun");
        let paths = apple::detect()?;
        apple::symlink(&layout, &paths)?;
        for id in ["macos/arm64", "macos/x86_64", "ios/arm64", "ios/x86_64"] {
            installed.push(id.to_string());
        }
        if sdk_changed {
            reporter.status("Toolchains", "installing the Apple OpenSSL archives");
            install::install_support(
                &github,
                &layout,
                &sdk,
                install::APPLE_SUPPORT_ASSET,
                reporter,
                "apple-support",
            )
            .await?;
        }
        version::AppleSdks {
            macos: Some(PathBuf::from(paths.macos)),
            ios_device: Some(PathBuf::from(paths.ios_device)),
            ios_sim: Some(PathBuf::from(paths.ios_sim)),
        }
    } else {
        version::AppleSdks::default()
    };

    // 5. The Windows toolchain via xwin, opt-in and gated on accepting the
    //    Microsoft license. Its splat writes only crt/ and sdk/, so the
    //    descriptors applied next still own toolchain.toml.
    if cli_info.flags.has_flag("windows") {
        if !cli_info.flags.has_flag("accept-microsoft-license") {
            return Err(SetupError::Process(format!(
                "the Windows toolchain is distributed under the Microsoft license ({}). \
                 Re-run with --accept-microsoft-license to install it",
                xwin::MS_LICENSE_URL
            )));
        }
        if force || !xwin::is_present(&layout) {
            // The Windows toolchain is optional. A failure here (for example the
            // Microsoft manifest host being unreachable) must not abort the rest
            // of the install, so it warns and continues instead of propagating.
            reporter.status("Toolchains", "splatting the Windows SDK and CRT via xwin");
            match xwin::splat(&layout, reporter) {
                Ok(()) => {
                    reporter.status("Toolchains", "installing the WebView2 SDK headers");
                    if let Err(e) = xwin::install_webview2(&github, &layout, reporter).await {
                        reporter.warning(format!("skipped the WebView2 SDK headers: {e}"));
                    }
                }
                Err(e) => {
                    reporter.warning(format!(
                        "skipped the Windows toolchain: {e}. Re-run with --windows \
                         --accept-microsoft-license to retry it"
                    ));
                }
            }
        } else {
            reporter.info("Windows toolchain already present");
        }
    }
    // Record the windows toolchain from what is on disk, so a later run without
    // --windows does not drop a previously installed toolchain from the manifest.
    if xwin::is_present(&layout) {
        installed.push("windows".to_string());
    }

    // 6. Toolchain descriptors from peko-tools, applied last so the canonical
    //    toolchain.toml files win over any bundled inside a payload.
    if tools_changed {
        let descriptors = tools
            .find_asset_containing("toolchain-descriptors")
            .ok_or_else(|| SetupError::AssetNotFound("toolchain descriptors".to_string()))?;
        reporter.status("Toolchains", "installing toolchain descriptors");
        install::install_archive(
            &github,
            &descriptors.browser_download_url,
            &layout.compiler(),
            reporter,
            "toolchain-descriptors",
        )
        .await?;
    }

    // 7. Global packages: std and pekoui, when a component changed or when a
    //    package is missing (a deleted registry heals on the next run).
    if sdk_changed || tools_changed || !install::global_packages_present(&layout) {
        reporter.status("Packages", "installing std and pekoui globally");
        install::install_packages(&layout)?;
    }

    // 8. Record the versions manifest. This is the canonical InstallManifest the
    //    toolchain resolver reads, so setup and routing share one schema.
    let channel = match &peko_channel {
        Channel::Latest => "stable".to_string(),
        Channel::Specific(tag) => tag.clone(),
    };
    let versions = InstallManifest {
        schema: 1,
        install_root: layout.root().to_path_buf(),
        host: version::HostInfo {
            os: host.os_token().to_string(),
            arch: host.arch_token().to_string(),
            triple: host.triple.to_string(),
        },
        peko_tools: version::PekoToolsInfo {
            channel,
            tag: tools.tag_name.clone(),
            version: tools.version().to_string(),
        },
        sdk: version::ComponentInfo {
            tag: sdk.tag_name.clone(),
            version: sdk.version().to_string(),
        },
        toolchains: version::ToolchainsInfo {
            tag: sdk.tag_name.clone(),
            version: sdk.version().to_string(),
            installed,
        },
        apple_sdks,
        path_configured: true,
        updated_at: now_timestamp(),
    };
    versions
        .save(layout.root())
        .map_err(|e| SetupError::Process(format!("write versions.json: {e}")))?;

    // 9. Configure PATH, then certify the finished install.
    pathenv::configure(&layout)?;
    install::certify(&layout)?;

    if sdk_changed || tools_changed {
        Ok(format!(
            "Peko {} installed at {}. Restart your shell to pick up the updated PATH.",
            sdk.version(),
            layout.root().display()
        ))
    } else {
        Ok(format!("Peko {} is already up to date.", sdk.version()))
    }
}

/// The current time as epoch seconds, recorded in the versions manifest.
fn now_timestamp() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default()
}
