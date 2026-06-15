//! `/api/v1/cli/me` + mailbox CRUD against the Worker.

use serde::Serialize;
use serde_json::json;

use super::{ApiClient, ApiError, CliMailbox, CliMe};

impl ApiClient {
    pub async fn me(&self) -> Result<CliMe, ApiError> {
        self.get_json("/api/v1/cli/me").await
    }

    /// Idempotent — creating the same address twice returns the existing one.
    /// `display_name` is only honored on the *first* create call; later updates
    /// must go through [`update_mailbox`].
    pub async fn create_mailbox(
        &self,
        address: &str,
        display_name: Option<&str>,
    ) -> Result<CliMailbox, ApiError> {
        #[derive(Serialize)]
        struct Body<'a> {
            address: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            display_name: Option<&'a str>,
        }
        self.post_json(
            "/api/v1/cli/mailboxes",
            &Body {
                address,
                display_name,
            },
        )
        .await
    }

    /// Tri-state on `display_name`:
    ///   * `Some(Some(s))` — set the name
    ///   * `Some(None)`    — clear the name
    ///   * `None`          — leave unchanged
    pub async fn update_mailbox(
        &self,
        address: &str,
        display_name: Option<Option<&str>>,
    ) -> Result<CliMailbox, ApiError> {
        let body = match display_name {
            Some(Some(name)) => json!({ "display_name": name }),
            Some(None) => json!({ "display_name": null }),
            None => json!({}),
        };
        let path = format!("/api/v1/cli/mailboxes/{}", mailbox_path_segment(address));
        self.put_json(&path, &body).await
    }

    pub async fn delete_mailbox(&self, address: &str) -> Result<(), ApiError> {
        let path = format!("/api/v1/cli/mailboxes/{}", mailbox_path_segment(address));
        self.delete(&path).await
    }
}

/// Same escape rules as `api/mailbox.rs::urlencoding` — duplicated here so
/// `me.rs` doesn't have to reach into a sibling. Email addresses keep `@`
/// and `+` un-encoded; everything else outside RFC 3986 unreserved is
/// percent-escaped.
fn mailbox_path_segment(s: &str) -> String {
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
