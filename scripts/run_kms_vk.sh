#!/usr/bin/env bash
set -euo pipefail
export WGPU_BACKEND=vulkan
exec cargo run --example "$1" --features kms_runner --no-default-features
