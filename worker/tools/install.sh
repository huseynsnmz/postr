#!/bin/bash
# Install pinned build tooling for postr-worker.
#
# We pin worker-build at 0.8.1 because newer versions (0.8.5+) inject
# --force-enable-abort-handler into wasm-bindgen, which requires an
# externref table that Rust's wasm32-unknown-unknown doesn't emit. Pinning
# the older version avoids the requirement entirely.
#
# Re-run whenever you bump the version or set up a fresh checkout.
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
WB_VERSION="0.8.4"

echo "Installing worker-build $WB_VERSION into $SCRIPT_DIR/worker-build ..."
cargo install worker-build --version "$WB_VERSION" --root "$SCRIPT_DIR/worker-build" --quiet
echo "Installed: $($SCRIPT_DIR/worker-build/bin/worker-build --version)"
