#!/usr/bin/env bash
# Build the macOS llvm18 bundles on an Apple Silicon Mac.
#
# arm64 is curated from the official LLVM 18.1.8 prebuilt. x86_64 has no
# official prebuilt for this version, so it is built from source as an x86_64
# slice on the arm host. Requires cmake, ninja, git, curl, zstd, and the Xcode
# command line tools.
#
# Usage:
#   tools/llvm18/build-macos.sh arm64     # curate the arm64 prebuilt
#   tools/llvm18/build-macos.sh x86_64    # source-build the x86_64 slice
#   tools/llvm18/build-macos.sh all       # both

set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=lib.sh
. "$here/lib.sh"

OUT="${OUT:-$here/out}"
WORK="${WORK:-$here/work}"
mkdir -p "$OUT" "$WORK"

build_arm64() {
  say "macOS arm64: curate prebuilt"
  local tarball="$WORK/llvm-arm64.tar.xz" tree="$WORK/llvm-arm64"
  local url="https://github.com/llvm/llvm-project/releases/download/llvmorg-$LLVM_VERSION/clang+llvm-$LLVM_VERSION-arm64-apple-macos11.tar.xz"
  [ -f "$tarball" ] || curl -fL "$url" -o "$tarball"
  rm -rf "$tree" && mkdir -p "$tree"
  tar -xf "$tarball" -C "$tree" --strip-components=1
  stage_bundle "$tree" "$OUT" macos-arm64
  validate_bundle "$OUT/llvm18-macos-arm64"
  report_bundle "$OUT/llvm18-macos-arm64"
}

build_x86_64() {
  say "macOS x86_64: source build (x86_64 slice on arm host)"
  local src="$WORK/llvm-project" build="$WORK/build-x86_64"
  if [ ! -d "$src" ]; then
    git clone --depth 1 --branch "llvmorg-$LLVM_VERSION" \
      https://github.com/llvm/llvm-project.git "$src"
  fi
  rm -rf "$build"
  # CMAKE_OSX_ARCHITECTURES=x86_64 makes the tools x86_64; the build-time tools
  # (tblgen) run under Rosetta on an arm host. CMAKE_POLICY_VERSION_MINIMUM lets
  # CMake 4 configure LLVM 18, whose older subprojects request a pre-3.5 policy.
  cmake -S "$src/llvm" -B "$build" -G Ninja \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_OSX_ARCHITECTURES=x86_64 \
    -DCMAKE_POLICY_VERSION_MINIMUM=3.5 \
    -DLLVM_ENABLE_PROJECTS=clang \
    -DLLVM_TARGETS_TO_BUILD="X86;AArch64" \
    -DLLVM_ENABLE_ZSTD=OFF \
    -DLLVM_INCLUDE_TESTS=OFF \
    -DLLVM_INCLUDE_BENCHMARKS=OFF \
    -DLLVM_INCLUDE_EXAMPLES=OFF \
    -DLLVM_LINK_LLVM_DYLIB=OFF
  # The build tree already has the bin and lib/clang layout the stager needs, so
  # it is staged directly without a separate install step.
  ninja -C "$build" clang clang-resource-headers llvm-rc llvm-lipo
  stage_bundle "$build" "$OUT" macos-x86_64
  validate_bundle "$OUT/llvm18-macos-x86_64"
  report_bundle "$OUT/llvm18-macos-x86_64"
}

case "${1:-all}" in
  arm64) build_arm64 ;;
  x86_64) build_x86_64 ;;
  all) build_arm64; build_x86_64 ;;
  *) echo "usage: $0 {arm64|x86_64|all}" >&2; exit 2 ;;
esac
