#!/usr/bin/env bash
# Print the release version: the clankermux crate's, which the four web
# packages must share. Fails naming each package that differs.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
version=$(cargo pkgid --manifest-path "$root/Cargo.toml" -p clankermux | sed 's/.*[#@]//')
status=0
for p in proto engine renderer client; do
    v=$(jq -r .version "$root/web/packages/$p/package.json")
    if [ "$v" != "$version" ]; then
        echo "@clankermux/$p is $v, clankermux is $version" >&2
        status=1
    fi
done
[ "$status" = 0 ] && echo "$version"
exit "$status"
