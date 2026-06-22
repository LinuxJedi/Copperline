#!/usr/bin/env bash
# Build the Linux test image and run the headless Vulkan present smoke test in
# a container. Safe from an Apple Silicon (or any) Mac host: the container runs
# as the host's native platform (linux/arm64 on Apple Silicon) and exercises
# the same wgpu code path as an x86_64 Linux host -- the surfaceless-GL bug the
# Vulkan policy avoids is hardware- and architecture-independent.
#
#   packaging/test-linux-vulkan/run.sh
#
# The first run builds the image and the release binary (slow); cargo's
# registry and a Linux-only target dir are kept in named Docker volumes so
# repeat runs are fast and the host's target/ is never clobbered.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$here/../.." && pwd)"
cd "$repo_root"

img=copperline-linux-test

echo "==> Building test image ($img)"
docker build -t "$img" -f packaging/test-linux-vulkan/Dockerfile .

echo "==> Running smoke test"
docker run --rm \
  -v "$repo_root":/src \
  -v copperline-lin-target:/lin-target \
  -v copperline-cargo-registry:/usr/local/cargo/registry \
  -e CARGO_TARGET_DIR=/lin-target \
  "$img"
