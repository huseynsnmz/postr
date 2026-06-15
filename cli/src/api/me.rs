//! `/api/v1/cli/me` — bearer-token whoami.

use serde::Serialize;

use super::{ApiClient, ApiError, CliMailbox, CliMe};

impl ApiClient {
    pub async fn me(&self) -> Result<CliMe, ApiError> {
        self.get_json("/api/v1/cli/me").await
    }

    /// Idempotent — creating the same address twice returns the existing one.
    pub async fn create_mailbox(&self, address: &str) -> Result<CliMailbox, ApiError> {
        #[derive(Serialize)]
        struct Body<'a> {
            address: &'a str,
        }
        self.post_json("/api/v1/cli/mailboxes", &Body { address })
            .await
    }
}
