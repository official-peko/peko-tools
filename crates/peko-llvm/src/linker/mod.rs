//! # Peko LLVM Linker
//!
//! `peko_llvm::linker` wraps the bundled `lld` static archive (built
//! from the `rust_lld/lldentry.cc` shim) and dispatches link
//! invocations to the correct driver for the target operating system.
//!
//! The only public entry point is [`lld_link`], which takes a target
//! description plus the set of input objects, sysroot, and output
//! path, and returns whether linking succeeded.

use std::ffi::{c_char, c_int};
use std::path::PathBuf;

use peko_core::target::{Architecture, OperatingSystem, PekoTarget};

use crate::codegen::cstr;

// Link the lld C++ entrypoint shipped as `rust_lld/{os}/{arch}/liblldentry.a`.
#[link(name = "lldentry", kind = "static")]
unsafe extern "C" {
    fn lldEntry(cmd: *const c_char) -> c_int;
}

/// Driver into LLD. Links compiled Peko object files into a final
/// executable or shared library for the supplied `target`.
///
/// Not configured for compiling other compiler outputs (i.e.
/// individual clang++ objects); behavior of this driver when used with
/// other compilation outputs is undefined.
///
/// `sysroot` must point to the target's corresponding sysroot in the
/// `.peko` folder (e.g. `~/.Peko/Compiler/toolchains/macos/arm`).
///
/// `entitlements` is an optional path to an entitlements plist. When supplied
/// on an iOS target it is embedded as the __TEXT,__entitlements section so the
/// simulator keychain grants the linked binary its declared access. It is
/// ignored on every other target and may be `None` when no entitlements are
/// needed.
///
/// Returns `true` on a successful link, `false` otherwise.
pub fn lld_link(
    target: PekoTarget,
    main_object: PathBuf,
    mut linked_objects: Vec<PathBuf>,
    sysroot: PathBuf,
    output: Option<PathBuf>,
    shared: bool,
    entitlements: Option<PathBuf>,
) -> bool {
    linked_objects.insert(0, main_object);

    // Collect platform-specific args, search folders, and library names.
    let mut linked_folders: Vec<PathBuf> = Vec::new();
    let mut linker_libs: Vec<&str> = Vec::new();
    let platform_specific_arguments = match target.operating_system {
        OperatingSystem::Android => {
            let (crt_start, crt_end, link_type) = if shared {
                ("crtbegin_so.o", "crtend_so.o", "-shared")
            } else {
                ("crtbegin_dynamic.o", "crtend_android.o", "-pie")
            };

            // Linker search folders.
            linked_folders.push(sysroot.join("linux/aarch64"));
            linked_folders.push(sysroot.join("sysroot/usr/lib/aarch64-linux-android/22"));
            linked_folders.push(sysroot.join("sysroot/usr/lib/aarch64-linux-android"));
            linked_folders.push(sysroot.join("sysroot/usr/lib"));

            // Runtime objects.
            linked_objects.push(sysroot.join("linux/libclang_rt.builtins-aarch64-android.a"));
            linked_objects.push(sysroot.join(format!(
                "sysroot/usr/lib/aarch64-linux-android/22/{crt_start}"
            )));
            linked_objects.push(sysroot.join(format!(
                "sysroot/usr/lib/aarch64-linux-android/22/{crt_end}"
            )));

            // System libraries.
            linker_libs.extend([
                ":libunwind.a",
                "dl",
                "c",
                "m",
                "c++_static",
                "c++abi",
                "log",
                "android",
                "EGL",
                "GLESv3",
                "OpenSLES",
            ]);

            // Android requires shared objects whose loadable segments align to
            // 16 KB so they load on devices that use a 16 KB page size. -z
            // max-page-size=16384 sets the segment alignment to 16 KB. A binary
            // built this way still loads on 4 KB page devices, so the flag is
            // safe across all Android devices.
            format!(
                "{link_type} -uANativeActivity_onCreate -dynamic-linker /system/bin/linker64 --sysroot={} -m aarch64linux -z max-page-size=16384",
                sysroot.join("sysroot").display()
            )
        }
        OperatingSystem::IOS => {
            let (arch, platform) = match target.architecture {
                Architecture::Arm => ("arm64", "ios"),
                _ => ("x86_64", "ios-simulator"),
            };

            linker_libs.extend(["objc", "c++", "c"]);

            // SSL / crypto.
            linked_objects.push(sysroot.parent().unwrap().join("openssl_libs/libssl.a"));
            linked_objects.push(sysroot.parent().unwrap().join("openssl_libs/libcrypto.a"));

            // Base iOS link arguments.
            let mut ios_arguments = format!(
                "-w -dynamic -arch {arch} -platform_version {platform} 15.0 26.2 -syslibroot {} -framework Security -framework Foundation -framework WebKit -framework UIKit",
                sysroot.join("iPhoneOS.sdk").display()
            );

            // When an entitlements file is supplied, embed it as the
            // __TEXT,__entitlements Mach-O section. On the simulator the
            // keychain reads entitlements from this section, so a build that
            // needs keychain access must include it. When no file is supplied
            // the section is omitted and the link proceeds unchanged.
            if let Some(entitlements_path) = &entitlements {
                ios_arguments.push_str(&format!(
                    " -sectcreate __TEXT __entitlements {}",
                    entitlements_path.display()
                ));
            }

            ios_arguments
        }
        OperatingSystem::Linux => {
            let (arch, linker, emulation) = match target.architecture {
                Architecture::Arm => ("aarch64", "/lib/ld-linux-aarch64.so.1", "aarch64linux"),
                _ => ("x86_64", "/lib64/ld-linux-x86-64.so.2", "elf_x86_64"),
            };

            // Search folders.
            linked_folders.push(sysroot.join("lib"));
            linked_folders.push(sysroot.join(format!("lib/gcc/{arch}-linux-gnu/14.3.0")));
            linked_folders.push(sysroot.join("lib64"));
            linked_folders.push(sysroot.join(format!("{arch}-linux-gnu/lib")));
            linked_folders.push(sysroot.join(format!("{arch}-linux-gnu/lib64")));
            linked_folders.push(sysroot.join(format!("{arch}-linux-gnu/sysroot/lib")));
            linked_folders.push(sysroot.join(format!("{arch}-linux-gnu/sysroot/lib64")));
            linked_folders.push(sysroot.join(format!("{arch}-linux-gnu/sysroot/usr/lib")));
            linked_folders.push(sysroot.join(format!("{arch}-linux-gnu/sysroot/usr/lib64")));

            // C runtime objects.
            linked_objects.push(sysroot.join(format!("{arch}-linux-gnu/sysroot/usr/lib/Scrt1.o")));
            linked_objects
                .push(sysroot.join(format!("lib/gcc/{arch}-linux-gnu/14.3.0/crtbeginS.o")));
            linked_objects.push(sysroot.join(format!("{arch}-linux-gnu/sysroot/usr/lib/crti.o")));
            linked_objects.push(sysroot.join(format!("{arch}-linux-gnu/sysroot/usr/lib/crtn.o")));
            linked_objects.push(sysroot.join(format!("lib/gcc/{arch}-linux-gnu/14.3.0/crtendS.o")));

            // SSL / crypto.
            linked_objects.push(sysroot.join("openssl_libs/libssl.a"));
            linked_objects.push(sysroot.join("openssl_libs/libcrypto.a"));

            // Base libs.
            linker_libs.extend(["stdc++", "m", "gcc_s", "gcc", "c", "pthread"]);

            // WebKit + GTK stack.
            linker_libs.extend([
                "webkit2gtk-4.0",
                "gtk-3",
                "gdk-3",
                "pangocairo-1.0",
                "pango-1.0",
                "harfbuzz",
                "atk-1.0",
                "cairo-gobject",
                "cairo",
                "gdk_pixbuf-2.0",
                "soup-2.4",
                "gmodule-2.0",
                "gio-2.0",
                "javascriptcoregtk-4.0",
                "gobject-2.0",
                "glib-2.0",
            ]);

            format!(
                "-m {emulation} -pie -dynamic-linker {linker} --sysroot={}",
                sysroot.join(format!("{arch}-linux-gnu/sysroot")).display()
            )
        }
        OperatingSystem::MacOS => {
            let arch = match target.architecture {
                Architecture::Arm => "arm64",
                _ => "x86_64",
            };

            // SSL / crypto.
            linked_objects.push(sysroot.join("openssl_libs/libssl.a"));
            linked_objects.push(sysroot.join("openssl_libs/libcrypto.a"));

            linker_libs.extend(["objc", "c", "z", "resolv", "c++", "System", "dl"]);

            format!(
                "-w -dynamic -arch {arch} -demangle -platform_version macos 11.0.0 11.0.0 -syslibroot {} -framework Security -framework WebKit -framework Foundation",
                sysroot.join("MacOSX.sdk").display()
            )
        }
        OperatingSystem::Windows => {
            linked_folders.push(sysroot.join("msvc-lib"));
            linked_folders.push(sysroot.join("ucrt-lib"));
            linked_folders.push(sysroot.join("um-lib"));
            linked_folders.push(sysroot.join("atlmfc-lib"));
            linked_folders.push(sysroot.join("win-lib"));

            linker_libs.extend(["ucrt", "msvcrt", "shell32", "shlwapi", "user32", "ws2_32"]);

            "-machine:x64 -subsystem:console -nologo".to_string()
        }
        _ => panic!("unsupported platform"),
    };

    let lld_driver = match target.operating_system {
        OperatingSystem::Android | OperatingSystem::Linux => "ld.lld",
        OperatingSystem::IOS | OperatingSystem::MacOS => "ld64.lld",
        OperatingSystem::Windows => "lld-link",
        OperatingSystem::Unknown => panic!("Cannot link for unknown platform"),
    };

    // Output path syntax differs between the COFF (Windows) driver and
    // the ELF / Mach-O drivers.
    let output_argument = match output {
        Some(output_path) => match target.operating_system {
            OperatingSystem::Windows => format!("-out:{}", output_path.to_str().unwrap()),
            _ => format!("-o {}", output_path.to_str().unwrap()),
        },
        None => match target.operating_system {
            OperatingSystem::Windows => "-out:a.exe".to_string(),
            _ => "-o a.out".to_string(),
        },
    };

    let linker_folder_prefix = match target.operating_system {
        OperatingSystem::Windows => "-libpath:",
        _ => "-L",
    };

    let linker_lib_prefix = match target.operating_system {
        OperatingSystem::Windows => "-defaultlib:",
        _ => "-l",
    };

    // Concatenate the object, search-folder, and library lists into the
    // single flat argument string the lld driver expects.
    let mut objects_to_link_argument = String::new();
    for file in &linked_objects {
        objects_to_link_argument.push_str(file.to_str().unwrap());
        objects_to_link_argument.push(' ');
    }

    let mut linker_directories_argument = String::new();
    for linker_directory in &linked_folders {
        linker_directories_argument.push_str(linker_folder_prefix);
        linker_directories_argument.push_str(linker_directory.to_str().unwrap());
        linker_directories_argument.push(' ');
    }

    let mut linker_libs_argument = String::new();
    for lib in &linker_libs {
        linker_libs_argument.push_str(linker_lib_prefix);
        linker_libs_argument.push_str(lib);
        linker_libs_argument.push(' ');
    }

    let command = cstr(format!(
        "{lld_driver} {output_argument} {platform_specific_arguments} {linker_directories_argument} {linker_libs_argument} {objects_to_link_argument}"
    ));

    unsafe { lldEntry(command.as_ptr()) == 0 }
}
