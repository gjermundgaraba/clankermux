#!/usr/bin/env bash
# Build ghostty-vt.wasm from the vendored tree into web/packages/engine.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
out="$root/target/wasm"
"${ZIG:-zig}" build -Demit-lib-vt -Dtarget=wasm32-freestanding -Doptimize=ReleaseSmall \
    "-Dversion-string=$(tr -d '\n' < "$root/vendor/libghostty-vt/VERSION")" \
    --prefix "$out" --cache-dir "$root/target/zig-cache" \
    --build-file "$root/vendor/libghostty-vt/build.zig"
cp "$out/bin/ghostty-vt.wasm" "$root/web/packages/engine/ghostty-vt.wasm"
