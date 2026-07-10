//! Windows toolchain provisioning. The MSVC CRT and Windows SDK come from the
//! embedded xwin library, which downloads and splats Microsoft's redistributable
//! packages. The WebView2 SDK headers come from NuGet. Both are needed to build
//! pekoui webview apps for Windows.
//!
//! Splatting Microsoft's surface requires accepting the Microsoft license, so
//! this whole path is opt-in behind a flag and gated on an explicit acceptance
//! flag.

use std::sync::Arc;

use crate::cli::reporting::Reporter;

use super::github::GithubClient;
use super::layout::Layout;
use super::{Result, SetupError, download, extract};

/// The Microsoft license the Windows SDK and CRT are distributed under.
pub const MS_LICENSE_URL: &str = "https://go.microsoft.com/fwlink/?LinkId=2086102";

/// The NuGet flat container index for the WebView2 SDK package.
const WEBVIEW2_INDEX: &str =
    "https://api.nuget.org/v3-flatcontainer/microsoft.web.webview2/index.json";

/// The Visual Studio manifest major version (17 is VS 2022) and channel.
const VS_MANIFEST_VERSION: u8 = 17;
const VS_CHANNEL: &str = "release";

/// Whether the windows toolchain is already splatted at `layout`.
pub fn is_present(layout: &Layout) -> bool {
    layout
        .toolchain_dir("windows")
        .join("crt/include")
        .is_dir()
}

/// Whether the WebView2 SDK headers are already installed at `layout`. The SDK
/// splat and the WebView2 NuGet package are separate downloads, so this is
/// checked apart from `is_present`.
pub fn webview2_present(layout: &Layout) -> bool {
    layout
        .toolchain_dir("windows")
        .join("webview2/build/native/include/WebView2.h")
        .is_file()
}

/// Convert a std path to the utf-8 path xwin uses. Install paths live under the
/// Peko root, which is utf-8 in every supported layout.
fn utf8(path: &std::path::Path) -> Result<xwin::PathBuf> {
    xwin::PathBuf::from_path_buf(path.to_path_buf())
        .map_err(|p| SetupError::Extract(format!("non utf-8 path: {}", p.display())))
}

/// Splat the MSVC CRT and Windows SDK into the windows toolchain directory. This
/// downloads roughly a gigabyte from Microsoft's CDN and runs synchronously.
/// xwin's own progress bars are hidden; the caller reports coarse status.
pub fn splat(layout: &Layout, _reporter: &Reporter) -> Result<()> {
    use xwin::{Arch, Ops, SplatConfig, Variant, WorkItem, util::ProgressTarget};

    let arches = Arch::X86_64 as u32;
    let variants = Variant::Desktop as u32;

    // The blocking agent xwin uses to reach the Microsoft CDN. Platform TLS
    // roots avoid bundling a certificate store.
    let client = {
        let tls = xwin::ureq::tls::TlsConfig::builder()
            .root_certs(xwin::ureq::tls::RootCerts::PlatformVerifier)
            .build();
        xwin::ureq::config::Config::builder()
            .tls_config(tls)
            .build()
            .new_agent()
    };

    let cache = layout.root().join(".xwin-cache");
    let ctx = xwin::Ctx::with_dir(utf8(&cache)?, ProgressTarget::Hidden, client, 5)
        .map_err(|e| SetupError::Process(format!("xwin context: {e}")))?;
    let ctx = Arc::new(ctx);

    let manifest = xwin::manifest::get_manifest(
        &ctx,
        VS_MANIFEST_VERSION,
        VS_CHANNEL,
        indicatif::ProgressBar::hidden(),
    )
    .map_err(|e| SetupError::Process(format!("xwin manifest: {e}")))?;
    let pkg_manifest =
        xwin::manifest::get_package_manifest(&ctx, &manifest, indicatif::ProgressBar::hidden())
            .map_err(|e| SetupError::Process(format!("xwin package manifest: {e}")))?;

    let pruned = xwin::prune_pkg_list(&pkg_manifest, arches, variants, false, false, None, None)
        .map_err(|e| SetupError::Process(format!("xwin prune: {e}")))?;

    let work_items: Vec<WorkItem> = pruned
        .payloads
        .into_iter()
        .map(|payload| WorkItem {
            payload: Arc::new(payload),
            progress: indicatif::ProgressBar::hidden(),
        })
        .collect();

    let output = layout.toolchain_dir("windows");
    std::fs::create_dir_all(&output)
        .map_err(|e| SetupError::io(format!("create {}", output.display()), e))?;

    // prep_splat removes only the crt and sdk subdirectories, so toolchain.toml
    // and the webview2 headers alongside them survive a re-splat.
    let splat_config = SplatConfig {
        include_debug_libs: false,
        include_debug_symbols: false,
        enable_symlinks: false,
        preserve_ms_arch_notation: false,
        use_winsysroot_style: false,
        output: utf8(&output)?,
        map: None,
        copy: true,
    };

    ctx.execute(
        pkg_manifest.packages,
        work_items,
        pruned.crt_version,
        pruned.sdk_version,
        pruned.vcr_version,
        arches,
        variants,
        Ops::Splat(splat_config),
    )
    .map_err(|e| SetupError::Process(format!("xwin splat: {e}")))?;

    let _ = std::fs::remove_dir_all(&cache);
    Ok(())
}

/// Download the WebView2 SDK from NuGet and extract its headers into
/// windows/webview2. The package is a zip; its build/native/include directory
/// carries WebView2.h, which pekoui webview needs to compile.
pub async fn install_webview2(
    github: &GithubClient,
    layout: &Layout,
    reporter: &Reporter,
) -> Result<()> {
    let index: serde_json::Value = github
        .http()
        .get(WEBVIEW2_INDEX)
        .send()
        .await
        .map_err(|e| SetupError::Network(e.to_string()))?
        .error_for_status()
        .map_err(|e| SetupError::Network(e.to_string()))?
        .json()
        .await
        .map_err(|e| SetupError::Network(e.to_string()))?;

    // The index lists versions oldest first; take the newest stable one.
    let version = index["versions"]
        .as_array()
        .and_then(|versions| {
            versions
                .iter()
                .rev()
                .filter_map(|v| v.as_str())
                .find(|v| !v.contains('-'))
        })
        .ok_or_else(|| SetupError::AssetNotFound("webview2 sdk version".to_string()))?
        .to_string();

    let url = format!(
        "https://api.nuget.org/v3-flatcontainer/microsoft.web.webview2/{version}/microsoft.web.webview2.{version}.nupkg"
    );
    let webview2_dir = layout.toolchain_dir("windows").join("webview2");
    let _ = std::fs::remove_dir_all(&webview2_dir);

    let archive =
        download::download_to_temp(github.http(), &url, |downloaded, total| {
            reporter.download_progress("webview2", downloaded, total);
        })
        .await?;
    extract::extract(archive.path(), extract::ArchiveFormat::Zip, &webview2_dir)
}
