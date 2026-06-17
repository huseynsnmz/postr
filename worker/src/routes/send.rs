//! Outbound send routes:
//!   * `POST /api/v1/mailboxes/:mailboxId/emails`
//!   * `POST /api/v1/mailboxes/:mailboxId/emails/:emailId/reply`
//!
//! Port of `worker/workers/index.ts:168-212` (fresh send) and
//! `worker/workers/routes/reply-forward.ts:24-113` (reply). Forward is
//! deferred to Workflow C.
//!
//! v1 scope limitations:
//!   * Plain text only — `html` and `attachments[]` in the request body
//!     produce a 400.
//!   * `to`/`cc`/`bcc` must be strings (the CLI only sends strings today).
//!     Array form is rejected as 400.
//!   * BCC is recorded on the DO row but not added to outbound headers.
//!   * Sender validation is enforced exactly like TS `validateSender`:
//!     `from` (or mailboxId if `from` is omitted) must equal mailboxId
//!     case-insensitively, and the domain must be non-empty.

use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;
use worker::*;

use crate::auth::{auth_error_response, check_auth};
use crate::mailbox;
use crate::routes::emails::do_rpc_request;
use crate::routes::mime::build_text_mime;

fn bad_request(error: &str) -> Result<Response> {
    Ok(Response::from_json(&json!({ "error": error }))?.with_status(400))
}

fn not_found(error: &str) -> Result<Response> {
    Ok(Response::from_json(&json!({ "error": error }))?.with_status(404))
}

fn rate_limited(error: &str) -> Result<Response> {
    Ok(Response::from_json(&json!({ "error": error }))?.with_status(429))
}

// ── Request body (subset of TS SendEmailRequestSchema) ───────────────

#[derive(Deserialize)]
#[serde(untagged)]
enum FromField {
    Plain(String),
    #[serde(rename_all = "snake_case")]
    Object {
        email: String,
        #[serde(default)]
        name: Option<String>,
    },
}

#[derive(Deserialize)]
struct SendBody {
    to: serde_json::Value,
    #[serde(default)]
    cc: Option<serde_json::Value>,
    #[serde(default)]
    bcc: Option<serde_json::Value>,
    from: FromField,
    subject: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    html: Option<serde_json::Value>,
    #[serde(default)]
    attachments: Option<serde_json::Value>,
    #[serde(default)]
    in_reply_to: Option<String>,
    #[serde(default)]
    references: Option<Vec<String>>,
    #[serde(default)]
    thread_id: Option<String>,
}

/// Joined recipient(s) as a comma-separated string. Rejects array form
/// in v1 to keep the surface narrow.
fn coerce_recipient(v: &serde_json::Value, field: &str) -> std::result::Result<String, String> {
    match v {
        serde_json::Value::String(s) => Ok(s.clone()),
        serde_json::Value::Array(_) => Err(format!(
            "{field} as array is not supported in v1 (use comma-separated string)"
        )),
        serde_json::Value::Null => Err(format!("missing {field}")),
        _ => Err(format!("{field} must be a string")),
    }
}

fn coerce_optional_recipient(
    v: &Option<serde_json::Value>,
    field: &str,
) -> std::result::Result<Option<String>, String> {
    match v {
        None => Ok(None),
        Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) if s.is_empty() => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s.clone())),
        Some(serde_json::Value::Array(_)) => Err(format!(
            "{field} as array is not supported in v1 (use comma-separated string)"
        )),
        _ => Err(format!("{field} must be a string")),
    }
}

struct ValidatedSender {
    to_str: String,
    from_email: String,
    from_display: String,
    from_domain: String,
    /// `true` when the request body explicitly set `from.name`. The
    /// mailbox-record fallback only applies when the caller did NOT
    /// supply a name — an explicit name (or per-message override) always
    /// wins.
    name_from_body: bool,
}

/// Port of TS `validateSender` (email-helpers.ts:53-71).
fn validate_sender(
    to: &serde_json::Value,
    from: &FromField,
    mailbox_id: &str,
) -> std::result::Result<ValidatedSender, String> {
    let to_str = coerce_recipient(to, "to")?.to_lowercase();
    let (from_email, from_display, name_from_body) = match from {
        FromField::Plain(s) => (s.to_lowercase(), s.clone(), false),
        FromField::Object { email, name } => {
            let lc = email.to_lowercase();
            match name {
                Some(n) if !n.trim().is_empty() => {
                    (lc, crate::routes::mime::format_from(n, email), true)
                }
                _ => (lc, email.clone(), false),
            }
        }
    };
    if from_email != mailbox_id.to_lowercase() {
        return Err("From address must match the mailbox email address".to_string());
    }
    let from_domain = from_email
        .split_once('@')
        .map(|(_, d)| d.to_string())
        .unwrap_or_default();
    if from_domain.is_empty() {
        return Err("Invalid sender email address".to_string());
    }
    Ok(ValidatedSender {
        to_str,
        from_email,
        from_display,
        from_domain,
        name_from_body,
    })
}

