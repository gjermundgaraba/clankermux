#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
dev="$root/target/dev"
mkdir -p "$dev"
if [ ! -f "$dev/cert.pem" ] || [ ! -f "$dev/key.pem" ]; then
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 3650 \
        -subj /CN=127.0.0.1 -addext subjectAltName=IP:127.0.0.1,DNS:localhost \
        -keyout "$dev/key.pem" -out "$dev/cert.pem"
fi
XDG_STATE_HOME="$dev/state" exec "$root/target/release/clankermux" --socket "$dev/sock" serve \
    --ws 127.0.0.1:7438 --tls-cert "$dev/cert.pem" --tls-key "$dev/key.pem" \
    --origin http://app.clankerspace-dev.localhost:10094 --origin http://127.0.0.1:8787 \
    "$@"
