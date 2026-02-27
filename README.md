# rust_embedded_wgpu

A minimal Rust + wgpu renderer that targets Linux DRM/KMS directly (no X11/Wayland). It prefers the Vulkan direct-to-DRM path and, when that isn’t available, falls back to a CPU KMS scanout path that remains tear‑free via double buffering and vblank‑synchronized page flips.

## 1. What this does
- Draws an animated triangle with wgpu (Vulkan backend preferred).
- Attempts to create a DRM/KMS surface via wgpu’s Vulkan path.
- If that surface cannot be created (common on Jetson with split GPU/display DRM devices), it automatically switches to a CPU scanout fallback:
  - Render offscreen with wgpu to an RGBA8 texture
  - Read back to CPU
  - Convert RGBA → XRGB8888
  - Blit into DRM "dumb" buffers
  - Page-flip using KMS with EVENT flag and wait for vblank (double-buffered, tear-free)

## 2. Prerequisites (on the target device)
- Linux with DRM/KMS (standard on modern distros, Raspberry Pi OS, Jetson Linux/L4T)
- Working Vulkan driver (Mesa V3DV for Pi; NVIDIA Vulkan via JetPack on Jetson)
- Tools you’ll likely want (Debian/Ubuntu names):
  - build-essential pkg-config git curl
  - libdrm-tests (modetest), kmscube, libdrm-dev
  - vulkan-tools (vulkaninfo), libvulkan1 (Mesa-based add: mesa-vulkan-drivers)
  - Optionally for rootless device access: seatd, libseat1
- Permissions (for rootless runs): add your user to video, render (and input if using ESC via evdev). Start seatd: `sudo systemctl enable --now seatd`.

Quick checks:
- `ls -l /dev/dri` (DRM nodes present)
- `modetest -c -p` (connectors/modes/planes)
- `vulkaninfo | head -n 20` (driver present)

## 3. Build
- Default (DRM path + CPU fallback):
```bash
cargo build --release --features wgpu_drm,kms_cpu_scanout
```
- Optional: enable ESC-to-exit via evdev (requires membership in the input group):
```bash
cargo build --release --features wgpu_drm,kms_cpu_scanout,esc_evdev
```

## 4. Run
Run from a DRM-capable virtual terminal (not under X/Wayland):
```bash
export WGPU_BACKEND=vulkan
export RUST_LOG=info
# If using seatd for rootless access:
# export LIBSEAT_BACKEND=seatd
./target/release/rust_embedded_wgpu
```
- Stop with Ctrl+C (or ESC if built with `esc_evdev`).

Expected message on Jetson and some devices (normal):
- `wgpu DRM surface create failed: ... Failed to find suitable drm device`
  - When this appears, the app switches to the CPU KMS fallback automatically. You should still see the animated triangle, rendered offscreen by wgpu and scanned out via dumb buffers with vblank‑synced page flips.

## 5. Feature flags
- `wgpu_drm`: Enables the direct Vulkan→DRM surface path in wgpu.
- `kms_cpu_scanout`: Enables the CPU fallback path (offscreen wgpu → CPU readback → RGBA→XRGB8888 → KMS dumb‑buffer blit with double‑buffered page flips).
- `esc_evdev`: Optional ESC listener via evdev to exit cleanly (adds `evdev`; needs `input` group).

## 6. Notes & troubleshooting
- Always run from a VT; window systems usually hold DRM master and block direct modesetting.
- If running rootless, ensure seatd is active and you’re in `video` and `render` (and `input` if using `esc_evdev`).
- Sanity tools: `modetest`, `kmscube`, `vulkaninfo`.
- If Vulkan direct-display extensions (e.g., VK_KHR_display, VK_EXT_acquire_drm_display) aren’t advertised by the driver, the direct wgpu DRM surface path won’t work—fallback will handle scanout instead.

## 7. Next steps (project roadmap)
- CLI options for connector-id / mode selection and smarter CRTC/plane picking
- Readback/format-conversion optimizations (SIMD/NEON, pipelined copy/convert, triple buffering)
- Broader input handling and unified shutdown across both render modes
