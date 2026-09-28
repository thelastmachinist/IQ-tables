#!/usr/bin/env bash
# Build IQ Tables:
#   site/index.html    single file (wasm embedded) — what gets deployed to IQ Pages
#   site/iqpages.json  IQ Pages manifest
#   dist/multi/        index.html + host.js + iq_tables.wasm (for local dev)
#
# Needs only Rust with the wasm32-unknown-unknown target:
#   rustup target add wasm32-unknown-unknown
# If that target can't be installed but rust-src can, set BUILD_STD=1 to build
# the standard library from source instead.
set -euo pipefail
cd "$(dirname "$0")"

if [[ "${BUILD_STD:-0}" == "1" ]]; then
  RUSTC_BOOTSTRAP=1 cargo build --release --lib --target wasm32-unknown-unknown -Z build-std=std,panic_abort
else
  cargo build --release --lib --target wasm32-unknown-unknown
fi

WASM=target/wasm32-unknown-unknown/release/iq_tables.wasm
rm -rf dist && mkdir -p dist/multi site
cp "$WASM" dist/multi/iq_tables.wasm
cp web/host.js dist/multi/host.js
sed 's#<!--WASM-->##' web/index.html > dist/multi/index.html

# single-file build: inline the wasm (base64) and the bridge script
B64=$(base64 -w0 "$WASM")
{
  while IFS= read -r line; do
    if [[ "$line" == *"<!--WASM-->"* ]]; then
      printf '<script type="application/wasm-base64" id="wasm-b64">%s</script>\n' "$B64"
    elif [[ "$line" == *'<script src="host.js"></script>'* ]]; then
      printf '<script>\n'; cat web/host.js; printf '</script>\n'
    else
      printf '%s\n' "$line"
    fi
  done < web/index.html
} > site/index.html

cat > site/iqpages.json <<'JSON'
{
  "name": "iq-tables",
  "version": "0.1.0",
  "description": "IQ Tables: browse, draft and inscribe databases on IQ Labs tables",
  "entry": "index.html"
}
JSON

echo "wasm: $(stat -c %s "$WASM") bytes; site/index.html: $(stat -c %s site/index.html) bytes"
