-- REFERENCE ONLY. Do NOT apply with `wrangler d1 migrations apply`.
--
-- The upstream TypeScript worker (cloudflare/agentic-inbox) has NO D1
-- binding. All email data lives inside `MailboxDO`'s per-DO SQLite storage
-- (one independent SQLite database per mailbox). This file documents the
-- final schema after applying all 8 migrations from the upstream
-- workers/durableObject/migrations.ts so a future migration-to-D1 has a
-- single source of truth.
--
-- Sources:
--   * worker/workers/db/schema.ts (drizzle definitions)
--   * worker/workers/durableObject/migrations.ts (8 sequential migrations)
--
-- Casing notes: column names are snake_case to match the drizzle schema.
-- INTEGER columns are 0/1 booleans (NULL allowed where DEFAULT 0 is missing).
-- `is_deletable` is 1 by default, but the seed rows for inbox/sent/trash/
-- archive/spam/draft all force it to 0 so the system folders cannot be
-- removed.


-- =====================================================================
-- folders  (migration 1, plus seed rows + draft from migration 3)
-- =====================================================================
CREATE TABLE folders (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL UNIQUE,
    is_deletable INTEGER NOT NULL DEFAULT 1
);

-- Seed rows (system folders — is_deletable = 0):
--   ('inbox',   'Inbox',   0)
--   ('sent',    'Sent',    0)
--   ('trash',   'Trash',   0)
--   ('archive', 'Archive', 0)
--   ('spam',    'Spam',    0)
--   ('draft',   'Drafts',  0)   -- added in migration 3


-- =====================================================================
-- emails  (migration 1 + 2 + 4 + 5 + 7)
-- =====================================================================
CREATE TABLE emails (
    id               TEXT PRIMARY KEY,
    folder_id        TEXT NOT NULL,
    subject          TEXT,
    sender           TEXT,
    recipient        TEXT,
    cc               TEXT,            -- added in migration 7
    bcc              TEXT,            -- added in migration 7
    date             TEXT,            -- ISO-8601 strings, sorted lexicographically
    read             INTEGER DEFAULT 0,
    starred          INTEGER DEFAULT 0,
    body             TEXT,
    in_reply_to      TEXT,            -- added in migration 2 (raw RFC 822 Message-ID, no angle brackets)
    email_references TEXT,            -- added in migration 2 (JSON array of message IDs as a string)
    thread_id        TEXT,            -- added in migration 2
    message_id       TEXT,            -- added in migration 4 (outgoing RFC 822 Message-ID for sent rows)
    raw_headers      TEXT,            -- added in migration 5 (JSON array of {key,value} pairs)
    FOREIGN KEY (folder_id) REFERENCES folders(id) ON DELETE CASCADE
);

CREATE INDEX idx_emails_thread_id              ON emails(thread_id);                  -- migration 2
CREATE INDEX idx_emails_in_reply_to            ON emails(in_reply_to);                -- migration 2
CREATE INDEX IF NOT EXISTS idx_emails_folder_id   ON emails(folder_id);               -- migration 8
CREATE INDEX IF NOT EXISTS idx_emails_date        ON emails(date);                    -- migration 8
CREATE INDEX IF NOT EXISTS idx_emails_folder_date ON emails(folder_id, date DESC);    -- migration 8

-- Migration 6 is a data fix-up only:
--   UPDATE emails SET read = 1 WHERE folder_id = 'sent' AND read = 0;


-- =====================================================================
-- attachments  (migration 1)
-- =====================================================================
CREATE TABLE attachments (
    id          TEXT PRIMARY KEY,
    email_id    TEXT NOT NULL,
    filename    TEXT NOT NULL,
    mimetype    TEXT NOT NULL,
    size        INTEGER NOT NULL,
    content_id  TEXT,
    disposition TEXT,
    FOREIGN KEY (email_id) REFERENCES emails(id) ON DELETE CASCADE
);


-- =====================================================================
-- d1_migrations  (tracking table — applyMigrations bootstraps this)
-- =====================================================================
CREATE TABLE d1_migrations (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    name       TEXT NOT NULL UNIQUE,
    applied_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- After all migrations have applied, this table contains rows:
--   ('1_initial_setup', ...)
--   ('2_add_email_threading', ...)
--   ('3_add_draft_folder', ...)
--   ('4_add_message_id', ...)
--   ('5_add_raw_headers', ...)
--   ('6_mark_sent_emails_as_read', ...)
--   ('7_add_cc_bcc', ...)
--   ('8_add_folder_date_indexes', ...)


-- =====================================================================
-- Schema-derived bonus: a top-level D1 port would also want a
-- `mailboxes` table that today lives only as R2 objects at
-- `mailboxes/{email}.json`. The /cli/me handler in the Rust worker
-- expects to read from this table once it exists. Suggested shape:
--
--   CREATE TABLE mailboxes (
--       id       TEXT PRIMARY KEY,        -- email address, lowercased
--       email    TEXT NOT NULL UNIQUE,    -- duplicate of id for clarity
--       settings TEXT NOT NULL            -- JSON blob
--   );
--
-- The Rust /cli/me handler queries
--   SELECT id, email AS address FROM mailboxes
-- so the column names above match what the handler asks for.
