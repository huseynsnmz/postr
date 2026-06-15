//! These methods target endpoints the Rust worker port deliberately omitted.
//! They will 404 against the Rust worker. Kept here for future use if the
//! agent (WebSocket) interface is re-introduced. Safe to delete in v2.
//!
//! Agent endpoints: history (GET/DELETE) and SSE chat stream (POST).

use std::pin::Pin;

use eventsource_stream::{Event, EventStreamError, Eventsource};
use futures_util::{Stream, StreamExt};

use super::{AgentHistory, AgentStreamRequest, ApiClient, ApiError};

/// Boxed SSE event stream — one event per `data:` frame from the Worker's
/// `/agent/stream` SSE wrapper. The transport already filters out
/// `: keepalive` comment frames.
pub type AgentEventStream = Pin<Box<dyn Stream<Item = Result<Event, ApiError>> + Send + 'static>>;

impl ApiClient {
    pub async fn agent_history(&self, mailbox_id: &str) -> Result<AgentHistory, ApiError> {
        let path = format!("/api/v1/mailboxes/{}/agent/history", url_escape(mailbox_id));
        self.get_json(&path).await
    }

    pub async fn agent_history_clear(&self, mailbox_id: &str) -> Result<(), ApiError> {
        let path = format!("/api/v1/mailboxes/{}/agent/history", url_escape(mailbox_id));
        self.delete(&path).await
    }

    /// Open a streaming connection to the Worker's SSE bridge. The bridge
    /// opens a WebSocket to the EmailAgent DO upstream and re-emits each
    /// frame as `data: <json>\n\n` (see
    /// `worker/workers/routes/agent-stream.ts`).
    pub async fn agent_stream(
        &self,
        mailbox_id: &str,
        message: &str,
    ) -> Result<AgentEventStream, ApiError> {
        let path = format!("/api/v1/mailboxes/{}/agent/stream", url_escape(mailbox_id));
        let resp = self
            .auth(
                self.http()
                    .post(self.url(&path))
                    .header(reqwest::header::ACCEPT, "text/event-stream")
                    .json(&AgentStreamRequest { message }),
            )
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(match status {
                reqwest::StatusCode::UNAUTHORIZED => ApiError::Unauthorized,
                reqwest::StatusCode::NOT_FOUND => ApiError::NotFound,
                s => ApiError::Server(s.as_u16(), body),
            });
        }

        let stream = resp
            .bytes_stream()
            .eventsource()
            .map(|res| res.map_err(map_sse_error));
        Ok(Box::pin(stream))
    }
}

fn map_sse_error(e: EventStreamError<reqwest::Error>) -> ApiError {
    match e {
        EventStreamError::Transport(t) => ApiError::Network(t),
        EventStreamError::Parser(p) => ApiError::Decode(p.to_string()),
        EventStreamError::Utf8(u) => ApiError::Decode(u.to_string()),
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
