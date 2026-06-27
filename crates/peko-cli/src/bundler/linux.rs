//! Linux AppImage bundler.
//!
//! Produces an `.AppImage` for each of arm64 and x86_64 by building a
//! squashfs filesystem containing the compiled binary, a `.desktop`
//! manifest, an `AppRun` shell entrypoint, and an icon - then
//! concatenating the appropriate AppImage runtime binary in front of
//! the squashfs.
//!
//! Note: throughout the cli "arm" always means 64-bit ARM (aarch64). 32-bit
//! ARM is not a supported target.

use std::fs::{self, File, OpenOptions};
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

use peko_core::target::{Architecture, OperatingSystem, PekoTarget};

use crate::bundler::{BundleError, BundleResult, CleanupGuard, io_at};
use crate::cli::CLIInfo;
use crate::cli::reporting::ProgressSink;
use crate::execution;
use crate::project::PekoProject;

/// Load a toolchain's bundled-dylib sonames from its `toolchain.toml`.
fn load_bundle_dylibs(toolchain_dir: &Path) -> BundleResult<Vec<String>> {
    let path = toolchain_dir.join("toolchain.toml");
    let toolchain = peko_core::config::Toolchain::load(&path).map_err(|source| {
        BundleError::Toolchain {
            path,
            source: Box::new(source),
        }
    })?;
    Ok(toolchain.link.bundle_dylibs)
}

