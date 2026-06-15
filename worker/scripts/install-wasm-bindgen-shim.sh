#!/bin/bash
# Install the wasm-bindgen wrapper shim that worker-build 0.8.5 needs to
# work with Rust 1.95. See ./wasm-bindgen-shim.sh for the explanation.
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
CACHE_DIR="$HOME/Library/Caches/worker-build/wasm-bindgen-aarch64-apple-darwin-0.2.125"
WB_PATH="$CACHE_DIR/wasm-bindgen"

# Make sure wasm-bindgen-cli 0.2.125 is installed system-wide (the shim calls
# the real binary via $HOME/.cargo/bin/wasm-bindgen).
if ! command -v wasm-bindgen >/dev/null || [ "$(wasm-bindgen --version)" != "wasm-bindgen 0.2.125" ]; then
  echo "Installing wasm-bindgen-cli 0.2.125 ..."
  cargo install wasm-bindgen-cli --version 0.2.125 --force --quiet
fi

# Make sure the cache dir exists. If worker-build has never run, it won't.
# In that case run it once to let it create the dir (it'll fail at the
# externref check — that's expected).
if [ ! -d "$CACHE_DIR" ]; then
  echo "Triggering worker-build once to populate cache dir ..."
  worker-build --release 2>/dev/null || true
fi

cp "$SCRIPT_DIR/wasm-bindgen-shim.sh" "$WB_PATH"
chmod +x "$WB_PATH"
echo "Installed shim at $WB_PATH"
echo "Run \`worker-build --release\` to build."
