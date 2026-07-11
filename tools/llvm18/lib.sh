#!/usr/bin/env bash
# Shared helpers for assembling a self-contained llvm18 bundle on macOS and
# Linux. Sourced by the per-platform build scripts.

set -euo pipefail

LLVM_VERSION="18.1.8"
LLVM_RESOURCE_MAJOR="18"

# Print a step banner.
say() { printf '\n== %s\n' "$*"; }

# Copy one tool from an extracted LLVM tree into the bundle bin, dereferencing
# symlinks and naming it canonically. Arguments: src_tree, bin_name, out_bin.
copy_tool() {
  local src_tree="$1" name="$2" out_bin="$3"
  local candidate
  for candidate in "$src_tree/bin/$name" "$src_tree/bin/$name-$LLVM_RESOURCE_MAJOR"; do
    if [ -e "$candidate" ]; then
      cp -L "$candidate" "$out_bin/$name"
      chmod +x "$out_bin/$name"
      return 0
    fi
  done
  echo "missing tool: $name in $src_tree/bin" >&2
  return 1
}

# Assemble the bundle. Arguments: src_tree (extracted clang+llvm), out_dir,
# host_id. Produces out_dir/llvm18-<host_id> holding only bin and the clang
# resource headers under lib/clang/<major>/include. The tools link only system
# libraries, so no LLVM archives or shared libraries are staged.
stage_bundle() {
  local src_tree="$1" out_dir="$2" host_id="$3"
  local bundle="$out_dir/llvm18-$host_id"
  rm -rf "$bundle"
  mkdir -p "$bundle/bin" "$bundle/lib/clang/$LLVM_RESOURCE_MAJOR"

  copy_tool "$src_tree" clang "$bundle/bin"
  copy_tool "$src_tree" llvm-rc "$bundle/bin"
  copy_tool "$src_tree" llvm-lipo "$bundle/bin"

  # Builtin headers. The release tree keeps them under lib/clang/<major>/include.
  local res_include="$src_tree/lib/clang/$LLVM_RESOURCE_MAJOR/include"
  if [ ! -d "$res_include" ]; then
    echo "missing resource headers: $res_include" >&2
    return 1
  fi
  cp -R "$res_include" "$bundle/lib/clang/$LLVM_RESOURCE_MAJOR/include"
}

# Confirm the bundle is self-contained: llvm-rc must find sibling clang and
# clang must find its resource headers, with PATH cleared. Argument: bundle dir.
validate_bundle() {
  local bundle="$1"
  # A subshell owns the temp dir so its EXIT trap cannot leak to the caller.
  (
    set -eu
    work="$(mktemp -d)"
    trap 'rm -rf "$work"' EXIT
    cat > "$work/res.rc" <<'RC'
#define APPVER 1,0,0,0
STRINGTABLE BEGIN 1 "peko" END
RC
    ( cd "$work" && env -i "$bundle/bin/llvm-rc" res.rc ) >/dev/null 2>"$work/err" || true
    if [ ! -s "$work/res.res" ]; then
      echo "validation failed: no res.res produced" >&2
      cat "$work/err" >&2
      exit 1
    fi
    if grep -q "Unable to find clang" "$work/err"; then
      echo "validation failed: llvm-rc could not find sibling clang" >&2
      exit 1
    fi
    # clang resolves its own builtin headers for a Windows target.
    printf '#include <stddef.h>\nsize_t s(void){return sizeof(void*);}\n' > "$work/t.c"
    if ! env -i "$bundle/bin/clang" -c -target x86_64-pc-win32 "$work/t.c" -o "$work/t.o" 2>"$work/cerr"; then
      echo "validation failed: clang could not compile with bundled headers" >&2
      cat "$work/cerr" >&2
      exit 1
    fi
    echo "validation ok: $bundle"
  )
}

# Report the finished bundle. Argument: bundle dir.
report_bundle() {
  local bundle="$1"
  echo "staged: $bundle ($(du -sh "$bundle" | cut -f1))"
}
