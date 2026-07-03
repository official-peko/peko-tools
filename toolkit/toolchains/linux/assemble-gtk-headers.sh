#!/usr/bin/env bash
# Assemble the Linux GTK and WebKit development headers for the Peko cross
# toolchain.
#
# The linux/arm and linux/x86_64 toolchains compile the std::webview GTK
# backend and include a shared header tree at linux/gtk. That tree is not
# checked in because it is a large third party blob. This script rebuilds it
# from Ubuntu 22.04 (jammy), the distribution whose runtime .so versions are
# already bundled in the toolchain (libwebkit2gtk-4.0.so.37,
# libjavascriptcoregtk-4.0.so.18, libicu*.so.70). Header and runtime versions
# stay matched that way.
#
# The headers are portable C and shared by both target architectures. The one
# architecture specific file, glibconfig.h, is identical for aarch64 and
# x86_64 because both are 64 bit little endian LP64, so a single arm64 pass
# populates the shared tree. Requires a running Docker daemon.
#
# Usage: assemble-gtk-headers.sh [DEST]
#   DEST defaults to ~/.Peko/Compiler/toolchains/linux/gtk
set -euo pipefail

DEST="${1:-$HOME/.Peko/Compiler/toolchains/linux/gtk}"
mkdir -p "$DEST"
echo "Assembling GTK/WebKit headers into $DEST"

docker run --rm --platform linux/arm64 -v "$DEST":/out ubuntu:22.04 \
  bash -euo pipefail -c '
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends \
      libgtk-3-dev libwebkit2gtk-4.0-dev libsoup2.4-dev \
      libatk1.0-dev libatk-bridge2.0-dev libatspi2.0-dev \
      libcairo2-dev libpango1.0-dev libgdk-pixbuf-2.0-dev \
      libglib2.0-dev libharfbuzz-dev libfreetype6-dev libfribidi-dev \
      libxml2-dev libpng-dev libpixman-1-dev libdbus-1-dev \
      libblkid-dev libmount-dev uuid-dev >/dev/null

    TRIPLE=aarch64-linux-gnu
    # cp -aL follows symlinks so the destination has no dangling links.
    copy() { rm -rf "/out/$2"; cp -aL "$1" "/out/$2"; }

    # Include directories taken verbatim from /usr/include.
    for d in webkitgtk-4.0 at-spi-2.0 atk-1.0 blkid cairo dbus-1.0 \
             freetype2 fribidi gdk-pixbuf-2.0 gio-unix-2.0 glib-2.0 \
             gtk-3.0 harfbuzz libmount libpng16 libsoup-2.4 libxml2 \
             pango-1.0 pixman-1 uuid; do
      copy "/usr/include/$d" "$d"
    done

    # Architecture specific and nested layouts.
    copy "/usr/lib/$TRIPLE/glib-2.0/include" glib-include
    copy "/usr/lib/$TRIPLE/dbus-1.0/include" dbus
    copy "/usr/include/at-spi2-atk/2.0" at-spi2-atk

    echo "assembled $(ls /out | wc -l) header directories"
  '

# Verify the header the Linux webview build first fails on is now present.
if [ -f "$DEST/webkitgtk-4.0/JavaScriptCore/JavaScript.h" ]; then
  echo "OK: JavaScriptCore/JavaScript.h present"
else
  echo "ERROR: JavaScriptCore/JavaScript.h missing after assembly" >&2
  exit 1
fi
