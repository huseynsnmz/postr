//! `GET /api/v1/cli/me` — bearer-auth whoami.
//!
//! Port of worker/workers/routes/cli.ts:14-22. Mailbox list now comes
//! from R2 (the per-DO SQLite is per-mailbox so it can't be the source
//! of truth for "all mailboxes") — same source as the TS worker.

use serde::Deserialize;
use worker::*;

use crate::auth::{auth_error_response, check_auth};
use crate::mailbox;
use crate::types::{CliMeResponse, MailboxBrief};

pub async fn me(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if let Err(e) = check_auth(&req, &ctx.env).await {
        return auth_error_response(e);
    }

    let mailboxes = mailbox::list_mailboxes(&ctx.env).await?;

    // EMAIL_ADDRESSES is a JSON array in wrangler vars. workers-rs
    // surfaces non-string vars via `object_var`, falling back to `var`
    // for the legacy comma-separated form.
    let email = primary_email_from_env(&ctx.env).unwrap_or_else(|| {
        mailboxes
            .first()
            .map(|m| m.address.clone())
            .unwrap_or_default()
    });

    Response::from_json(&CliMeResponse { email, mailboxes })
}

/// `POST /api/v1/cli/mailboxes` — idempotent mailbox creation.
///
/// Writes `mailboxes/{address}.json` to R2 if the key doesn't already exist.
/// The file's content is just a marker — `require_mailbox` only checks
/// existence via HEAD. Returns the new (or existing) mailbox.
pub async fn create_mailbox(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if let Err(e) = check_auth(&req, &ctx.env).await {
        return auth_error_response(e);
    }

    #[derive(Deserialize)]
    struct Body {
        address: String,
    }

    let body: Body = match req.json().await {
        Ok(b) => b,
        Err(_) => {
            let mut r =
                Response::from_json(&serde_json::json!({"error": "invalid_json"}))?;
            r = r.with_status(400);
            return Ok(r);
        }
    };

    let address = body.address.trim().to_lowercase();
    if address.is_empty() || !address.contains('@') {
        let mut r = Response::from_json(&serde_json::json!({"error": "invalid_address"}))?;
        r = r.with_status(400);
        return Ok(r);
    }

    let bucket = ctx.env.bucket("BUCKET")?;
    let key = format!("mailboxes/{}.json", address);

    let already = bucket.head(&key).await?.is_some();
    if !already {
        let body = serde_json::json!({"address": address}).to_string();
        bucket.put(&key, body).execute().await?;
    }

    let mut r = Response::from_json(&MailboxBrief {
        id: address.clone(),
        address,
    })?;
    if !already {
        r = r.with_status(201);
    }
    Ok(r)
}

/// Pull the first entry of `EMAIL_ADDRESSES`. Tolerates either:
///   * `["a@x.com", "b@x.com"]`        — preferred (matches TS wrangler)
///   * `"a@x.com,b@x.com"`             — legacy
///
/// Returns `None` if absent / empty / unparseable.
fn primary_email_from_env(env: &Env) -> Option<String> {
    if let Ok(list) = env.object_var::<Vec<String>>("EMAIL_ADDRESSES") {
        return list.into_iter().find(|s| !s.is_empty());
    }
    let raw = env.var("EMAIL_ADDRESSES").ok()?.to_string();
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed
        .split(',')
        .map(|s| s.trim())
        .find(|s| !s.is_empty())
        .map(|s| s.to_string())
}
