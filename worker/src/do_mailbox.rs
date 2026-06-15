//! `MailboxDO` — Durable Object port of
//! `worker/workers/durableObject/index.ts`. Storage is the per-DO SQLite
//! exposed at `state.storage().sql()`; see worker 0.8 `src/sql.rs`.
//!
//! This module ports the **read** surface only — the routes that the CLI
//! currently exercises (`getEmails`, `countEmails`, `getEmail`,
//! `getThreadEmails`). Writes (`createEmail`, `updateEmail`,
//! `markThreadRead`, folder mutations, send-rate limit, …) are out of
//! scope for this workflow.
//!
//! RPC shape: each method is a `POST /rpc/<name>` against the stub. The
//! body is JSON, the response is JSON. Keeps the public surface small
//! and avoids leaking SQL into the route layer.

use serde::{Deserialize, Serialize};
use worker::*;

use crate::types::{AttachmentMeta, EmailFull, EmailMeta};

// ── Sort whitelist (defence in depth — interpolated into ORDER BY) ────

const ALLOWED_SORT_COLUMNS: &[&str] = &[
    "id",
    "subject",
    "sender",
    "recipient",
    "date",
    "read",
    "starred",
];

fn whitelist_sort_col(input: Option<&str>) -> Option<&'static str> {
    let v = input?;
    ALLOWED_SORT_COLUMNS.iter().copied().find(|c| *c == v)
}

// ── DO struct ─────────────────────────────────────────────────────────

#[durable_object]
pub struct MailboxDO {
    state: State,
    #[allow(dead_code)] // kept for future write paths (send_email, AI, etc.)
    env: Env,
}

impl DurableObject for MailboxDO {
    fn new(state: State, env: Env) -> Self {
        let s = Self { state, env };
        // Idempotent: subsequent calls after first init are no-ops
        // (each migration checks d1_migrations and skips if applied).
        if let Err(e) = s.run_migrations() {
            console_error!("MailboxDO migration failed: {e}");
        }
        s
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        // The DO sees a URL like `https://do/rpc/get_emails`. The host
        // is whatever the caller passed to Request::new_with_init — we
        // route on path only.
        let url = req.url()?;
        let path = url.path().to_string();
        match (req.method(), path.as_str()) {
            (Method::Post, "/rpc/get_emails") => self.rpc_get_emails(&mut req).await,
            (Method::Post, "/rpc/count_emails") => self.rpc_count_emails(&mut req).await,
            (Method::Post, "/rpc/get_email") => self.rpc_get_email(&mut req).await,
            (Method::Post, "/rpc/get_thread_emails") => self.rpc_get_thread_emails(&mut req).await,
            (Method::Post, "/rpc/create_email") => self.rpc_create_email(&mut req).await,
            (Method::Post, "/rpc/update_email") => self.rpc_update_email(&mut req).await,
            (Method::Post, "/rpc/delete_email") => self.rpc_delete_email(&mut req).await,
            (Method::Post, "/rpc/move_email") => self.rpc_move_email(&mut req).await,
            (Method::Post, "/rpc/mark_thread_read") => self.rpc_mark_thread_read(&mut req).await,
            (Method::Post, "/rpc/check_send_rate_limit") => {
                self.rpc_check_send_rate_limit(&mut req).await
            }
            (Method::Post, "/rpc/search_emails") => self.rpc_search_emails(&mut req).await,
            (Method::Post, "/rpc/count_search_results") => {
                self.rpc_count_search_results(&mut req).await
            }
            (Method::Post, "/rpc/find_thread_by_subject") => {
                self.rpc_find_thread_by_subject(&mut req).await
            }
            (Method::Post, "/rpc/purge_old_trash") => self.rpc_purge_old_trash(&mut req).await,
            (Method::Post, "/rpc/mark_all_read") => self.rpc_mark_all_read(&mut req).await,
            (Method::Post, "/rpc/seed_demo") => self.rpc_seed_demo(&mut req).await,
            _ => Response::error(format!("MailboxDO: unknown rpc path {path}"), 404),
        }
    }
}

// ── Migrations (port of worker/workers/durableObject/migrations.ts) ───

/// A single migration. `name` is the value persisted in `d1_migrations`
/// for idempotency.
struct Migration {
    name: &'static str,
    sql: &'static str,
}

/// Verbatim port of `mailboxMigrations` in TS. Order matters — they run
/// in array order, and later migrations alter tables defined earlier.
const MAILBOX_MIGRATIONS: &[Migration] = &[
    Migration {
        name: "1_initial_setup",
        sql: r#"
            CREATE TABLE folders (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                is_deletable INTEGER NOT NULL DEFAULT 1
            );

            INSERT INTO folders (id, name, is_deletable) VALUES
                ('inbox', 'Inbox', 0),
                ('sent', 'Sent', 0),
                ('trash', 'Trash', 0),
                ('archive', 'Archive', 0),
                ('spam', 'Spam', 0);

            CREATE TABLE emails (
                id TEXT PRIMARY KEY,
                folder_id TEXT NOT NULL,
                subject TEXT,
                sender TEXT,
                recipient TEXT,
                date TEXT,
                read INTEGER DEFAULT 0,
                starred INTEGER DEFAULT 0,
                body TEXT,
                FOREIGN KEY(folder_id) REFERENCES folders(id) ON DELETE CASCADE
            );

            CREATE TABLE attachments (
                id TEXT PRIMARY KEY,
                email_id TEXT NOT NULL,
                filename TEXT NOT NULL,
                mimetype TEXT NOT NULL,
                size INTEGER NOT NULL,
                content_id TEXT,
                disposition TEXT,
                FOREIGN KEY(email_id) REFERENCES emails(id) ON DELETE CASCADE
            );
        "#,
    },
    Migration {
        name: "2_add_email_threading",
        sql: r#"
            ALTER TABLE emails ADD COLUMN in_reply_to TEXT;
            ALTER TABLE emails ADD COLUMN email_references TEXT;
            ALTER TABLE emails ADD COLUMN thread_id TEXT;

            CREATE INDEX idx_emails_thread_id ON emails(thread_id);
            CREATE INDEX idx_emails_in_reply_to ON emails(in_reply_to);
        "#,
    },
    Migration {
        name: "3_add_draft_folder",
        sql: "INSERT INTO folders (id, name, is_deletable) VALUES ('draft', 'Drafts', 0);",
    },
    Migration {
        name: "4_add_message_id",
        sql: "ALTER TABLE emails ADD COLUMN message_id TEXT;",
    },
    Migration {
        name: "5_add_raw_headers",
        sql: "ALTER TABLE emails ADD COLUMN raw_headers TEXT;",
    },
    Migration {
        name: "6_mark_sent_emails_as_read",
        sql: "UPDATE emails SET read = 1 WHERE folder_id = 'sent' AND read = 0;",
    },
    Migration {
        name: "7_add_cc_bcc",
        sql: r#"
            ALTER TABLE emails ADD COLUMN cc TEXT;
            ALTER TABLE emails ADD COLUMN bcc TEXT;
        "#,
    },
    Migration {
        name: "8_add_folder_date_indexes",
        sql: r#"
            CREATE INDEX IF NOT EXISTS idx_emails_folder_id ON emails(folder_id);
            CREATE INDEX IF NOT EXISTS idx_emails_date ON emails(date);
            CREATE INDEX IF NOT EXISTS idx_emails_folder_date ON emails(folder_id, date DESC);
        "#,
    },
];

