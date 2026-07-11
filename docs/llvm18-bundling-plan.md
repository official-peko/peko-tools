# Self-contained LLVM 18 bundle plan

## Problem

The Windows build fails at the `.rc` step with `llvm-rc: Unable to find clang,
skipping preprocessing`. Root cause, confirmed empirically:

- `llvm-rc` preprocesses every `.rc` by spawning a program named exactly
  `clang` (observed argv: `clang --driver-mode=gcc -target
  <arch>-pc-windows-msvc-coff -E -xc -DRC_INVOKED <in> -o <tmp>`). It searches
  PATH and its own directory. When no `clang` is reachable it prints the error
  above and either skips preprocessing or fails.
- The shipped clang is in a different directory from `llvm-rc`
  (`Compiler/bin/clang/` vs `Compiler/bin/llvm-rc/`) and on macOS and Linux it
  is named `clang-darwin-arm` / `clang-linux-x86_64`, never plain `clang`. So
  `llvm-rc` cannot find it on any host.
- Separately, `clang` locates its builtin headers (stddef.h, stdint.h, the
  intrinsics) in a resource directory at `<clang-dir>/../lib/clang/18`. Those
  headers are shipped but only resolve when clang sits one level under a
  `lib/clang/18` sibling.

Both discovery mechanisms are directory-relative. A single directory that holds
`clang`, `llvm-rc`, and `llvm-lipo` as siblings, with the resource headers one
level up, satisfies both with no flags.

## Target layout

One directory per host under `Compiler/llvm18`, named for the host os-arch:

```
Compiler/llvm18/<os>-<arch>/
  bin/
    clang        (clang.exe on Windows)
    llvm-rc      (llvm-rc.exe on Windows)
    llvm-lipo    (llvm-lipo.exe on Windows)
  lib/
    clang/
      18/
        include/    builtin headers (stddef.h, stdint.h, *intrin.h, ...)
```

The host directories are `macos-arm64`, `macos-x86_64`, `linux-arm64`,
`linux-x86_64`, and `windows-x86_64`.

- `llvm-rc` finds sibling `clang` in `bin/`.
- `clang` finds `bin/../lib/clang/18/include`.
- Every tool is named canonically, so no per-host name matching remains.
- `clang++` is not shipped: the build only ever invokes `clang`.

## Distribution model

The five host directories ship inside the existing `Compiler.tar.zst` on
`peko-sdk-dist`, under `Compiler/llvm18/<os>-<arch>/`. `peko setup` extracts the
whole Compiler, so the tools install with no setup change. The old
`Compiler/bin/{clang,lib,llvm-rc,llvm-lipo}` per-host trees are removed.

Because the release re-uses the `v2.0.0` tag, an already-installed host needs
`peko setup --force` to pick up the refreshed Compiler.

## Build sources

LLVM 18.1.8 official prebuilts exist for four of the five hosts. Curating those
is minutes of work and ships the same binaries and resource headers as a source
build. Only macOS x86_64 has no official prebuilt and needs a source build.

| Host           | Source                                                        |
| -------------- | ------------------------------------------------------------- |
| macos-arm64   | prebuilt `clang+llvm-18.1.8-arm64-apple-macos11.tar.xz`       |
| macos-x86_64  | source build on the arm Mac, `CMAKE_OSX_ARCHITECTURES=x86_64` |
| windows-x86_64 | prebuilt `clang+llvm-18.1.8-x86_64-pc-windows-msvc.tar.xz`    |
| linux-x86_64   | prebuilt `clang+llvm-18.1.8-x86_64-linux-gnu-ubuntu-18.04.tar.xz` |
| linux-arm64    | prebuilt `clang+llvm-18.1.8-aarch64-linux-gnu.tar.xz`         |

Build-host assignment:

- macOS arm64 and x86_64: this Mac.
- Windows x86_64: the Windows machine.
- Linux x86_64 and arm64: Docker on the Windows machine (curation only, no
  compile, so a linux/amd64 container curates both prebuilts without qemu).

## Staging and validation

Every build path ends in the shared stager, which:

1. Copies `clang`, `llvm-rc`, `llvm-lipo` into `llvm18/bin` (dereferencing the
   `clang -> clang-18` symlink and naming them canonically).
2. Copies `lib/clang/18/include` into `llvm18/lib/clang/18/include`.
3. Confirms the tools link only system libraries (the release clang binaries are
   static), so no LLVM archives or shared libraries are staged. `strip.sh`
   applies the same reduction to any produced bundle after the fact.
4. Validates by running `llvm-rc` on a test `.rc` under a cleared PATH. A
   produced `.res` proves `llvm-rc` found the sibling `clang` and `clang` found
   its resource headers, with no external tooling.

## CLI rewiring

A shared resolver in `execution/native.rs` maps the running host to
`Compiler/llvm18/<os>-<arch>/bin` and returns a tool there, or the bare tool
name on PATH when the bundle is absent:

- `host_clang` uses the bundled clang on every host. It ships with its resource
  headers at the sibling `../lib/clang`, so it compiles for any target through
  `-target`.
- `bundler/windows.rs` resolves `llvm-rc` from the same directory, beside the
  clang it invokes to preprocess the `.rc`.
- `bundler/macos.rs` resolves `llvm-lipo` from the same directory.
- The per-host name matching in all three is removed.

`peko setup` needs no change: the host directories ship inside `Compiler.tar.zst`
and install with the rest of the Compiler.
