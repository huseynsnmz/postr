//! `GET /api/v1/cli/me` — bearer-auth whoami.
//!
//! Port of worker/workers/routes/cli.ts:14-22. Mailbox list now comes
//! from R2 (the per-DO SQLite is per-mailbox so it can't be the source
//! of truth for "all mailboxes") — same source as the TS worker.

use serde::Deserialize;
use worker::*;

use crate::auth::{auth_error_response, check_auth};
use crate::mailbox::{self, MailboxRecord};
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

fn err_response(status: u16, error: &str) -> Result<Response> {
    Ok(Response::from_json(&serde_json::json!({ "error": error }))?.with_status(status))
}

fn brief(rec: &MailboxRecord) -> MailboxBrief {
    MailboxBrief {
        id: rec.address.clone(),
        address: rec.address.clone(),
        display_name: rec.display_name.clone(),
        alias: rec.alias.clone(),
    }
}

fn normalize_display_name(raw: Option<String>) -> Option<String> {
    raw.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn normalize_alias(raw: Option<String>) -> Option<String> {
    raw.map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
}

/// `POST /api/v1/cli/mailboxes` — idempotent mailbox creation.
///
/// Writes `mailboxes/{address}.json` to R2 if the key doesn't already exist.
/// The body shape is `MailboxRecord` — `address` plus optional `display_name`
/// for outbound `From:` personalization. Returns the new (or existing) mailbox.
pub async fn create_mailbox(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if let Err(e) = check_auth(&req, &ctx.env).await {
        return auth_error_response(e);
    }

    #[derive(Deserialize)]
    struct Body {
        address: String,
        #[serde(default)]
        display_name: Option<String>,
        #[serde(default)]
        alias: Option<String>,
    }

    let body: Body = match req.json().await {
        Ok(b) => b,
        Err(_) => return err_response(400, "invalid_json"),
    };

    let address = body.address.trim().to_lowercase();
    if address.is_empty() || !address.contains('@') {
        return err_response(400, "invalid_address");
    }

    let existing = mailbox::load_record(&ctx.env, &address).await?;
    let (rec, status) = match existing {
        Some(rec) => (rec, 200),
        None => (
            MailboxRecord {
                address: address.clone(),
                display_name: normalize_display_name(body.display_name),
                alias: normalize_alias(body.alias),
            },
            201,
        ),
    };
    if status == 201 {
        mailbox::save_record(&ctx.env, &rec).await?;
    }

    Ok(Response::from_json(&brief(&rec))?.with_status(status))
}

/// `PUT /api/v1/cli/mailboxes/:mailboxId` — partial update of a mailbox.
///
/// Currently only `display_name` is mutable — addresses can't be renamed
/// (the address is the R2 key and the DO id). Pass `display_name: null` to
/// clear it.
pub async fn update_mailbox(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if let Err(e) = check_auth(&req, &ctx.env).await {
        return auth_error_response(e);
    }
    let Some(mailbox_id) = ctx.param("mailboxId").cloned() else {
        return err_response(400, "missing_mailbox_id");
    };

    #[derive(Deserialize)]
    struct Body {
        // Tri-state: omit → leave unchanged; `null` → clear; string → set.
        #[serde(default, deserialize_with = "deserialize_some")]
        display_name: Option<Option<String>>,
        #[serde(default, deserialize_with = "deserialize_some")]
        alias: Option<Option<String>>,
    }

    let body: Body = match req.json().await {
        Ok(b) => b,
        Err(_) => return err_response(400, "invalid_json"),
    };

    let Some(mut rec) = mailbox::load_record(&ctx.env, &mailbox_id).await? else {
        return err_response(404, "mailbox_not_found");
    };

    if let Some(field) = body.display_name {
        rec.display_name = normalize_display_name(field);
    }
    if let Some(field) = body.alias {
        rec.alias = normalize_alias(field);
    }
    mailbox::save_record(&ctx.env, &rec).await?;
    Response::from_json(&brief(&rec))
}

/// `DELETE /api/v1/cli/mailboxes/:mailboxId` — remove the R2 marker.
///
/// The Durable Object's stored data is left intact. The DO is addressed by
/// `idFromName(address)`, so re-creating the mailbox later resurrects the
/// same SQLite contents — which is the behavior the user usually wants when
/// "delete and re-add" is really "fix a typo in display_name".
pub async fn delete_mailbox(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if let Err(e) = check_auth(&req, &ctx.env).await {
        return auth_error_response(e);
    }
    let Some(mailbox_id) = ctx.param("mailboxId").cloned() else {
        return err_response(400, "missing_mailbox_id");
    };
    if mailbox::load_record(&ctx.env, &mailbox_id).await?.is_none() {
        return err_response(404, "mailbox_not_found");
    }
    mailbox::delete_record(&ctx.env, &mailbox_id).await?;
    Response::empty().map(|r| r.with_status(204))
}

/// Distinguishes "field missing" from "field set to null" during JSON
/// deserialization — needed for tri-state PATCH-style updates.
fn deserialize_some<'de, T, D>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    T: serde::Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    T::deserialize(deserializer).map(Some)
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
