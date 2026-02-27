# rust_embedded_wgpu

Requirements specification and development steps for running a wgpu-based renderer directly on Linux DRM/KMS (no X11/Wayland) on Raspberry Pi and NVIDIA Jetson.

## 1. Goal and scope
- Render with Rust + wgpu directly to the display using the Linux DRM/KMS stack (no windowing system).
- Primary backend: Vulkan via wgpu.
- Targets: Raspberry Pi 4/5 (Mesa V3DV) and NVIDIA Jetson (L4T/JetPack with Vulkan).
- Input handled via libinput on the TTY; rootless device access via seatd/libseat.

Non-goals (initially): compositors, multi-seat, hotplug across multiple GPUs, audio.

## 2. High-level architecture
- Linux kernel DRM/KMS → libdrm for connector/mode enumeration and ownership.
- Vulkan backend in wgpu.
- wgpu creates a DRM/KMS-backed surface (wgpu’s unsafe DRM surface path on Vulkan) and presents frames.
- Input gathered with libinput (via evdev) on the same TTY session.
- seatd/libseat provides device access without running as root.

Key Vulkan extensions typically required by the direct-display path:
- VK_KHR_display
- VK_EXT_acquire_drm_display
- (Helpful) VK_EXT_physical_device_drm

If these are missing on a given driver, direct-to-DRM via wgpu may not be available.

## 3. System prerequisites (device)
- Linux with DRM/KMS enabled (standard on recent Raspberry Pi OS and Jetson Linux/L4T).
- Working Vulkan driver for your SoC/GPU:
  - Raspberry Pi: Mesa V3DV (Vulkan) with KMS enabled (vc4/v3d drivers).
  - Jetson: JetPack/L4T includes NVIDIA’s Vulkan driver.
- Packages (Debian/Ubuntu family):
  - Tooling: build-essential, pkg-config, cmake (optional), git, curl
  - DRM tooling: libdrm-tests (modetest), kmscube (sanity check), libdrm-dev (if building DRM utilities)
  - Vulkan: vulkan-tools (vulkaninfo), libvulkan1; for Mesa-based systems add mesa-vulkan-drivers
  - Input/session: libinput-dev, libinput-tools, libudev-dev, libxkbcommon-dev, seatd, libseat1, libseat-dev
  - Rust toolchain: rustup (installs cargo/rustc)

Groups/permissions for rootless operation: add your user to video, input, render, and seat (or seatd) groups.

## 4. Quick checks (before coding)
Run these on the target device (Pi/Jetson). Use sudo where noted.

### 4.1 Install base packages (Debian/Ubuntu)
```bash
sudo apt-get update
# Core build + DRM + Vulkan + input + seatd
sudo apt-get install -y \
  build-essential pkg-config cmake git curl \
  libdrm-tests kmscube libdrm-dev \
  vulkan-tools libvulkan1 \
  libinput-dev libinput-tools libudev-dev libxkbcommon-dev \
  seatd libseat1 libseat-dev

# On Mesa-based (e.g., Raspberry Pi OS / Ubuntu on Pi)
sudo apt-get install -y mesa-vulkan-drivers

# On Jetson: do NOT install mesa-vulkan-drivers; JetPack provides NVIDIA’s Vulkan
```

### 4.2 DRM/KMS present and working
```bash
# Devices should exist
ls -l /dev/dri

# Inspect connectors/modes/encoders/planes
modetest -c -p

# Optional: try kmscube (proves KMS scanout path works)
kmscube -D /dev/dri/card0 || echo "kmscube failed (may be expected on some Jetson builds)"
```

### 4.3 Vulkan present and direct-display extensions
```bash
# Basic Vulkan sanity
vulkaninfo | head -n 20

# Look for key extensions (instance or device)
vulkaninfo | grep -E "VK_(KHR_display|EXT_acquire_drm_display|EXT_physical_device_drm)"
```
If `VK_KHR_display` and `VK_EXT_acquire_drm_display` are not reported by your Vulkan driver, wgpu’s DRM surface path will likely be unavailable on that device.

