//! Build script for `peko_llvm`.
//!
//! Compiles the `lldentry` shim (`rust_lld/lldentry.cc`) from source with the
//! system C++ compiler against the LLVM/LLD headers in the
//! `LLVM_SYS_180_PREFIX` prefix, and links the LLD driver libraries from that
//! prefix. Cargo caches the compiled shim and only rebuilds it when the shim,
//! the build script, or the prefix changes. The LLVM core libraries are linked
//! by the `llvm-sys` crate; `zstd` is pulled in by LLVM 18 for object-file
//! compression.

use std::path::PathBuf;

/// The LLD driver libraries linked from the LLVM prefix.
const LLD_LIBS: &[&str] = &[
    "lldMachO",
    "lldCOFF",
    "lldELF",
    "lldMinGW",
    "lldWasm",
    "lldCommon",
];

fn main() {
    let llvm_prefix = PathBuf::from(env_var(
        "LLVM_SYS_180_PREFIX",
        "set this to the LLVM 18 install root (the directory containing `bin/`, `lib/`, `include/`)",
    ));
    let zstd_lib_prefix = PathBuf::from(env_var(
        "ZSTD_LIB_PREFIX",
        "set this to the directory containing libzstd.{a,lib}",
    ));

    // Compile the lld entry shim against the LLVM/LLD headers. These flags
    // mirror `llvm-config --cxxflags` for LLVM 18, which is built without
    // exceptions or RTTI. `cc` emits the static-link directive for the
    // compiled `lldentry` archive.
    cc::Build::new()
        .cpp(true)
        .file("rust_lld/lldentry.cc")
        .include(llvm_prefix.join("include"))
        .std("c++17")
        .flag("-fno-exceptions")
        .flag("-fno-rtti")
        .flag_if_supported("-funwind-tables")
        // The LLVM headers trip -Wunused-parameter under the compiler default
        // warnings; silence it so the shim build stays quiet.
        .flag_if_supported("-Wno-unused-parameter")
        .define("__STDC_CONSTANT_MACROS", None)
        .define("__STDC_FORMAT_MACROS", None)
        .define("__STDC_LIMIT_MACROS", None)
        .compile("lldentry");

    // Link the LLD driver libraries from the prefix.
    println!(
        "cargo:rustc-link-search=native={}",
        llvm_prefix.join("lib").display()
    );
    println!(
        "cargo:rustc-link-search=native={}",
        zstd_lib_prefix.display()
    );
    for lib in LLD_LIBS {
        println!("cargo:rustc-link-lib=static={lib}");
    }

    println!("cargo:rerun-if-env-changed=LLVM_SYS_180_PREFIX");
    println!("cargo:rerun-if-env-changed=ZSTD_LIB_PREFIX");
    println!("cargo:rerun-if-changed=rust_lld/lldentry.cc");
    println!("cargo:rerun-if-changed=build.rs");
}

/// Read an env var, panicking with a custom message if it is unset.
fn env_var(name: &str, hint: &str) -> String {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("env var `{name}` is required for peko_llvm builds; {hint}"))
}