/// Build a squashfs filesystem at `output_file` containing the project's
/// compiled binary plus the AppImage-format metadata (`.desktop` file,
/// `AppRun` script, icon, lib directory).
fn create_linux_squashfs(
    project: &PekoProject,
    output_file: &Path,
    main_binary: &Path,
    toolchain_lib_dir: &Path,
    bundle_dylibs: &[String],
    multiarch_triple: &str,
) -> BundleResult<()> {
    let mut filesystem_writer = backhand::FilesystemWriter::default();
    filesystem_writer.set_current_time();
    filesystem_writer.set_block_size(backhand::DEFAULT_BLOCK_SIZE);
    filesystem_writer.set_only_root_id();
    filesystem_writer.set_compressor(
        backhand::FilesystemCompressor::new(backhand::compression::Compressor::Zstd, None).unwrap(),
    );
    filesystem_writer.set_kind(backhand::kind::Kind::from_const(backhand::kind::LE_V4_0).unwrap());

    let filesystem_header = backhand::NodeHeader::default();
    let executable_header = backhand::NodeHeader::new(0o755, 0, 0, 0);
    // Default NodeHeader has mode 0, which means nothing can read the
    // file. Data files (icon, desktop entry) need read perms or the
    // desktop environment cannot load them.
    let readable_header = backhand::NodeHeader::new(0o644, 0, 0, 0);
    filesystem_writer.set_root_mode(0o777);

    // Mounted directory layout: /usr/bin/exec for the binary, /usr/lib
    // for the bundled dynamic libraries, AppRun + <name>.desktop + icon
    // + .DirIcon at the root.
    filesystem_writer
        .push_dir("usr", filesystem_header)
        .unwrap();
    filesystem_writer
        .push_dir("usr/bin", filesystem_header)
        .unwrap();

    let binary_handle = io_at(main_binary, File::open(main_binary))?;
    filesystem_writer
        .push_file(binary_handle, "/usr/bin/exec", executable_header)
        .unwrap();

    // .desktop manifest tells the desktop environment how to launch the
    // app and what icon to show. The Icon field is a name with no
    // extension, not a file path. It must match the icon file basename
    // (icon.png on disk becomes Icon=icon here).
    let desktop_file_contents = format!(
        "[Desktop Entry]\n\
         StartupWMClass=Exec\n\
         Name={}\n\
         Exec=exec\n\
         Icon=icon\n\
         Type=Application",
        project.name
    );
    filesystem_writer
        .push_file(
            Cursor::new(desktop_file_contents.clone().into_bytes()),
            format!("{}.desktop", project.name),
            readable_header,
        )
        .unwrap();

    // Also install the .desktop into usr/share/applications. AppImage
    // integration (and desktop environments that register the app) read
    // it from here to create the menu and taskbar entry, which then uses
    // the hicolor themed icon installed below.
    filesystem_writer
        .push_dir("usr/share", filesystem_header)
        .unwrap();
    filesystem_writer
        .push_dir("usr/share/applications", filesystem_header)
        .unwrap();
    filesystem_writer
        .push_file(
            Cursor::new(desktop_file_contents.into_bytes()),
            format!("usr/share/applications/{}.desktop", project.name),
            readable_header,
        )
        .unwrap();

    // AppRun script - the entrypoint the AppImage runtime executes.
    // LD_LIBRARY_PATH prepends the bundled usr/lib so our libraries take
    // priority over any (possibly older or newer) versions on the host.
    // Appending instead would let a mismatched host library load first
    // and cause undefined-symbol errors.
    //
    // GIO_MODULE_DIR points GIO at our own (empty) modules directory so
    // it does not scan the host's /usr/lib/.../gio/modules and load
    // host plugins like libdconfsettings.so. Those host modules are
    // built against a newer glib than we bundle and would fail to bind
    // newer symbols against our older glib. A webview app does not need
    // the dconf settings backend; GIO falls back to its built-in
    // backends when no modules are found.
    //
    // GIO_USE_VFS=local forces GIO to use plain local file access and
    // skip gvfs (GNOME's virtual filesystem) entirely. Without this,
    // GIO can pull in host gvfs libraries (libgvfscommon.so and friends)
    // which hit the same newer-glib symbol mismatch. A webview app does
    // not need virtual filesystem features (network mounts, trash,
    // recent), so local VFS is sufficient.
    //
    // This build of WebKitGTK bakes the absolute host path
    // /usr/lib/<multiarch>/webkit2gtk-4.0 for its helper processes and
    // ignores WEBKIT_EXEC_PATH. We patch the library to use a relative
    // path (lib/<multiarch>/webkit2gtk-4.0) instead, so this AppRun must
    // cd to the mount root before launching so that relative path
    // resolves into the bundle. The working directory must not change
    // while the app runs, or webkit will lose track of its helpers.
    //
    // WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1 turns off webkit's
    // bubblewrap (bwrap) sandbox. The sandbox spawns the network and web
    // processes inside an isolated mount namespace that does not see the
    // bundle, and bwrap is often absent, so it makes webkit fail to
    // spawn its child processes. A local app UI loading its own trusted
    // content does not need the sandbox.
    let app_run_contents = "#!/bin/sh\n\
        CD=\"$(dirname \"$(readlink -f \"${0}\")\")\"\n\
        cd \"${CD}\"\n\
        EXEC=\"${CD}/usr/bin/exec\"\n\
        export LD_LIBRARY_PATH=\"${CD}/usr/lib:${LD_LIBRARY_PATH}\"\n\
        export GIO_MODULE_DIR=\"${CD}/usr/lib/gio/modules\"\n\
        export GIO_USE_VFS=local\n\
        export WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1\n\
        exec \"${EXEC}\"\n";
    filesystem_writer
        .push_file(
            Cursor::new(app_run_contents.as_bytes().to_vec()),
            "AppRun",
            executable_header,
        )
        .unwrap();

    // App icon as a PNG. We render the bytes once and write them to two
    // places: icon.png (referenced by the .desktop Icon field) and
    // .DirIcon at the root (what file managers use to show the icon on
    // the AppImage file itself). Both must exist for the icon to appear
    // everywhere.
    //
    // The icon is resized to 256x256 first. The freedesktop icon spec
    // only recognizes standard sizes (16, 22, 24, 32, 48, 64, 128, 256,
    // 512); a non-standard size like 1024 gets ignored by GNOME and most
    // file managers, so the icon would not show even though the file is
    // valid and present. 256 is the safe universal default.
    let mut icon_buffer = Vec::new();
    {
        let mut icon_bytes = Cursor::new(&mut icon_buffer);
        project
            .ui_project_info
            .as_ref()
            .unwrap()
            .icon
            .resize(256, 256)
            .to_png(&mut icon_bytes);
    }
    filesystem_writer
        .push_file(
            Cursor::new(icon_buffer.clone()),
            "icon.png",
            readable_header,
        )
        .unwrap();
    filesystem_writer
        .push_file(
            Cursor::new(icon_buffer.clone()),
            ".DirIcon",
            readable_header,
        )
        .unwrap();

    // Also install the icon into the freedesktop hicolor icon theme at
    // usr/share/icons/hicolor/256x256/apps/<name>.png. This is the path
    // desktop integration looks at to find the icon by the name in the
    // .desktop Icon field. Without it, the app menu and taskbar entry
    // have no icon even when .DirIcon shows on the AppImage file. The
    // filename must match the Icon field (Icon=icon means icon.png here).
    // Note: usr/share was already created when the .desktop was installed
    // into usr/share/applications above.
    filesystem_writer
        .push_dir("usr/share/icons", filesystem_header)
        .unwrap();
    filesystem_writer
        .push_dir("usr/share/icons/hicolor", filesystem_header)
        .unwrap();
    filesystem_writer
        .push_dir("usr/share/icons/hicolor/256x256", filesystem_header)
        .unwrap();
    filesystem_writer
        .push_dir("usr/share/icons/hicolor/256x256/apps", filesystem_header)
        .unwrap();
    filesystem_writer
        .push_file(
            Cursor::new(icon_buffer),
            "usr/share/icons/hicolor/256x256/apps/icon.png",
            readable_header,
        )
        .unwrap();

    // Project assets go to usr/share/assets/<name> in the squashfs. The
    // assets package's Linux native layer fetches them from
    // $APPDIR/usr/share/assets/<name> at runtime. Names are hierarchical
    // (forward-slash separated), so we create each asset's parent dirs
    // inside the squashfs before writing the file.
    if project.ui_project_info.is_some() {
        let asset_index = project.asset_index();
        if !asset_index.is_empty() {
            // Track which squashfs dirs we have already created so we do
            // not push the same dir twice (which backhand rejects).
            let mut created_dirs = std::collections::HashSet::new();
            // The fixed root segments always exist for any asset.
            for base in ["usr/share/assets"] {
                if created_dirs.insert(base.to_string()) {
                    filesystem_writer.push_dir(base, filesystem_header).unwrap();
                }
            }

            for (name, source_path) in &asset_index {
                // Create each intermediate directory in the asset name.
                let mut dir_accum = String::from("usr/share/assets");
                let parts: Vec<&str> = name.split('/').collect();
                for segment in &parts[..parts.len().saturating_sub(1)] {
                    dir_accum.push('/');
                    dir_accum.push_str(segment);
                    if created_dirs.insert(dir_accum.clone()) {
                        filesystem_writer
                            .push_dir(&dir_accum, filesystem_header)
                            .unwrap();
                    }
                }

                let dest = format!("usr/share/assets/{name}");
                let asset_handle = io_at(source_path, File::open(source_path))?;
                filesystem_writer
                    .push_file(asset_handle, dest, readable_header)
                    .unwrap();
            }
        }
    }

    // usr/lib holds the bundled runtime libraries. We copy the webkit
    // and gtk stack from the toolchain so the AppImage runs on a clean
    // machine without these installed. The names below are the sonames
    // (the .so.MAJOR symlinks the binary actually links against). We
    // resolve each soname to the real file in the toolchain and write
    // it into the squashfs under the same soname.
    filesystem_writer
        .push_dir("usr/lib", filesystem_header)
        .unwrap();

    // Empty gio/modules dir. GIO_MODULE_DIR in AppRun points here so GIO
    // scans this (empty) directory instead of the host's module dir,
    // avoiding loading host GIO plugins that are built against a newer
    // glib than we bundle.
    filesystem_writer
        .push_dir("usr/lib/gio", filesystem_header)
        .unwrap();
    filesystem_writer
        .push_dir("usr/lib/gio/modules", filesystem_header)
        .unwrap();

    for soname in bundle_dylibs {
        let lib_source = toolchain_lib_dir.join(soname);
        // Skip libs that aren't in the toolchain rather than failing the
        // whole bundle. A missing lib shows up at runtime and is easier
        // to debug than a hard error here.
        if !lib_source.exists() {
            continue;
        }

        // libwebkit2gtk bakes the absolute host path
        // /usr/lib/<multiarch>/webkit2gtk-4.0 for its helper processes
        // and injected bundle. Inside an AppImage that path does not
        // exist, and webkit ignores WEBKIT_EXEC_PATH, so it fails to
        // spawn its child processes. We patch the baked string in the
        // library bytes, replacing the leading "/usr/lib" with
        // "././/lib" (the same length, so the binary is not corrupted).
        // That makes webkit resolve the path relative to the current
        // working directory: "lib/<multiarch>/webkit2gtk-4.0". AppRun
        // cd's to the mount root before launching, so this resolves to
        // <mount>/lib/<multiarch>/webkit2gtk-4.0, where we place the
        // helpers. This is the standard fix used by AppImage projects
        // that bundle webkit2gtk.
        if soname.starts_with("libwebkit2gtk-4.0.so") {
            let mut bytes = std::fs::read(&lib_source).map_err(|e| BundleError::Io {
                path: lib_source.clone(),
                source: e,
            })?;
            patch_webkit_paths(&mut bytes);
            filesystem_writer
                .push_file(
                    Cursor::new(bytes),
                    format!("/usr/lib/{soname}"),
                    executable_header,
                )
                .unwrap();
            continue;
        }

        let lib_handle = io_at(&lib_source, File::open(&lib_source))?;
        filesystem_writer
            .push_file(lib_handle, format!("/usr/lib/{soname}"), executable_header)
            .unwrap();
    }

    // WebKitGTK spawns helper executables (WebKitNetworkProcess,
    // WebKitWebProcess) and loads an injected-bundle library at runtime.
    // After patching the library (see above) webkit looks for these at
    // lib/<multiarch>/webkit2gtk-4.0 relative to the current working
    // directory, which AppRun sets to the mount root. So the helpers go
    // at the squashfs root lib/<multiarch>/webkit2gtk-4.0 (NOT usr/lib).
    // The helper tree (including the injected-bundle subdir) lives in
    // the toolchain at <arch_root>/webkit2gtk-4.0.
    if let Some(arch_root) = toolchain_lib_dir.parent() {
        let webkit_helper_dir = arch_root.join("webkit2gtk-4.0");
        if webkit_helper_dir.is_dir() {
            let dest_prefix = format!("lib/{multiarch_triple}/webkit2gtk-4.0");
            // Create lib, lib/<multiarch>, then the webkit dir.
            filesystem_writer
                .push_dir("lib", filesystem_header)
                .unwrap();
            filesystem_writer
                .push_dir(format!("lib/{multiarch_triple}"), filesystem_header)
                .unwrap();
            filesystem_writer
                .push_dir(&dest_prefix, filesystem_header)
                .unwrap();
            copy_dir_into_squashfs(
                &mut filesystem_writer,
                &webkit_helper_dir,
                &webkit_helper_dir,
                &dest_prefix,
                executable_header,
                filesystem_header,
            )?;
        }
    }

    let mut output_stream = io_at(output_file, File::create(output_file))?;
    filesystem_writer
        .write(&mut output_stream)
        .map_err(|e| BundleError::Io {
            path: output_file.to_path_buf(),
            source: std::io::Error::other(format!("squashfs write failed: {e}")),
        })?;
    Ok(())
}

