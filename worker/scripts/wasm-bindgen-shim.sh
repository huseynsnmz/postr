#!/bin/bash
# Workaround shim for wasm-bindgen 0.2.125 + Rust 1.95 incompatibility.
#
# worker-build 0.8.5 invokes wasm-bindgen with --force-enable-abort-handler,
# which requires the wasm binary to have an externref table. Rust 1.95's
# wasm32-unknown-unknown target enables `reference-types` codegen but does
# not emit an externref table unless something in the code uses one — which
# nothing does in this project. wasm-bindgen then refuses to add code that
# would use the table because there's no table to use, producing:
#     error: externref table required for catch wrappers
#
# This shim drops --force-enable-abort-handler from worker-build's call.
# Trade-off: Rust panics surface as raw worker errors instead of going
# through wasm-bindgen's abort handler. Acceptable for v1.
#
# Install with:   ./scripts/install-wasm-bindgen-shim.sh
# Verify with:    worker-build --release

args=()
for arg in "$@"; do
  if [ "$arg" != "--force-enable-abort-handler" ]; then
    args+=("$arg")
  fi
done
exec "$HOME/.cargo/bin/wasm-bindgen" "${args[@]}"
