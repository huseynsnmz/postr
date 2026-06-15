//! Base HTTP client targeting the Cloudflare Worker `/api/v1/*` surface.
//!
//! The Worker accepts a bearer token on every `/api/v1/*` route (the dual-auth
//! middleware landed in Phase 1 of the Worker work). This module owns the
//! reqwest client, URL joining, auth header injection, and error mapping.

use std::time::Duration;

use reqwest::{RequestBuilder, StatusCode};
use serde::{de::DeserializeOwned, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("unauthorized")]
    Unauthorized,
    #[error("not found")]
    NotFound,
    #[error("server error ({0}): {1}")]
    Server(u16, String),
    #[error(transparent)]
    Network(#[from] reqwest::Error),
    #[error("decode error: {0}")]
    Decode(String),
}

#[derive(Debug, Clone)]
pub struct ApiClient {
    base_url: String,
    token: String,
    http: reqwest::Client,
}

impl ApiClient {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            base_url: base_url.into(),
            token: token.into(),
            http,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    pub fn url(&self, path: &str) -> String {
        let base = self.base_url.trim_end_matches('/');
        if path.starts_with('/') {
            format!("{base}{path}")
        } else {
            format!("{base}/{path}")
        }
    }

    pub fn auth(&self, rb: RequestBuilder) -> RequestBuilder {
        rb.bearer_auth(&self.token)
    }

    pub async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, ApiError> {
        let resp = self.auth(self.http.get(self.url(path))).send().await?;
        decode_json(resp).await
    }

    pub async fn post_json<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, ApiError> {
        let resp = self
            .auth(self.http.post(self.url(path)).json(body))
            .send()
            .await?;
        decode_json(resp).await
    }

    pub async fn post_empty<T: DeserializeOwned>(&self, path: &str) -> Result<T, ApiError> {
        let resp = self.auth(self.http.post(self.url(path))).send().await?;
        decode_json(resp).await
    }

    pub async fn put_json<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, ApiError> {
        let resp = self
            .auth(self.http.put(self.url(path)).json(body))
            .send()
            .await?;
        decode_json(resp).await
    }

    /// POST a body and ignore the response (other than success/error).
    pub async fn post_no_response<B: Serialize + ?Sized>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<(), ApiError> {
        let resp = self
            .auth(self.http.post(self.url(path)).json(body))
            .send()
            .await?;
        ensure_ok(resp).await.map(|_| ())
    }

    pub async fn delete(&self, path: &str) -> Result<(), ApiError> {
        let resp = self.auth(self.http.delete(self.url(path))).send().await?;
        ensure_ok(resp).await.map(|_| ())
    }
}

async fn ensure_ok(resp: reqwest::Response) -> Result<reqwest::Response, ApiError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    match status {
        StatusCode::UNAUTHORIZED => Err(ApiError::Unauthorized),
        StatusCode::NOT_FOUND => Err(ApiError::NotFound),
        s => {
            let body = resp.text().await.unwrap_or_default();
            Err(ApiError::Server(s.as_u16(), body))
        }
    }
}

async fn decode_json<T: DeserializeOwned>(resp: reqwest::Response) -> Result<T, ApiError> {
    let resp = ensure_ok(resp).await?;
    let bytes = resp.bytes().await?;
    serde_json::from_slice::<T>(&bytes).map_err(|e| ApiError::Decode(e.to_string()))
}
