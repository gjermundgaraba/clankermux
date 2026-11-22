#!/usr/bin/env bash
set -euo pipefail
version=${1:?usage: scripts/pin.sh <libghostty-vt release, e.g. 1.0.1>}
root=$(cd "$(dirname "$0")/.." && pwd)
pin="$root/vendor/pin.json"
base="https://release.files.ghostty.org/libghostty-vt/$version"
tarball="libghostty-vt-$version.tar.gz"
# Ghostty's release key, published in upstream's PACKAGING-LIBGHOSTTY.md.
key=RWQlAjJC23149WL2sEpT/l0QKy7hMIFhYdQOFy0Z7z7PbneUgvlsnYcV
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

for f in SHA256SUMS SHA256SUMS.minisig "$tarball"; do
    curl -fsSL -o "$tmp/$f" "$base/$f"
done
minisign -Vq -P "$key" -m "$tmp/SHA256SUMS"
sum=$(shasum -a 256 "$tmp/$tarball" | cut -d' ' -f1)
listed=$(awk -v f="$tarball" '$2 == f { print $1 }' "$tmp/SHA256SUMS")
if [ "$listed" != "$sum" ]; then
    echo "sha256 of $tarball is $sum, signed SHA256SUMS says ${listed:-nothing}" >&2
    exit 1
fi
if [ -f "$pin" ] && [ "$(jq -r .version "$pin")" = "$version" ]; then
    recorded=$(jq -r .sha256 "$pin")
    if [ "$recorded" != "$sum" ]; then
        echo "sha256 mismatch for $version: recorded $recorded, downloaded $sum" >&2
        exit 1
    fi
fi

mkdir "$tmp/tree"
tar -xzf "$tmp/$tarball" -C "$tmp/tree"
rm -rf "$root/vendor/libghostty-vt"
mkdir -p "$root/vendor"
mv "$tmp/tree/libghostty-vt-$version" "$root/vendor/libghostty-vt"
jq -n --arg version "$version" --arg sha256 "$sum" \
    --arg lib "$(tr -d '\n' < "$root/vendor/libghostty-vt/LIB_VERSION")" \
    --arg date "$(date -u +%Y-%m-%d)" \
    '{version: $version, sha256: $sha256, date: $date, engine: ("ghostty-vt@" + $lib)}' \
    > "$pin"
jq -r '"export const ENGINE = \(.engine | tojson);"' \
    "$pin" > "$root/web/packages/clankermux/src/engine/pin.ts"
cat "$pin"
