# florecon — task runner
# `just` with no args lists all recipes.

# Show available recipes.
default:
    @just --list

# Build the native Rust library (release).
build:
    cargo build --release

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

# Scaffold a new plugin project from templates/plugin (default: ../<name>).
new-plugin name dest='..':
    mkdir -p {{dest}}
    cp -r templates/plugin {{dest}}/{{name}}
    cd {{dest}}/{{name}} && grep -rl '__CRATE__\|__LIB__\|__DOMAIN__' . | xargs sed -i \
        -e 's/__CRATE__/{{name}}/g' \
        -e "s/__LIB__/$(echo {{name}} | tr '-' '_')/g" \
        -e 's/__DOMAIN__/example.{{name}}/g'
    chmod +x {{dest}}/{{name}}/build_wasm.sh
    @echo "scaffolded {{dest}}/{{name}} — edit solver/src/lib.rs (4 marked spots), then: cd {{dest}}/{{name}} && just run"

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
