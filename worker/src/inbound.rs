//! Inbound Email Routing handler — port of `receiveEmail` in
//! worker/workers/index.ts:348-410.
//!
//! Reads the raw RFC822 blob from the `ForwardableEmailMessage`, parses
//! with `mail_parser`, resolves the mailbox via `EMAIL_ADDRESSES`,
//! stores attachments under R2 `attachments/{messageId}/...`, computes
//! the thread id (RFC threading + subject fallback), and persists via
//! the DO's `/rpc/create_email`.
//!
//! Deferred to Workflow D:
//!   * Agent trigger (`EMAIL_AGENT` namespace not yet ported).

use chrono::Utc;
use mail_parser::{Address, HeaderValue, MessageParser, MimeHeaders};
use serde_json::{json, Value};
use uuid::Uuid;
use worker::wasm_bindgen::JsValue;
use worker::*;

use crate::mailbox;

const MAX_EMAIL_SIZE: f64 = 25.0 * 1024.0 * 1024.0;

/// `#[event(email)]` entry point. Errors are logged but never propagated
/// (the macro panics on Err, which would kill the worker — soft-fail
/// instead so a single malformed email doesn't take the route down).
pub async fn receive_email(
    message: ForwardableEmailMessage,
    env: Env,
    _ctx: Context,
) -> Result<()> {
    if let Err(e) = receive_email_inner(message, env).await {
        console_error!("receive_email failed: {e}");
    }
    Ok(())
}