/// Port of `generateMessageId` (email-helpers.ts:85-92).
fn generate_message_id(from_domain: &str) -> (String, String) {
    let message_id = Uuid::new_v4().to_string();
    let outgoing = format!("{message_id}@{from_domain}");
    (message_id, outgoing)
}

/// `POST /api/v1/mailboxes/:mailboxId/emails` — fresh send.
pub async fn send_fresh(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if let Err(e) = check_auth(&req, &ctx.env).await {
        return auth_error_response(e);
    }
    let Some(mailbox_id) = ctx.param("mailboxId").cloned() else {
        return bad_request("missing mailboxId");
    };
    if !mailbox::require_mailbox(&ctx.env, &mailbox_id).await? {
        return not_found("Not found");
    }
    send_impl(req, ctx, mailbox_id, None).await
}

/// `POST /api/v1/mailboxes/:mailboxId/emails/:emailId/reply` — reply send.
pub async fn send_reply(req: Request, ctx: RouteContext<()>) -> Result<Response> {
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
    send_impl(req, ctx, mailbox_id, Some(email_id)).await
}

/// Threading info resolved from the original email when this is a reply.
struct Threading {
    in_reply_to: Option<String>,
    references: Vec<String>,
    thread_id: String,
}

async fn send_impl(
    mut req: Request,
    ctx: RouteContext<()>,
    mailbox_id: String,
    original_id: Option<String>,
) -> Result<Response> {
    let body: SendBody = match req.json().await {
        Ok(b) => b,
        Err(e) => return bad_request(&format!("invalid JSON body: {e}")),
    };

    // v1 limitations.
    if body.html.is_some() {
        return bad_request("v1 supports plain-text only (html field not allowed)");
    }
    if matches!(&body.attachments, Some(serde_json::Value::Array(a)) if !a.is_empty()) {
        return bad_request("v1 does not support attachments");
    }
    let text = match body.text.as_deref() {
        Some(t) => t.to_string(),
        None => return bad_request("text body is required in v1"),
    };

    // Sender validation.
    let mut sender = match validate_sender(&body.to, &body.from, &mailbox_id) {
        Ok(s) => s,
        Err(e) => return bad_request(&e),
    };

    // If the body didn't carry an explicit display name, fall back to the
    // mailbox-record default. The R2 read is cheap (it's a tiny JSON marker)
    // and only happens once per send.
    if !sender.name_from_body {
        if let Some(rec) = mailbox::load_record(&ctx.env, &mailbox_id).await? {
            if let Some(name) = rec.display_name.as_deref() {
                let trimmed = name.trim();
                if !trimmed.is_empty() {
                    sender.from_display =
                        crate::routes::mime::format_from(trimmed, &sender.from_email);
                }
            }
        }
    }

    let cc_str = match coerce_optional_recipient(&body.cc, "cc") {
        Ok(v) => v,
        Err(e) => return bad_request(&e),
    };
    let bcc_str = match coerce_optional_recipient(&body.bcc, "bcc") {
        Ok(v) => v,
        Err(e) => return bad_request(&e),
    };

    let stub = mailbox::mailbox_stub(&ctx.env, &mailbox_id)?;

    // Resolve threading. For replies, fetch the original to build refs.
    let threading: Threading = if let Some(ref oid) = original_id {
        let get_req = do_rpc_request("/rpc/get_email", &json!({ "id": oid }))?;
        let mut resp = stub.fetch_with_request(get_req).await?;
        let payload: serde_json::Value = resp.json().await?;
        if payload.is_null() {
            return not_found("Original email not found");
        }
        // Mirror TS buildReferencesChain (email-helpers.ts:99-116).
        let orig_msg_id = payload
            .get("message_id")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                payload
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string()
            });
        let existing_refs: Vec<String> = payload
            .get("email_references")
            .and_then(|v| v.as_str())
            .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
            .unwrap_or_default();
        let mut references = existing_refs;
        if !orig_msg_id.is_empty() {
            references.push(orig_msg_id.clone());
        }
        let thread_id = payload
            .get("thread_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                payload
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string()
            });
        Threading {
            in_reply_to: Some(orig_msg_id),
            references,
            thread_id,
        }
    } else {
        // Fresh send. Mirror TS:193 — thread_id = body.thread_id || in_reply_to || messageId.
        // The messageId isn't known until below, so handle it after.
        Threading {
            in_reply_to: body.in_reply_to.clone(),
            references: body.references.clone().unwrap_or_default(),
            thread_id: body
                .thread_id
                .clone()
                .or_else(|| body.in_reply_to.clone())
                .unwrap_or_default(),
        }
    };

    // Rate limit check.
    let rl_req = do_rpc_request("/rpc/check_send_rate_limit", &json!({}))?;
    let mut rl_resp = stub.fetch_with_request(rl_req).await?;
    let rl_payload: serde_json::Value = rl_resp.json().await?;
    if let Some(err) = rl_payload.get("error").and_then(|v| v.as_str()) {
        return rate_limited(err);
    }

    let (message_id, outgoing_msg_id) = generate_message_id(&sender.from_domain);
    let thread_id = if threading.thread_id.is_empty() {
        message_id.clone()
    } else {
        threading.thread_id.clone()
    };

    let now_iso = chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string();
    let now_rfc2822 = chrono::Utc::now().to_rfc2822();

    // Persist the sent row before delivery (matches TS ordering: write
    // DB then send so the user always sees the row even if SMTP fails).
    let raw_headers = build_raw_headers(
        &sender.from_display,
        &sender.to_str,
        cc_str.as_deref(),
        bcc_str.as_deref(),
        &body.subject,
        &now_iso,
        &outgoing_msg_id,
        threading.in_reply_to.as_deref(),
        &threading.references,
    );
    let email_references_json = if threading.references.is_empty() && original_id.is_none() {
        serde_json::Value::Null
    } else {
        serde_json::Value::String(serde_json::to_string(&threading.references)?)
    };

    let create_body = json!({
        "folder": "sent",
        "email": {
            "id": message_id,
            "subject": body.subject,
            "sender": sender.from_email,
            "recipient": sender.to_str,
            "cc": cc_str.as_ref().map(|s| s.to_lowercase()),
            "bcc": bcc_str.as_ref().map(|s| s.to_lowercase()),
            "date": now_iso,
            "body": text,
            "in_reply_to": threading.in_reply_to,
            "email_references": email_references_json,
            "thread_id": thread_id,
            "message_id": outgoing_msg_id,
            "raw_headers": raw_headers,
        },
        "attachments": [],
    });
    let create_req = do_rpc_request("/rpc/create_email", &create_body)?;
    let mut create_resp = stub.fetch_with_request(create_req).await?;
    let create_payload: serde_json::Value = create_resp.json().await?;
    if let Some(err) = create_payload.get("error").and_then(|v| v.as_str()) {
        return bad_request(err);
    }

    // For replies, mark the thread read (matches reply-forward.ts:88).
    if original_id.is_some() {
        let mt_req = do_rpc_request("/rpc/mark_thread_read", &json!({ "thread_id": thread_id }))?;
        let _ = stub.fetch_with_request(mt_req).await;
    }

    // Build MIME and dispatch via the EMAIL binding.
    let raw_mime = build_text_mime(
        &sender.from_display,
        &sender.to_str,
        cc_str.as_deref(),
        &body.subject,
        &now_rfc2822,
        &outgoing_msg_id,
        threading.in_reply_to.as_deref(),
        &threading.references,
        &text,
    );

    let send_binding = ctx.env.send_email("EMAIL")?;
    let email_message = EmailMessage::new(&sender.from_email, &sender.to_str, &raw_mime)
        .map_err(|e| Error::from(worker::wasm_bindgen::JsValue::from(e)))?;
    if let Err(e) = send_binding.send(&email_message).await {
        // Match the TS behaviour: we already persisted the row, so the
        // user sees the sent record. Surface the delivery error in the
        // response without rolling back.
        let err_msg = format!("{e:?}");
        console_error!("send_email failed: {err_msg}");
        return Ok(Response::from_json(&json!({
            "id": message_id,
            "status": "queued",
            "warning": format!("delivery error: {err_msg}"),
        }))?
        .with_status(202));
    }

    Ok(Response::from_json(&json!({
        "id": message_id,
        "status": "sent",
    }))?
    .with_status(202))
}