/// Patch the baked absolute webkit paths in the library bytes to make
/// them relative to the current working directory.
///
/// libwebkit2gtk hardcodes "/usr/lib/<multiarch>/webkit2gtk-4.0" (and
/// the injected-bundle path under it) as the place to find its helper
/// processes. Inside an AppImage that absolute host path does not
/// exist. We replace the leading "/usr/lib" with "././/lib", which is
/// exactly the same number of bytes (8), so offsets in the binary stay
/// valid. The result resolves to "lib/<multiarch>/webkit2gtk-4.0"
/// relative to the working directory, which AppRun sets to the mount
/// root. We only rewrite the "/usr/lib/" byte sequences that are near a
/// "webkit2gtk" substring, to avoid touching unrelated paths.
fn patch_webkit_paths(bytes: &mut [u8]) {
    let from = b"/usr/lib/";
    let to = b"././/lib/";
    debug_assert_eq!(from.len(), to.len());

    let needle_len = from.len();
    let context = b"webkit2gtk";

    let mut i = 0;
    while i + needle_len <= bytes.len() {
        if &bytes[i..i + needle_len] == from {
            // Look ahead a bounded window for "webkit2gtk" so we only
            // patch the webkit helper and injected-bundle paths, not
            // unrelated /usr/lib strings that might appear in the binary.
            let window_end = usize::min(i + needle_len + 64, bytes.len());
            let window = &bytes[i + needle_len..window_end];
            let has_context = window.windows(context.len()).any(|w| w == context);
            if has_context {
                bytes[i..i + needle_len].copy_from_slice(to);
            }
        }
        i += 1;
    }
}