/// Row shape used to detect already-applied migrations.
#[derive(Deserialize)]
struct MigrationNameRow {
    name: String,
}

impl MailboxDO {
    /// Create the tracking table if needed, then run any migrations not
    /// yet recorded in `d1_migrations`. Each migration's SQL is passed
    /// through `sql.exec` in one call — SQLite supports multi-statement
    /// strings here. CF's DO runtime forbids SQL-level `BEGIN/COMMIT`,
    /// so the migration text must not contain transaction boundaries.
    fn run_migrations(&self) -> Result<()> {
        let sql = self.state.storage().sql();

        sql.exec(
            "CREATE TABLE IF NOT EXISTS d1_migrations (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE,
                applied_at TEXT NOT NULL DEFAULT (datetime('now'))
            )",
            None,
        )?;

        let applied: Vec<MigrationNameRow> = sql
            .exec("SELECT name FROM d1_migrations", None)?
            .to_array()?;
        let applied: std::collections::HashSet<String> =
            applied.into_iter().map(|r| r.name).collect();

        for m in MAILBOX_MIGRATIONS {
            if applied.contains(m.name) {
                continue;
            }
            sql.exec(m.sql, None)?;
            sql.exec(
                "INSERT INTO d1_migrations (name) VALUES (?)",
                Some(vec![m.name.into()]),
            )?;
        }
        Ok(())
    }
}

// ── RPC argument shapes ───────────────────────────────────────────────

#[derive(Deserialize)]
struct GetEmailsArgs {
    folder: Option<String>,
    thread_id: Option<String>,
    page: Option<u32>,
    limit: Option<u32>,
    sort_column: Option<String>,
    sort_direction: Option<String>,
}

#[derive(Deserialize)]
struct CountEmailsArgs {
    folder: Option<String>,
    thread_id: Option<String>,
}

#[derive(Deserialize)]
struct GetEmailArgs {
    id: String,
}

#[derive(Deserialize)]
struct GetThreadEmailsArgs {
    thread_id: String,
}

#[derive(Serialize)]
struct CountResponse {
    count: i64,
}

// ── Write-path RPC argument shapes ────────────────────────────────────

#[derive(Deserialize)]
struct CreateEmailRow {
    id: String,
    subject: Option<String>,
    sender: Option<String>,
    recipient: Option<String>,
    cc: Option<String>,
    bcc: Option<String>,
    date: String,
    body: Option<String>,
    in_reply_to: Option<String>,
    email_references: Option<String>,
    thread_id: Option<String>,
    message_id: Option<String>,
    raw_headers: Option<String>,
}

#[derive(Deserialize)]
struct AttachmentInsertRow {
    id: String,
    email_id: String,
    filename: String,
    mimetype: String,
    size: i64,
    content_id: Option<String>,
    disposition: Option<String>,
}

#[derive(Deserialize)]
struct CreateEmailArgs {
    /// Folder name OR id (matches TS createEmail behaviour: TS:830).
    folder: String,
    email: CreateEmailRow,
    #[serde(default)]
    attachments: Vec<AttachmentInsertRow>,
}

#[derive(Deserialize)]
struct UpdateEmailArgs {
    id: String,
    #[serde(default)]
    read: Option<bool>,
    #[serde(default)]
    starred: Option<bool>,
}

#[derive(Deserialize)]
struct DeleteEmailArgs {
    id: String,
}

#[derive(Deserialize)]
struct MoveEmailArgs {
    id: String,
    folder_id: String,
}

#[derive(Deserialize)]
struct MarkThreadReadArgs {
    thread_id: String,
}

// ── Search RPC arg shapes (port of TS SearchFilterOptions) ────────────

#[derive(Deserialize)]
struct SearchEmailsArgs {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    folder: Option<String>,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    date_start: Option<String>,
    #[serde(default)]
    date_end: Option<String>,
    #[serde(default)]
    is_read: Option<bool>,
    #[serde(default)]
    is_starred: Option<bool>,
    #[serde(default)]
    has_attachment: Option<bool>,
    #[serde(default)]
    page: Option<u32>,
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Deserialize)]
struct FindThreadBySubjectArgs {
    subject: String,
    #[serde(default)]
    sender_address: Option<String>,
}

#[derive(Deserialize)]
struct ThreadCandidateRow {
    thread_id: String,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    senders: Option<String>,
    #[serde(default)]
    recipients: Option<String>,
}

#[derive(Deserialize)]
struct FolderIdRow {
    id: String,
}

/// Subset of attachment row returned by `/rpc/delete_email` so the route
/// can issue R2 deletes after the SQL row vanishes.
#[derive(Deserialize, Serialize)]
struct DeletedAttachment {
    id: String,
    filename: String,
}

// ── DO-side row shapes (deserialised from SqlCursor::to_array) ────────
//
// Column names must match the SELECT projection exactly because
// `to_array` returns `{column_name: value, …}` objects (per
// serde_wasm_bindgen). `read`/`starred` are INTEGER in SQLite — we
// pull them as i64 and coerce to bool at the boundary.

#[derive(Deserialize)]
struct EmailListSqlRow {
    id: String,
    subject: Option<String>,
    sender: Option<String>,
    recipient: Option<String>,
    cc: Option<String>,
    bcc: Option<String>,
    date: Option<String>,
    #[serde(default)]
    read: i64,
    #[serde(default)]
    starred: i64,
    in_reply_to: Option<String>,
    email_references: Option<String>,
    thread_id: Option<String>,
    folder_id: Option<String>,
    snippet: Option<String>,
    /// Lit by `EXISTS (SELECT 1 FROM attachments ...)` in the list query.
    /// Defaults to 0 when the projection doesn't include it (other call
    /// sites that share `meta_from_sql` won't break).
    #[serde(default)]
    has_attachments: i64,
}

#[derive(Deserialize)]
struct EmailFullSqlRow {
    id: String,
    subject: Option<String>,
    sender: Option<String>,
    recipient: Option<String>,
    cc: Option<String>,
    bcc: Option<String>,
    date: Option<String>,
    #[serde(default)]
    read: i64,
    #[serde(default)]
    starred: i64,
    body: Option<String>,
    in_reply_to: Option<String>,
    email_references: Option<String>,
    thread_id: Option<String>,
    folder_id: Option<String>,
    message_id: Option<String>,
    raw_headers: Option<String>,
}

#[derive(Deserialize)]
struct AttachmentSqlRow {
    id: String,
    email_id: String,
    filename: String,
    mimetype: String,
    size: i64,
    content_id: Option<String>,
    disposition: Option<String>,
}

#[derive(Deserialize)]
struct CountSqlRow {
    total: i64,
}

// ── Row → wire-shape conversions ──────────────────────────────────────

