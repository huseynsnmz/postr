//! `GET    /api/v1/mailboxes/:mailboxId/emails`                  — list
//! `GET    /api/v1/mailboxes/:mailboxId/emails/:emailId`         — single row
//! `PUT    /api/v1/mailboxes/:mailboxId/emails/:emailId`         — flag update
//! `DELETE /api/v1/mailboxes/:mailboxId/emails/:emailId`         — hard delete
//! `POST   /api/v1/mailboxes/:mailboxId/emails/:emailId/move`    — folder move
//!
//! All handlers dispatch into `MailboxDO` via a stub fetch. The DO owns
//! all SQL; this layer is just shape-massaging — parse query params,
//! call the right RPC, fold counts into the `{emails, totalCount}`
//! envelope the CLI expects.
//!
//! Wire contract (matches TS worker/workers/index.ts:158-163):
//!   * folder set     -> { emails, totalCount }
//!   * folder absent  -> raw EmailMeta[]

use std::collections::HashMap;

use serde::Deserialize;
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

/// Build a POST request to the DO with a JSON body.
pub(crate) fn do_rpc_request(path: &str, body: &serde_json::Value) -> Result<Request> {
    let body_str = serde_json::to_string(body)?;
    let headers = Headers::new();
    headers.set("Content-Type", "application/json")?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(JsValue::from_str(&body_str)));
    // Host doesn't matter for DO routing — the DO matches on path.
    Request::new_with_init(&format!("https://do{path}"), &init)
}

pub async fn list(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if let Err(e) = check_auth(&req, &ctx.env).await {
        return auth_error_response(e);
    }

    let Some(mailbox_id) = ctx.param("mailboxId").cloned() else {
        return bad_request("missing mailboxId");
    };
    if !mailbox::require_mailbox(&ctx.env, &mailbox_id).await? {
        return not_found("Not found");
    }

    // Pull all query params upfront. The TS worker accepts `threaded`
    // and `sortColumn`/`sortDirection`; we plumb them through but the DO
    // currently ignores `threaded` (see do_mailbox::rpc_get_emails note).
    let url = req.url()?;
    let qp: HashMap<String, String> = url.query_pairs().into_owned().collect();

    let folder = qp.get("folder").cloned();
    let thread_id = qp.get("thread_id").cloned();
    let page = qp.get("page").and_then(|v| v.parse::<u32>().ok());
    let limit = qp.get("limit").and_then(|v| v.parse::<u32>().ok());
    let sort_column = qp.get("sortColumn").cloned();
    let sort_direction = qp.get("sortDirection").cloned();

    let emails_body = json!({
        "folder": folder,
        "thread_id": thread_id,
        "page": page,
        "limit": limit,
        "sort_column": sort_column,
        "sort_direction": sort_direction,
    });

    let stub = mailbox::mailbox_stub(&ctx.env, &mailbox_id)?;
    let do_req = do_rpc_request("/rpc/get_emails", &emails_body)?;
    let mut emails_resp = stub.fetch_with_request(do_req).await?;
    let emails: serde_json::Value = emails_resp.json().await?;

    // If folder is set, the TS contract requires a {emails, totalCount}
    // envelope. Otherwise the bare array is the response.
    if let Some(ref f) = folder {
        let count_body = json!({ "folder": f, "thread_id": thread_id });
        let count_req = do_rpc_request("/rpc/count_emails", &count_body)?;
        let count_stub = mailbox::mailbox_stub(&ctx.env, &mailbox_id)?;
        let mut count_resp = count_stub.fetch_with_request(count_req).await?;
        let count_payload: serde_json::Value = count_resp.json().await?;
        let total = count_payload
            .get("count")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
            .max(0) as u64;
        return Response::from_json(&json!({
            "emails": emails,
            "totalCount": total,
        }));
    }

    Response::from_json(&emails)
}

pub async fn get_one(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if let Err(e) = check_auth(&req, &ctx.env).await {
        return auth_error_response(e);
    }

    let Some(mailbox_id) = ctx.param("mailboxId").cloned() else {
        return bad_request("missing mailboxId");
    };
    let Some(email_id) = ctx.param("emailId").cloned() else {
        return bad_request("missing emailId");
    };
    if !mailbox::require_mailbox(&ctx.env, &mailbox_id).await? {
        return not_found("Not found");
    }

    let stub = mailbox::mailbox_stub(&ctx.env, &mailbox_id)?;
    let do_req = do_rpc_request("/rpc/get_email", &json!({ "id": email_id }))?;
    let mut resp = stub.fetch_with_request(do_req).await?;
    let payload: serde_json::Value = resp.json().await?;
    if payload.is_null() {
        return not_found("Email not found");
    }
    Response::from_json(&payload)
}

// ── Mutating handlers (Workflow B) ────────────────────────────────────

/// `PUT /api/v1/mailboxes/:mailboxId/emails/:emailId` — port of TS index.ts:238-242.
/// Body: `{ "read"?: bool, "starred"?: bool }`. Returns full email row,
/// or 404 if the row does not exist.
#[derive(Deserialize)]
struct UpdateFlagsBody {
    #[serde(default)]
    read: Option<bool>,
    #[serde(default)]
    starred: Option<bool>,
}

