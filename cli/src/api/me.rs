//! `/api/v1/cli/me` — bearer-token whoami.

use super::{ApiClient, ApiError, CliMe};

impl ApiClient {
    pub async fn me(&self) -> Result<CliMe, ApiError> {
        self.get_json("/api/v1/cli/me").await
    }
}