fn meta_from_sql(r: EmailListSqlRow) -> EmailMeta {
    EmailMeta {
        id: r.id,
        subject: r.subject,
        sender: r.sender,
        recipient: r.recipient,
        cc: r.cc,
        bcc: r.bcc,
        date: r.date,
        read: r.read != 0,
        starred: r.starred != 0,
        in_reply_to: r.in_reply_to,
        email_references: r.email_references,
        thread_id: r.thread_id,
        folder_id: r.folder_id,
        snippet: r.snippet,
        has_attachments: r.has_attachments != 0,
    }
}

fn full_from_sql(r: EmailFullSqlRow, atts: Vec<AttachmentMeta>) -> EmailFull {
    EmailFull {
        id: r.id,
        subject: r.subject,
        sender: r.sender,
        recipient: r.recipient,
        cc: r.cc,
        bcc: r.bcc,
        date: r.date,
        read: r.read != 0,
        starred: r.starred != 0,
        body: r.body,
        in_reply_to: r.in_reply_to,
        email_references: r.email_references,
        thread_id: r.thread_id,
        folder_id: r.folder_id,
        message_id: r.message_id,
        raw_headers: r.raw_headers,
        attachments: atts,
    }
}

fn att_from_sql(a: AttachmentSqlRow) -> AttachmentMeta {
    AttachmentMeta {
        id: a.id,
        filename: a.filename,
        mimetype: a.mimetype,
        size: a.size,
        content_id: a.content_id,
        disposition: a.disposition,
    }
}

// ── RPC implementations ───────────────────────────────────────────────

impl MailboxDO {
    /// Port of `MailboxDO.getEmails` (TS:114-177).
    ///
    /// Threaded mode (TS:213-386) is **not yet ported** — it requires a
    /// large CTE that depends on the `Folders.DRAFT` constant and a SQL
    /// expression we'd have to inline byte-for-byte. The CLI surface
    /// hitting this DO at the moment only uses the non-threaded path
    /// (per worker/workers/routes/emails.ts), so we shortcut the
    /// threaded branch and fall back to the simple SELECT. When
    /// threading is wired up, port the two CTEs verbatim from TS.
    async fn rpc_get_emails(&self, req: &mut Request) -> Result<Response> {
        let args: GetEmailsArgs = req.json().await?;
        let page = args.page.unwrap_or(1).max(1) as i64;
        let limit = args.limit.unwrap_or(25).clamp(1, 100) as i64;
        let sort_col = whitelist_sort_col(args.sort_column.as_deref()).unwrap_or("date");
        let sort_dir = match args.sort_direction.as_deref() {
            Some("ASC") | Some("asc") => "ASC",
            _ => "DESC",
        };
        let offset = (page - 1) * limit;

        // Build WHERE incrementally. SQLite uses `?` for positional
        // placeholders here; the worker SqlStorage API binds Vec<SqlStorageValue>
        // in order, so we don't number them.
        let mut conditions: Vec<&str> = Vec::new();
        let mut bindings: Vec<SqlStorageValue> = Vec::new();

        if let Some(ref f) = args.folder {
            conditions
                .push("folder_id = (SELECT id FROM folders WHERE (name = ? OR id = ?) LIMIT 1)");
            bindings.push(f.as_str().into());
            bindings.push(f.as_str().into());
        }
        if let Some(ref t) = args.thread_id {
            conditions.push("thread_id = ?");
            bindings.push(t.as_str().into());
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };

        let query = format!(
            "SELECT id, subject, sender, recipient, cc, bcc, date, read, starred,
                    in_reply_to, email_references, thread_id, folder_id,
                    SUBSTR(body, 1, 300) as snippet,
                    (SELECT COUNT(1) > 0 FROM attachments WHERE email_id = emails.id) as has_attachments
             FROM emails
             {where_clause}
             ORDER BY {sort_col} {sort_dir}
             LIMIT ? OFFSET ?"
        );
        bindings.push(limit.into());
        bindings.push(offset.into());

        let sql_storage = self.state.storage().sql();
        let cursor = sql_storage.exec(&query, Some(bindings))?;
        let rows: Vec<EmailListSqlRow> = cursor.to_array()?;
        let emails: Vec<EmailMeta> = rows.into_iter().map(meta_from_sql).collect();

        Response::from_json(&emails)
    }

    /// Port of `MailboxDO.countEmails` (TS:182-209).
    async fn rpc_count_emails(&self, req: &mut Request) -> Result<Response> {
        let args: CountEmailsArgs = req.json().await?;
        let mut conditions: Vec<&str> = Vec::new();
        let mut bindings: Vec<SqlStorageValue> = Vec::new();

        if let Some(ref f) = args.folder {
            conditions
                .push("folder_id = (SELECT id FROM folders WHERE (name = ? OR id = ?) LIMIT 1)");
            bindings.push(f.as_str().into());
            bindings.push(f.as_str().into());
        }
        if let Some(ref t) = args.thread_id {
            conditions.push("thread_id = ?");
            bindings.push(t.as_str().into());
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };
        let query = format!("SELECT COUNT(*) as total FROM emails {where_clause}");

        let sql_storage = self.state.storage().sql();
        let cursor = sql_storage.exec(&query, Some(bindings))?;
        let rows: Vec<CountSqlRow> = cursor.to_array()?;
        let count = rows.first().map(|r| r.total).unwrap_or(0);
        Response::from_json(&CountResponse { count })
    }

    /// Port of `MailboxDO.getEmail` (TS:439-460). Returns `null` if the
    /// row is missing; route layer translates that to 404.
    async fn rpc_get_email(&self, req: &mut Request) -> Result<Response> {
        let args: GetEmailArgs = req.json().await?;
        let sql_storage = self.state.storage().sql();

        let rows: Vec<EmailFullSqlRow> = sql_storage
            .exec(
                "SELECT id, subject, sender, recipient, cc, bcc, date, read, starred,
                        body, in_reply_to, email_references, thread_id, folder_id,
                        message_id, raw_headers
                 FROM emails
                 WHERE id = ?
                 LIMIT 1",
                Some(vec![args.id.as_str().into()]),
            )?
            .to_array()?;

        let Some(row) = rows.into_iter().next() else {
            return Response::from_json(&serde_json::Value::Null);
        };

        let att_rows: Vec<AttachmentSqlRow> = sql_storage
            .exec(
                "SELECT id, email_id, filename, mimetype, size, content_id, disposition
                 FROM attachments
                 WHERE email_id = ?",
                Some(vec![args.id.as_str().into()]),
            )?
            .to_array()?;

        let attachments: Vec<AttachmentMeta> = att_rows.into_iter().map(att_from_sql).collect();
        Response::from_json(&full_from_sql(row, attachments))
    }

