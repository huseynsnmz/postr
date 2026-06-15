//! Mailbox-scoped REST endpoints: emails, threads, drafts, flag updates,
//! folder moves. Maps 1:1 to the Worker routes mounted in
//! `worker/workers/index.ts`.
//!
//! Paths use camelCase IDs (`mailboxId`, `emailId`, `threadId`) because that
//! is how Hono's `c.req.param` keys are referenced — but on the wire the
//! values are simple `:segment` placeholders, so any string works.

use serde::Deserialize;
use serde_json::json;

use super::{
    ApiClient, ApiError, CliMailbox, DraftInput, DraftResponse, EmailFull, EmailList, EmailMeta,
    MoveEmailBody, ThreadFull, UpdateEmailFlags,
};

#[derive(Debug, Deserialize)]
struct SendResponse {
    #[allow(dead_code)]
    pub id: String,
    #[allow(dead_code)]
    pub status: String,
}

impl ApiClient {
    // ── Mailboxes ──────────────────────────────────────────────

    /// List mailboxes via `/cli/me` (no separate listing endpoint is needed —
    /// the `me` response is the source of truth for the active CLI session).
    pub async fn list_mailboxes(&self) -> Result<Vec<CliMailbox>, ApiError> {
        Ok(self.me().await?.mailboxes)
    }

    // ── Emails: list / get ─────────────────────────────────────

    /// Convenience: inbox is folder=inbox, page=1.
    pub async fn get_inbox(&self, mailbox_id: &str) -> Result<Vec<EmailMeta>, ApiError> {
        self.get_emails(mailbox_id, "inbox", 1).await
    }

    /// Like [`get_inbox`] but returns the full envelope so the caller can
    /// surface `totalCount` for the "…N more" line.
    pub async fn get_inbox_list(&self, mailbox_id: &str) -> Result<EmailList, ApiError> {
        let path = format!(
            "/api/v1/mailboxes/{}/emails?folder=inbox&page=1",
            urlencoding(mailbox_id),
        );
        self.get_json(&path).await
    }

    pub async fn get_emails(
        &self,
        mailbox_id: &str,
        folder: &str,
        page: u32,
    ) -> Result<Vec<EmailMeta>, ApiError> {
        let path = format!(
            "/api/v1/mailboxes/{}/emails?folder={}&page={}",
            urlencoding(mailbox_id),
            urlencoding(folder),
            page
        );
        // When `folder` is set the Worker returns `{ emails, totalCount }`.
        let list: EmailList = self.get_json(&path).await?;
        Ok(list.emails)
    }

    pub async fn get_email(&self, mailbox_id: &str, email_id: &str) -> Result<EmailFull, ApiError> {
        let path = format!(
            "/api/v1/mailboxes/{}/emails/{}",
            urlencoding(mailbox_id),
            urlencoding(email_id),
        );
        self.get_json(&path).await
    }

    pub async fn get_thread(
        &self,
        mailbox_id: &str,
        thread_id: &str,
    ) -> Result<ThreadFull, ApiError> {
        let path = format!(
            "/api/v1/mailboxes/{}/threads/{}",
            urlencoding(mailbox_id),
            urlencoding(thread_id),
        );
        self.get_json(&path).await
    }

    // ── Folder moves: archive / trash / generic move ───────────

    pub async fn move_email(
        &self,
        mailbox_id: &str,
        email_id: &str,
        folder_id: &str,
    ) -> Result<(), ApiError> {
        let path = format!(
            "/api/v1/mailboxes/{}/emails/{}/move",
            urlencoding(mailbox_id),
            urlencoding(email_id),
        );
        self.post_no_response(
            &path,
            &MoveEmailBody {
                folder_id: folder_id.to_string(),
            },
        )
        .await
    }

    pub async fn archive(&self, mailbox_id: &str, email_id: &str) -> Result<(), ApiError> {
        self.move_email(mailbox_id, email_id, "archive").await
    }

    pub async fn trash(&self, mailbox_id: &str, email_id: &str) -> Result<(), ApiError> {
        self.move_email(mailbox_id, email_id, "trash").await
    }

    // ── Flag updates: read / star ──────────────────────────────

