//! # Peko LLVM Linker
//!
//! `peko_llvm::linker` drives LLD through the `lldentry` shim and assembles the
//! link command from a resolved toolchain description. The only public entry
//! point is [`lld_link`].

use std::ffi::{c_char, c_int};
use std::path::{Path, PathBuf};

use peko_core::config::{Toolchain, resolve_flag};
use peko_core::target::{OperatingSystem, PekoTarget};

use crate::codegen::cstr;

// The lld C++ entrypoint, compiled from `rust_lld/lldentry.cc` by the build
// script and linked against the LLD driver libraries from the LLVM prefix.
#[link(name = "lldentry", kind = "static")]
unsafe extern "C" {
    fn lldEntry(cmd: *const c_char) -> c_int;
}

/// Link compiled Peko objects into an executable or shared library for
/// `target`, driven by the resolved `toolchain` and its directory.
///
/// `toolchain_dir` is the base for the toolchain's relative paths (its
/// `Compiler/toolchains/<os>/<arch>` directory). `output` is the binary path,
/// or `None` for the driver default. `shared` builds a shared library on the
/// targets that support it. `entitlements`, when supplied on an iOS target, is
/// embedded as the `__TEXT,__entitlements` section. `package_link_args` are the
/// raw linker arguments a package requests through its `[native.link]` table
/// (for example `-framework Cocoa` for the desktop webview); they are passed to
/// the driver ahead of the input objects.
///
/// Returns `true` on a successful link.
pub fn lld_link(
    target: PekoTarget,
    main_object: PathBuf,
    mut linked_objects: Vec<PathBuf>,
    toolchain: &Toolchain,
    toolchain_dir: &Path,
    output: Option<PathBuf>,
    shared: bool,
    entitlements: Option<PathBuf>,
    package_link_args: Vec<String>,
) -> bool {
    linked_objects.insert(0, main_object);

    let link = &toolchain.link;
    let windows = target.operating_system == OperatingSystem::Windows;

    // Resolve the toolchain's relative inputs against its directory.
    let lib_paths: Vec<PathBuf> = link
        .lib_paths
        .iter()
        .map(|path| toolchain_dir.join(path))
        .collect();
    let mut objects: Vec<PathBuf> = link
        .objects
        .iter()
        .map(|object| toolchain_dir.join(object))
        .collect();
    let mut flags: Vec<String> = link
        .flags
        .iter()
        .map(|flag| resolve_flag(toolchain_dir, flag))
        .collect();

    apply_conditionals(
        target.operating_system,
        shared,
        entitlements.as_deref(),
        &mut objects,
        &mut flags,
    );

    let mut tokens: Vec<String> = vec![link.driver.clone()];

    // Output path. The COFF driver spells it differently from ELF / Mach-O.
    match output {
        Some(path) if windows => tokens.push(format!("-out:{}", path.display())),
        Some(path) => {
            tokens.push("-o".to_owned());
            tokens.push(path.display().to_string());
        }
        None if windows => tokens.push("-out:a.exe".to_owned()),
        None => {
            tokens.push("-o".to_owned());
            tokens.push("a.out".to_owned());
        }
    }

    // Driver flags, then frameworks.
    tokens.extend(flags);
    for framework in &link.frameworks {
        tokens.push("-framework".to_owned());
        tokens.push(framework.clone());
    }

    // Library search paths and libraries.
    let path_prefix = if windows { "-libpath:" } else { "-L" };
    for path in &lib_paths {
        tokens.push(format!("{path_prefix}{}", path.display()));
    }
    let lib_prefix = if windows { "-defaultlib:" } else { "-l" };
    for lib in &link.libs {
        tokens.push(format!("{lib_prefix}{lib}"));
    }

    // Package-requested link arguments from `[native.link]`, passed to the
    // driver verbatim. A framework request is two tokens (`-framework`,
    // `WebKit`); each array entry is already one token.
    tokens.extend(package_link_args);

    // Objects last: the project objects, then the toolchain's runtime objects.
    for object in linked_objects.iter().chain(objects.iter()) {
        tokens.push(object.display().to_string());
    }

    // Quote every token so paths containing spaces survive the shim's split.
    let command = cstr(
        tokens
            .iter()
            .map(|token| format!("\"{token}\""))
            .collect::<Vec<_>>()
            .join(" "),
    );

    unsafe { lldEntry(command.as_ptr()) == 0 }
}

/// Apply the target-specific link adjustments the toolchain data cannot express
/// on its own.
///
/// An Android shared library swaps the executable crt objects for the shared
/// variants and `-pie` for `-shared`. An iOS build with entitlements appends
/// the `-sectcreate __TEXT __entitlements` section.
fn apply_conditionals(
    os: OperatingSystem,
    shared: bool,
    entitlements: Option<&Path>,
    objects: &mut [PathBuf],
    flags: &mut Vec<String>,
) {
    match os {
        OperatingSystem::Android if shared => {
            for object in objects.iter_mut() {
                swap_file_name(object, "crtbegin_dynamic.o", "crtbegin_so.o");
                swap_file_name(object, "crtend_android.o", "crtend_so.o");
            }
            for flag in flags.iter_mut() {
                if flag == "-pie" {
                    *flag = "-shared".to_owned();
                }
            }
        }
        OperatingSystem::IOS => {
            if let Some(entitlements) = entitlements {
                flags.push("-sectcreate".to_owned());
                flags.push("__TEXT".to_owned());
                flags.push("__entitlements".to_owned());
                flags.push(entitlements.display().to_string());
            }
        }
        _ => {}
    }
}

/// Rename a path in place when its file name matches `from`.
fn swap_file_name(path: &mut PathBuf, from: &str, to: &str) {
    if path.file_name().and_then(|name| name.to_str()) == Some(from) {
        path.set_file_name(to);
    }
}
