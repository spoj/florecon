# florecon — task runner
# `just` with no args lists all recipes.

# Port the local static server binds to.
port := "8787"
# URL of the web UI once the static server is up.
ui_url := "http://localhost:" + port + "/web/"

# Show available recipes.
default:
    @just --list

# Serve the web UI locally AND expose it on the public internet via a
# Cloudflare quick tunnel. Prints a https://*.trycloudflare.com URL.
cf: ensure-wasm
    #!/usr/bin/env bash
    set -euo pipefail
    # Static root is the project root so the importmap's `../node_modules`
    # (relative to /web/index.html) resolves correctly. UI lives at /web/.
    python3 -m http.server {{port}} --bind 127.0.0.1 >/dev/null 2>&1 &
    server_pid=$!
    trap 'kill $server_pid 2>/dev/null || true' EXIT
    sleep 1
    echo "local UI:  {{ui_url}}"
    echo "tunneling /web/ via cloudflare..."
    cloudflared tunnel --url {{ui_url}}

# Serve the web UI locally only (no tunnel). Opens at the UI url.
serve: ensure-wasm
    @echo "serving at {{ui_url}}"
    python3 -m http.server {{port}} --bind 127.0.0.1

# Build the WASM core and stage it into the web + python packages.
build-wasm:
    ./scripts/build_wasm.sh

# Build the WASM core only if it hasn't been staged yet.
ensure-wasm:
    @test -f web/core/engine.wasm || ./scripts/build_wasm.sh

# Build the native Rust library (release).
build:
    cargo build --release

# Run the Rust test suite.
test:
    cargo test

# Run all the web smoke tests.
smoke:
    npm run smoke
    npm run smoke:ingest
    npm run smoke:dom

# Format Rust code.
fmt:
    cargo fmt

# Lint Rust code with clippy.
lint:
    cargo clippy --all-targets -- -D warnings

# Install JS dependencies.
install:
    npm install

# Remove build artifacts.
clean:
    cargo clean
    rm -f web/core/engine.wasm py/src/florecon/_engine.wasm
