#!/usr/bin/env bash
# Build IQ Tables:
#   site/index.html        single file (wasm embedded) — what gets deployed to IQ Pages
#   site/iqpages.json      IQ Pages manifest
#   site/iqt-decoder.wasm  the embeddable decoder (decoder/), sealed: no imports
#   site/iqt-loader.mjs    the file developers keep to run it (embed/)
#   site/iqt-formats.json  which decoder reads each storage format
#   dist/multi/        index.html + host.js + iq_tables.wasm (for local dev)
#
# Needs only Rust with the wasm32-unknown-unknown target:
#   rustup target add wasm32-unknown-unknown
# If that target can't be installed but rust-src can, set BUILD_STD=1 to build
# the standard library from source instead.
set -euo pipefail
cd "$(dirname "$0")"

build() {
  if [[ "${BUILD_STD:-0}" == "1" ]]; then
    RUSTC_BOOTSTRAP=1 cargo build --release --lib --target wasm32-unknown-unknown -Z build-std=std,panic_abort "$@"
  else
    cargo build --release --lib --target wasm32-unknown-unknown "$@"
  fi
}
build -p iq_tables
build -p iqt_decoder

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

# the embeddable decoder and the files that go with it
DEC=target/wasm32-unknown-unknown/release/iqt_decoder.wasm
# it must not import anything: that's what keeps it sealed off
bytes=($(od -An -v -tu1 "$DEC"))
if [[ "${bytes[*]:0:4}" != "0 97 115 109" ]]; then echo "decoder: not a wasm file" >&2; exit 1; fi
i=8
while (( i < ${#bytes[@]} )); do
  id=${bytes[i]}; i=$((i + 1)); size=0; shift=0
  while :; do b=${bytes[i]}; i=$((i + 1)); size=$((size | ((b & 127) << shift))); shift=$((shift + 7)); ((b < 128)) && break; done
  if ((id == 2)); then echo "decoder: it imports something; it must be sealed (no imports)" >&2; exit 1; fi
  i=$((i + size))
done
cp "$DEC" site/iqt-decoder.wasm
cp embed/iqt-loader.mjs site/iqt-loader.mjs
cp embed/iqt-formats.json site/iqt-formats.json

echo "wasm: $(stat -c %s "$WASM") bytes; site/index.html: $(stat -c %s site/index.html) bytes; site/iqt-decoder.wasm: $(stat -c %s site/iqt-decoder.wasm) bytes"