/// Recursively copy a directory tree from disk into the squashfs.
///
/// `base` is the root of the source tree (used to compute relative
/// paths), `current` is the directory being walked this call, and
/// `dest_prefix` is the squashfs path the tree is being written under.
/// Files are written with `file_header` (executable perms, since the
/// webkit helpers are executables and their injected-bundle .so), and
/// subdirectories with `dir_header`.
fn copy_dir_into_squashfs(
    writer: &mut backhand::FilesystemWriter,
    base: &Path,
    current: &Path,
    dest_prefix: &str,
    file_header: backhand::NodeHeader,
    dir_header: backhand::NodeHeader,
) -> BundleResult<()> {
    let entries = std::fs::read_dir(current).map_err(|e| BundleError::Io {
        path: current.to_path_buf(),
        source: e,
    })?;

    for entry in entries {
        let entry = entry.map_err(|e| BundleError::Io {
            path: current.to_path_buf(),
            source: e,
        })?;
        let path = entry.path();

        // Relative path from the source base, used to build the squashfs
        // destination path under dest_prefix.
        let rel = path.strip_prefix(base).unwrap_or(&path);
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        let dest = format!("{dest_prefix}/{rel_str}");

        if path.is_dir() {
            writer.push_dir(&dest, dir_header).unwrap();
            copy_dir_into_squashfs(writer, base, &path, dest_prefix, file_header, dir_header)?;
        } else {
            let handle = io_at(&path, File::open(&path))?;
            writer.push_file(handle, &dest, file_header).unwrap();
        }
    }

    Ok(())
}