    /// Port of `MailboxDO.getThreadEmails` (TS:467-502).
    async fn rpc_get_thread_emails(&self, req: &mut Request) -> Result<Response> {
        let args: GetThreadEmailsArgs = req.json().await?;
        let sql_storage = self.state.storage().sql();

        let email_rows: Vec<EmailFullSqlRow> = sql_storage
            .exec(
                "SELECT id, subject, sender, recipient, cc, bcc, date, read, starred,
                        body, in_reply_to, email_references, thread_id, folder_id,
                        message_id, raw_headers
                 FROM emails
                 WHERE thread_id = ?
                 ORDER BY date ASC",
                Some(vec![args.thread_id.as_str().into()]),
            )?
            .to_array()?;

        if email_rows.is_empty() {
            let empty: Vec<EmailFull> = Vec::new();
            return Response::from_json(&empty);
        }

        // Batch-fetch all attachments for the thread in one query.
        // TS does `email_id IN (?, ?, …)` with one binding per id; we
        // do the same. Build placeholders + bindings together.
        let ids: Vec<&str> = email_rows.iter().map(|r| r.id.as_str()).collect();
        let placeholders = vec!["?"; ids.len()].join(",");
        let att_query = format!(
            "SELECT id, email_id, filename, mimetype, size, content_id, disposition
             FROM attachments
             WHERE email_id IN ({placeholders})"
        );
        let bindings: Vec<SqlStorageValue> = ids.iter().map(|s| (*s).into()).collect();
        let att_rows: Vec<AttachmentSqlRow> =
            sql_storage.exec(&att_query, Some(bindings))?.to_array()?;

        // Group attachments by email_id.
        let mut by_email: std::collections::HashMap<String, Vec<AttachmentMeta>> =
            std::collections::HashMap::new();
        for a in att_rows {
            by_email
                .entry(a.email_id.clone())
                .or_default()
                .push(att_from_sql(a));
        }

        let thread: Vec<EmailFull> = email_rows
            .into_iter()
            .map(|r| {
                let atts = by_email.remove(&r.id).unwrap_or_default();
                full_from_sql(r, atts)
            })
            .collect();
        Response::from_json(&thread)
    }

    // ── Write-path RPCs (Workflow B) ──────────────────────────────────

    /// Port of `MailboxDO.createEmail` (TS:821-871). Resolves the folder
    /// by id-or-name, inserts the email row (forcing `read=1` for the
    /// `sent` folder per TS:842-845), then inserts attachments if any.
    /// Returns `{"ok": true}` on success, 400 with `{"error": ...}` if
    /// the folder does not exist.
    async fn rpc_create_email(&self, req: &mut Request) -> Result<Response> {
        let args: CreateEmailArgs = req.json().await?;
        let sql_storage = self.state.storage().sql();

        // Resolve folder by id OR name (matches TS or() of eq id, eq name).
        let folder_rows: Vec<FolderIdRow> = sql_storage
            .exec(
                "SELECT id FROM folders WHERE id = ? OR name = ? LIMIT 1",
                Some(vec![
                    args.folder.as_str().into(),
                    args.folder.as_str().into(),
                ]),
            )?
            .to_array()?;
        let Some(folder_row) = folder_rows.into_iter().next() else {
            return Ok(Response::from_json(&serde_json::json!({
                "error": format!("createEmail: folder \"{}\" not found", args.folder),
            }))?
            .with_status(400));
        };
        let folder_id = folder_row.id;
        let is_sent = folder_id == "sent";

        let e = args.email;
        let read_val: i64 = if is_sent { 1 } else { 0 };
        let bindings: Vec<SqlStorageValue> = vec![
            e.id.as_str().into(),
            folder_id.as_str().into(),
            e.subject.clone().into(),
            e.sender.clone().into(),
            e.recipient.clone().into(),
            e.cc.clone().into(),
            e.bcc.clone().into(),
            e.date.as_str().into(),
            read_val.into(),
            e.body.clone().into(),
            e.in_reply_to.clone().into(),
            e.email_references.clone().into(),
            e.thread_id.clone().into(),
            e.message_id.clone().into(),
            e.raw_headers.clone().into(),
        ];

        sql_storage.exec(
            "INSERT INTO emails
                (id, folder_id, subject, sender, recipient, cc, bcc, date,
                 read, starred, body, in_reply_to, email_references,
                 thread_id, message_id, raw_headers)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?, ?, ?, ?, ?)",
            Some(bindings),
        )?;

        for att in &args.attachments {
            sql_storage.exec(
                "INSERT INTO attachments
                    (id, email_id, filename, mimetype, size, content_id, disposition)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
                Some(vec![
                    att.id.as_str().into(),
                    att.email_id.as_str().into(),
                    att.filename.as_str().into(),
                    att.mimetype.as_str().into(),
                    att.size.into(),
                    att.content_id.clone().into(),
                    att.disposition.clone().into(),
                ]),
            )?;
        }

