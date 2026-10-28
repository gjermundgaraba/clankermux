#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
out="$root/target/wasm"
version=$(tr -d '\n' < "$root/vendor/libghostty-vt/VERSION")
"${ZIG:-zig}" build -Demit-lib-vt -Dtarget=wasm32-freestanding -Doptimize=ReleaseSmall \
    "-Dversion-string=$version" "-Dlib-version-string=$version" \
    --prefix "$out" --cache-dir "$root/target/zig-cache" \
    --build-file "$root/vendor/libghostty-vt/build.zig"
cp "$out/bin/ghostty-vt.wasm" "$root/web/packages/clankermux/ghostty-vt.wasm"
