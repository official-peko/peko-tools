//! Windows `.exe` bundler.
//!
//! Compiles the project for x86_64 Windows, embeds a `.rc` resource
//! file that pulls in the project's icon and version metadata, and
//! links it all together. Architectures other than x86_64 are not
//! currently supported on Windows.
//!
//! The `llvm-rc` resource compiler is picked per host (linux-arm,
//! linux-x86_64, darwin-arm, darwin-x86_64, or windows.exe). On Linux,
//! `std::env::consts::ARCH` reports `"arm"` for 32-bit ARM and
//! `"aarch64"` for 64-bit ARM - both are treated as "arm" here since
//! the toolchain ships a single ARM build that covers both.

use std::fs::{self, File};
use std::path::PathBuf;
use std::process::Command;

use peko_core::target::{Architecture, OperatingSystem, PekoTarget};

use crate::bundler::{io_at, run_tool, signing, BundleError, BundleResult, CleanupGuard};
use crate::cli::reporting::ProgressSink;
use crate::cli::CLIInfo;
use crate::execution;
use crate::project::PekoProject;

/// Resource-script template - pulled in by `llvm-rc` to embed the icon
/// and version metadata into the final binary. `{bundle_id}`,
/// `{version}`, and `{name}` are substituted at bundle time.
const WINDOWS_RC_TEMPLATE: &str = r#"MAINICON ICON "icon.ico"

VS_VERSION_INFO VERSIONINFO
    FILEVERSION 0, 0, 0, 0
    PRODUCTVERSION 0,0,0,0
    FILEFLAGSMASK 0x3FL
    FILEOS 0x4L
    FILETYPE 0x1L
    FILESUBTYPE 0x0L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904E4"
        BEGIN
            VALUE "CompanyName", "{bundle_id}"
            VALUE "ProductVersion", "{version}"
            VALUE "ProductName", "{name}"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x409, 1252
    END
END"#;

