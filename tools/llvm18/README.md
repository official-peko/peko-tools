# llvm18 bundle builders

These scripts produce the self-contained `llvm18/` toolchain bundles the Peko
SDK ships (`clang`, `clang++`, `llvm-rc`, `llvm-lipo`, and the clang builtin
headers), one dist asset per host. See `docs/llvm18-bundling-plan.md` for the
layout and the reasoning.

Each script downloads or builds LLVM 18.1.8, stages the bundle, validates that
`llvm-rc` finds its sibling `clang` and `clang` finds its resource headers with
a cleared environment, then packs `out/llvm18-<host>.tar.zst`.

## Output

All assets land in `tools/llvm18/out/`:

- `llvm18-macos-arm64.tar.zst`
- `llvm18-macos-x86_64.tar.zst`
- `llvm18-windows-x86_64.tar.zst`
- `llvm18-linux-x86_64.tar.zst`
- `llvm18-linux-arm64.tar.zst`

## Build hosts

### macOS (this Mac, Apple Silicon)

Produces both macOS assets. arm64 is curated from the official prebuilt;
x86_64 is a from-source x86_64 slice (no official 18.1.8 prebuilt exists).

Prerequisites: `brew install cmake ninja zstd` and the Xcode command line tools.

```
tools/llvm18/build-macos.sh arm64     # fast, curated
tools/llvm18/build-macos.sh x86_64    # source build, 30-60 min
tools/llvm18/build-macos.sh all
```

### Windows x86_64 (the Windows machine)

Curates the official `x86_64-pc-windows-msvc` prebuilt.

Prerequisites: `zstd` on PATH (`winget install facebook.zstandard`). `tar` ships
with Windows 10 and later.

```
powershell -ExecutionPolicy Bypass -File tools\llvm18\build-windows.ps1
```

### Linux x86_64 and arm64 (Docker on the Windows machine)

Curation only downloads and repackages prebuilts, so both Linux assets are
produced from one `linux/amd64` container. No qemu, no arm emulation.

Prerequisites: Docker Desktop.

```
tools/llvm18/run-linux-docker.sh
```

To curate on a native Linux host instead of Docker:

```
tools/llvm18/build-linux.sh all
```

## Publishing

Upload the five assets to the `peko-sdk-dist` release. `peko setup` selects the
asset matching the running host and extracts it to `Compiler/llvm18/`.