pub async fn update(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if let Err(e) = check_auth(&req, &ctx.env).await {
        return auth_error_response(e);
    }
    let Some(mailbox_id) = ctx.param("mailboxId").cloned() else {
        return bad_request("missing mailboxId");
    };
    let Some(email_id) = ctx.param("emailId").cloned() else {
        return bad_request("missing emailId");
    };
    if !mailbox::require_mailbox(&ctx.env, &mailbox_id).await? {
        return not_found("Not found");
    }

    let body: UpdateFlagsBody = req.json().await?;
    let stub = mailbox::mailbox_stub(&ctx.env, &mailbox_id)?;
    let rpc_body = json!({
        "id": email_id,
        "read": body.read,
        "starred": body.starred,
    });
    let do_req = do_rpc_request("/rpc/update_email", &rpc_body)?;
    let mut resp = stub.fetch_with_request(do_req).await?;
    let payload: serde_json::Value = resp.json().await?;
    if payload.is_null() {
        return not_found("Email not found");
    }
    Response::from_json(&payload)
}

/// `DELETE /api/v1/mailboxes/:mailboxId/emails/:emailId` — port of TS index.ts:244-250.
/// Returns 204 on success, 404 if the email did not exist. Also deletes
/// R2 attachment blobs at `attachments/{id}/{att.id}/{att.filename}`.
pub async fn remove(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if let Err(e) = check_auth(&req, &ctx.env).await {
        return auth_error_response(e);
    }
    let Some(mailbox_id) = ctx.param("mailboxId").cloned() else {
        return bad_request("missing mailboxId");
    };
    let Some(email_id) = ctx.param("emailId").cloned() else {
        return bad_request("missing emailId");
    };
    if !mailbox::require_mailbox(&ctx.env, &mailbox_id).await? {
        return not_found("Not found");
    }

    let stub = mailbox::mailbox_stub(&ctx.env, &mailbox_id)?;
    let do_req = do_rpc_request("/rpc/delete_email", &json!({ "id": email_id }))?;
    let mut resp = stub.fetch_with_request(do_req).await?;
    let payload: serde_json::Value = resp.json().await?;
    if payload.is_null() {
        return not_found("Not found");
    }

    // Best-effort R2 cleanup — match TS:248 (deletes the full list in one call,
    // but workers-rs delete() only takes a single key per call).
    if let Some(arr) = payload.as_array() {
        let bucket = ctx.env.bucket("BUCKET")?;
        for att in arr {
            let id = att.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let filename = att.get("filename").and_then(|v| v.as_str()).unwrap_or("");
            if id.is_empty() || filename.is_empty() {
                continue;
            }
            let key = format!("attachments/{email_id}/{id}/{filename}");
            // Ignore individual delete errors so one bad blob doesn't
            // mask successful row deletion (matches TS waitUntil semantics).
            let _ = bucket.delete(key).await;
        }
    }

    Ok(Response::empty()?.with_status(204))
}

/// `POST /api/v1/mailboxes/:mailboxId/emails/:emailId/move` — port of TS
/// index.ts:252-256. Body: `{ "folderId": "..." }`.
#[derive(Deserialize)]
struct MoveBody {
    #[serde(rename = "folderId")]
    folder_id: String,
}

/// `POST /api/v1/mailboxes/:mailboxId/mark_all_read` — flip every unread row
/// in the folder named by the JSON body (`{"folder": "inbox"}`). Returns
/// `{"updated": N}` so the CLI can flash how many rows changed.
pub async fn mark_all_read(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if let Err(e) = check_auth(&req, &ctx.env).await {
        return auth_error_response(e);
    }
    let Some(mailbox_id) = ctx.param("mailboxId").cloned() else {
        return bad_request("missing mailboxId");
    };
    if !mailbox::require_mailbox(&ctx.env, &mailbox_id).await? {
        return not_found("Not found");
    }

    #[derive(serde::Deserialize)]
    struct Body {
        #[serde(default = "default_folder")]
        folder: String,
    }
    fn default_folder() -> String {
        "inbox".into()
    }
    let body: Body = match req.json().await {
        Ok(b) => b,
        Err(_) => Body {
            folder: default_folder(),
        },
    };

    let stub = mailbox::mailbox_stub(&ctx.env, &mailbox_id)?;
    let do_req = do_rpc_request("/rpc/mark_all_read", &json!({ "folder": body.folder }))?;
    let mut resp = stub.fetch_with_request(do_req).await?;
    let payload: serde_json::Value = resp.json().await?;
    Response::from_json(&payload)
}

pub async fn move_to(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if let Err(e) = check_auth(&req, &ctx.env).await {
        return auth_error_response(e);
    }
    let Some(mailbox_id) = ctx.param("mailboxId").cloned() else {
        return bad_request("missing mailboxId");
    };
    let Some(email_id) = ctx.param("emailId").cloned() else {
        return bad_request("missing emailId");
    };
    if !mailbox::require_mailbox(&ctx.env, &mailbox_id).await? {
        return not_found("Not found");
    }

    let body: MoveBody = req.json().await?;
    let stub = mailbox::mailbox_stub(&ctx.env, &mailbox_id)?;
    let do_req = do_rpc_request(
        "/rpc/move_email",
        &json!({ "id": email_id, "folder_id": body.folder_id }),
    )?;
    let mut resp = stub.fetch_with_request(do_req).await?;
    let payload: serde_json::Value = resp.json().await?;
    let ok = payload.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    if ok {
        Response::from_json(&json!({ "status": "moved" }))
    } else {
        bad_request("Folder not found")
    }
}
