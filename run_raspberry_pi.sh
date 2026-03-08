#!/bin/bash
#
# Run embedded Rust WGPU demonstration. (No X11/Wayland required, goes straight to hardware)

RUST_LOG=info WGPU_BACKEND=gl cargo run --release --features kms_runner --bin rust_embedded_wgpu

