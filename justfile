# Required checks for every commit.
check: check-rust check-web

check-rust:
    cargo fmt --all --check
    cargo xtask bindings --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace

check-web: wasm fixtures
    cd web && pnpm install --frozen-lockfile && pnpm -r typecheck && pnpm -r test

# Build ghostty-vt.wasm from the vendored tree.
wasm:
    scripts/build-wasm.sh

# Snapshot fixtures encoded by the native engine for the cross-engine test.
fixtures:
    cargo run --quiet -p ghostty-vt --example encode-fixture -- web/packages/engine/test/fixtures

# Regenerate the committed bindings.
bindings:
    cargo xtask bindings

# Re-pin libghostty-vt to an upstream main commit.
pin sha:
    scripts/pin.sh {{sha}}