/// Build the `raw_headers` JSON array stored on the email row.
/// Mirrors TS index.ts:194-201 (fresh) and reply-forward.ts:73-83 (reply).
#[allow(clippy::too_many_arguments)]
fn build_raw_headers(
    from_display: &str,
    to: &str,
    cc: Option<&str>,
    bcc: Option<&str>,
    subject: &str,
    date_iso: &str,
    outgoing_msg_id: &str,
    in_reply_to: Option<&str>,
    references: &[String],
) -> String {
    let mut headers: Vec<serde_json::Value> = vec![
        json!({ "key": "from", "value": from_display }),
        json!({ "key": "to", "value": to }),
    ];
    if let Some(v) = cc {
        headers.push(json!({ "key": "cc", "value": v }));
    }
    if let Some(v) = bcc {
        headers.push(json!({ "key": "bcc", "value": v }));
    }
    headers.push(json!({ "key": "subject", "value": subject }));
    headers.push(json!({ "key": "date", "value": date_iso }));
    headers.push(json!({ "key": "message-id", "value": format!("<{outgoing_msg_id}>") }));
    if let Some(irt) = in_reply_to {
        headers.push(json!({ "key": "in-reply-to", "value": format!("<{irt}>") }));
    }
    if !references.is_empty() {
        let joined = references
            .iter()
            .map(|r| format!("<{r}>"))
            .collect::<Vec<_>>()
            .join(" ");
        headers.push(json!({ "key": "references", "value": joined }));
    }
    serde_json::to_string(&headers).unwrap_or_else(|_| "[]".to_string())
}
