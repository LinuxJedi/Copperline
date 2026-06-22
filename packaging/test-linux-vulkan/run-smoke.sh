#!/usr/bin/env bash
# Build Copperline inside the Linux test container and verify the presentation
# path initializes headlessly on software Vulkan (Mesa lavapipe).
#
# Runs inside the image from packaging/test-linux-vulkan/Dockerfile. The repo
# is bind-mounted at /src and CARGO_TARGET_DIR points at a cache volume, so the
# host's (macOS) target/ is never touched. Mirrors the environment of a host
# without a hardware Vulkan driver: lavapipe is the only ICD present.
set -euo pipefail

echo "==> Vulkan ICDs visible to the loader:"
if ! vulkaninfo --summary 2>/dev/null | sed -n '1,25p'; then
  echo "ERROR: no usable Vulkan driver in the container" >&2
  exit 1
fi

echo
echo "==> Building release binary (Linux, in-container target dir)"
cargo build --release --locked --bin copperline

bin="${CARGO_TARGET_DIR:-target}/release/copperline"
out="/tmp/copperline-smoke.png"
rm -f "$out"

echo
echo "==> Headless screenshot under Xvfb (exercises the Vulkan present init)"
# A hidden window + present surface are still created in screenshot mode, so a
# successful PNG proves the wgpu instance/adapter/surface came up on Vulkan.
xvfb-run -a -s "-screen 0 1280x720x24" \
  "$bin" --noaudio --screenshot-after 3 "$out"

if [ -s "$out" ]; then
  echo
  echo "PASS: present path initialized and rendered ($(stat -c %s "$out") bytes) -> $out"
else
  echo "FAIL: no screenshot produced; the present surface did not initialize" >&2
  exit 1
fi
