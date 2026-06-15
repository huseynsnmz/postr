<div align="center">
  <h1>postr</h1>
  <p><em>A keyboard-first email TUI with an AI agent, running entirely on Cloudflare Workers</em></p>
</div>

postr lets you send, receive, and manage emails from a single scrolling terminal buffer — all powered by your own Cloudflare account. Incoming mail arrives via [Cloudflare Email Routing](https://developers.cloudflare.com/email-routing/), each mailbox is isolated in its own [Durable Object](https://developers.cloudflare.com/durable-objects/) with a SQLite database, and attachments are stored in [R2](https://developers.cloudflare.com/r2/).

An **AI-powered backend** can summarize threads, draft replies, search across mail, and triage your inbox — built on [Workers AI](https://developers.cloudflare.com/workers-ai/) (`@cf/moonshotai/kimi-k2.5`).

The interaction model borrows from Claude Code: a single scrolling column, a `›` prompt, slash commands, tree-style output, and a rounded input box. There are **no panes, no mouse, no chrome** — everything is driven from the prompt and a small set of single-key bindings.

Inspired by Cloudflare's [agentic-inbox](https://github.com/cloudflare/agentic-inbox) — a React web client built on the same Workers + Durable Objects + Email Routing stack. postr re-targets that architecture onto a terminal client written in Rust, with the worker also ported to Rust via [`workers-rs`](https://github.com/cloudflare/workers-rs).

## How to set up

### 1. Deploy the Worker

```bash
cd worker
cargo install -q worker-build           # one-time
./scripts/install-wasm-bindgen-shim.sh  # one-time — see "Build workaround" below
worker-build --release                  # produces build/worker/shim.mjs
npx wrangler deploy
```

The deploy flow will create the `MailboxDO` Durable Object namespace, the `agentic-inbox` R2 bucket, and bind Workers AI + the Email Routing send binding. You'll be prompted for `DOMAINS`, the domain you want to receive mail for (e.g. `yourdomain.com`).

### 2. Configure Cloudflare Access (production)

Enable [one-click Cloudflare Access](https://developers.cloudflare.com/changelog/post/2025-10-03-one-click-access-for-workers/) on your Worker. The modal shows `POLICY_AUD` and `TEAM_DOMAIN`; set both as Worker secrets. Set `ENVIRONMENT=production` so the dev bypass turns off.

> The Rust worker currently authenticates the CLI via a bearer token (next step). CF Access JWT verification for browser callers is on the TODO and bypassed in dev — see `worker/src/auth.rs`.

### 3. Set up Email Routing

In the Cloudflare dashboard, go to your domain → Email Routing → create a catch-all rule that forwards to this Worker. Inbound messages will hit the Rust `#[event(email)]` handler and land in the recipient's mailbox.

### 4. Enable Email Service

The worker needs the `send_email` binding (already declared in `wrangler.jsonc`) to send outbound mail. See [Email Service docs](https://developers.cloudflare.com/email-routing/email-workers/send-email-workers/).

### 5. Provision a CLI token

```bash
npx wrangler secret put CLI_TOKEN     # paste any 32+ char random string
```

### 6. Build and log in with the CLI

```bash
cd cli
cargo build --release
./target/release/postr login https://<your-worker>.workers.dev
# paste the same CLI_TOKEN
./target/release/postr tui
```

### Build workaround

`worker-build 0.8.5` invokes `wasm-bindgen 0.2.125` with `--force-enable-abort-handler`, which requires the WASM to have an externref table. Rust 1.95's `wasm32-unknown-unknown` enables `reference-types` codegen but doesn't emit an externref table unless something uses one — producing `error: externref table required for catch wrappers`. The `worker/scripts/install-wasm-bindgen-shim.sh` installer drops a shim into worker-build's cache that strips that flag. Trade-off: Rust panics surface as raw worker errors instead of going through the abort handler. Tracked in [TODO.md](./TODO.md).

### Troubleshooting

- **`Token rejected. Run postr login <url> again`** — the worker's `CLI_TOKEN` secret was rotated or the keychain entry is stale. The TUI already deleted the bad token; re-login.
- **`Cloudflare Access must be configured in production`** — set `POLICY_AUD` and `TEAM_DOMAIN` secrets, or set `ENVIRONMENT=development` for local dev.
- **`worker-build` errors with `externref table required for catch wrappers`** — run `./scripts/install-wasm-bindgen-shim.sh` first (see Build workaround above).
- **`cargo build` fails on `keyring` crate (Linux)** — install `libdbus-1-dev` and a Secret Service implementation (gnome-keyring or keepassxc).
- **Terminal renders boxes instead of glyphs** — use a Nerd-Font-capable terminal font; the TUI relies on `✉ ❯ › ✦ ⎿ └ ●` etc.

## Features

- **Keyboard-first TUI** — single scroll buffer, `›` prompt, slash commands; no panes, no mouse
- **Full email client** — send and receive via Email Routing with reply/forward threading, folder organization, search, and attachment metadata
- **Per-mailbox isolation** — each mailbox runs in its own Durable Object with `state.storage().sql()` and R2 for attachments
- **AI slash commands** — `/summarize`, `/draft <prompt>`, `/ask <query>`, `/triage` all backed by Workers AI; suggested-reply pills feed directly into Compose
- **Bring-your-own-cloud** — runs entirely on your Cloudflare account; no Anthropic, no Gmail, no third-party email server

## Stack

- **CLI / TUI:** Rust 2021, [ratatui](https://ratatui.rs) 0.29, crossterm 0.28, tui-textarea, tokio, reqwest (rustls), `keyring`, `directories`
- **Worker:** Rust, [workers-rs](https://github.com/cloudflare/workers-rs) 0.8.5, `mail-parser`, `chrono`, hand-rolled `worker::Router` (no Hono)
- **Storage:** Durable Object SQLite (one DB per mailbox, 8-migration schema preserved from the original TS project) + R2 for attachments and per-mailbox config
- **AI:** Workers AI — `@cf/moonshotai/kimi-k2.5` (summarize/draft/triage) and `@cf/meta/llama-4-scout-17b-16e-instruct` (ask filter inference)
- **Auth:** bearer token for the CLI (production secret); Cloudflare Access JWT verification for browser callers is reserved (TODO)

## Keybindings

### Inbox

| Key       | Action                          |
|-----------|---------------------------------|
| `j` / `↓` | Next message                    |
| `k` / `↑` | Prev message                    |
| `g` / `G` | Jump to first / last row        |
| `1`–`9`   | Open that numbered row          |
| `⏎`       | Open selected                   |
| `s`       | Toggle star                     |
| `e`       | Archive                         |
| `d`       | Delete                          |
| `u`       | Undo last archive (30s window)  |
| `c`       | Compose new                     |
| `/`       | Slash command menu              |
| `q`       | Quit                            |

### Reading

| Key   | Action                |
|-------|-----------------------|
| `j/k` | Next / prev message   |
| `z`   | Toggle quoted block   |
| `r/a` | Reply                 |
| `f`   | Forward               |
| `esc` | Back to inbox         |

### Compose

| Key   | Action     |
|-------|------------|
| `Tab` | Next field |
| `⌃⏎`  | Send       |
| `⌃d`  | Save draft |
| `esc` | Discard    |

## Slash commands

| Command         | Scope    | What it does                          |
|-----------------|----------|---------------------------------------|
| `/compose`      | any      | Open a blank compose                  |
| `/reply`        | reading  | Reply to the open message             |
| `/forward`      | reading  | Forward the open message              |
| `/archive`      | any      | Archive selected/open                 |
| `/delete`       | any      | Delete selected/open                  |
| `/star`         | any      | Toggle star                           |
| `/summarize`    | reading  | AI summary + suggested replies        |
| `/draft <p>`    | any      | AI-generated draft from prompt `<p>`  |
| `/ask <q>`      | any      | AI-search across mail                 |
| `/triage`       | any      | Auto-categorize new mail              |
| `/logout`       | any      | Quit                                  |

## Configuration

- **Config file** — `worker_base_url`, `email`, `default_mailbox_id`:
  - macOS: `~/Library/Application Support/dev.postr.postr/config.toml`
  - Linux: `~/.config/postr/config.toml`
  - Windows: `%APPDATA%\postr\postr\config\config.toml`
- **Token** — OS keychain (Keychain on macOS, Secret Service on Linux, Credential Manager on Windows); service `postr`, user `default`.

`postr logout` wipes both.

## Prerequisites

- Cloudflare account with a domain
- [Email Routing](https://developers.cloudflare.com/email-routing/) enabled for receiving
- [Email Service](https://developers.cloudflare.com/email-service/) enabled for sending
- [Workers AI](https://developers.cloudflare.com/workers-ai/) enabled
- [Cloudflare Access](https://developers.cloudflare.com/cloudflare-one/policies/access/) configured for deployed environments (or `ENVIRONMENT=development` for local)
- Rust 1.95+ with the `wasm32-unknown-unknown` target (`rustup target add wasm32-unknown-unknown`)
- A truecolor terminal (24-bit color) with a Nerd-Font-capable monospace font

## Architecture

```
┌─────────────────┐    HTTPS + Bearer     ┌───────────────────┐    /rpc/*    ┌────────────────────┐
│  Rust TUI       │ ─────────────────────▶│  Rust Worker      │─────────────▶│  MailboxDO         │
│  (cli/)         │     /api/v1/*         │  (worker/)        │  stub.fetch  │  SQLite + R2 attach│
│  ratatui +      │                       │  workers-rs 0.8.5 │              │  8 ported migrations│
│  crossterm      │                       │  worker::Router   │              └────────────────────┘
└─────────────────┘                       └────────┬──────────┘
                                                   │
                                                   │  env.ai("AI").run(...)
                                                   ▼
                                    ┌──────────────────────────────┐
                                    │  Workers AI                  │
                                    │  /summarize /draft /ask      │
                                    │  /triage                     │
                                    └──────────────────────────────┘

  Inbound:  Email Routing  ─#[event(email)]─▶  mail-parser  ─▶  R2 attachments  ─▶  MailboxDO
  Outbound: Compose  ─▶  /rpc/create_email  ─▶  env.send_email("EMAIL").send(...)
```

## Layout

```
postr/
├─ worker/             # Rust Cloudflare Worker (workers-rs)
├─ cli/                # Rust CLI/TUI
├─ README.md           # this file
├─ TODO.md             # open work + post-v1 items
└─ RUST_CLI_HANDOVER.md
```

## License

Apache 2.0 — see [LICENSE](LICENSE).
