#!/usr/bin/env bash
# Print the release version: the clankermux crate's, which the web package
# must share. Fails naming the package when it differs.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
version=$(cargo pkgid --manifest-path "$root/Cargo.toml" -p clankermux | sed 's/.*[#@]//')
manifest="$root/web/packages/clankermux/package.json"
v=$(jq -r .version "$manifest")
if [ "$v" != "$version" ]; then
    echo "$(jq -r .name "$manifest") is $v, clankermux is $version" >&2
    exit 1
fi
echo "$version"