    pub async fn mark_read(
        &self,
        mailbox_id: &str,
        email_id: &str,
        read: bool,
    ) -> Result<EmailFull, ApiError> {
        let path = format!(
            "/api/v1/mailboxes/{}/emails/{}",
            urlencoding(mailbox_id),
            urlencoding(email_id),
        );
        self.put_json(
            &path,
            &UpdateEmailFlags {
                read: Some(read),
                starred: None,
            },
        )
        .await
    }

    pub async fn star(
        &self,
        mailbox_id: &str,
        email_id: &str,
        on: bool,
    ) -> Result<EmailFull, ApiError> {
        let path = format!(
            "/api/v1/mailboxes/{}/emails/{}",
            urlencoding(mailbox_id),
            urlencoding(email_id),
        );
        self.put_json(
            &path,
            &UpdateEmailFlags {
                read: None,
                starred: Some(on),
            },
        )
        .await
    }

    /// Hard delete (the Worker also cleans up R2 attachment blobs).
    pub async fn delete_email(&self, mailbox_id: &str, email_id: &str) -> Result<(), ApiError> {
        let path = format!(
            "/api/v1/mailboxes/{}/emails/{}",
            urlencoding(mailbox_id),
            urlencoding(email_id),
        );
        self.delete(&path).await
    }

    // ── Threads ────────────────────────────────────────────────

    pub async fn mark_thread_read(
        &self,
        mailbox_id: &str,
        thread_id: &str,
    ) -> Result<(), ApiError> {
        let path = format!(
            "/api/v1/mailboxes/{}/threads/{}/read",
            urlencoding(mailbox_id),
            urlencoding(thread_id),
        );
        self.post_empty::<serde_json::Value>(&path)
            .await
            .map(|_| ())
    }

    // ── Drafts ─────────────────────────────────────────────────

    pub async fn save_draft(
        &self,
        mailbox_id: &str,
        draft: &DraftInput,
    ) -> Result<DraftResponse, ApiError> {
        let path = format!("/api/v1/mailboxes/{}/drafts", urlencoding(mailbox_id));
        self.post_json(&path, draft).await
    }

    /// Send a previously-saved draft.
    ///
    /// TODO(worker:worker/workers/index.ts:271): the Worker does not expose
    /// a "send this draft id" route — `POST /emails/:id/reply` and
    /// `POST /emails/:id/forward` take a full `SendEmailRequest` body and
    /// produce the sent row from that, then delete the draft row separately.
    /// For a draft created by the AI flow we need to resurrect its body and
    /// recipients first; that requires `get_email(draft_id)` then `POST
    /// /reply` with the resolved fields. Wire that up once the composer
    /// surface decides where the body resolution lives.
    pub async fn send_draft(&self, mailbox_id: &str, draft_id: &str) -> Result<(), ApiError> {
        let draft = self.get_email(mailbox_id, draft_id).await?;
        let to = draft
            .recipient
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(", ");
        let body = json!({
            "to": to,
            "from": draft.sender,
            "subject": draft.subject,
            "text": draft.body.unwrap_or_default(),
            "in_reply_to": draft.in_reply_to,
            "thread_id": draft.thread_id,
        });
        // If the draft has an in_reply_to, use the reply route; otherwise send fresh.
        let path = if let Some(original_id) = draft.in_reply_to.as_ref() {
            format!(
                "/api/v1/mailboxes/{}/emails/{}/reply",
                urlencoding(mailbox_id),
                urlencoding(original_id),
            )
        } else {
            format!("/api/v1/mailboxes/{}/emails", urlencoding(mailbox_id))
        };
        let _resp: SendResponse = self.post_json(&path, &body).await?;
        // Drop the draft row now that the sent row exists.
        let _ = self.delete_email(mailbox_id, draft_id).await;
        Ok(())
    }
}

/// Minimal URL component escaper for path segments. The IDs the Worker
/// hands us are UUIDs and email addresses; only the email case actually
/// needs escaping (the `@`). We keep this dependency-free.
fn urlencoding(s: &str) -> String {
    // RFC 3986 path segment: pchar = unreserved / pct-encoded / sub-delims / ":" / "@"
    // We preserve unreserved + the chars that appear in email addresses (which
    // are the dominant path-segment ID shape here): @, +. Other sub-delims stay
    // encoded for safety.
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'@' | b'+' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
