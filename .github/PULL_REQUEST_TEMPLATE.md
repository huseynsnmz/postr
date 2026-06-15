## Summary

<!-- One or two sentences on what this PR changes and why. -->

## How tested

<!-- Manual repro steps, screenshots/asciinemas for UI changes, the
     output of `cargo test` for logic changes. -->

## Checklist

- [ ] `cargo build` is clean in the touched crate(s)
- [ ] `cargo clippy --all-targets -- -D warnings` passes
- [ ] `cargo test` passes (CLI) / `cargo check --target wasm32-unknown-unknown` passes (worker)
- [ ] Docs updated (README, CONTRIBUTING, TODO) if behavior changed
- [ ] PR is focused on one concern
