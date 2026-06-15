# Contributing to postr

Thanks for your interest. postr is a small, opinionated project — patches and
issues are welcome.

## Running locally

See the top-level [README.md](./README.md) for the full setup. The TL;DR:

1. Deploy the worker (`cd worker && worker-build --release && npx wrangler deploy`).
2. Set a `CLI_TOKEN` secret on the worker.
3. Build the CLI (`cd cli && cargo build --release`).
4. `./cli/target/release/postr login https://<your-worker>.workers.dev`.

The worker has a `wasm-bindgen` shim workaround documented in the README under
"Build workaround"; please skim that before filing a worker build issue.

## Reporting bugs

Open a [GitHub issue](https://github.com/huseynsnmz/postr/issues/new/choose)
with the bug-report template. Include OS, terminal, Rust version, and the
exact command you ran. Screenshots of TUI glitches are very helpful.

## Submitting pull requests

1. Fork the repo and create a feature branch from `main`.
2. Make the change. Keep the diff small and focused — one concern per PR.
3. Before pushing, in both `cli/` and `worker/`:
   ```bash
   cargo fmt
   cargo clippy --all-targets -- -D warnings
   cargo test               # cli/ only — the worker has no native tests
   ```
4. Open a PR against `main`. Fill out the PR template; describe what you
   changed and how you tested it.

## Code style

- Rust 2021 edition. `cargo fmt` (default config) is the source of truth.
- Clippy must pass with `-D warnings`. If you need to allow a lint locally,
  comment why.
- Match the surrounding style — this codebase prefers explicit over clever.

## License of contributions

By submitting a contribution you agree to license it under the
[Apache License, Version 2.0](./LICENSE), the same license that covers
the rest of the project.
