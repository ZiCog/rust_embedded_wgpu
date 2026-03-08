#!/bin/bash
#
# Run embedded Rust WGPU demonstration. (No X11/Wayland required, goes straight to hardware)

WGPU_BACKEND=gl RUST_LOG=info ./target/release/rust_embedded_wgpu

