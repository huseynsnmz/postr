//! Shared request and response types for the Worker `/api/v1/*` surface.
//!
//! Field names track the Worker's wire format. The Worker is a TypeScript
//! Hono app that returns snake_case for the email row shape (`thread_id`,
//! `in_reply_to`, `is_read` is actually `read` etc.) and camelCase for the
//! newer CLI/AI routes (`mailboxes[].address`, `suggestedReplies`, ...).
//! We mirror each route's exact casing rather than forcing a single style.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── /api/v1/cli/me ─────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct CliMe {
    /// Primary email derived from `EMAIL_ADDRESSES` (or first mailbox).
    pub email: String,
    pub mailboxes: Vec<CliMailbox>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CliMailbox {
    pub id: String,
    /// Worker returns `address` here (not `email`).
    pub address: String,
    /// Personal name used in outbound `From:` headers, e.g.
    /// `"Hüseyin Sönmez" <me@x.com>`. None ⇒ bare address.
    #[serde(default)]
    pub display_name: Option<String>,
}

// ── /api/v1/mailboxes ──────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct Mailbox {
    pub id: String,
    pub email: String,
    pub name: String,
}

// ── Email row shapes ───────────────────────────────────────────────

/// Subset of `EmailMetadata` from `workers/lib/schemas.ts`.
/// Lifted to its own struct for list endpoints where bodies and headers
/// aren't returned.
#[derive(Debug, Clone, Deserialize)]
pub struct EmailMeta {
    pub id: String,
    pub subject: String,
    pub sender: String,
    pub recipient: String,
    #[serde(default)]
    pub cc: Option<String>,
    #[serde(default)]
    pub bcc: Option<String>,
    pub date: String,
    pub read: bool,
    pub starred: bool,
    #[serde(default)]
    pub in_reply_to: Option<String>,
    #[serde(default)]
    pub email_references: Option<String>,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub folder_id: Option<String>,
    #[serde(default)]
    pub snippet: Option<String>,
}

/// Full `EmailFull` shape — list rows extended with body + attachments meta.
#[derive(Debug, Clone, Deserialize)]
pub struct EmailFull {
    pub id: String,
    pub subject: String,
    pub sender: String,
    pub recipient: String,
    #[serde(default)]
    pub cc: Option<String>,
    #[serde(default)]
    pub bcc: Option<String>,
    pub date: String,
    pub read: bool,
    pub starred: bool,
    #[serde(default)]
    pub in_reply_to: Option<String>,
    #[serde(default)]
    pub email_references: Option<String>,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub folder_id: Option<String>,
    #[serde(default)]
    pub snippet: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub raw_headers: Option<String>,
    #[serde(default)]
    pub attachments: Vec<AttachmentInfo>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AttachmentInfo {
    pub id: String,
    pub filename: String,
    pub mimetype: String,
    pub size: u64,
    #[serde(default)]
    pub content_id: Option<String>,
    #[serde(default)]
    pub disposition: Option<String>,
}

/// Wrapper for paginated email list responses:
/// `{ emails: Email[], totalCount: number }`.
#[derive(Debug, Clone, Deserialize)]
pub struct EmailList {
    pub emails: Vec<EmailMeta>,
    #[serde(rename = "totalCount", default)]
    pub total_count: u64,
}

// ── Threads ────────────────────────────────────────────────────────

/// `GET /threads/:threadId` returns `EmailFull[]` directly — no envelope.
pub type ThreadFull = Vec<EmailFull>;

// ── Drafts ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize)]
pub struct DraftInput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bcc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    pub body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// If set, the Worker deletes the old draft and creates a new one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub draft_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DraftResponse {
    pub id: String,
    pub status: String,
    pub subject: String,
    pub recipient: String,
    pub date: String,
}

// ── Flag updates / move ────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize)]
pub struct UpdateEmailFlags {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub starred: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MoveEmailBody {
    #[serde(rename = "folderId")]
    pub folder_id: String,
}

// ── Agent history ──────────────────────────────────────────────────

/// `AIChatAgent` persists `UIMessage[]`. The exact shape is not stable
/// across upstream SDK versions, so the content of each message is
/// kept as raw JSON.
#[derive(Debug, Clone, Deserialize)]
pub struct AgentMessage {
    #[serde(default)]
    pub id: Option<String>,
    pub role: String,
    /// `parts: UIMessagePart[]` — keep as `Value` until the TUI needs to
    /// branch on `type` (text, dynamic-tool, tool-<name>, state).
    #[serde(default)]
    pub parts: Option<Value>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

pub type AgentHistory = Vec<AgentMessage>;

// ── /summarize ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct SummarizeRequest<'a> {
    #[serde(rename = "threadId")]
    pub thread_id: &'a str,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SummarizeResponse {
    pub thread: SummarizeThread,
    pub summary: Vec<SummaryBullet>,
    #[serde(rename = "suggestedReplies", default)]
    pub suggested_replies: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SummarizeThread {
    pub subject: String,
    #[serde(rename = "messageCount")]
    pub message_count: u64,
    #[serde(rename = "peopleCount")]
    pub people_count: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SummaryBullet {
    pub text: String,
    #[serde(rename = "isActionItem", default)]
    pub is_action_item: bool,
}

// ── /draft ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct AiDraftRequest<'a> {
    pub prompt: &'a str,
    #[serde(rename = "threadId", skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<&'a str>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AiDraftResponse {
    pub to: String,
    pub subject: String,
    pub body: String,
}

// ── /ask ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct AskRequest<'a> {
    pub query: &'a str,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AskResponse {
    pub summary: String,
    pub results: Vec<AskResult>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AskResult {
    pub sender: String,
    pub subject: String,
    pub date: String,
    #[serde(rename = "threadId")]
    pub thread_id: String,
    #[serde(default)]
    pub glyph: Option<String>,
}

// ── /triage ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct TriageResponse {
    pub categories: Vec<TriageCategory>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TriageCategory {
    pub label: String,
    pub glyph: String,
    pub count: u64,
    #[serde(rename = "threadIds", default)]
    pub thread_ids: Vec<String>,
}

// ── /agent/stream request body ─────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct AgentStreamRequest<'a> {
    pub message: &'a str,
}
