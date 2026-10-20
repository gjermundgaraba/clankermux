# Zig 0.16 as mise pins it, for build-wasm.sh and ghostty-vt-sys; ZIG wins.
export ZIG := env("ZIG", `mise which zig 2>/dev/null || echo zig`)

# Required checks for every commit.
check: check-rust check-web

check-rust:
    cargo fmt --all --check
    cargo xtask bindings --check
    cargo xtask protocol --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace

check-web: wasm fixtures
    cd web && pnpm install --frozen-lockfile && pnpm -r typecheck && pnpm -r test && pnpm -r build

# Build ghostty-vt.wasm from the vendored tree.
wasm:
    scripts/build-wasm.sh

# Snapshot fixtures encoded by the native engine for the cross-engine test.
fixtures:
    cargo run --quiet -p ghostty-vt --example encode-fixture -- web/packages/clankermux/test/engine/fixtures

# Regenerate the committed bindings.
bindings:
    cargo xtask bindings

# Render PROTOCOL.md's tables and the web package's protocol types from the catalog.
protocol:
    cargo xtask protocol

# Re-pin libghostty-vt to an upstream main commit.
pin sha:
    scripts/pin.sh {{sha}}

# Build and pack the npm package into web/dist-packages.
pack-web: wasm
    scripts/pack-web.sh

# The release binary, as a tagged release builds it.
release-build:
    cargo build --release -p clankermux
    @echo "{{justfile_directory()}}/target/release/clankermux"

# The release binary as a server for local development, under target/dev.
dev-serve: release-build
    scripts/dev-serve.sh
