# Headless Linux GPU smoke test for Copperline's presentation path.
#
# Recreates what a Vulkan-less / software-GPU Linux host looks like: software
# Vulkan (Mesa lavapipe) plus a virtual X server (Xvfb), with no hardware GPU.
# Used to verify that the Vulkan present path initializes and renders a frame
# headlessly, which guards the "Vulkan required on Linux" policy in window.rs.
# See packaging/test-linux-vulkan/run-smoke.sh for the actual checks.
#
# Build from the repo root:
#   docker build -t copperline-linux-test -f packaging/test-linux-vulkan/Dockerfile .
# Or just use packaging/test-linux-vulkan/run.sh, which also wires up caches.
FROM rust:bookworm

RUN apt-get update && apt-get install -y --no-install-recommends \
      pkg-config \
      libasound2-dev \
      libudev-dev \
      xvfb \
      xauth \
      libvulkan1 \
      mesa-vulkan-drivers \
      vulkan-tools \
      libxkbcommon0 \
      libxkbcommon-x11-0 \
      libx11-6 \
      libx11-xcb1 \
      libxcb1 \
      libxcursor1 \
      libxi6 \
      libxrandr2 \
      libxrender1 \
      libwayland-client0 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
ENTRYPOINT ["bash", "packaging/test-linux-vulkan/run-smoke.sh"]
