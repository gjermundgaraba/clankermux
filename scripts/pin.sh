#!/usr/bin/env bash
# Vendor the libghostty-vt source archive for one upstream Ghostty commit.
#
# usage: scripts/pin.sh <sha>
#
# Downloads https://tip.files.ghostty.org/<sha>/libghostty-vt-source.tar.gz
# into a fresh temporary directory, records its sha256 in vendor/pin.json,
# and replaces vendor/libghostty-vt with the extracted tree. When pin.json
# already names this sha, the download must match the recorded sha256.
set -euo pipefail
sha=${1:?usage: scripts/pin.sh <sha>}
root=$(cd "$(dirname "$0")/.." && pwd)
pin="$root/vendor/pin.json"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

curl -fsSL -o "$tmp/src.tar.gz" "https://tip.files.ghostty.org/$sha/libghostty-vt-source.tar.gz"
sum=$(shasum -a 256 "$tmp/src.tar.gz" | cut -d' ' -f1)
if [ -f "$pin" ] && [ "$(jq -r .sha "$pin")" = "$sha" ]; then
    recorded=$(jq -r .sha256 "$pin")
    if [ "$recorded" != "$sum" ]; then
        echo "sha256 mismatch for $sha: recorded $recorded, downloaded $sum" >&2
        exit 1
    fi
fi

mkdir "$tmp/tree"
tar -xzf "$tmp/src.tar.gz" -C "$tmp/tree"
top=$(ls "$tmp/tree")
rm -rf "$root/vendor/libghostty-vt"
mkdir -p "$root/vendor"
mv "$tmp/tree/$top" "$root/vendor/libghostty-vt"
jq -n --arg sha "$sha" --arg sha256 "$sum" \
    --arg version "$(tr -d '\n' < "$root/vendor/libghostty-vt/VERSION")" \
    --arg date "$(date -u +%Y-%m-%d)" \
    '{sha: $sha, sha256: $sha256, version: $version, date: $date, engine: ("ghostty-vt@" + $version)}' \
    > "$pin"
cat "$pin"
