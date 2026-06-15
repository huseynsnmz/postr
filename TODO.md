## Done in v0.1.0 (Phase 1–6)
- [x] Worker auth + dual-token middleware (Phase 1)
- [x] Async event loop with feedback flash (Phase 2)
- [x] Slash command menu + scoped filter (Phase 3)
- [x] Compose / Reply / Forward + draft save (Phase 4)
- [x] AI panels — /summarize, /draft, /ask, /triage (Phase 5)
- [x] Resize re-wraps Reading body (Phase 6, Track 1)
- [x] 401 wipes keyring + routes to canonical error screen (Phase 6, Track 2)
- [x] Undo for archive within 30 s (Phase 6, Track 3)
- [x] Unit tests for state/command/body (Phase 6, Track 4)
- [x] README at workspace root (Phase 6, Track 5)

## Done in worker (Workflows A–D)
- [x] Rust worker scaffold (workers-rs 0.8.5)
- [x] MailboxDO ported with all 8 migrations
- [x] Auth (bearer; dev bypass)
- [x] Read-only routes: /cli/me, list/get emails, get thread
- [x] Mutation routes: drafts, send, PUT flags, DELETE, move
- [x] AI routes: summarize, draft, ask, triage
- [x] Inbound email handler (mail-parser)

## Open for v1.0
- [ ] Drop the `wasm-bindgen` shim in `worker/scripts/` once `worker-build`
      stops passing `--force-enable-abort-handler` (or `wasm-bindgen` adds
      an option to allocate the externref table itself). Tracks the build
      workaround documented in `README.md`.
- [ ] Worker: expose `POST /drafts/:draftId/send` so the CLI doesn't have to
      reassemble the draft body before calling `/reply` (see
      `cli/src/api/mailbox.rs::send_draft`)
- [ ] `EmailMeta.has_attachment` from the Worker list response — TUI hard-codes
      `false` today (`cli/src/state/mod.rs::TuiMessage::from_meta`)
- [ ] Pagination: bind `G` to fetch page 2 instead of jumping to the last
      visible row
- [ ] `/search` slash command (literal substring search) — currently only
      `/ask` exists, which goes through the LLM
- [ ] Delete undo — needs a Worker "restore from trash" endpoint
- [ ] Send undo — short hold window before Worker commits
- [ ] Terminal capability detection: drop to 256-color when truecolor is
      unsupported (today `theme.rs` assumes truecolor)
- [ ] CF Access JWT verification in worker auth (currently bearer-only)
- [ ] `/draft` persists via `toolDraftReply` equivalent (currently returns AI
      shape without a server-side draft row)
- [ ] Inbound email agent trigger (no Rust agent in scope; TS `EMAIL_AGENT`
      was dropped)
- [ ] Production cutover: handing the `MailboxDO` class from worker-ts-legacy
      (TS) → worker (Rust) — currently they're isolated classes with
      independent storage

## Out of scope for v1
- [ ] Multi-account support — v1 is single-account
- [ ] IMAP/JMAP fallback — Cloudflare Workers AI + Email Routing only
- [ ] Light "warm paper" theme — dark theme only (user explicitly dropped)
- [ ] Mouse support — keyboard-only (per design)
- [ ] Attachment download/preview — `@` glyph displays, no fetch flow in v1
- [ ] Vim-style raw command mode (`:`) — reserved but not implemented

## Post-v1 nice-to-haves
- [ ] SSE reconnect-with-exponential-backoff (v1 has basic reconnect)
- [ ] Saved drafts list view
- [ ] Multi-recipient compose (cc, bcc fields)
- [ ] Settings persistence beyond local config (server-side preferences)
- [ ] Undo window past 30 seconds
- [ ] Configurable keybindings
- [ ] Search history persistence (`/ask` queries)
- [ ] Worker secret rotation flow for the CLI bearer token
- [ ] `postr login` interactive URL prompt (currently requires `postr login <url>`)

## Worker debt (post-v1)
- [ ] Replace shared-secret bearer token with magic-link or device-code auth
- [ ] Per-CLI-session token revocation endpoint
- [ ] Rate limiting on /api/v1/cli/* endpoints
