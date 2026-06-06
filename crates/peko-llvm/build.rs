//! Build script for `peko_llvm`.
//!
//! Sets up the static link paths for:
//! - The bundled `lld` driver static archive at `rust_lld/<os>/<arch>/`.
//!   This is the home of `liblldentry.a`, the C++ shim defining the
//!   `lldEntry` symbol called from `linker/mod.rs`, plus the LLD
//!   driver libs (`lldMachO`, `lldCOFF`, `lldELF`, `lldMinGW`,
//!   `lldWasm`, `lldCommon`).
//! - LLVM's own lib directory, located via the `LLVM_SYS_180_PREFIX`
//!   env var that `llvm-sys-180` requires.
//! - `zstd` (LLVM ≥ 18 links against zstd for object-file compression).

use std::path::PathBuf;

fn main() {
    // Required env vars. Each carries a tailored error so a missing
    // variable fails fast with a useful message rather than the
    // default `Result::unwrap()` panic.
    let zstd_lib_prefix = PathBuf::from(env_var(
        "ZSTD_LIB_PREFIX",
        "set this to the directory containing libzstd.{a,lib}",
    ));
    let llvm_build = PathBuf::from(env_var(
        "LLVM_SYS_180_PREFIX",
        "set this to the LLVM 18 install root (the directory containing `bin/`, `lib/`, `include/`)",
    ));
    let project_path = PathBuf::from(env_var(
        "CARGO_MANIFEST_DIR",
        "Cargo should always set this; if you are seeing this error, the build script is being run outside of Cargo",
    ));

    // Select the right `rust_lld/<os>/<arch>/` subdirectory for the
    // host platform. ARM is the fallback for any non-x86 architecture
    // on macOS / Linux — adjust here if RISC-V or other targets are
    // ever supported.
    let rust_lld_root = project_path.join("rust_lld");
    let rust_lld_lib = match std::env::consts::OS {
        "macos" => {
            let os_path = rust_lld_root.join("macos");
            match std::env::consts::ARCH {
                "x86" | "x86_64" => os_path.join("x86_64"),
                _ => os_path.join("arm"),
            }
        }
        "linux" => {
            let os_path = rust_lld_root.join("linux");
            match std::env::consts::ARCH {
                "x86" | "x86_64" => os_path.join("x86_64"),
                _ => os_path.join("arm"),
            }
        }
        "windows" => rust_lld_root.join("windows"),
        other => panic!("unsupported host OS for peko_llvm build: `{other}`"),
    };

    // Emit search paths.
    println!(
        "cargo:rustc-link-search=native={}",
        zstd_lib_prefix.display()
    );
    println!(
        "cargo:rustc-link-search=native={}",
        llvm_build.join("lib").display()
    );
    println!("cargo:rustc-link-search=native={}", rust_lld_lib.display());

    // Emit static-link directives for each LLD driver library.
    for lib in [
        "lldentry",
        "lldMachO",
        "lldCOFF",
        "lldELF",
        "lldMinGW",
        "lldWasm",
        "lldCommon",
    ] {
        println!("cargo:rustc-link-lib=static={lib}");
    }

    // Cargo only re-runs `build.rs` when the script itself changes by
    // default, which means swapping a static lib or pointing LLVM
    // elsewhere wouldn't trigger a rebuild. Track the env vars and the
    // `rust_lld/` directory explicitly so changes are picked up
    // without a `cargo clean`.
    println!("cargo:rerun-if-env-changed=ZSTD_LIB_PREFIX");
    println!("cargo:rerun-if-env-changed=LLVM_SYS_180_PREFIX");
    println!("cargo:rerun-if-changed=rust_lld");
    println!("cargo:rerun-if-changed=build.rs");
}

/// Read an env var, panicking with a custom message if it is unset.
fn env_var(name: &str, hint: &str) -> String {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("env var `{name}` is required for peko_llvm builds; {hint}"))
}
