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


Backend selection in short: on Raspberry Pi prefer `WGPU_BACKEND=gl`; on Jetson prefer Vulkan if drivers support direct-display, otherwise fallback engages automatically.

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
Run from a DRM-capable virtual terminal (not under X/Wayland). Examples:

- Raspberry Pi (recommended): use GL backend for the stable Mesa v3d driver
```bash
export RUST_LOG=info
WGPU_BACKEND=gl ./target/release/rust_embedded_wgpu
```

- Vulkan (only if you have a real Vulkan driver, e.g., Jetson or Pi with v3dv)
```bash
export RUST_LOG=info
WGPU_BACKEND=vulkan ./target/release/rust_embedded_wgpu
```

- Unset (try Vulkan first, then GL)
```bash
export RUST_LOG=info
./target/release/rust_embedded_wgpu
```

What to expect in logs (RUST_LOG=info):
- Which backends are allowed (from WGPU_BACKEND)
- Whether a Vulkan DRM surface was attempted
- Which adapter/backend was selected, e.g.:
  - `using adapter: name='V3D …' backend=Gl type=Integrated` (hardware GL on Pi)
  - `using adapter: name='llvmpipe …' backend=Vulkan type=Cpu` (software Vulkan / lavapipe)

Notes:
- If the Vulkan direct-display extension isn’t available, the app uses the CPU KMS fallback automatically (offscreen render → CPU copy → dumb buffer → vblank‑synced page flips). You’ll still see the triangle.
- Stop with Ctrl+C (or ESC if built with `esc_evdev`).

## 5. Feature flags
- `wgpu_drm`: Enables the direct Vulkan→DRM surface path in wgpu.
- `kms_cpu_scanout`: Enables the CPU fallback path (offscreen wgpu → CPU readback → RGBA→XRGB8888 → KMS dumb‑buffer blit with double‑buffered page flips).
- `esc_evdev`: Optional ESC listener via evdev to exit cleanly (adds `evdev`; needs `input` group).

## 6. Notes & troubleshooting
### Troubleshooting: lavapipe vs v3d
On Raspberry Pi you typically want the hardware GL driver (v3d) instead of software Vulkan (lavapipe).

- Symptom (software path): logs show `using adapter: name='llvmpipe' backend=Vulkan type=Cpu` and you may see `WARNING: lavapipe is not a conformant vulkan implementation`.
- Preferred (hardware GL): logs show something like `using adapter: name='V3D …' backend=Gl type=Integrated`.

Quick fixes
- Force GL on Pi (recommended):
  \- `RUST_LOG=info WGPU_BACKEND=gl ./target/release/rust_embedded_wgpu`
- If you want Vulkan on Pi, install the driver and tools and confirm it’s not lavapipe:
```bash
sudo apt-get update
sudo apt-get install -y mesa-vulkan-drivers vulkan-tools
vulkaninfo | head -n 40   # look for V3D; avoid llvmpipe/lavapipe
```
Notes
- Even with a working Vulkan driver, the Vulkan direct-display extension usually isn’t available on Pi; the app will still use the CPU KMS fallback and show the triangle.
- Use `RUST_LOG=info` to see which backend/adapter wgpu actually selected.

- Always run from a VT; window systems usually hold DRM master and block direct modesetting.
- If running rootless, ensure seatd is active and you’re in `video` and `render` (and `input` if using `esc_evdev`).
- Sanity tools: `modetest`, `kmscube`, `vulkaninfo`.
- If Vulkan direct-display extensions (e.g., VK_KHR_display, VK_EXT_acquire_drm_display) aren’t advertised by the driver, the direct wgpu DRM surface path won’t work—fallback will handle scanout instead.

## 7. Next steps (project roadmap)
- CLI options for connector-id / mode selection and smarter CRTC/plane picking
- Readback/format-conversion optimizations (SIMD/NEON, pipelined copy/convert, triple buffering)
- Broader input handling and unified shutdown across both render modes


## Running the upstream examples

The repository vendors upstream wgpu examples under `examples/features/` with a tiny shim so each example can run either headless (DRM/KMS) or windowed (winit), from the same source file.

- Windowed (winit; e.g., macOS/Linux desktop)
  - Build & run cube (vendor):
    - `cargo run --example cube_vendor --features examples_upstream`
  - Build & run hello_triangle:
    - `cargo run --example hello_triangle --features examples_upstream`

- Headless DRM/KMS (Raspberry Pi / Jetson on a real VT)
  - Prereqs: run from a real VT (not X/Wayland), have a connected HDMI sink (or dummy), proper permissions for `/dev/dri/*` (user in `video`, `render`). On Jetson, ensure `tegra-udrm` with modeset is active.
  - Raspberry Pi suggested backend (Mesa v3d):
    - `WGPU_BACKEND=gl cargo run --example cube_vendor --features kms_runner --no-default-features`
  - Jetson suggested backend (Vulkan):
    - `WGPU_BACKEND=vulkan cargo run --example cube_vendor --features kms_runner --no-default-features`
  - hello_triangle (headless):
    - `WGPU_BACKEND=gl cargo run --example hello_triangle --features kms_runner --no-default-features`

Notes
- If you see lavapipe/Vulkan software warnings on Pi, switch to `WGPU_BACKEND=gl`.
- The KMS path renders offscreen, CPU-copies to a dumb buffer, and page-flips synchronized to vblank.
- SurfaceConfiguration in these examples sets `desired_maximum_frame_latency = 2` for wgpu 0.25.
