//! `/api/v1/cli/me` + mailbox CRUD against the Worker.

use serde::Serialize;
use serde_json::json;

use super::{ApiClient, ApiError, CliMailbox, CliMe};

impl ApiClient {
    pub async fn me(&self) -> Result<CliMe, ApiError> {
        self.get_json("/api/v1/cli/me").await
    }

    /// Idempotent — creating the same address twice returns the existing one.
    /// `display_name` and `alias` are only honored on the *first* create call;
    /// later updates must go through [`update_mailbox`].
    pub async fn create_mailbox(
        &self,
        address: &str,
        display_name: Option<&str>,
        alias: Option<&str>,
    ) -> Result<CliMailbox, ApiError> {
        #[derive(Serialize)]
        struct Body<'a> {
            address: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            display_name: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            alias: Option<&'a str>,
        }
        self.post_json(
            "/api/v1/cli/mailboxes",
            &Body {
                address,
                display_name,
                alias,
            },
        )
        .await
    }

    /// Tri-state on every field:
    ///   * `Some(Some(s))` — set the value
    ///   * `Some(None)`    — clear the value
    ///   * `None`          — leave unchanged
    pub async fn update_mailbox(
        &self,
        address: &str,
        display_name: Option<Option<&str>>,
        alias: Option<Option<&str>>,
    ) -> Result<CliMailbox, ApiError> {
        let mut body = serde_json::Map::new();
        match display_name {
            Some(Some(name)) => {
                body.insert("display_name".into(), json!(name));
            }
            Some(None) => {
                body.insert("display_name".into(), serde_json::Value::Null);
            }
            None => {}
        }
        match alias {
            Some(Some(a)) => {
                body.insert("alias".into(), json!(a));
            }
            Some(None) => {
                body.insert("alias".into(), serde_json::Value::Null);
            }
            None => {}
        }
        let path = format!("/api/v1/cli/mailboxes/{}", mailbox_path_segment(address));
        self.put_json(&path, &serde_json::Value::Object(body)).await
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