async fn receive_email_inner(message: ForwardableEmailMessage, env: Env) -> Result<()> {
    if message.raw_size() > MAX_EMAIL_SIZE {
        return Err(Error::RustError("Email too large".into()));
    }

    let raw = message.raw_bytes().await?;
    let Some(parsed) = MessageParser::default().parse(&raw) else {
        console_log!("receive_email: failed to parse MIME");
        return Ok(());
    };

    // Recipients (lowercased, deduped only by virtue of header order).
    let to_recipients = collect_addresses(parsed.to());
    let cc_recipients = collect_addresses(parsed.cc());
    let bcc_recipients = collect_addresses(parsed.bcc());

    if to_recipients.is_empty() {
        console_log!("receive_email: empty To: header");
        return Ok(());
    }

    let from = parsed
        .from()
        .and_then(|a| a.first())
        .and_then(|a| a.address())
        .map(|s| s.to_lowercase())
        .unwrap_or_default();

    let subject = parsed.subject().unwrap_or("").to_string();

    // Body — prefer HTML, fall back to text (matches TS:400).
    let body = parsed
        .body_html(0)
        .map(|c| c.into_owned())
        .or_else(|| parsed.body_text(0).map(|c| c.into_owned()))
        .unwrap_or_default();

    // Header-derived ids.
    let in_reply_to = parsed
        .in_reply_to()
        .as_text()
        .map(extract_msg_id)
        .filter(|s| !s.is_empty());
    let references: Vec<String> = match parsed.references() {
        HeaderValue::Text(t) => t
            .split_whitespace()
            .filter(|s| !s.is_empty())
            .map(extract_msg_id)
            .collect(),
        HeaderValue::TextList(list) => list
            .iter()
            .flat_map(|r| {
                r.split_whitespace()
                    .filter(|s| !s.is_empty())
                    .map(extract_msg_id)
                    .collect::<Vec<_>>()
            })
            .collect(),
        _ => Vec::new(),
    };
    let original_message_id = parsed
        .message_id()
        .map(extract_msg_id)
        .filter(|s| !s.is_empty());

    // Resolve mailbox (TS:354-364).
    let allowed = allowed_addresses(&env);
    let mailbox_id: Option<String> = if !allowed.is_empty() {
        to_recipients
            .iter()
            .find(|addr| allowed.iter().any(|a| a == *addr))
            .cloned()
    } else {
        to_recipients.first().cloned()
    };
    let Some(mailbox_id) = mailbox_id else {
        console_log!("receive_email: no recipient matches EMAIL_ADDRESSES");
        return Ok(());
    };

    if !mailbox::require_mailbox(&env, &mailbox_id).await? {
        console_log!("receive_email: mailbox {mailbox_id} does not exist");
        return Ok(());
    }

    // Row id (distinct from RFC Message-ID).
    let message_id = Uuid::new_v4().to_string();

    // Store attachments in R2 + build the rows.
    let bucket = env.bucket("BUCKET")?;
    let mut attachment_rows: Vec<Value> = Vec::new();
    for att in parsed.attachments() {
        let att_id = Uuid::new_v4().to_string();
        let filename = sanitize_filename(att.attachment_name().unwrap_or("untitled"));
        let content = att.contents().to_vec();
        let size = content.len() as i64;
        let mimetype = att
            .content_type()
            .map(|ct| match ct.c_subtype.as_deref() {
                Some(sub) => format!("{}/{}", ct.c_type, sub),
                None => ct.c_type.to_string(),
            })
            .unwrap_or_else(|| "application/octet-stream".to_string());
        let content_id = att.content_id().map(str::to_string);
        let disposition = att
            .content_disposition()
            .map(|cd| cd.c_type.to_string())
            .unwrap_or_else(|| "attachment".to_string());

        let key = format!("attachments/{message_id}/{att_id}/{filename}");
        bucket.put(&key, content).execute().await?;

        attachment_rows.push(json!({
            "id": att_id,
            "email_id": message_id,
            "filename": filename,
            "mimetype": mimetype,
            "size": size,
            "content_id": content_id,
            "disposition": disposition,
        }));
    }

    // Thread id (TS:386-391).
    let mut thread_id = references
        .first()
        .cloned()
        .or_else(|| in_reply_to.clone())
        .unwrap_or_else(|| message_id.clone());
    if in_reply_to.is_none() && references.is_empty() {
        let lookup = rpc_call(
            &env,
            &mailbox_id,
            "/rpc/find_thread_by_subject",
            &json!({
                "subject": subject,
                "sender_address": if from.is_empty() { None } else { Some(from.clone()) },
            }),
        )
        .await?;
        if let Some(found) = lookup.as_str() {
            if !found.is_empty() {
                thread_id = found.to_string();
            }
        }
    }

    // Raw headers — serialised as `{name: value}` JSON object, matching
    // the TS shape (JSON.stringify of postal-mime's `headers` array).
    let raw_headers: Vec<Value> = parsed
        .headers_raw()
        .map(|(name, value)| json!({ "key": name, "value": value }))
        .collect();
    let raw_headers_str = serde_json::to_string(&raw_headers).ok();

    // email_references: TS stores `JSON.stringify(array)` (or null).
    let email_references_str = if references.is_empty() {
        None
    } else {
        serde_json::to_string(&references).ok()
    };

    let now = Utc::now().to_rfc3339();
    let cc_str = if cc_recipients.is_empty() {
        None
    } else {
        Some(cc_recipients.join(", "))
    };
    let bcc_str = if bcc_recipients.is_empty() {
        None
    } else {
        Some(bcc_recipients.join(", "))
    };

    let create_body = json!({
        "folder": "inbox",
        "email": {
            "id": message_id,
            "subject": subject,
            "sender": from,
            "recipient": to_recipients.join(", "),
            "cc": cc_str,
            "bcc": bcc_str,
            "date": now,
            "body": body,
            "in_reply_to": in_reply_to,
            "email_references": email_references_str,
            "thread_id": thread_id,
            "message_id": original_message_id,
            "raw_headers": raw_headers_str,
        },
        "attachments": attachment_rows,
    });

    let _ = rpc_call(&env, &mailbox_id, "/rpc/create_email", &create_body).await?;

    // FIXME(workflow-D): fire `EMAIL_AGENT` onNewEmail trigger once the
    // Rust agent exists. TS uses `ctx.waitUntil(agentStub.fetch(...))`.
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────

fn collect_addresses(addr: Option<&Address<'_>>) -> Vec<String> {
    let Some(a) = addr else { return Vec::new() };
    a.iter()
        .filter_map(|x| x.address())
        .map(|s| s.to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

fn extract_msg_id(s: &str) -> String {
    if let (Some(lt), Some(gt)) = (s.find('<'), s.find('>')) {
        if gt > lt {
            return s[lt + 1..gt].to_string();
        }
    }
    s.split_whitespace().next().unwrap_or("").to_string()
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect()
}

/// Read `EMAIL_ADDRESSES` from env. Tolerates JSON-array form
/// (`["a@x", "b@x"]`) and the legacy comma-separated string. Returns
/// lowercased addresses.
fn allowed_addresses(env: &Env) -> Vec<String> {
    if let Ok(list) = env.object_var::<Vec<String>>("EMAIL_ADDRESSES") {
        return list
            .into_iter()
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
    }
    let Ok(raw) = env.var("EMAIL_ADDRESSES") else {
        return Vec::new();
    };
    raw.to_string()
        .split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

async fn rpc_call(env: &Env, mailbox_id: &str, path: &str, body: &Value) -> Result<Value> {
    let stub = mailbox::mailbox_stub(env, mailbox_id)?;
    let body_str = serde_json::to_string(body)?;
    let headers = Headers::new();
    headers.set("Content-Type", "application/json")?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(JsValue::from_str(&body_str)));
    let req = Request::new_with_init(&format!("https://do{path}"), &init)?;
    let mut resp = stub.fetch_with_request(req).await?;
    resp.json().await
}
