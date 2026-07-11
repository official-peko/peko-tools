#!/usr/bin/env bash
# Run the Linux curation inside a linux/amd64 container. Curation only downloads
# and repackages prebuilts, so both linux-x86_64 and linux-arm64 are produced
# from one amd64 container without qemu. Meant to run from Docker on the Windows
# machine (Git Bash or WSL) or any Docker host.
#
# The built assets land in tools/llvm18/out on the host.

set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"

docker run --rm --platform linux/amd64 \
  -v "$repo:/work" -w /work \
  ubuntu:22.04 \
  bash -lc '
    set -euo pipefail
    apt-get update -qq
    apt-get install -y -qq curl xz-utils zstd tar >/dev/null
    tools/llvm18/build-linux.sh all
  '
echo "assets in $here/out"
