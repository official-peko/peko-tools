#!/usr/bin/env bash
# Build the Linux llvm18 bundles by curating the official LLVM 18.1.8 prebuilts.
# Both linux-x86_64 and linux-arm64 are curated (no compile), so this runs in a
# single linux/amd64 container with no qemu. Meant to run inside the container
# started by run-linux-docker.sh, but works on any Linux host with curl, tar,
# xz, and zstd.
#
# Usage:
#   tools/llvm18/build-linux.sh x86_64
#   tools/llvm18/build-linux.sh arm64
#   tools/llvm18/build-linux.sh all

set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=lib.sh
. "$here/lib.sh"

OUT="${OUT:-$here/out}"
WORK="${WORK:-$here/work}"
mkdir -p "$OUT" "$WORK"

curate() {
  local host_id="$1" prebuilt="$2"
  say "Linux $host_id: curate prebuilt"
  local tarball="$WORK/llvm-$host_id.tar.xz" tree="$WORK/llvm-$host_id"
  local url="https://github.com/llvm/llvm-project/releases/download/llvmorg-$LLVM_VERSION/$prebuilt"
  [ -f "$tarball" ] || curl -fL "$url" -o "$tarball"
  rm -rf "$tree" && mkdir -p "$tree"
  tar -xf "$tarball" -C "$tree" --strip-components=1
  stage_bundle "$tree" "$OUT" "$host_id"
  validate_bundle "$OUT/llvm18-$host_id"
  report_bundle "$OUT/llvm18-$host_id"
}

case "${1:-all}" in
  x86_64) curate linux-x86_64 "clang+llvm-$LLVM_VERSION-x86_64-linux-gnu-ubuntu-18.04.tar.xz" ;;
  arm64)  curate linux-arm64  "clang+llvm-$LLVM_VERSION-aarch64-linux-gnu.tar.xz" ;;
  all)
    curate linux-x86_64 "clang+llvm-$LLVM_VERSION-x86_64-linux-gnu-ubuntu-18.04.tar.xz"
    curate linux-arm64  "clang+llvm-$LLVM_VERSION-aarch64-linux-gnu.tar.xz"
    ;;
  *) echo "usage: $0 {x86_64|arm64|all}" >&2; exit 2 ;;
esac
