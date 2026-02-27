#!/usr/bin/env bash
set -euo pipefail

print_section() { echo; echo "==== $1 ===="; }

print_section "Kernel & GPU drivers"
uname -a || true
lsmod | grep -E "(vc4|v3d|nvidia|tegra|drm)" || true

print_section "/dev/dri nodes"
ls -l /dev/dri || { echo "No /dev/dri nodes; DRM/KMS not available"; exit 1; }

print_section "modetest connectors & planes (first 200 lines)"
if command -v modetest >/dev/null; then
  modetest -c -p | sed -n '1,200p'
else
  echo "modetest not found (install libdrm-tests)"
fi

print_section "kmscube"
if command -v kmscube >/dev/null; then
  kmscube -D /dev/dri/card0 || echo "kmscube reported an error (may be expected on some platforms)"
else
  echo "kmscube not found (optional, install kmscube)"
fi

print_section "Vulkan presence & required extensions"
if command -v vulkaninfo >/dev/null; then
  vulkaninfo | head -n 30 || true
  echo "-- Required/Helpful extensions present? --"
  vulkaninfo | grep -E "VK_(KHR_display|EXT_acquire_drm_display|EXT_physical_device_drm)" || echo "Required extensions not reported"
else
  echo "vulkaninfo not found (install vulkan-tools)"
fi

print_section "seatd/libseat"
if systemctl status seatd --no-pager >/dev/null 2>&1; then
  systemctl status seatd --no-pager | sed -n '1,40p'
else
  echo "seatd service not found or not active"
fi

print_section "Groups for current user"
groups

echo
echo "[OK] Verification script completed. Review sections above for any missing pieces."