/// Concatenate `runtime` followed by `squashfs` into an AppImage at
/// `output`. The AppImage runtime is a self-extracting binary that,
/// when run, mounts the squashfs trailer and executes its `AppRun`.
fn build_appimage(runtime: &Path, squashfs: &Path, output: &Path) -> BundleResult<()> {
    let mut writer = io_at(
        output,
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(output),
    )?;
    let runtime_bytes = io_at(runtime, fs::read(runtime))?;
    io_at(output, writer.write_all(&runtime_bytes))?;
    let squashfs_bytes = io_at(squashfs, fs::read(squashfs))?;
    io_at(output, writer.write_all(&squashfs_bytes))?;
    io_at(output, writer.flush())?;
    Ok(())
}

/// Build Linux AppImages for the project (both arm64 and x86_64).
pub fn bundle(
    cli_info: &CLIInfo,
    project: &mut PekoProject,
    linux_build_directory: PathBuf,
    progress: &dyn ProgressSink,
) -> BundleResult<()> {
    if linux_build_directory.exists() {
        let removal = if linux_build_directory.is_dir() {
            fs::remove_dir_all(&linux_build_directory)
        } else {
            fs::remove_file(&linux_build_directory)
        };
        io_at(&linux_build_directory, removal)?;
    }
    io_at(
        &linux_build_directory,
        fs::create_dir_all(&linux_build_directory),
    )?;

    let guard = CleanupGuard::new(linux_build_directory.clone());

    // Five user-visible phases; the two inner compiles contribute their
    // own units via add_to_total.
    progress.add_to_total(5);

    progress.tick("Linux: preparing per-arch build directories");

    let arm_build_dir = linux_build_directory.join("arm");
    let x86_64_build_dir = linux_build_directory.join("x86_64");
    io_at(&arm_build_dir, fs::create_dir_all(&arm_build_dir))?;
    io_at(&x86_64_build_dir, fs::create_dir_all(&x86_64_build_dir))?;

    // Compile + link for each architecture.
    progress.tick("Linux: compiling arm64 binary");
    let arm_app_binary = arm_build_dir.join("exec");
    let arm_target = PekoTarget::new(OperatingSystem::Linux, Architecture::Arm, false);
    let (_, arm_diagnostics) = execution::incremental::compile_project(
        cli_info.get_peko_root(),
        project,
        arm_target,
        project.get_root().join(".peko/incremental"),
        Some(arm_app_binary.clone()),
        false,
        Vec::new(),
        None,
        None,
        None,
        None,
        progress,
    )?;
    if let Some(diagnostics) = arm_diagnostics {
        return Err(BundleError::CompileDiagnostics(diagnostics));
    }

    progress.tick("Linux: compiling x86_64 binary");
    let x86_64_app_binary = x86_64_build_dir.join("exec");
    let x86_64_target = PekoTarget::new(OperatingSystem::Linux, Architecture::X86_64, false);
    let (_, x86_64_diagnostics) = execution::incremental::compile_project(
        cli_info.get_peko_root(),
        project,
        x86_64_target,
        project.get_root().join(".peko/incremental"),
        Some(x86_64_app_binary.clone()),
        false,
        Vec::new(),
        None,
        None,
        None,
        None,
        progress,
    )?;
    if let Some(diagnostics) = x86_64_diagnostics {
        return Err(BundleError::CompileDiagnostics(diagnostics));
    }

    // Build a squashfs for each architecture. Each arch pulls its libs
    // from the matching toolchain lib directory.
    progress.tick("Linux: packing squashfs filesystems");
    let toolchain_root = cli_info.get_peko_root().join("Compiler/toolchains/linux");
    let arm_lib_dir = toolchain_root.join("arm/lib");
    let x86_64_lib_dir = toolchain_root.join("x86_64/lib");

    // The libraries to bundle come from each toolchain's `toolchain.toml`.
    let arm_dylibs = load_bundle_dylibs(&toolchain_root.join("arm"))?;
    let x86_64_dylibs = load_bundle_dylibs(&toolchain_root.join("x86_64"))?;

    let arm_squashfs = arm_build_dir.join("appdir.squashfs");
    create_linux_squashfs(
        project,
        &arm_squashfs,
        &arm_app_binary,
        &arm_lib_dir,
        &arm_dylibs,
        "aarch64-linux-gnu",
    )?;

    let x86_64_squashfs = x86_64_build_dir.join("appdir.squashfs");
    create_linux_squashfs(
        project,
        &x86_64_squashfs,
        &x86_64_app_binary,
        &x86_64_lib_dir,
        &x86_64_dylibs,
        "x86_64-linux-gnu",
    )?;

    // Concatenate runtime + squashfs to produce the AppImage.
    progress.tick("Linux: assembling AppImage binaries");
    let arm_appimage = arm_build_dir.join(format!("{}.AppImage", project.name));
    let arm_runtime = cli_info
        .get_peko_root()
        .join("Compiler/bundling/appimagerts/runtime-aarch64");
    build_appimage(&arm_runtime, &arm_squashfs, &arm_appimage)?;

    let x86_64_appimage = x86_64_build_dir.join(format!("{}.AppImage", project.name));
    let x86_64_runtime = cli_info
        .get_peko_root()
        .join("Compiler/bundling/appimagerts/runtime-x86_64");
    build_appimage(&x86_64_runtime, &x86_64_squashfs, &x86_64_appimage)?;

    // Clean up intermediate squashfs files and the raw binaries.
    io_at(&arm_squashfs, fs::remove_file(&arm_squashfs))?;
    io_at(&x86_64_squashfs, fs::remove_file(&x86_64_squashfs))?;
    io_at(&arm_app_binary, fs::remove_file(&arm_app_binary))?;
    io_at(&x86_64_app_binary, fs::remove_file(&x86_64_app_binary))?;

    guard.commit();
    Ok(())
}
