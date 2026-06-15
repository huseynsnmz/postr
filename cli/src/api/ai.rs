//! AI slash-command endpoints: `/summarize`, `/draft`, `/ask`, `/triage`.

use super::{
    AiDraftRequest, AiDraftResponse, ApiClient, ApiError, AskRequest, AskResponse,
    SummarizeRequest, SummarizeResponse, TriageResponse,
};

impl ApiClient {
    pub async fn summarize(
        &self,
        mailbox_id: &str,
        thread_id: &str,
    ) -> Result<SummarizeResponse, ApiError> {
        let path = format!("/api/v1/mailboxes/{}/summarize", url_escape(mailbox_id));
        self.post_json(&path, &SummarizeRequest { thread_id }).await
    }

    pub async fn draft(
        &self,
        mailbox_id: &str,
        prompt: &str,
        thread_id: Option<&str>,
    ) -> Result<AiDraftResponse, ApiError> {
        let path = format!("/api/v1/mailboxes/{}/draft", url_escape(mailbox_id));
        self.post_json(&path, &AiDraftRequest { prompt, thread_id })
            .await
    }

    pub async fn ask(&self, mailbox_id: &str, query: &str) -> Result<AskResponse, ApiError> {
        let path = format!("/api/v1/mailboxes/{}/ask", url_escape(mailbox_id));
        self.post_json(&path, &AskRequest { query }).await
    }

    /// `/triage` takes no body — the Worker scans the inbox itself.
    pub async fn triage(&self, mailbox_id: &str) -> Result<TriageResponse, ApiError> {
        let path = format!("/api/v1/mailboxes/{}/triage", url_escape(mailbox_id));
        self.post_json(&path, &serde_json::json!({})).await
    }
}

fn url_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