### 4.4 seatd/libseat for rootless device access
```bash
# Enable and start seatd (system service providing device access)
sudo systemctl enable --now seatd.service

# Add your user to groups (log out/in after running this):
sudo usermod -aG video,input,render,seat $USER || sudo usermod -aG video,input,render,seatd $USER

# Confirm seatd is active
systemctl status seatd --no-pager
```

### 4.5 Raspberry Pi specific
```bash
# KMS overlay should be enabled; check vc4 module
lsmod | grep -E "vc4|v3d"
# If not present, ensure KMS overlay in /boot/firmware/config.txt (Pi OS Bookworm typically OK):
# dtoverlay=vc4-kms-v3d
```

### 4.6 Jetson specific
```bash
# Check DRM driver name and nodes
ls -l /dev/dri
modetest -c -p | sed -n '1,120p'
# Depending on Jetson release, DRM driver may appear as tegra or nvidia

# Confirm Vulkan reports NVIDIA driver and extensions
vulkaninfo | grep -E "vendor|driver" | head -n 5
vulkaninfo | grep -E "VK_(KHR_display|EXT_acquire_drm_display)"
```

## 5. Development steps

### 5.1 Project setup (already created here)
- This repository was created with `cargo new --vcs none rust_embedded_wgpu`.
- You will write the renderer using wgpu (Vulkan backend) and enumerate DRM via `drm`/`drm-rs`.

### 5.2 Dependencies (Rust)
Add as you implement (do not add Linux-only crates on macOS builds unless behind cfg):
- anyhow (error handling)
- wgpu (targeting Vulkan backend)
- drm (aka drm-rs) for KMS connector/mode enumeration
- nix or rustix for FDs and ioctl wrappers (if needed)
- input/libinput-rs + xkbcommon for keyboard
- libseat (via FFI crate, or call through seatd CLI for prototyping)

Example Cargo.toml snippets (Linux-only crates under cfg to keep macOS builds happy):
```toml
[target.'cfg(all(unix, not(target_vendor = "apple")))'.dependencies]
drm = "0.14"
input = "0.8"
```

### 5.3 Core program flow
1) Acquire DRM device (non-root via libseat) and choose a connected connector + preferred mode using drm-rs.
2) Create `wgpu::Instance` restricted to Vulkan backend.
3) Create `wgpu` surface from DRM using the unsafe DRM target; keep the DRM fd alive for the lifetime of the surface.
4) Request adapter/device/queue with the surface as the present target; get capabilities and configure the surface.
5) Render frames; present; handle input events each frame.
6) On exit, drop the surface before closing the DRM fd; restore terminal state.

### 5.4 Build and run on device
```bash
# Install Rust toolchain (one-time)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"

# In this project directory on the DEVICE (Pi/Jetson):
cargo build --release

# Prefer Vulkan backend explicitly
export WGPU_BACKEND=vulkan

# seatd backend env (if needed)
export LIBSEAT_BACKEND=seatd

# Run on a VT (no X/Wayland). You may want to stop display manager or switch to a free TTY.
./target/release/rust_embedded_wgpu
```

## 6. Convenience scripts (provided in scripts/)
- `scripts/install_prereqs_debian.sh` – Installs required packages on Debian/Ubuntu devices (Pi/Jetson). Adjust for Jetson by omitting mesa-vulkan-drivers.
- `scripts/verify_stack.sh` – Verifies DRM nodes, modetest output, Vulkan presence and extensions, and seatd status.

## 7. Risks and fallbacks
- If the Vulkan driver does not advertise `VK_KHR_display` and `VK_EXT_acquire_drm_display`, the direct-to-DRM wgpu path will not work on that device. Consider updating drivers/mesa or using a different present path. As a last resort, render offscreen and manage KMS/planes yourself (significantly more work and not portable).
- Running without a seat/session (or correct groups) will prevent DRM/input access as non-root; use seatd.

## 8. Next implementation steps
- Wire up drm-rs to pick a connector/mode and keep the DRM fd open.
- Create the `wgpu` DRM surface and draw a triangle.
- Integrate libinput for keyboard; ESC to exit.
- Add config flags (env/CLI) for connector-id, mode, and plane index.

---
This document lives in README so it can travel with the project. See `scripts/` for ready-to-run checks.
