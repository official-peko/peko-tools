//! Bundler dispatch: per-platform app-bundle producers.
//!
//! Each platform-specific module ([`android`], [`ios`], [`linux`],
//! [`macos`], [`windows`]) exposes its own `bundle` (and where relevant,
//! `sign`) entry point. The build command drives them all in order based
//! on the project's `target_platforms`.
//!
//! This module additionally provides:
//!
//! - [`BundleError`] / [`BundleResult`] - the typed error surface that
//!   every bundler returns through. The build command renders the error
//!   via its [`Reporter`](crate::cli::reporting::Reporter), so bundlers
//!   never write to stderr directly.
//! - [`io_at`] / [`run_tool`] - common wrappers used by every bundler to
//!   convert raw `io::Result` and `Command::status` into `BundleError`
//!   with the path / tool context attached.
//! - [`CleanupGuard`] - RAII helper that nukes a build directory on early
//!   return. Bundlers call `.commit()` on the guard before returning `Ok`.
//! - [`recursive_zip_add`] - shared helper used by the android bundler to
//!   embed assets / lib / res trees into the APK.
//! - [`regenerate_application_bundle_files`] - initializes (or, when
//!   invoked again via `--regenconfig`, recreates) the
//!   `.peko/bundling/configfiles/` tree of per-platform config templates.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use peko_core::diagnostics::DiagnosticList;
use thiserror::Error;
use zip::write::{ExtendedFileOptions, FileOptions};
use zip::{CompressionMethod, ZipWriter};

use crate::project::PekoProject;

pub mod android;
pub mod cartool;
pub mod ios;
pub mod linux;
pub mod macos;
pub mod signing;
pub mod windows;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// One failure mode for a platform bundler.
#[derive(Debug, Error)]
pub enum BundleError {
    /// An on-disk operation failed. `path` identifies the file or
    /// directory being touched when the error occurred.
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// Compilation reported errors and bundling was aborted. The caller
    /// is expected to feed the [`DiagnosticList`] back through the
    /// reporter for proper rendering.
    #[error("compilation produced errors")]
    CompileDiagnostics(DiagnosticList),

    /// An external tool (`aapt2`, `jarsigner`, `bundletool`, `llvm-rc`,
    /// `osslsigncode`, `java`, etc.) couldn't be launched (typically
    /// because the binary is missing or non-executable on this host).
    #[error("`{tool}` could not be launched: {source}")]
    ToolLaunch {
        tool: String,
        #[source]
        source: io::Error,
    },

    /// An external tool ran but exited with a non-zero status.
    #[error("`{tool}` exited with status {status}")]
    Tool { tool: String, status: ExitStatus },

