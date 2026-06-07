#!/usr/bin/env bash
# Build the plugin to wasm. run.py loads it from the target dir directly.
set -euo pipefail
cd "$(dirname "$0")/solver"
cargo build --release --target wasm32-unknown-unknown
echo "built -> solver/target/wasm32-unknown-unknown/release/__LIB__.wasm"
