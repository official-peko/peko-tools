//! `peko clangflags`: print the clang flags peko_core would pass when
//! compiling a C/C++/Objective-C source for a given target.
//!
//! Used by Pekoscript projects that bring along native sources and need
//! to invoke clang themselves with the same flags Peko uses.

use std::path::PathBuf;
use std::process::ExitCode;

use peko_core::target::{Architecture, OperatingSystem};

use crate::cli::reporting::Reporter;
use crate::cli::CLIInfo;

/// GTK / system include subdirectories needed on Linux. Each is joined
/// onto `Compiler/toolchains/linux/gtk/`.
const LINUX_GTK_SUBDIRS: &[&str] = &[
    "webkitgtk-4.0",
    "at-spi-2.0",
    "at-spi2-atk",
    "atk-1.0",
    "blkid",
    "cairo",
    "dbus",
    "dbus-1.0",
    "freetype2",
    "fribidi",
    "gdk-pixbuf-2.0",
    "gio-unix-2.0",
    "glib-2.0",
    "glib-include",
    "gtk-3.0",
    "harfbuzz",
    "libmount",
    "libpng16",
    "libsoup-2.4",
    "libxml2",
    "pango-1.0",
    "pixman-1",
    "uuid",
];

/// Windows include subdirectories needed on Windows.
const WINDOWS_INCLUDE_SUBDIRS: &[&str] = &[
    "msvc-inc",
    "msvc-inc/msclr",
    "ucrt-inc",
    "um-inc",
    "shared-inc",
    "winrt-inc",
    "webview2/build/native/include",
];

/// Execute the `clangflags` subcommand.
pub async fn execute(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    let target_operating_system = match require_os(cli_info, reporter) {
        Some(os) => os,
        None => return ExitCode::FAILURE,
    };
    let target_architecture = match require_arch(cli_info, reporter) {
        Some(arch) => arch,
        None => return ExitCode::FAILURE,
    };

    let mut include_directories: Vec<PathBuf> = Vec::new();
    let toolchains = cli_info.get_peko_root().join("Compiler/toolchains");

    let (target_specific_flags, target_triple) = match target_operating_system {
        OperatingSystem::Android => {
            let android = toolchains.join("android");
            include_directories.extend([
                android.join("sysroot/usr/include/c++/v1"),
                android.join("include"),
                android.join("sysroot/usr/include/aarch64-linux-android"),
                android.join("sysroot/usr/include"),
            ]);
            (
                "-fPIC".to_owned(),
                "aarch64-unknown-linux-android22".to_owned(),
            )
        }

        OperatingSystem::IOS => {
            let (sysroot, arch) = match target_architecture {
                Architecture::Arm => {
                    let arm_root = toolchains.join("ios/arm64");
                    include_directories.push(arm_root.join("include/c++"));
                    (arm_root, "arm64")
                }
                Architecture::X86_64 => (toolchains.join("ios/x86_64"), "x86_64"),
                Architecture::Unknown => {
                    reporter.error("unsupported target CPU architecture");
                    return ExitCode::FAILURE;
                }
            };
            (
                format!(
                    "-fobjc-arc -isysroot {}",
                    sysroot.join("iPhoneOS.sdk").display()
                ),
                format!("{arch}-apple-ios"),
            )
        }

        OperatingSystem::Linux => {
            let (sysroot, arch) = match target_architecture {
                Architecture::Arm => (toolchains.join("linux/arm"), "aarch64"),
                Architecture::X86_64 => (toolchains.join("linux/x86_64"), "x86_64"),
                Architecture::Unknown => {
                    reporter.error("unsupported target CPU architecture");
                    return ExitCode::FAILURE;
                }
            };
            include_directories.push(sysroot.join("include"));
            include_directories.push(sysroot.join(format!("{arch}-linux-gnu/include/c++/14.3.0")));
            include_directories.push(sysroot.join(format!(
                "{arch}-linux-gnu/include/c++/14.3.0/{arch}-linux-gnu"
            )));
            include_directories
                .push(sysroot.join(format!("{arch}-linux-gnu/include/c++/14.3.0/backward")));
            include_directories.push(sysroot.join(format!("{arch}-linux-gnu/sysroot/usr/include")));

            let gtk_dir = toolchains.join("linux/gtk");
            for sub in LINUX_GTK_SUBDIRS {
                include_directories.push(gtk_dir.join(sub));
            }

            (String::new(), format!("{arch}-pc-linux"))
        }

        OperatingSystem::MacOS => {
            let (sysroot, arch) = match target_architecture {
                Architecture::Arm => (toolchains.join("macos/arm64"), "aarch64"),
                Architecture::X86_64 => (toolchains.join("macos/x86_64"), "x86_64"),
                Architecture::Unknown => {
                    reporter.error("unsupported target CPU architecture");
                    return ExitCode::FAILURE;
                }
            };
            (
                format!("-isysroot {}", sysroot.join("MacOSX.sdk").display()),
                format!("{arch}-apple-macosx"),
            )
        }

        OperatingSystem::Windows => {
            let windows = toolchains.join("windows");
            for sub in WINDOWS_INCLUDE_SUBDIRS {
                include_directories.push(windows.join(sub));
            }
            (
                "-Wno-microsoft-anon-tag -Wno-ignored-pragma-intrinsic \
                 -Wno-ignored-attributes -Wno-pragma-pack \
                 -Wno-nonportable-include-path -D_DLL"
                    .to_owned(),
                "x86_64-pc-win32".to_owned(),
            )
        }

        OperatingSystem::Unknown => {
            reporter.error("unsupported target operating system");
            return ExitCode::FAILURE;
        }
    };

    // Render `-I<dir>` for each include directory.
    let mut include_directories_arg = String::new();
    for dir in &include_directories {
        include_directories_arg.push_str("-I");
        include_directories_arg.push_str(&dir.to_string_lossy());
        include_directories_arg.push(' ');
    }

    // --nostd suppresses the `-std=c++17` flag (useful when compiling
    // plain C or Objective-C).
    let std_flag = if cli_info.flags.has_flag("nostd") {
        ""
    } else {
        "-std=c++17"
    };

    // Print the result to stdout. Errors and informational lines go to
    // stderr via `reporter`; the actual clang-flags output is the
    // command's product and goes to stdout so it can be captured.
    println!(
        "-c {std_flag} -target {target_triple} {target_specific_flags} {include_directories_arg}"
    );

    ExitCode::SUCCESS
}

