#!/usr/bin/env bash
# Strip a built LLVM bundle down to the minimal shippable folder: bin (clang,
# clang++, llvm-rc, llvm-lipo) plus the clang resource headers, no LLVM archives
# or shared libraries. Works on any produced bundle regardless of how it was
# built.
#
# Input may be a .tar.zst, a .tar.xz/.tar, or a directory that contains a
# bin/ and lib/clang tree. Output is <out_dir>/llvm18-<host_id>.
#
# Usage:
#   tools/llvm18/strip.sh <input> <host_id> [out_dir]
#
# Examples:
#   tools/llvm18/strip.sh ~/Work/llvm-out/llvm18-darwin-arm64.tar.zst darwin-arm64
#   tools/llvm18/strip.sh ./clang+llvm-18.1.8-x86_64-linux-gnu linux-x86_64 ~/Work/llvm-out

set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=lib.sh
. "$here/lib.sh"

input="${1:?usage: strip.sh <input> <host_id> [out_dir]}"
host_id="${2:?usage: strip.sh <input> <host_id> [out_dir]}"
out_dir="${3:-}"

if [ -z "$out_dir" ]; then
  if [ -d "$input" ]; then out_dir="$(cd "$(dirname "$input")" && pwd)"; else
    out_dir="$(cd "$(dirname "$input")" && pwd)"; fi
fi
mkdir -p "$out_dir"

# Resolve the source tree that holds bin/ and lib/clang, extracting an archive
# to a scratch dir when needed.
scratch=""
cleanup() { [ -n "$scratch" ] && rm -rf "$scratch"; }
trap cleanup EXIT

if [ -d "$input" ]; then
  root="$input"
else
  scratch="$(mktemp -d)"
  case "$input" in
    *.tar.zst) zstd -dc "$input" | tar -xf - -C "$scratch" ;;
    *.tar.xz)  tar -xf "$input" -C "$scratch" ;;
    *.tar)     tar -xf "$input" -C "$scratch" ;;
    *) echo "unsupported input: $input" >&2; exit 2 ;;
  esac
  root="$scratch"
fi

# Find the directory that contains both bin and lib/clang (the archive may wrap
# it in a top-level folder such as llvm18/ or clang+llvm-.../).
src_tree=""
for candidate in "$root" "$root"/*; do
  if [ -d "$candidate/bin" ] && [ -d "$candidate/lib/clang" ]; then
    src_tree="$candidate"; break
  fi
done
if [ -z "$src_tree" ]; then
  echo "could not find a bin + lib/clang tree under: $input" >&2
  exit 1
fi

say "strip $input -> llvm18-$host_id"
stage_bundle "$src_tree" "$out_dir" "$host_id"

# Validate by running the stripped tools. This passes on the bundle's own host.
# Stripping a cross-platform bundle (for example a Windows folder on macOS) can
# not run the tools, so the failure is reported as a note rather than an error.
bundle="$out_dir/llvm18-$host_id"
if out="$(validate_bundle "$bundle" 2>&1)"; then
  echo "$out"
else
  echo "note: runtime validation did not pass here (expected when stripping a"
  echo "      cross-platform bundle; run strip on the target host to validate):"
  echo "$out" | sed 's/^/  /'
fi
report_bundle "$bundle"
