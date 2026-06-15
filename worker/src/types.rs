//! Response shapes for the `/api/v1/*` routes implemented by this worker.
//!
//! Wire format mirrors the TS Hono worker that the CLI client was built
//! against (see ../../cli/src/api/types.rs).
//! The email row shape is the legacy snake_case form (`read`, `starred`,
//! `thread_id`, `in_reply_to`, ...). The list envelope uses `totalCount`
//! (camelCase). The CLI mailbox brief uses `address` rather than `email`.
//!
//! Casing is therefore mixed and deliberate — `#[serde(rename_all = ...)]`
//! at the struct level would corrupt one camp or the other, so we don't.

use serde::Serialize;

// ── /api/v1/cli/me ─────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct CliMeResponse {
    pub email: String,
    pub mailboxes: Vec<MailboxBrief>,
}

#[derive(Debug, Serialize)]
pub struct MailboxBrief {
    pub id: String,
    /// CLI client expects `address` (not `email`) — see
    /// cli/src/api/types.rs::CliMailbox.
    pub address: String,
    /// Personal name to attach to outbound `From:` headers, e.g.
    /// `"Hüseyin Sönmez" <me@x.com>`. None ⇒ bare address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

// ── Email rows (list + single) ─────────────────────────────────────

/// Light row used by the list endpoint. Mirrors the projection in
/// worker/workers/durableObject/index.ts#getEmails (lines 148–170).
#[derive(Debug, Serialize)]
pub struct EmailMeta {
    pub id: String,
    pub subject: Option<String>,
    pub sender: Option<String>,
    pub recipient: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bcc: Option<String>,
    pub date: Option<String>,
    /// SQLite stores 0/1, but the TS handler coerces to bool before
    /// serialising (`!!email.read`). We follow the same wire format.
    pub read: bool,
    pub starred: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email_references: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder_id: Option<String>,
    /// `SUBSTR(body, 1, 300)` — only present on list rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
}

/// Full row returned by `GET /emails/:id` and `GET /threads/:threadId`.
#[derive(Debug, Serialize)]
pub struct EmailFull {
    pub id: String,
    pub subject: Option<String>,
    pub sender: Option<String>,
    pub recipient: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bcc: Option<String>,
    pub date: Option<String>,
    pub read: bool,
    pub starred: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email_references: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_headers: Option<String>,
    pub attachments: Vec<AttachmentMeta>,
}

#[derive(Debug, Serialize)]
pub struct AttachmentMeta {
    pub id: String,
    pub filename: String,
    pub mimetype: String,
    pub size: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disposition: Option<String>,
}

/// `{ emails, totalCount }` envelope. `totalCount` is camelCase to
/// match the TS handler at worker/workers/index.ts:158,163.
///
/// Currently unused at the route layer — the emails handler emits the
/// envelope as an ad-hoc `serde_json::Value` because the DO RPC already
/// returns the inner array as a value. Kept as a typed reference for
/// the wire shape and for future use by a non-DO call path.
#[allow(dead_code)]
#[derive(Debug, Serialize)]
pub struct EmailList {
    pub emails: Vec<EmailMeta>,
    #[serde(rename = "totalCount")]
    pub total_count: u64,
}

/// Thread payload: `EmailFull[]` directly, no envelope. Matches
/// `worker/workers/index.ts:261` which returns `getThreadEmails`'s
/// result as-is. Same dead-code caveat as `EmailList`.
#[allow(dead_code)]
pub type ThreadFull = Vec<EmailFull>;
