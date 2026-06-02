#!/usr/bin/env bash
# Build the WASM core and stage it into the Python wheel and the npm package.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release --target wasm32-unknown-unknown --features wasm --lib
WASM=target/wasm32-unknown-unknown/release/florecon.wasm
cp "$WASM" py/src/florecon/_engine.wasm
cp "$WASM" web/core/engine.wasm
echo "staged $(wc -c < "$WASM") bytes -> py/src/florecon/_engine.wasm, web/core/engine.wasm"
