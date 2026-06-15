//! `POST /api/v1/mailboxes/:mailboxId/drafts` — port of
//! `worker/workers/index.ts:214-228`.
//!
//! Body shape (`DraftBody`, TS:34-43): all fields optional except `body`.
//! If `draft_id` is set we delete the prior draft first (mirrors
//! TS:218 — not atomic, but matches existing behaviour).
//!
//! Stores the draft in the `draft` folder via `MailboxDO::create_email`.

use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;
use worker::*;

use crate::auth::{auth_error_response, check_auth};
use crate::mailbox;
use crate::routes::emails::do_rpc_request;

fn bad_request(error: &str) -> Result<Response> {
    Ok(Response::from_json(&json!({ "error": error }))?.with_status(400))
}

fn not_found(error: &str) -> Result<Response> {
    Ok(Response::from_json(&json!({ "error": error }))?.with_status(404))
}

#[derive(Deserialize)]
struct DraftBody {
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    cc: Option<String>,
    #[serde(default)]
    bcc: Option<String>,
    #[serde(default)]
    subject: Option<String>,
    body: String,
    #[serde(default)]
    in_reply_to: Option<String>,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    draft_id: Option<String>,
}

pub async fn create(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if let Err(e) = check_auth(&req, &ctx.env).await {
        return auth_error_response(e);
    }
    let Some(mailbox_id) = ctx.param("mailboxId").cloned() else {
        return bad_request("missing mailboxId");
    };
    if !mailbox::require_mailbox(&ctx.env, &mailbox_id).await? {
        return not_found("Not found");
    }
    let body: DraftBody = req.json().await?;

    let stub = mailbox::mailbox_stub(&ctx.env, &mailbox_id)?;

    // Replace existing draft if requested (TS:218). Best-effort.
    if let Some(ref draft_id) = body.draft_id {
        let del_req = do_rpc_request("/rpc/delete_email", &json!({ "id": draft_id }))?;
        let _ = stub.fetch_with_request(del_req).await;
    }

    let message_id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string();
    let to_lower = body
        .to
        .as_deref()
        .map(|s| s.to_lowercase())
        .unwrap_or_default();
    let subject = body.subject.clone().unwrap_or_default();
    let thread_id = body
        .thread_id
        .clone()
        .or_else(|| body.in_reply_to.clone())
        .unwrap_or_else(|| message_id.clone());

    let create_body = json!({
        "folder": "draft",
        "email": {
            "id": message_id,
            "subject": subject,
            "sender": mailbox_id.to_lowercase(),
            "recipient": to_lower,
            "cc": body.cc.as_ref().map(|s| s.to_lowercase()),
            "bcc": body.bcc.as_ref().map(|s| s.to_lowercase()),
            "date": now,
            "body": body.body,
            "in_reply_to": body.in_reply_to,
            "email_references": serde_json::Value::Null,
            "thread_id": thread_id,
            "message_id": serde_json::Value::Null,
            "raw_headers": serde_json::Value::Null,
        },
        "attachments": [],
    });
    let do_req = do_rpc_request("/rpc/create_email", &create_body)?;
    let mut resp = stub.fetch_with_request(do_req).await?;
    let payload: serde_json::Value = resp.json().await?;
    if payload.get("error").is_some() {
        return Ok(Response::from_json(&payload)?.with_status(400));
    }

    let response_body = json!({
        "id": message_id,
        "status": "draft",
        "subject": subject,
        "recipient": body.to.unwrap_or_default(),
        "date": now,
    });
    Ok(Response::from_json(&response_body)?.with_status(201))
}