/// Build a Windows `.exe` for the project.
pub fn bundle(
    cli_info: &CLIInfo,
    project: &mut PekoProject,
    windows_build_directory: PathBuf,
    progress: &dyn ProgressSink,
) -> BundleResult<()> {
    if windows_build_directory.exists() {
        let removal = if windows_build_directory.is_dir() {
            fs::remove_dir_all(&windows_build_directory)
        } else {
            fs::remove_file(&windows_build_directory)
        };
        io_at(&windows_build_directory, removal)?;
    }
    io_at(
        &windows_build_directory,
        fs::create_dir_all(&windows_build_directory),
    )?;

    let guard = CleanupGuard::new(windows_build_directory.clone());

    // Two user-visible phases; the inner compile contributes its own
    // units via add_to_total.
    progress.add_to_total(2);

    progress.tick("Windows: writing icon and resource file");

    // Convert the project's icon to a Windows .ico and write the
    // accompanying .rc resource script with the project metadata baked
    // in.
    let converted_icon = windows_build_directory.join("icon.ico");
    let icon = &project.ui_project_info.as_ref().unwrap().icon;
    icon.to_ico(&mut io_at(&converted_icon, File::create(&converted_icon))?);

    let ui_info = project.ui_project_info.as_ref().unwrap();
    let mut resource_code = WINDOWS_RC_TEMPLATE
        .replace("{bundle_id}", &ui_info.bundle_id)
        .replace("{version}", &ui_info.version)
        .replace("{name}", &project.name);

    // Embed each project asset as a resource of custom type PEKO_ASSET.
    // The resource name is the asset's path uppercased (what the assets
    // package's Windows native layer looks up via
    // FindResourceA(UPPER(name), "PEKO_ASSET")). The quoted path is where
    // llvm-rc reads the bytes at compile time; we copy each asset into an
    // "assets" subdir of the build dir and reference it with a relative,
    // backslash-separated path so llvm-rc (run with current_dir set to
    // the build dir) finds it.
    let asset_index = project.asset_index();
    if !asset_index.is_empty() {
        let rc_assets_dir = windows_build_directory.join("assets");
        for (name, source_path) in &asset_index {
            // Copy the asset into the build dir under assets/<name>,
            // preserving subdirs.
            let dest = rc_assets_dir.join(name);
            if let Some(parent) = dest.parent() {
                io_at(parent, fs::create_dir_all(parent))?;
            }
            io_at(source_path, fs::copy(source_path, &dest).map(|_| ()))?;

            // Resource name = uppercased asset path (forward slashes
            // kept; that is the lookup key the native layer uppercases
            // too). The file path is relative to the build dir and uses
            // forward slashes: that matches the on-disk layout the
            // bundler just created, and llvm-rc accepts forward slashes.
            // Backslashes are avoided because rc treats a single
            // backslash as an escape and a doubled one as a literal pair
            // (which then names a nonexistent file).
            let resource_name = name.to_uppercase();
            let rc_path = format!("assets/{name}");
            resource_code.push_str(&format!("\n{resource_name}  PEKO_ASSET  \"{rc_path}\"\n"));
        }
    }

    let resource_file = windows_build_directory.join("res.rc");
    io_at(&resource_file, fs::write(&resource_file, resource_code))?;

    // Compile the .rc to a .res using the appropriate llvm-rc binary
    // for the host (we're cross-compiling from this host to Windows).
    // See the module doc for notes on the linux-arm / linux-aarch64
    // collapse.
    let rc_compiler = match std::env::consts::OS {
        "linux" => match std::env::consts::ARCH {
            "arm" | "aarch64" => cli_info
                .get_peko_root()
                .join("Compiler/bin/llvm-rc/llvm-rc-linux-arm"),
            _ => cli_info
                .get_peko_root()
                .join("Compiler/bin/llvm-rc/llvm-rc-linux-x86_64"),
        },
        "macos" => match std::env::consts::ARCH {
            "arm" | "aarch64" => cli_info
                .get_peko_root()
                .join("Compiler/bin/llvm-rc/llvm-rc-darwin-arm"),
            _ => cli_info
                .get_peko_root()
                .join("Compiler/bin/llvm-rc/llvm-rc-darwin-x86_64"),
        },
        _ => cli_info
            .get_peko_root()
            .join("Compiler/bin/llvm-rc/llvm-rc-windows.exe"),
    };

    run_tool(
        "llvm-rc",
        Command::new(rc_compiler)
            .arg("res.rc")
            .current_dir(resource_file.parent().unwrap()),
    )?;

    let resource_output = windows_build_directory.join("res.res");

    // Compile + link the application binary, with the resource file
    // passed in as an extra linker input.
    progress.tick("Windows: compiling x86_64 binary");
    let windows_target = PekoTarget::new(OperatingSystem::Windows, Architecture::X86_64, false);
    let (_, diagnostics) = execution::incremental::compile_project(
        cli_info.get_peko_root(),
        project,
        windows_target,
        project.get_root().join(".peko/incremental"),
        Some(windows_build_directory.join(format!("{}.exe", project.name))),
        false,
        vec![resource_output.clone()],
        None,
        None,
        None,
        None,
        progress,
    )?;
    if let Some(diagnostics) = diagnostics {
        return Err(BundleError::CompileDiagnostics(diagnostics));
    }

    // Clean up intermediate files.
    io_at(&converted_icon, fs::remove_file(&converted_icon))?;
    io_at(&resource_file, fs::remove_file(&resource_file))?;
    io_at(&resource_output, fs::remove_file(&resource_output))?;

    guard.commit();
    Ok(())
}

/// Optionally sign the Windows `.exe` with the system `osslsigncode`.
///
/// Windows signing is optional. When no Windows key is registered, or
/// `osslsigncode` is not installed on the system, the unsigned executable
/// is left in place. When a key is registered and the tool is present,
/// the executable is signed with the registered PKCS#12 certificate.
pub fn sign(
    _cli_info: &CLIInfo,
    project: &PekoProject,
    windows_build_directory: PathBuf,
) -> BundleResult<signing::OptionalSignOutcome> {
    let Some(ui_info) = project.ui_project_info.as_ref() else {
        return Ok(signing::OptionalSignOutcome::NoKey);
    };

    let key = match signing::resolve_windows(project.get_root(), &ui_info.bundle_id)? {
        Some(key) => key,
        None => return Ok(signing::OptionalSignOutcome::NoKey),
    };

    if !signing::osslsigncode_available() {
        return Ok(signing::OptionalSignOutcome::ToolUnavailable);
    }

    let exe = windows_build_directory.join(format!("{}.exe", project.name));
    if !exe.exists() {
        return Err(BundleError::Signing(format!(
            "executable not found at {}",
            exe.display()
        )));
    }

    signing::osslsigncode_sign(&exe, &key)?;
    Ok(signing::OptionalSignOutcome::Signed)
}