        Response::from_json(&serde_json::json!({ "ok": true }))
    }

    /// Port of `MailboxDO.updateEmail` (TS:504-527). Builds a partial
    /// UPDATE from whichever of `{read, starred}` is provided, then
    /// returns the full row (same shape as `rpc_get_email`). Returns
    /// `null` if the row does not exist.
    async fn rpc_update_email(&self, req: &mut Request) -> Result<Response> {
        let args: UpdateEmailArgs = req.json().await?;
        let sql_storage = self.state.storage().sql();

        // Dynamic SET clause (TS:508-518). When neither field is set we
        // skip the UPDATE — same behaviour as TS:516-518 which returns
        // the current row unchanged.
        let mut sets: Vec<&str> = Vec::new();
        let mut bindings: Vec<SqlStorageValue> = Vec::new();
        if let Some(r) = args.read {
            sets.push("read = ?");
            bindings.push((if r { 1i64 } else { 0i64 }).into());
        }
        if let Some(s) = args.starred {
            sets.push("starred = ?");
            bindings.push((if s { 1i64 } else { 0i64 }).into());
        }
        if !sets.is_empty() {
            bindings.push(args.id.as_str().into());
            let query = format!("UPDATE emails SET {} WHERE id = ?", sets.join(", "));
            sql_storage.exec(&query, Some(bindings))?;
        }

        // Re-fetch the row (same projection as rpc_get_email).
        let rows: Vec<EmailFullSqlRow> = sql_storage
            .exec(
                "SELECT id, subject, sender, recipient, cc, bcc, date, read, starred,
                        body, in_reply_to, email_references, thread_id, folder_id,
                        message_id, raw_headers
                 FROM emails
                 WHERE id = ?
                 LIMIT 1",
                Some(vec![args.id.as_str().into()]),
            )?
            .to_array()?;

        let Some(row) = rows.into_iter().next() else {
            return Response::from_json(&serde_json::Value::Null);
        };

        let att_rows: Vec<AttachmentSqlRow> = sql_storage
            .exec(
                "SELECT id, email_id, filename, mimetype, size, content_id, disposition
                 FROM attachments
                 WHERE email_id = ?",
                Some(vec![args.id.as_str().into()]),
            )?
            .to_array()?;

        let attachments: Vec<AttachmentMeta> = att_rows.into_iter().map(att_from_sql).collect();
        Response::from_json(&full_from_sql(row, attachments))
    }

    /// Port of `MailboxDO.deleteEmail` (TS:537-561). Returns the list of
    /// `{id, filename}` attachments so the route layer can clean up R2
    /// blobs. Returns `null` when the email row does not exist.
    async fn rpc_delete_email(&self, req: &mut Request) -> Result<Response> {
        let args: DeleteEmailArgs = req.json().await?;
        let sql_storage = self.state.storage().sql();

        // Existence check (TS:538-544).
        let existing: Vec<FolderIdRow> = sql_storage
            .exec(
                "SELECT id FROM emails WHERE id = ?",
                Some(vec![args.id.as_str().into()]),
            )?
            .to_array()?;
        if existing.is_empty() {
            return Response::from_json(&serde_json::Value::Null);
        }

        // Snapshot attachments before deletion. The TS code relies on
        // ON DELETE CASCADE to remove the rows, but reads the metadata
        // first so the worker can issue R2 deletes.
        let att_rows: Vec<DeletedAttachment> = sql_storage
            .exec(
                "SELECT id, filename FROM attachments WHERE email_id = ?",
                Some(vec![args.id.as_str().into()]),
            )?
            .to_array()?;

        sql_storage.exec(
            "DELETE FROM emails WHERE id = ?",
            Some(vec![args.id.as_str().into()]),
        )?;

        Response::from_json(&att_rows)
    }

    /// Insert a curated set of demo emails into this mailbox so screenshots
    /// (and onboarding) have realistic-looking data without round-tripping
    /// through the real Email Routing path. Idempotent: rows are keyed by
    /// `demo-{n}-…` ids and existing demo rows are wiped first.
    async fn rpc_seed_demo(&self, req: &mut Request) -> Result<Response> {
        #[derive(serde::Deserialize)]
        struct Args {
            #[serde(default)]
            recipient: String,
            #[serde(default = "default_now")]
            now_iso: String,
        }
        fn default_now() -> String {
            chrono::Utc::now()
                .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                .to_string()
        }
        let args: Args = req.json().await?;
        let recipient = args.recipient;
        let sql = self.state.storage().sql();

        // Wipe any prior demo rows so re-seeding stays idempotent.
        sql.exec("DELETE FROM emails WHERE id LIKE 'demo-%'", None)?;
        sql.exec("DELETE FROM attachments WHERE email_id LIKE 'demo-%'", None)?;

        let now: chrono::DateTime<chrono::Utc> =
            chrono::DateTime::parse_from_rfc3339(&args.now_iso)
                .map(|d| d.with_timezone(&chrono::Utc))
                .unwrap_or_else(|_| chrono::Utc::now());

        let rows = demo_rows();
        for r in &rows {
            insert_demo_email(&sql, r, &recipient, now)?;
        }
        // One demo attachment so the @ glyph is visible in the inbox.
        if let Some(attach_target) = rows.iter().find(|r| r.id == "demo-04-stripe-invoice") {
            sql.exec(
                "INSERT INTO attachments
                    (id, email_id, filename, mimetype, size, content_id, disposition)
                 VALUES (?, ?, ?, ?, ?, NULL, 'attachment')",
                Some(vec![
                    "demo-att-01".into(),
                    attach_target.id.into(),
                    "invoice-2026-06.pdf".into(),
                    "application/pdf".into(),
                    243_512_i64.into(),
                ]),
            )?;
        }

        Response::from_json(&serde_json::json!({ "created": rows.len() }))
    }

    /// Flip `read = 1` on every unread email in the given folder. Returns
    /// `{ updated: <count> }` so the route + CLI can flash the actual
    /// number of rows that changed.
    async fn rpc_mark_all_read(&self, req: &mut Request) -> Result<Response> {
        #[derive(serde::Deserialize)]
        struct Args {
            folder: String,
        }
        let args: Args = req.json().await?;
        let sql_storage = self.state.storage().sql();

        // Count first so the response reflects what actually changed
        // (post-update SQLite has no rows_changed surface available here).
        let count_rows: Vec<CountSqlRow> = sql_storage
            .exec(
                "SELECT COUNT(1) AS total FROM emails
                 WHERE folder_id = ? AND read = 0",
                Some(vec![args.folder.as_str().into()]),
            )?
            .to_array()?;
        let updated = count_rows.first().map(|r| r.total).unwrap_or(0);

        if updated > 0 {
            sql_storage.exec(
                "UPDATE emails SET read = 1 WHERE folder_id = ? AND read = 0",
                Some(vec![args.folder.as_str().into()]),
            )?;
        }
        Response::from_json(&serde_json::json!({ "updated": updated }))
    }

    /// Sweep emails out of the `trash` folder that have been sitting there
    /// for at least `days` days. Returns a list of
    /// `{ email_id, attachments: [{id, filename}] }` records so the
    /// scheduled-event caller can clean up the freed R2 attachment blobs.
    ///
    /// Cutoff is computed against the email's `date` column (RFC 3339
    /// strings written by `rpc_create_email`); SQLite's `datetime()` parses
    /// ISO timestamps natively.
    async fn rpc_purge_old_trash(&self, req: &mut Request) -> Result<Response> {
        #[derive(serde::Deserialize)]
        struct PurgeArgs {
            #[serde(default = "default_days")]
            days: i64,
        }
        fn default_days() -> i64 {
            30
        }
        let args: PurgeArgs = req.json().await?;
        let days = args.days.max(1);
        let sql_storage = self.state.storage().sql();

        // Pick up everything older than the cutoff, then snapshot their
        // attachments before the cascade-delete fires.
        let modifier = format!("-{days} days");
        let stale_emails: Vec<FolderIdRow> = sql_storage
            .exec(
                "SELECT id FROM emails
                 WHERE folder_id = 'trash'
                   AND date IS NOT NULL
                   AND datetime(date) < datetime('now', ?)",
                Some(vec![modifier.as_str().into()]),
            )?
            .to_array()?;

        if stale_emails.is_empty() {
            return Response::from_json(&serde_json::Value::Array(Vec::new()));
        }

        let mut purged: Vec<serde_json::Value> = Vec::with_capacity(stale_emails.len());
        for row in &stale_emails {
            let atts: Vec<DeletedAttachment> = sql_storage
                .exec(
                    "SELECT id, filename FROM attachments WHERE email_id = ?",
                    Some(vec![row.id.as_str().into()]),
                )?
                .to_array()?;
            sql_storage.exec(
                "DELETE FROM emails WHERE id = ?",
                Some(vec![row.id.as_str().into()]),
            )?;
            purged.push(serde_json::json!({
                "email_id": row.id,
                "attachments": atts,
            }));
        }

        Response::from_json(&purged)
    }

    /// Port of `MailboxDO.moveEmail` (TS:634-650). Folder lookup is
    /// **id-only** in TS (no name fallback), so we follow that.
    async fn rpc_move_email(&self, req: &mut Request) -> Result<Response> {
        let args: MoveEmailArgs = req.json().await?;
        let sql_storage = self.state.storage().sql();

        let folder_rows: Vec<FolderIdRow> = sql_storage
            .exec(
                "SELECT id FROM folders WHERE id = ?",
                Some(vec![args.folder_id.as_str().into()]),
            )?
            .to_array()?;
        if folder_rows.is_empty() {
            return Response::from_json(&serde_json::json!({ "ok": false }));
        }

        sql_storage.exec(
            "UPDATE emails SET folder_id = ? WHERE id = ?",
            Some(vec![
                args.folder_id.as_str().into(),
                args.id.as_str().into(),
            ]),
        )?;

        Response::from_json(&serde_json::json!({ "ok": true }))
    }

    /// Port of `MailboxDO.markThreadRead` (TS:529-535).
    async fn rpc_mark_thread_read(&self, req: &mut Request) -> Result<Response> {
        let args: MarkThreadReadArgs = req.json().await?;
        let sql_storage = self.state.storage().sql();
        sql_storage.exec(
            "UPDATE emails SET read = 1 WHERE thread_id = ? AND read = 0",
            Some(vec![args.thread_id.as_str().into()]),
        )?;
        Response::from_json(&serde_json::json!({ "ok": true }))
    }

    /// Port of `MailboxDO.searchEmails` + `#buildSearchConditions`
    /// (TS:658-723). Same projection as the list endpoint plus a
    /// `folder_name` column (left-join). Returns `Vec<EmailMeta>` so the
    /// AI `/ask` route can hand it back to the CLI verbatim.
    async fn rpc_search_emails(&self, req: &mut Request) -> Result<Response> {
        let args: SearchEmailsArgs = req.json().await?;
        let page = args.page.unwrap_or(1).max(1) as i64;
        let limit = args.limit.unwrap_or(25).clamp(1, 100) as i64;
        let offset = (page - 1) * limit;

        let (where_clause, bindings) = build_search_conditions(&args, "e");

        let query = format!(
            "SELECT e.id, e.subject, e.sender, e.recipient, e.cc, e.bcc, e.date,
                    e.read, e.starred, e.in_reply_to, e.email_references,
                    e.thread_id, e.folder_id,
                    SUBSTR(e.body, 1, 300) as snippet,
                    (SELECT COUNT(1) > 0 FROM attachments WHERE email_id = e.id) as has_attachments
             FROM emails e
             {where_clause}
             ORDER BY e.date DESC
             LIMIT ? OFFSET ?"
        );

        let mut binds = bindings;
        binds.push(limit.into());
        binds.push(offset.into());

        let sql_storage = self.state.storage().sql();
        let rows: Vec<EmailListSqlRow> = sql_storage.exec(&query, Some(binds))?.to_array()?;
        let emails: Vec<EmailMeta> = rows.into_iter().map(meta_from_sql).collect();
        Response::from_json(&emails)
    }

    /// Port of `MailboxDO.countSearchResults` (TS:728-738).
    async fn rpc_count_search_results(&self, req: &mut Request) -> Result<Response> {
        let args: SearchEmailsArgs = req.json().await?;
        let (where_clause, bindings) = build_search_conditions(&args, "");

        let query = format!("SELECT COUNT(*) as total FROM emails {where_clause}");
        let sql_storage = self.state.storage().sql();
        let rows: Vec<CountSqlRow> = sql_storage.exec(&query, Some(bindings))?.to_array()?;
        let count = rows.first().map(|r| r.total).unwrap_or(0);
        Response::from_json(&CountResponse { count })
    }

    /// Port of `MailboxDO.findThreadBySubject` (TS:742-784). Returns the
    /// matched `thread_id` as a JSON string, or `null`.
    async fn rpc_find_thread_by_subject(&self, req: &mut Request) -> Result<Response> {
        let args: FindThreadBySubjectArgs = req.json().await?;
        let normalized = normalize_subject(&args.subject);
        if normalized.is_empty() {
            return Response::from_json(&serde_json::Value::Null);
        }

        let sql_storage = self.state.storage().sql();
        let rows: Vec<ThreadCandidateRow> = sql_storage
            .exec(
                "SELECT thread_id, subject,
                        GROUP_CONCAT(DISTINCT LOWER(sender)) as senders,
                        GROUP_CONCAT(DISTINCT LOWER(recipient)) as recipients
                 FROM emails
                 WHERE thread_id IS NOT NULL
                   AND thread_id != id
                   AND date >= datetime('now', '-7 days')
                 GROUP BY thread_id
                 ORDER BY MAX(date) DESC
                 LIMIT 50",
                None,
            )?
            .to_array()?;

        let sender_lc = args
            .sender_address
            .as_deref()
            .map(|s| s.trim().to_lowercase());

        for row in rows {
            let row_subject = row.subject.unwrap_or_default();
            let row_norm = normalize_subject(&row_subject);
            if row_norm != normalized {
                continue;
            }
            if let Some(ref s) = sender_lc {
                if s.is_empty() {
                    return Response::from_json(&serde_json::Value::String(row.thread_id));
                }
                let senders = row.senders.unwrap_or_default();
                let recipients = row.recipients.unwrap_or_default();
                let all = format!("{senders},{recipients}");
                if !all.contains(s.as_str()) {
                    continue;
                }
            }
            return Response::from_json(&serde_json::Value::String(row.thread_id));
        }
        Response::from_json(&serde_json::Value::Null)
    }

    /// Port of `MailboxDO.checkSendRateLimit` (TS:793-817). Caller body
    /// is empty (`{}`); we don't bother parsing it. Limits: 20/hour,
    /// 100/day per mailbox.
    async fn rpc_check_send_rate_limit(&self, _req: &mut Request) -> Result<Response> {
        let sql_storage = self.state.storage().sql();

        let hour_rows: Vec<CountSqlRow> = sql_storage
            .exec(
                "SELECT COUNT(*) as total FROM emails
                 WHERE folder_id = 'sent'
                   AND date >= datetime('now', '-1 hour')",
                None,
            )?
            .to_array()?;
        if hour_rows.first().map(|r| r.total).unwrap_or(0) >= 20 {
            return Response::from_json(&serde_json::json!({
                "error": "Rate limit exceeded: max 20 emails per hour per mailbox"
            }));
        }

        let day_rows: Vec<CountSqlRow> = sql_storage
            .exec(
                "SELECT COUNT(*) as total FROM emails
                 WHERE folder_id = 'sent'
                   AND date >= datetime('now', '-1 day')",
                None,
            )?
            .to_array()?;
        if day_rows.first().map(|r| r.total).unwrap_or(0) >= 100 {
            return Response::from_json(&serde_json::json!({
                "error": "Rate limit exceeded: max 100 emails per day per mailbox"
            }));
        }

        Response::from_json(&serde_json::json!({ "ok": true }))
    }
}

