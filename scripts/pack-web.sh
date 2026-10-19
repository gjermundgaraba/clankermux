#!/usr/bin/env bash
# Pack the publishable web packages into web/dist-packages, as npm would
# receive them, with the commit they came from in COMMIT.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
out="$root/web/dist-packages"
packages=(proto engine renderer client)
version=$("$root/scripts/version.sh")

cd "$root/web"
pnpm install --frozen-lockfile
for p in "${packages[@]}"; do (cd "packages/$p" && rm -rf dist && pnpm run build); done
rm -rf "$out"
mkdir -p "$out"
for p in "${packages[@]}"; do (cd "packages/$p" && pnpm pack --pack-destination "$out" >/dev/null); done
commit=$(git -C "$root" rev-parse --short HEAD)
[ -z "$(git -C "$root" status --porcelain)" ] || commit="$commit-dirty"
echo "$commit" > "$out/COMMIT"

# Each tarball holds its manifest and build, and publishes every subpath
# the workspace exports; the engine also holds its wasm and the id the
# server matches.
engine=$(jq -r .engine "$root/vendor/pin.json")
for p in "${packages[@]}"; do
    jq -e '(.exports | if type == "string" then ["."] else keys end) == (.publishConfig.exports | keys)' \
        "packages/$p/package.json" >/dev/null || { echo "@clankermux/$p: publishConfig.exports and exports differ" >&2; exit 1; }
    tarball="$out/clankermux-$p-$version.tgz"
    files=$(tar tzf "$tarball")
    for want in package/package.json package/dist/index.js package/dist/index.d.ts; do
        grep -qx "$want" <<< "$files" || { echo "$tarball lacks $want" >&2; exit 1; }
    done
    extra=$(grep -v -e '^package/package\.json$' -e '^package/dist/' -e '^package/ghostty-vt\.wasm$' <<< "$files" || true)
    [ -z "$extra" ] || { echo "$tarball holds files outside dist: $extra" >&2; exit 1; }
    if tar xzOf "$tarball" package/package.json | grep -q 'workspace:\|src/index.ts'; then
        echo "$tarball's package.json still names the workspace" >&2
        exit 1
    fi
done
engine_tgz="$out/clankermux-engine-$version.tgz"
tar tzf "$engine_tgz" | grep -qx package/ghostty-vt.wasm || { echo "$engine_tgz lacks the wasm" >&2; exit 1; }
tar xzOf "$engine_tgz" package/dist/pin.js | grep -qF "\"$engine\"" || { echo "$engine_tgz does not name $engine" >&2; exit 1; }

echo "version $version, commit $commit, engine $engine"
ls -1 "$out"/*.tgz