    /// A zip operation failed.
    #[error("zip operation failed: {0}")]
    Zip(#[from] zip::result::ZipError),

    /// A peko_core operation surfaced an error (e.g. building the
    /// package index inside `compile_project`).
    #[error(transparent)]
    Peko(#[from] peko_core::error::PekoError),

    /// A signing step failed. The string carries the underlying context
    /// (key loading, keychain access, apple-codesign, or an external
    /// signing tool).
    #[error("signing error: {0}")]
    Signing(String),
}

/// Convenience alias matching the rest of the crate's `*Result` style.
pub type BundleResult<T> = Result<T, BundleError>;

// ---------------------------------------------------------------------------
// Helpers shared across all platform bundlers
// ---------------------------------------------------------------------------

/// Wrap an `io::Result<T>` into a [`BundleResult<T>`], attaching `path`
/// to the failure so the rendered error tells the user which file or
/// directory was involved.
pub(crate) fn io_at<T>(path: &Path, op: io::Result<T>) -> BundleResult<T> {
    op.map_err(|source| BundleError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Copy the project's assets into `dest_root`, preserving the hierarchical
/// asset names (forward-slash separated, e.g. "icons/home.png").
///
/// The asset set comes from [`PekoProject::asset_index`], which reads the
/// project's `assets/` directory. Each asset is copied to `dest_root/<name>`,
/// creating any subdirectories the name implies. This is used by the Linux,
/// macOS, iOS, and Android bundlers, which all just need the asset bytes laid
/// out as files under a platform specific root (the squashfs assets dir, the
/// app bundle Resources, the APK assets dir). Windows is different: it embeds
/// assets as resources at link time, so it does not use this helper.
///
/// Does nothing (and is not an error) if the project has no UI info or no
/// assets.
pub(crate) fn copy_project_assets(project: &PekoProject, dest_root: &Path) -> BundleResult<()> {
    if project.ui_project_info.is_none() {
        return Ok(());
    }

    for (name, source_path) in project.asset_index() {
        // The destination mirrors the asset name under dest_root. Names
        // are forward-slash separated and relative; join handles the
        // subdirectories. Create the parent dirs first.
        let dest = dest_root.join(&name);
        if let Some(parent) = dest.parent() {
            io_at(parent, fs::create_dir_all(parent))?;
        }
        io_at(&source_path, fs::copy(&source_path, &dest).map(|_| ()))?;
    }

    Ok(())
}

/// Run an external tool to completion, checking both that it could be
/// launched and that its exit status reflects success. A launch failure
/// surfaces as [`BundleError::ToolLaunch`] and a non-zero exit status as
/// [`BundleError::Tool`].
pub(crate) fn run_tool(tool: &str, command: &mut Command) -> BundleResult<()> {
    let status = command.status().map_err(|source| BundleError::ToolLaunch {
        tool: tool.to_owned(),
        source,
    })?;
    if !status.success() {
        return Err(BundleError::Tool {
            tool: tool.to_owned(),
            status,
        });
    }
    Ok(())
}

/// The host-specific subdirectory under `Compiler/java` that holds the
/// JDK for the machine running the build. macOS and Linux are keyed by
/// operating system and architecture (`macos/arm`, `macos/x86_64`,
/// `linux/arm`, `linux/x86_64`). Windows uses a single `windows`
/// directory.
fn jdk_host_subdir() -> PathBuf {
    use std::env::consts::{ARCH, OS};
    if OS == "windows" {
        return PathBuf::from("windows");
    }
    let arch = match ARCH {
        "aarch64" => "arm",
        other => other,
    };
    PathBuf::from(OS).join(arch)
}

/// Resolve a Java tool (`java`, `jarsigner`, `keytool`) from the JDK
/// shipped in the toolchain. JDKs live under `Compiler/java` in a
/// host-specific subdirectory selected by [`jdk_host_subdir`]. On Windows
/// the tool name gains a `.exe` suffix. The macOS JDK layout nests the
/// runtime under `Contents/Home`, so that location is checked as well.
/// When neither path exists the direct path is returned and the launch
/// failure surfaces through [`run_tool`].
pub(crate) fn java_tool(peko_root: &Path, tool: &str) -> PathBuf {
    let exe = if cfg!(windows) {
        format!("{tool}.exe")
    } else {
        tool.to_string()
    };
    let base = peko_root.join("Compiler/java").join(jdk_host_subdir());
    let direct = base.join("bin").join(&exe);
    if direct.exists() {
        return direct;
    }
    let nested = base.join("Contents/Home/bin").join(&exe);
    if nested.exists() {
        return nested;
    }
    direct
}

/// RAII guard that nukes a build directory if the bundler returns early
/// via `?`. Successful completion calls [`commit`](Self::commit) to
/// disarm the cleanup.
pub(crate) struct CleanupGuard {
    path: Option<PathBuf>,
}

impl CleanupGuard {
    /// Construct a guard rooted at `path`. The path doesn't need to
    /// exist yet - `Drop` is best-effort.
    pub fn new(path: PathBuf) -> Self {
        CleanupGuard { path: Some(path) }
    }

    /// Disarm the guard. Call this on the success path so the build
    /// directory survives.
    pub fn commit(mut self) {
        self.path = None;
    }
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            if path.is_dir() {
                let _ = fs::remove_dir_all(&path);
            } else if path.exists() {
                let _ = fs::remove_file(&path);
            }
        }
    }
}

/// Recursively add a directory and its contents to an open zip archive
/// under `base_in_zip`. Subdirectories are followed; every regular file
/// is added with `Stored` compression. Used by the android bundler to
/// embed the `assets/`, `lib/`, and `res/` trees into the APK.
///
/// (`packager::ziputil::zip_add_folder` is similar but supports
/// extension filtering and uses `Deflated` compression for files. The
/// android bundler needs `Stored` and every-file behavior, hence this
/// separate helper.)
pub(crate) fn recursive_zip_add(
    zip: &mut ZipWriter<File>,
    dir_path: &Path,
    base_in_zip: &str,
) -> BundleResult<()> {
    zip.add_directory::<&str, ExtendedFileOptions>(base_in_zip, FileOptions::default())?;

    for entry in io_at(dir_path, fs::read_dir(dir_path))? {
        let entry = io_at(dir_path, entry)?;
        let entry_path = entry.path();
        let entry_zip_name = format!("{base_in_zip}/{}", entry.file_name().display());

        if io_at(&entry_path, entry.file_type())?.is_dir() {
            recursive_zip_add(zip, &entry_path, &entry_zip_name)?;
        } else {
            zip.start_file::<&str, ExtendedFileOptions>(
                entry_zip_name.as_str(),
                FileOptions::default().compression_method(CompressionMethod::Stored),
            )?;
            let bytes = io_at(&entry_path, fs::read(&entry_path))?;
            io_at(&entry_path, zip.write_all(&bytes))?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Config-file templates
// ---------------------------------------------------------------------------

/// Android `res/values/strings.xml` template. `{name}` and `{bundle_id}`
/// are substituted at generation time.
const ANDROID_STRINGS_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<resources>
    <string name="app_name">{name}</string>
    <string name="package_name">{bundle_id}</string>
</resources>
"#;

/// Android `AndroidManifest.xml` template.
const ANDROID_MANIFEST_XML: &str = r#"<?xml version="1.0" encoding="utf-8" standalone="no"?>
<manifest xmlns:tools="http://schemas.android.com/tools"
          xmlns:android="http://schemas.android.com/apk/res/android"
          package="{bundle_id}"
          android:versionCode="{version_code}"
          android:versionName="{version}">
    <uses-sdk android:minSdkVersion="22" android:targetSdkVersion="35" />
    <uses-permission android:name="android.permission.SET_RELEASE_APP"/>
    <uses-permission android:name="android.permission.INTERNET"/>
    <application android:usesCleartextTraffic="true"
                 android:hasCode="false"
                 tools:replace="android:icon,android:theme,android:allowBackup,label"
                 android:icon="@mipmap/icon">
        <activity android:configChanges="keyboardHidden|orientation"
                  android:name="android.app.NativeActivity"
                  android:label="{name}"
                  android:exported="true">
            <meta-data android:name="android.app.lib_name" android:value="PekoApp"/>
            <intent-filter>
                <action android:name="android.intent.action.MAIN"/>
                <category android:name="android.intent.category.LAUNCHER"/>
            </intent-filter>
        </activity>
    </application>
</manifest>
"#;

/// iOS `Info.plist` template.
const IOS_INFO_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>{name}</string>
    <key>DTSDKName</key><string>iphoneos26.2</string>
    <key>DTXcode</key><string>2630</string>
    <key>DTSDKBuild</key><string>23C57</string>
    <key>CFBundleVersion</key><string>{version}</string>
    <key>DTPlatformName</key><string>iphoneos</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>{version}</string>
    <key>CFBundleExecutable</key><string>{name}</string>
    <key>CFBundleSupportedPlatforms</key>
    <array><string>iPhoneOS</string></array>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>ITSAppUsesNonExemptEncryption</key><false/>
    <key>UISupportedInterfaceOrientations</key>
    <array>
        <string>UIInterfaceOrientationPortrait</string>
        <string>UIInterfaceOrientationLandscapeLeft</string>
        <string>UIInterfaceOrientationLandscapeRight</string>
    </array>
    <key>UIRequiredDeviceCapabilities</key>
    <array><string>arm64</string></array>
    <key>MinimumOSVersion</key><string>15</string>
    <key>CFBundleIdentifier</key><string>{bundle_id}</string>
    <key>UIDeviceFamily</key>
    <array><integer>1</integer><integer>2</integer></array>
    <key>DTPlatformVersion</key><string>26.2</string>
    <key>DTXcodeBuild</key><string>17C529</string>
    <key>DTPlatformBuild</key><string>23C57</string>
    <key>UIRequiresFullScreen</key><true/>
    <key>UILaunchScreen</key>
    <dict><key>UIColorName</key><string>black</string></dict>
    <key>CFBundleDevelopmentRegion</key><string>en</string>
    <key>CFBundleIcons~ipad</key>
    <dict>
        <key>CFBundlePrimaryIcon</key>
        <dict>
            <key>CFBundleIconFiles</key>
            <array><string>AppIcon60x60</string><string>AppIcon76x76</string></array>
            <key>CFBundleIconName</key><string>AppIcon</string>
        </dict>
    </dict>
    <key>CFBundleIcons</key>
    <dict>
        <key>CFBundlePrimaryIcon</key>
        <dict>
            <key>CFBundleIconFiles</key>
            <array><string>AppIcon60x60</string></array>
            <key>CFBundleIconName</key><string>AppIcon</string>
        </dict>
    </dict>
</dict>
</plist>
"#;

/// iOS entitlements template. The values carry no team prefix, which is
/// correct for the simulator. Device builds need team-prefixed values and
/// take their entitlements through the code signature instead.
const IOS_ENTITLEMENTS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>application-identifier</key>
    <string>{bundle_id}</string>
    <key>keychain-access-groups</key>
    <array>
        <string>{bundle_id}</string>
    </array>
</dict>
</plist>
"#;

/// Apple privacy manifest template (`PrivacyInfo.xcprivacy`), used by both
/// the iOS and macOS bundles.
///
/// Declares that the app does not track and collects no data types. The
/// accessed-API entries cover the required-reason API categories the
/// linked native code touches through the SQLite database file: file
/// timestamps and disk space. Each entry pairs the API category with a
/// reason code from Apple's fixed list. C617.1 covers timestamps of files
/// in the app container. 85F4.1 covers free space of the app container.
const APPLE_PRIVACY_MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>NSPrivacyTracking</key><false/>
    <key>NSPrivacyTrackingDomains</key>
    <array/>
    <key>NSPrivacyCollectedDataTypes</key>
    <array/>
    <key>NSPrivacyAccessedAPITypes</key>
    <array>
        <dict>
            <key>NSPrivacyAccessedAPIType</key>
            <string>NSPrivacyAccessedAPICategoryFileTimestamp</string>
            <key>NSPrivacyAccessedAPITypeReasons</key>
            <array><string>C617.1</string></array>
        </dict>
        <dict>
            <key>NSPrivacyAccessedAPIType</key>
            <string>NSPrivacyAccessedAPICategoryDiskSpace</string>
            <key>NSPrivacyAccessedAPITypeReasons</key>
            <array><string>85F4.1</string></array>
        </dict>
    </array>
</dict>
</plist>
"#;

/// macOS `Info.plist` template.
const MACOS_INFO_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>{name}</string>
    <key>CFBundleDisplayName</key><string>{name}</string>
    <key>CFBundleExecutable</key><string>exec</string>
    <key>CFBundleIdentifier</key><string>{bundle_id}</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>{version}</string>
    <key>CFBundleVersion</key><string>{version}</string>
    <key>CFBundleDevelopmentRegion</key><string>en</string>
    <key>CFBundleIconFile</key><string>icon</string>
    <key>CFBundleSupportedPlatforms</key>
    <array><string>MacOSX</string></array>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>ITSAppUsesNonExemptEncryption</key><false/>
    <key>DTPlatformName</key><string>macosx</string>
    <key>DTSDKName</key><string>macosx26.2</string>
    <key>DTPlatformVersion</key><string>26.2</string>
</dict>
</plist>
"#;

/// Derive a monotonic Android versionCode from a semver version string.
/// The scheme is major * 1000000 + minor * 1000 + patch. The value climbs
/// as the project version climbs and stays well within the Play limit of
/// 2100000000. Any pre-release or build metadata suffix is ignored, and any
/// unparsable component counts as zero.
fn android_version_code(version: &str) -> u64 {
    let core = version.split(['-', '+']).next().unwrap_or(version);
    let mut parts = core
        .split('.')
        .map(|p| p.trim().parse::<u64>().unwrap_or(0));
    let major = parts.next().unwrap_or(0);
    let minor = parts.next().unwrap_or(0);
    let patch = parts.next().unwrap_or(0);
    major * 1_000_000 + minor * 1_000 + patch
}

/// Normalize a project version into one to three period-separated
/// integers. A leading "v" or "V" is removed, any pre-release or build
/// suffix is dropped, and each kept component is parsed as an integer so
/// leading zeros are stripped. Parsing stops at the first non-integer
/// component. A version with no leading integer yields "0". The result is
/// valid for `CFBundleVersion`, `CFBundleShortVersionString`, and the
/// Android `versionName`.
fn normalize_version(version: &str) -> String {
    let trimmed = version.trim();
    let without_v = trimmed
        .strip_prefix('v')
        .or_else(|| trimmed.strip_prefix('V'))
        .unwrap_or(trimmed);
    let core = without_v.split(['-', '+']).next().unwrap_or(without_v);

    let mut components: Vec<String> = Vec::new();
    for part in core.split('.') {
        match part.trim().parse::<u64>() {
            Ok(number) => components.push(number.to_string()),
            Err(_) => break,
        }
        if components.len() == 3 {
            break;
        }
    }

    if components.is_empty() {
        "0".to_string()
    } else {
        components.join(".")
    }
}

/// Substitute the placeholders in a template with the actual project
/// values.
fn fill_template(template: &str, name: &str, bundle_id: &str, version: &str) -> String {
    let version = normalize_version(version);
    template
        .replace("{name}", name)
        .replace("{bundle_id}", bundle_id)
        .replace(
            "{version_code}",
            &android_version_code(&version).to_string(),
        )
        .replace("{version}", &version)
}

/// Fill `template`'s placeholders with the supplied values and write
/// the result to `path`.
fn write_template(
    path: &Path,
    template: &str,
    name: &str,
    bundle_id: &str,
    version: &str,
) -> BundleResult<()> {
    io_at(
        path,
        fs::write(path, fill_template(template, name, bundle_id, version)),
    )
}

/// Recreate the project's `.peko/bundling/configfiles/` tree from
/// scratch, writing the per-platform config templates with the
/// project's name, bundle id, and version substituted in.
///
/// Called automatically on the first build (when the tree doesn't yet
/// exist), and re-runnable via the build command's `--regenconfig` flag
/// to re-sync the templates after a metadata change. **Calling this
/// will overwrite any user edits to the template files** - that's by
/// design; the templates are authoritative.
pub fn regenerate_application_bundle_files(project: &PekoProject) -> BundleResult<()> {
    let project_bundling_folder = project.get_root().join(".peko/bundling/configfiles");
    if project_bundling_folder.is_dir() {
        io_at(
            &project_bundling_folder,
            fs::remove_dir_all(&project_bundling_folder),
        )?;
    } else if project_bundling_folder.exists() {
        io_at(
            &project_bundling_folder,
            fs::remove_file(&project_bundling_folder),
        )?;
    }
    io_at(
        &project_bundling_folder,
        fs::create_dir_all(&project_bundling_folder),
    )?;

    let ui_info = project.ui_project_info.as_ref().unwrap();
    let name = &project.name;
    let bundle_id = &ui_info.bundle_id;
    let version = &ui_info.version;

    // Android
    let android_folder = project_bundling_folder.join("android");
    io_at(&android_folder, fs::create_dir_all(&android_folder))?;
    write_template(
        &android_folder.join("strings.xml"),
        ANDROID_STRINGS_XML,
        name,
        bundle_id,
        version,
    )?;
    write_template(
        &android_folder.join("AndroidManifest.xml"),
        ANDROID_MANIFEST_XML,
        name,
        bundle_id,
        version,
    )?;

    // iOS
    let ios_folder = project_bundling_folder.join("ios");
    io_at(&ios_folder, fs::create_dir_all(&ios_folder))?;
    write_template(
        &ios_folder.join("Info.plist"),
        IOS_INFO_PLIST,
        name,
        bundle_id,
        version,
    )?;
    write_template(
        &ios_folder.join("app.entitlements"),
        IOS_ENTITLEMENTS,
        name,
        bundle_id,
        version,
    )?;
    write_template(
        &ios_folder.join("PrivacyInfo.xcprivacy"),
        APPLE_PRIVACY_MANIFEST,
        name,
        bundle_id,
        version,
    )?;

    // macOS
    let macos_folder = project_bundling_folder.join("macos");
    io_at(&macos_folder, fs::create_dir_all(&macos_folder))?;
    write_template(
        &macos_folder.join("Info.plist"),
        MACOS_INFO_PLIST,
        name,
        bundle_id,
        version,
    )?;
    write_template(
        &macos_folder.join("PrivacyInfo.xcprivacy"),
        APPLE_PRIVACY_MANIFEST,
        name,
        bundle_id,
        version,
    )?;

    Ok(())
}