// ── Search helpers (shared by /rpc/search_emails + /rpc/count_search_results) ─

/// Port of TS `#buildSearchConditions` (TS:658-695). Builds a `WHERE …`
/// clause + binding vector. `prefix` is the table alias (`"e"` for the
/// joined query, `""` for the bare COUNT). All bindings use unnumbered
/// `?` (CF SqlStorage binds positionally).
fn build_search_conditions(
    args: &SearchEmailsArgs,
    table_alias: &str,
) -> (String, Vec<SqlStorageValue>) {
    let prefix = if table_alias.is_empty() {
        String::new()
    } else {
        format!("{table_alias}.")
    };
    let mut conditions: Vec<String> = Vec::new();
    let mut bindings: Vec<SqlStorageValue> = Vec::new();

    if let Some(q) = args.query.as_deref().filter(|s| !s.is_empty()) {
        let like = format!("%{q}%");
        // 6 OR-arms (subject, body, sender, recipient, cc, bcc). TS reuses
        // the same param for cc/bcc; we keep the bindings 1:1 with the
        // placeholders for clarity.
        conditions.push(format!(
            "({prefix}subject LIKE ? OR {prefix}body LIKE ? OR {prefix}sender LIKE ? OR {prefix}recipient LIKE ? OR {prefix}cc LIKE ? OR {prefix}bcc LIKE ?)"
        ));
        for _ in 0..6 {
            bindings.push(like.as_str().into());
        }
    }
    if let Some(f) = args.folder.as_deref().filter(|s| !s.is_empty()) {
        conditions.push(format!(
            "{prefix}folder_id = (SELECT id FROM folders WHERE name = ? OR id = ? LIMIT 1)"
        ));
        bindings.push(f.into());
        bindings.push(f.into());
    }
    if let Some(from) = args.from.as_deref().filter(|s| !s.is_empty()) {
        conditions.push(format!("{prefix}sender LIKE ?"));
        bindings.push(format!("%{from}%").into());
    }
    if let Some(to) = args.to.as_deref().filter(|s| !s.is_empty()) {
        let like = format!("%{to}%");
        conditions.push(format!(
            "({prefix}recipient LIKE ? OR {prefix}cc LIKE ? OR {prefix}bcc LIKE ?)"
        ));
        bindings.push(like.as_str().into());
        bindings.push(like.as_str().into());
        bindings.push(like.into());
    }
    if let Some(subj) = args.subject.as_deref().filter(|s| !s.is_empty()) {
        conditions.push(format!("{prefix}subject LIKE ?"));
        bindings.push(format!("%{subj}%").into());
    }
    if let Some(d) = args.date_start.as_deref().filter(|s| !s.is_empty()) {
        conditions.push(format!("{prefix}date >= ?"));
        bindings.push(d.into());
    }
    if let Some(d) = args.date_end.as_deref().filter(|s| !s.is_empty()) {
        conditions.push(format!("{prefix}date <= ?"));
        bindings.push(d.into());
    }
    if let Some(r) = args.is_read {
        conditions.push(format!("{prefix}read = ?"));
        bindings.push((if r { 1i64 } else { 0i64 }).into());
    }
    if let Some(s) = args.is_starred {
        conditions.push(format!("{prefix}starred = ?"));
        bindings.push((if s { 1i64 } else { 0i64 }).into());
    }
    if let Some(true) = args.has_attachment {
        conditions.push(format!(
            "{prefix}id IN (SELECT DISTINCT email_id FROM attachments)"
        ));
    }

    let clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };
    (clause, bindings)
}

