#!/usr/bin/env bash
set -euo pipefail
example="$1"; shift || true
exec cargo run --example "$example" --features examples_upstream "$@"
