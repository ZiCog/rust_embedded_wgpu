#!/usr/bin/env bash
set -euo pipefail

# Installs build tools, DRM tools, Vulkan tools, input libs, and seatd on Debian/Ubuntu.
# NOTE: On NVIDIA Jetson, DO NOT install mesa-vulkan-drivers.

sudo apt-get update
sudo apt-get install -y \
  build-essential pkg-config cmake git curl \
  libdrm-tests kmscube libdrm-dev \
  vulkan-tools libvulkan1 \
  libinput-dev libinput-tools libudev-dev libxkbcommon-dev \
  seatd libseat1 libseat-dev

# Mesa Vulkan driver (for Raspberry Pi / other Mesa-based GPUs). Skip on Jetson.
if [[ "${INSTALL_MESA_VULKAN_DRIVERS:-1}" == "1" ]]; then
  sudo apt-get install -y mesa-vulkan-drivers || true
fi

echo "[OK] Base packages installed. If running on Jetson, ensure mesa-vulkan-drivers was not installed."