/// Validate and parse the `--os=<value>` flag. Reports the error
/// through `reporter` and returns `None` if missing or invalid.
fn require_os(cli_info: &CLIInfo, reporter: &Reporter) -> Option<OperatingSystem> {
    if !cli_info.flags.has_flag("os") {
        reporter.error(format!(
            "'{} clangflags' requires the 'os' flag",
            cli_info.executable
        ));
        reporter.help(format!(
            "run '{} help clangflags' to see how this command works",
            cli_info.executable
        ));
        return None;
    }

    let value = match cli_info.flags.get_flag("os") {
        Some(v) => v,
        None => {
            reporter.error("'os' flag requires a value");
            reporter.help(format!(
                "run '{} help clangflags' to see how this command works",
                cli_info.executable
            ));
            return None;
        }
    };

    match OperatingSystem::from_name(&value) {
        OperatingSystem::Unknown => {
            reporter.error(format!("'{value}' is not a valid Operating System target"));
            reporter.help(format!(
                "run '{} help clangflags' to see how this command works",
                cli_info.executable
            ));
            None
        }
        os => Some(os),
    }
}

/// Validate and parse the `--arch=<value>` flag. Reports the error
/// through `reporter` and returns `None` if missing or invalid.
fn require_arch(cli_info: &CLIInfo, reporter: &Reporter) -> Option<Architecture> {
    if !cli_info.flags.has_flag("arch") {
        reporter.error(format!(
            "'{} clangflags' requires the 'arch' flag",
            cli_info.executable
        ));
        reporter.help(format!(
            "run '{} help clangflags' to see how this command works",
            cli_info.executable
        ));
        return None;
    }

    let value = match cli_info.flags.get_flag("arch") {
        Some(v) => v,
        None => {
            reporter.error("'arch' flag requires a value");
            reporter.help(format!(
                "run '{} help clangflags' to see how this command works",
                cli_info.executable
            ));
            return None;
        }
    };

    match Architecture::from_name(&value) {
        Architecture::Unknown => {
            reporter.error(format!("'{value}' is not a valid CPU Architecture target"));
            reporter.help(format!(
                "run '{} help clangflags' to see how this command works",
                cli_info.executable
            ));
            None
        }
        arch => Some(arch),
    }
}
