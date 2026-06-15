//! `GET /api/v1/mailboxes/:mailboxId/threads/:threadId` — full thread.
//!
//! Dispatches to `MailboxDO::rpc_get_thread_emails`. The DO returns the
//! raw `EmailFull[]` (matches TS worker/workers/index.ts:261).

use serde_json::json;
use worker::wasm_bindgen::JsValue;
use worker::*;

use crate::auth::{auth_error_response, check_auth};
use crate::mailbox;

fn bad_request(error: &str) -> Result<Response> {
    Ok(Response::from_json(&json!({ "error": error }))?.with_status(400))
}

fn not_found(error: &str) -> Result<Response> {
    Ok(Response::from_json(&json!({ "error": error }))?.with_status(404))
}

fn do_rpc_request(path: &str, body: &serde_json::Value) -> Result<Request> {
    let body_str = serde_json::to_string(body)?;
    let headers = Headers::new();
    headers.set("Content-Type", "application/json")?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(JsValue::from_str(&body_str)));
    Request::new_with_init(&format!("https://do{path}"), &init)
}

pub async fn get_one(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if let Err(e) = check_auth(&req, &ctx.env).await {
        return auth_error_response(e);
    }

    let Some(mailbox_id) = ctx.param("mailboxId").cloned() else {
        return bad_request("missing mailboxId");
    };
    let Some(thread_id) = ctx.param("threadId").cloned() else {
        return bad_request("missing threadId");
    };
    if !mailbox::require_mailbox(&ctx.env, &mailbox_id).await? {
        return not_found("Not found");
    }

    let stub = mailbox::mailbox_stub(&ctx.env, &mailbox_id)?;
    let do_req = do_rpc_request("/rpc/get_thread_emails", &json!({ "thread_id": thread_id }))?;
    let mut resp = stub.fetch_with_request(do_req).await?;
    let payload: serde_json::Value = resp.json().await?;
    Response::from_json(&payload)
}

/// `POST /api/v1/mailboxes/:mailboxId/threads/:threadId/read` — port of
/// TS index.ts:264-267. Body is empty. Returns `{ "status": "marked_read" }`.
pub async fn mark_read(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if let Err(e) = check_auth(&req, &ctx.env).await {
        return auth_error_response(e);
    }
    let Some(mailbox_id) = ctx.param("mailboxId").cloned() else {
        return bad_request("missing mailboxId");
    };
    let Some(thread_id) = ctx.param("threadId").cloned() else {
        return bad_request("missing threadId");
    };
    if !mailbox::require_mailbox(&ctx.env, &mailbox_id).await? {
        return not_found("Not found");
    }

    let stub = mailbox::mailbox_stub(&ctx.env, &mailbox_id)?;
    let do_req = do_rpc_request("/rpc/mark_thread_read", &json!({ "thread_id": thread_id }))?;
    let _ = stub.fetch_with_request(do_req).await?;
    Response::from_json(&json!({ "status": "marked_read" }))
}