/// Strip leading reply/forward prefixes (case-insensitive, repeatable),
/// trim, and lowercase. Mirrors the TS regex
/// `/^(?:(?:re|fwd?|fw|aw|wg|r[eé]f|sv)\s*:\s*)+/i`.
fn normalize_subject(input: &str) -> String {
    let prefixes = ["re", "fwd", "fw", "aw", "wg", "ref", "réf", "sv"];
    let mut s = input.trim().to_string();
    loop {
        let lower = s.to_lowercase();
        let mut matched = false;
        for p in &prefixes {
            if lower.starts_with(p) {
                // Skip the prefix bytes (all ASCII except "réf" — handle UTF-8 char count).
                let prefix_char_count = p.chars().count();
                let rest_after_prefix: String = s.chars().skip(prefix_char_count).collect();
                let trimmed = rest_after_prefix.trim_start();
                if let Some(after_colon) = trimmed.strip_prefix(':') {
                    s = after_colon.trim_start().to_string();
                    matched = true;
                    break;
                }
            }
        }
        if !matched {
            break;
        }
    }
    s.trim().to_lowercase()
}

// ── Demo seeder ───────────────────────────────────────────────────────

struct DemoRow {
    id: &'static str,
    folder: &'static str,
    subject: &'static str,
    sender: &'static str,
    /// Hours before `now` the row should be backdated to.
    hours_ago: i64,
    body: &'static str,
    read: bool,
    starred: bool,
    thread_id: Option<&'static str>,
    in_reply_to: Option<&'static str>,
}

