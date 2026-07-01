#!/bin/sh
# Rebuilds classes.dex, the prebuilt Java helper for std::webview on Android.
#
# The Peko app is a NativeActivity with no application DEX of its own, so this
# helper ships precompiled with the standard library, the same way the native
# static libraries ship prebuilt. It is rebuilt only when the Java sources
# change, by the toolchain maintainer, never by a user's build.
#
# Requirements: a JDK (javac) and the Android SDK (android.jar and d8). Set
# ANDROID_HOME, or edit the paths below.
set -eu

ANDROID_HOME="${ANDROID_HOME:-$HOME/Library/Android/sdk}"
API="${API:-34}"
BUILD_TOOLS="${BUILD_TOOLS:-34.0.0}"

ANDROID_JAR="$ANDROID_HOME/platforms/android-$API/android.jar"
D8="$ANDROID_HOME/build-tools/$BUILD_TOOLS/d8"

here="$(cd "$(dirname "$0")" && pwd)"
cd "$here"

rm -rf classes classes.dex
mkdir -p classes

javac -source 8 -target 8 \
    -bootclasspath "$ANDROID_JAR" -classpath "$ANDROID_JAR" \
    -d classes src/dev/peko/webview/*.java

"$D8" --release --min-api 22 --lib "$ANDROID_JAR" --output . \
    $(find classes -name '*.class')

rm -rf classes
echo "wrote $here/classes.dex"
