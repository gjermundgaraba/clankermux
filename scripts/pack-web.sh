#!/usr/bin/env bash
# Pack the publishable web package into web/dist-packages, as npm would
# receive it, with the commit it came from in COMMIT.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
out="$root/web/dist-packages"
dir="$root/web/packages/clankermux"
version=$("$root/scripts/version.sh")

cd "$root/web"
pnpm install --frozen-lockfile
(cd "$dir" && rm -rf dist && pnpm run build)
rm -rf "$out"
mkdir -p "$out"
(cd "$dir" && pnpm pack --pack-destination "$out" >/dev/null)
commit=$(git -C "$root" rev-parse --short HEAD)
[ -z "$(git -C "$root" status --porcelain)" ] || commit="$commit-dirty"
echo "$commit" > "$out/COMMIT"

# The tarball holds its manifest, its build and the wasm, and publishes
# every subpath the workspace exports, each file it names included; the
# engine in it names the id the server matches.
engine=$(jq -r .engine "$root/vendor/pin.json")
jq -e '(.exports | keys) == (.publishConfig.exports | keys)' "$dir/package.json" >/dev/null ||
    { echo "publishConfig.exports and exports differ" >&2; exit 1; }
tarball="$out/gjermundgaraba-clankermux-$version.tgz"
files=$(tar tzf "$tarball")
for want in package.json $(jq -r '.publishConfig.exports[] | if type == "string" then . else .[] end | ltrimstr("./")' "$dir/package.json"); do
    grep -qx "package/$want" <<< "$files" || { echo "$tarball lacks $want" >&2; exit 1; }
done
extra=$(grep -v -e '^package/package\.json$' -e '^package/README\.md$' -e '^package/dist/' -e '^package/ghostty-vt\.wasm$' <<< "$files" || true)
[ -z "$extra" ] || { echo "$tarball holds files outside dist: $extra" >&2; exit 1; }
if tar xzOf "$tarball" package/package.json | grep -q 'workspace:\|src/'; then
    echo "$tarball's package.json still names the workspace" >&2
    exit 1
fi
tar xzOf "$tarball" package/dist/engine/pin.js | grep -qF "\"$engine\"" || { echo "$tarball does not name $engine" >&2; exit 1; }

echo "version $version, commit $commit, engine $engine"
ls -1 "$out"/*.tgz
