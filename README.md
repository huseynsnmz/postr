<div align="center">
  <h1>postr</h1>
  <p><em>A keyboard-first email TUI with an AI agent, running entirely on Cloudflare Workers</em></p>
</div>

postr lets you send, receive, and manage emails from a single scrolling terminal buffer — all powered by your own Cloudflare account. Inbound mail arrives via [Cloudflare Email Routing](https://developers.cloudflare.com/email-routing/), each mailbox is isolated in its own [Durable Object](https://developers.cloudflare.com/durable-objects/) with a SQLite database, and attachments are stored in [R2](https://developers.cloudflare.com/r2/). An AI layer on top of [Workers AI](https://developers.cloudflare.com/workers-ai/) can summarize threads, draft replies, search across mail, and triage your inbox.

The interaction model borrows from Claude Code: a single scrolling column, a `›` prompt, slash commands, and a small set of single-key bindings. No panes, no mouse, no chrome.

Inspired by Cloudflare's [agentic-inbox](https://github.com/cloudflare/agentic-inbox) — postr re-targets the same Workers + Durable Objects + Email Routing architecture onto a terminal client written in Rust, with the worker also ported to Rust via [`workers-rs`](https://github.com/cloudflare/workers-rs).

## How to set up

1. **Build and deploy the worker**

   ```bash
   cd worker
   ./tools/install.sh     # one-time, installs pinned worker-build into worker/tools/
   npx wrangler deploy
   ```

   You'll be prompted for `DOMAINS` — the domain you want to receive mail for.

2. **Make sure Cloudflare Access is off** for this worker — Settings → Cloudflare Access toggle. postr has no web UI and authenticates the CLI with a bearer token (step 5), so Access would just block the CLI at the edge. If you want Access on later for a web UI, add a **Bypass** policy for path `/api/v1/*` so CLI routes reach the worker.

3. **Set up Email Routing** — Create a catch-all rule on your domain that forwards to this worker.

4. **Enable Email Service** — The worker needs the `send_email` binding for outbound mail. See [Email Service docs](https://developers.cloudflare.com/email-routing/email-workers/send-email-workers/).

5. **Set the CLI token**

   ```bash
   npx wrangler secret put CLI_TOKEN
   ```

6. **Build, log in, create a mailbox**

   ```bash
   cd ../cli && cargo build --release
   ./target/release/postr login https://<your-worker>.workers.dev
   ./target/release/postr add-mailbox me@yourdomain.com   # one-time per address
   ./target/release/postr tui
   ```

   `add-mailbox` writes the marker R2 object the worker checks for; the address must be on a domain whose Email Routing forwards to this worker.

### Why we pin `worker-build`

`worker-build 0.8.4` is the sweet spot: it externalizes `cloudflare:email` in its esbuild step (broken in 0.8.1) but doesn't yet pass `--force-enable-abort-handler` to wasm-bindgen (added in 0.8.5, which requires an externref table that Rust's `wasm32-unknown-unknown` doesn't currently emit). `tools/install.sh` drops 0.8.4 into `worker/tools/worker-build/`; `wrangler.jsonc`'s `build.command` points there. Tracked in [TODO.md](./TODO.md).

### Troubleshooting

1. **`Token rejected.`** — The worker's `CLI_TOKEN` was rotated. Run `postr login` again.
2. **`302 → cloudflareaccess.com/cdn-cgi/access/login`** — Cloudflare Access is enabled on the worker and intercepting at the edge. Turn Access off for this worker, or add a Bypass policy for `/api/v1/*`. (See step 2 above.)
3. **`Could not resolve "cloudflare:email"` / `externref table required for catch wrappers`** — Run `./worker/tools/install.sh` to (re-)install the pinned `worker-build`.
4. **Boxes instead of glyphs in the TUI** — Use a Nerd-Font-capable terminal font.

## Features

- **Keyboard-first TUI** — single scroll buffer, `›` prompt, slash commands, no mouse
- **Full email client** — send and receive via Email Routing with reply/forward threading, folders, search, and attachment metadata
- **Per-mailbox isolation** — each mailbox runs in its own Durable Object with `state.storage().sql()` + R2 for attachments
- **AI slash commands** — `/summarize`, `/draft <prompt>`, `/ask <query>`, `/triage` backed by Workers AI; suggested-reply pills feed straight into compose
- **Bring-your-own-cloud** — runs entirely on your Cloudflare account; no Anthropic, no Gmail, no third-party email server

## Stack

- **CLI / TUI:** Rust, [ratatui](https://ratatui.rs), crossterm, `tui-textarea`, tokio, reqwest (rustls), `keyring`
- **Worker:** Rust, [`workers-rs`](https://github.com/cloudflare/workers-rs) 0.8.5, `mail-parser`, hand-rolled `worker::Router` (no Hono)
- **Storage:** Durable Object SQLite (one DB per mailbox, 8 migrations preserved from agentic-inbox) + R2 attachments
- **AI:** Workers AI — `@cf/moonshotai/kimi-k2.5` (summarize/draft/triage) + `@cf/meta/llama-4-scout-17b-16e-instruct` (ask filter inference)
- **Auth:** Bearer token for the CLI; Cloudflare Access JWT for browser callers (TODO in the Rust worker)

## Getting started

After deploying the worker, run `postr login <worker-url>` then `postr tui`. Press `/` in the TUI to open the slash menu, or run `postr --help` for the CLI.

## Prerequisites

- Cloudflare account with a domain
- [Email Routing](https://developers.cloudflare.com/email-routing/) + [Email Service](https://developers.cloudflare.com/email-service/) enabled
- [Workers AI](https://developers.cloudflare.com/workers-ai/) enabled
- [Cloudflare Access](https://developers.cloudflare.com/cloudflare-one/policies/access/) configured for production
- Rust 1.95+ with the `wasm32-unknown-unknown` target
- A truecolor terminal with a Nerd-Font-capable monospace font

## Architecture

```
┌─────────────┐    HTTPS+Bearer    ┌────────────────┐    stub.fetch    ┌──────────────┐
│  postr CLI  │ ─────/api/v1/*────▶│  postr-worker  │─────/rpc/*──────▶│  MailboxDO   │
│  ratatui    │                    │  workers-rs    │                  │  SQLite + R2 │
└─────────────┘                    └────────┬───────┘                  └──────────────┘
                                            │
                              env.ai("AI")  │  Workers AI
                                            ▼
                            /summarize /draft /ask /triage

Inbound:  Email Routing → #[event(email)] → mail-parser → MailboxDO
Outbound: Compose → /rpc/create_email → env.send_email("EMAIL").send(...)
```

## License

Apache 2.0 — see [LICENSE](./LICENSE).
