# florecon — task runner
# `just` with no args lists all recipes.

# Show available recipes.
default:
    @just --list

# Build the native Rust library (release).
build:
    cargo build --release

# Cut a release: bump all three manifests to <version>, verify, tag, and create
# the GitHub Release (which triggers .github/workflows/release.yml -> crates.io + PyPI).
# Usage: just release 0.1.2   (a leading 'v' is accepted too)
release version:
    #!/usr/bin/env bash
    set -euo pipefail
    v="{{version}}"; v="${v#v}"
    if [ -n "$(git status --porcelain)" ]; then echo "working tree not clean — commit or stash first" >&2; exit 1; fi
    echo "bumping all three manifests to $v"
    sed -i -E '0,/^version = "[^"]*"/s//version = "'"$v"'"/' Cargo.toml
    sed -i -E '0,/^version = "[^"]*"/s//version = "'"$v"'"/' florecon-derive/Cargo.toml
    sed -i -E '0,/^version = "[^"]*"/s//version = "'"$v"'"/' hosts/python/pyproject.toml
    bash scripts/check-versions.sh "v$v"
    cargo build -q          # refresh Cargo.lock
    just lint
    just test
    git commit -aqm "release: v$v"
    git tag "v$v"
    git push origin HEAD "v$v"
    gh release create "v$v" --title "v$v" --generate-notes
    echo "released v$v — the release workflow is now publishing. watch it with:"
    echo "  gh run watch \$(gh run list -w release -L1 --json databaseId -q '.[0].databaseId') --exit-status"

# Run the full test suite (lib + plugins), all features.
test:
    cargo test --workspace --features sdk

# Format Rust code.
fmt:
    cargo fmt

# Lint with clippy, warnings as errors.
lint:
    cargo clippy --workspace --all-targets --features sdk -- -D warnings

# Build the interco plugin to wasm and stage it into the Python host.
build-wasm:
    cargo build -p interco-plugin --release --target wasm32-unknown-unknown
    cp target/wasm32-unknown-unknown/release/interco_plugin.wasm hosts/python/src/florecon/_engine.wasm
    @echo "staged $(wc -c < hosts/python/src/florecon/_engine.wasm) bytes -> hosts/python/src/florecon/_engine.wasm"

# Fast inner loop: rebuild + re-stage the interco wasm on every Rust change.
dev:
    watchexec -e rs -- just build-wasm

# Build + run the author starter (examples/starter-plugin) as an author would.
# Keeps the seed (which CI also runs) from drifting against the SDK: native
# harness on the sample, then the ship wasm.
starter:
    cd examples/starter-plugin && cargo run --profile author -p harness -- data/sample.csv
    cd examples/starter-plugin && cargo build -p solver --release --target wasm32-unknown-unknown

# Build the starter's distributable wheel (Phase 3), exactly as `florecon
# package` does: embed the ship wasm and produce a universal py3-none-any wheel.
# Guards the packaging path against rot. Requires `uv`.
starter-package:
    cd examples/starter-plugin && cargo build -p solver --release --target wasm32-unknown-unknown
    cd examples/starter-plugin && cp target/wasm32-unknown-unknown/release/solver.wasm package/src/starter/solver.wasm
    cd examples/starter-plugin && uv build package --out-dir dist

# Drive the interco plugin wasm through the generic Python host.
# Uses the repo .venv if present, else whatever `python` is on PATH.
smoke-py: build-wasm
    PYTHONPATH=hosts/python/src $(test -x .venv/bin/python && echo .venv/bin/python || echo python) hosts/python/smoke_stateful.py

# Regenerate the golden wire vectors from the Rust source of truth.
golden:
    UPDATE_GOLDEN=1 cargo test -p interco-plugin --test golden

# Check both halves of the wire contract: Rust self-verify + Python wasm replay.
golden-check: build-wasm
    cargo test -p interco-plugin --test golden
    PYTHONPATH=hosts/python/src $(test -x .venv/bin/python && echo .venv/bin/python || echo python) hosts/python/golden_replay.py

# Remove build artifacts.
clean:
    cargo clean
    rm -f hosts/python/src/florecon/_engine.wasm