fn demo_rows() -> Vec<DemoRow> {
    vec![
        DemoRow {
            id: "demo-01-quarterly-review",
            folder: "inbox",
            subject: "Re: Q4 planning sync — agenda draft",
            sender: "founders@upstream.co",
            hours_ago: 1,
            body: "Hey — pulled together a draft agenda for tomorrow:\n\n1. Pipeline review (15 min)\n2. Hiring update (10 min)\n3. Open metric: north-star MoM growth\n\nThoughts before I send it to the wider list?\n\n— Sam",
            read: false,
            starred: true,
            thread_id: Some("thread-quarterly"),
            in_reply_to: None,
        },
        DemoRow {
            id: "demo-02-github-pr",
            folder: "inbox",
            subject: "[postr] PR #42 ready for review — folder picker",
            sender: "notifications@github.com",
            hours_ago: 2,
            body: "huseynsnmz opened pull request #42:\n\n  feat(tui): folder filter (/folder) + All-mailboxes row\n\n5 commits · 332 additions · 65 deletions\n\nReview at https://github.com/huseynsnmz/postr/pull/42",
            read: false,
            starred: false,
            thread_id: None,
            in_reply_to: None,
        },
        DemoRow {
            id: "demo-03-yc-friday",
            folder: "inbox",
            subject: "Office hours this Friday?",
            sender: "alex@yc.com",
            hours_ago: 4,
            body: "Hi — saw your demo from last batch dinner, would love to hear how things are going. Open slot Friday 11-12 PT if you can make it.\n\nAlex",
            read: false,
            starred: false,
            thread_id: None,
            in_reply_to: None,
        },
        DemoRow {
            id: "demo-04-stripe-invoice",
            folder: "inbox",
            subject: "Your invoice from Stripe (paid · $243.51)",
            sender: "billing@stripe.com",
            hours_ago: 7,
            body: "Receipt for June\n\nPlan: Business · $243.51\nPeriod: Jun 1 – Jun 30, 2026\n\nInvoice attached as PDF.",
            read: false,
            starred: false,
            thread_id: None,
            in_reply_to: None,
        },
        DemoRow {
            id: "demo-05-mira-followup",
            folder: "inbox",
            subject: "thanks for the intro 🙏",
            sender: "mira@parable.dev",
            hours_ago: 10,
            body: "Really appreciate the intro to Patrick — chat was great, lots to chew on. Let me know whenever you want that coffee.\n\n— M",
            read: true,
            starred: false,
            thread_id: None,
            in_reply_to: None,
        },
        DemoRow {
            id: "demo-06-quarterly-followup",
            folder: "inbox",
            subject: "Re: Q4 planning sync — agenda draft",
            sender: "priya@upstream.co",
            hours_ago: 11,
            body: "+1 to Sam's draft — also worth a 5-min slot for the new pricing experiment at the end?",
            read: true,
            starred: false,
            thread_id: Some("thread-quarterly"),
            in_reply_to: Some("demo-01-quarterly-review"),
        },
        DemoRow {
            id: "demo-07-cf-status",
            folder: "inbox",
            subject: "Resolved: Cloudflare Workers — elevated error rates",
            sender: "status@cloudflarestatus.com",
            hours_ago: 22,
            body: "Incident resolved at 18:42 UTC. Workers error rates returned to normal levels. We'll post a full RCA within 72h.",
            read: true,
            starred: false,
            thread_id: None,
            in_reply_to: None,
        },
        DemoRow {
            id: "demo-08-cron",
            folder: "inbox",
            subject: "Daily digest · 6 new repos starred",
            sender: "digest@github.com",
            hours_ago: 26,
            body: "Top: tokio-rs/tokio (+1.2k), cloudflare/workers-rs (+243), ratatui/ratatui (+88) …",
            read: true,
            starred: false,
            thread_id: None,
            in_reply_to: None,
        },
        DemoRow {
            id: "demo-09-payroll",
            folder: "inbox",
            subject: "Action needed: confirm direct deposit by Friday",
            sender: "support@gusto.com",
            hours_ago: 36,
            body: "We weren't able to verify your bank routing — please re-confirm at https://gusto.com/banking before Friday so the payroll run isn't delayed.",
            read: false,
            starred: true,
            thread_id: None,
            in_reply_to: None,
        },
        DemoRow {
            id: "demo-10-conf-cfp",
            folder: "inbox",
            subject: "RustConf 2026 CFP closes in 5 days",
            sender: "cfp@rustconf.com",
            hours_ago: 48,
            body: "Reminder: the CFP closes Friday at midnight UTC. Talks on async runtimes, embedded, and Wasm tooling are especially welcome this year.",
            read: false,
            starred: false,
            thread_id: None,
            in_reply_to: None,
        },
        DemoRow {
            id: "demo-11-quarterly-thirdmsg",
            folder: "inbox",
            subject: "Re: Q4 planning sync — agenda draft",
            sender: "founders@upstream.co",
            hours_ago: 60,
            body: "Perfect, added the pricing slot. Updated calendar invite incoming.\n\n— Sam",
            read: true,
            starred: false,
            thread_id: Some("thread-quarterly"),
            in_reply_to: Some("demo-06-quarterly-followup"),
        },
        DemoRow {
            id: "demo-12-newsletter",
            folder: "inbox",
            subject: "Issue #218 — five new tools every TUI dev should know",
            sender: "weekly@console.dev",
            hours_ago: 90,
            body: "This week: textual 0.55, gum recipes, ratatui-textarea, fzf-tab tips, and a tour of vhs for terminal recordings.",
            read: true,
            starred: false,
            thread_id: None,
            in_reply_to: None,
        },
        DemoRow {
            id: "demo-13-archive-onboarding",
            folder: "archive",
            subject: "Welcome to postr — get started",
            sender: "noreply@postr.dev",
            hours_ago: 360,
            body: "Press / to open the command popover. Try /summarize on any open thread, or /switch to flip between mailboxes.",
            read: true,
            starred: false,
            thread_id: None,
            in_reply_to: None,
        },
        DemoRow {
            id: "demo-14-archive-receipt",
            folder: "archive",
            subject: "Receipt — Anthropic API · $47.20",
            sender: "billing@anthropic.com",
            hours_ago: 720,
            body: "Charged $47.20 for May usage. Detailed breakdown attached.",
            read: true,
            starred: false,
            thread_id: None,
            in_reply_to: None,
        },
        DemoRow {
            id: "demo-15-sent-reply",
            folder: "sent",
            subject: "Re: thanks for the intro 🙏",
            sender: "" /* filled below */,
            hours_ago: 9,
            body: "Anytime — really glad it landed. Coffee next week?",
            read: true,
            starred: false,
            thread_id: None,
            in_reply_to: None,
        },
        DemoRow {
            id: "demo-16-drafts-rfc",
            folder: "draft",
            subject: "Draft · proposing the multi-account flow",
            sender: "" /* filled below */,
            hours_ago: 17,
            body: "WIP — pasting the design doc inline so I can keep iterating without leaving the buffer…",
            read: true,
            starred: false,
            thread_id: None,
            in_reply_to: None,
        },
        DemoRow {
            id: "demo-17-trash-recruiter",
            folder: "trash",
            subject: "[recruiter] Senior infra role at MetaSuper",
            sender: "ann@meta-recruit.io",
            hours_ago: 240,
            body: "Saw your GitHub — would you be open to a quick chat about a senior infra role?",
            read: false,
            starred: false,
            thread_id: None,
            in_reply_to: None,
        },
        DemoRow {
            id: "demo-18-trash-promo",
            folder: "trash",
            subject: "Last day · 40% off annual",
            sender: "promo@dropletco.com",
            hours_ago: 360,
            body: "Final hours to lock in 40% off your annual plan…",
            read: true,
            starred: false,
            thread_id: None,
            in_reply_to: None,
        },
    ]
}

fn insert_demo_email(
    sql: &SqlStorage,
    r: &DemoRow,
    recipient: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    let date = (now - chrono::Duration::hours(r.hours_ago))
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string();
    let read_val: i64 = if r.read { 1 } else { 0 };
    let starred_val: i64 = if r.starred { 1 } else { 0 };

    // Sent and Drafts rows belong to the user — override sender when the
    // fixture leaves it blank so the inbox column reads correctly.
    let sender: String = if matches!(r.folder, "sent" | "draft") && r.sender.is_empty() {
        recipient.to_string()
    } else {
        r.sender.to_string()
    };
    // Reciprocal: outgoing rows go to someone else; inbox/archive/trash
    // rows are addressed to the user. Use a sensible demo destination so
    // the reading view's "To" line isn't empty.
    let row_recipient: String = match r.folder {
        "sent" => "team@upstream.co".to_string(),
        "draft" => "founders@upstream.co".to_string(),
        _ => recipient.to_string(),
    };

    let in_reply_to: SqlStorageValue = match r.in_reply_to {
        Some(s) => s.into(),
        None => SqlStorageValue::Null,
    };
    let thread_id: SqlStorageValue = match r.thread_id {
        Some(s) => s.into(),
        None => SqlStorageValue::Null,
    };

    sql.exec(
        "INSERT INTO emails
            (id, folder_id, subject, sender, recipient, cc, bcc, date,
             read, starred, body, in_reply_to, email_references,
             thread_id, message_id, raw_headers)
         VALUES (?, ?, ?, ?, ?, NULL, NULL, ?, ?, ?, ?, ?, NULL, ?, NULL, NULL)",
        Some(vec![
            r.id.into(),
            r.folder.into(),
            r.subject.into(),
            sender.as_str().into(),
            row_recipient.as_str().into(),
            date.as_str().into(),
            read_val.into(),
            starred_val.into(),
            r.body.into(),
            in_reply_to,
            thread_id,
        ]),
    )?;
    Ok(())
}
