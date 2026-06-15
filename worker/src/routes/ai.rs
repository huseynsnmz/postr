//! AI slash-command routes — port of worker/workers/routes/ai.ts.
//!
//! Four endpoints under /api/v1/mailboxes/:mailboxId — /summarize, /draft,
//! /ask, /triage. Each makes one Workers AI call and folds the response
//! into the shape the CLI expects (see cli/src/api/types.rs).
//!
//! Workers AI binding: worker 0.8.5 exposes `Ai::run<T: Serialize, U: DeserializeOwned>`
//! at `env.ai("AI")?`. Input is the OpenAI-compatible chat payload; output
//! is a `{ response?: String }` envelope.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use worker::*;

use crate::auth::{auth_error_response, check_auth};
use crate::mailbox;
use crate::routes::emails::do_rpc_request;

const AGENT_MODEL: &str = "@cf/moonshotai/kimi-k2.5";
const FAST_MODEL: &str = "@cf/meta/llama-4-scout-17b-16e-instruct";

// ── HTTP helpers ──────────────────────────────────────────────────────

fn bad_request(error: &str) -> Result<Response> {
    Ok(Response::from_json(&json!({ "error": error }))?.with_status(400))
}

fn not_found(error: &str) -> Result<Response> {
    Ok(Response::from_json(&json!({ "error": error }))?.with_status(404))
}

fn server_error(error: &str, status: u16) -> Result<Response> {
    Ok(Response::from_json(&json!({ "error": error }))?.with_status(status))
}

// ── AI plumbing ───────────────────────────────────────────────────────

#[derive(Serialize)]
struct AiChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct AiChatRequest<'a> {
    messages: Vec<AiChatMessage<'a>>,
    max_tokens: u32,
    temperature: f32,
}

#[derive(Deserialize)]
struct AiChatResponse {
    #[serde(default)]
    response: Option<String>,
}

async fn run_ai(
    ai: &Ai,
    model: &str,
    system: &str,
    user: &str,
    max_tokens: u32,
    temperature: f32,
) -> Result<String> {
    let req = AiChatRequest {
        messages: vec![
            AiChatMessage {
                role: "system",
                content: system,
            },
            AiChatMessage {
                role: "user",
                content: user,
            },
        ],
        max_tokens,
        temperature,
    };
    let resp: AiChatResponse = ai.run(model, req).await?;
    Ok(resp.response.unwrap_or_default())
}

/// Port of TS `safeParseJson` — try a direct parse, then extract the first
/// `{…}` block (handles the common case where the model wraps JSON in
/// prose or code fences).
fn safe_parse_json<T: serde::de::DeserializeOwned>(text: &str) -> Option<T> {
    if text.is_empty() {
        return None;
    }
    if let Ok(v) = serde_json::from_str::<T>(text) {
        return Some(v);
    }
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str::<T>(&text[start..=end]).ok()
}

/// Plain-text projection of an HTML body. Port of TS `stripHtmlToText`
/// (email-helpers.ts:182). Regex-free pass over bytes: drop `<style>` and
/// `<script>` blocks, strip every other `<…>` tag, collapse runs of
/// whitespace into single spaces.
pub(crate) fn strip_html_to_text(html: &str) -> String {
    if html.is_empty() {
        return String::new();
    }
    let without_blocks = strip_block_tag(html, "style");
    let without_blocks = strip_block_tag(&without_blocks, "script");
    let stripped = strip_all_tags(&without_blocks);
    collapse_whitespace(&stripped)
}

/// Remove `<{tag}…>…</{tag}>` blocks (case-insensitive). Greedily skips
/// to the matching closer; mismatched tags fall through to `strip_all_tags`.
fn strip_block_tag(input: &str, tag: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<'
            && i + 1 + tag.len() <= bytes.len()
            && bytes[i + 1..i + 1 + tag.len()].eq_ignore_ascii_case(tag.as_bytes())
        {
            // Confirm what follows the tag name is `>`, whitespace, or `/`.
            let after = i + 1 + tag.len();
            let valid_terminator = bytes
                .get(after)
                .map(|b| matches!(*b, b'>' | b' ' | b'\t' | b'\n' | b'\r' | b'/'))
                .unwrap_or(false);
            if valid_terminator {
                // Find the opening `>` of this tag.
                if let Some(rel_close) = bytes[after..].iter().position(|&b| b == b'>') {
                    let open_end = after + rel_close + 1;
                    // Find matching `</{tag}>` (case-insensitive).
                    let close_pat_start = find_case_insensitive(&bytes[open_end..], b"</");
                    if let Some(rel_lt) = close_pat_start {
                        let candidate_pos = open_end + rel_lt;
                        let inner = candidate_pos + 2;
                        if inner + tag.len() <= bytes.len()
                            && bytes[inner..inner + tag.len()].eq_ignore_ascii_case(tag.as_bytes())
                        {
                            if let Some(rel_gt) =
                                bytes[inner + tag.len()..].iter().position(|&b| b == b'>')
                            {
                                let block_end = inner + tag.len() + rel_gt + 1;
                                i = block_end;
                                continue;
                            }
                        }
                    }
                }
            }
        }
        // Push one byte at a time but respect UTF-8 boundaries.
        let ch_len = utf8_char_len(bytes[i]);
        let end = (i + ch_len).min(bytes.len());
        out.push_str(&input[i..end]);
        i = end;
    }
    out
}

/// Lower-cased substring match (only ASCII alpha rolled to lower).
fn find_case_insensitive(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    'outer: for i in 0..=haystack.len() - needle.len() {
        for (j, &nb) in needle.iter().enumerate() {
            if !haystack[i + j].eq_ignore_ascii_case(&nb) {
                continue 'outer;
            }
        }
        return Some(i);
    }
    None
}

/// Drop every `<…>` tag, replacing with a single space (matches TS regex
/// `/<[^>]+>/g` → `" "`).
fn strip_all_tags(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            if let Some(rel_gt) = bytes[i + 1..].iter().position(|&b| b == b'>') {
                i += rel_gt + 2;
                out.push(' ');
                continue;
            }
        }
        let ch_len = utf8_char_len(bytes[i]);
        let end = (i + ch_len).min(bytes.len());
        out.push_str(&input[i..end]);
        i = end;
    }
    out
}

/// `/\s+/g` -> single space, then trim.
fn collapse_whitespace(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_ws = false;
    for ch in input.chars() {
        if ch.is_whitespace() {
            if !in_ws {
                out.push(' ');
                in_ws = true;
            }
        } else {
            out.push(ch);
            in_ws = false;
        }
    }
    out.trim().to_string()
}

fn utf8_char_len(b: u8) -> usize {
    if b < 0xC0 {
        1
    } else if b < 0xE0 {
        2
    } else if b < 0xF0 {
        3
    } else {
        4
    }
}

// ── Shared route setup ────────────────────────────────────────────────

async fn check_route(req: &Request, ctx: &RouteContext<()>) -> Result<Option<String>> {
    // Surface auth error to caller via Result; converted at call site.
    check_auth(req, &ctx.env).await?;
    let Some(mailbox_id) = ctx.param("mailboxId").cloned() else {
        return Ok(None);
    };
    if !mailbox::require_mailbox(&ctx.env, &mailbox_id).await? {
        return Ok(None);
    }
    Ok(Some(mailbox_id))
}

async fn rpc_call(env: &Env, mailbox_id: &str, path: &str, body: &Value) -> Result<Value> {
    let stub = mailbox::mailbox_stub(env, mailbox_id)?;
    let req = do_rpc_request(path, body)?;
    let mut resp = stub.fetch_with_request(req).await?;
    resp.json().await
}

// ── /summarize ────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct SummarizeBody {
    #[serde(default, rename = "threadId")]
    thread_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SummaryItem {
    Object {
        text: String,
        #[serde(default, rename = "isActionItem")]
        is_action_item: bool,
    },
    Text(String),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SummaryField {
    List(Vec<SummaryItem>),
    Text(String),
}

#[derive(Deserialize)]
struct SummarizeAiShape {
    #[serde(default)]
    summary: Option<SummaryField>,
    #[serde(default, rename = "suggestedReplies")]
    suggested_replies: Option<Vec<String>>,
}

const SYS_SUMMARIZE: &str = r#"Summarize this email thread. Output STRICT JSON with this shape:
{
  "summary": [{"text": "short sentence about the thread", "isActionItem": false}, ...],
  "suggestedReplies": ["reply 1", "reply 2"]
}
- "summary" is 2-4 short bullets. Mark a bullet isActionItem=true only when it describes a concrete action the inbox owner must take.
- "suggestedReplies" is 1-3 short reply ideas (<=2 sentences each), plain text.
No markdown, no commentary."#;

pub async fn summarize(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let mailbox_id = match check_route(&req, &ctx).await {
        Ok(Some(id)) => id,
        Ok(None) => return not_found("Not found"),
        Err(e) => return auth_error_response(e),
    };

    let body: SummarizeBody = match req.json().await {
        Ok(b) => b,
        Err(_) => return bad_request("invalid_json"),
    };
    let thread_id = match body.thread_id {
        Some(t) if !t.is_empty() => t,
        _ => return bad_request("missing_threadId"),
    };

    let thread_val = rpc_call(
        &ctx.env,
        &mailbox_id,
        "/rpc/get_thread_emails",
        &json!({ "thread_id": thread_id }),
    )
    .await?;
    let messages = thread_val.as_array().cloned().unwrap_or_default();
    if messages.is_empty() {
        return Ok(Response::from_json(&json!({ "error": "thread_not_found" }))?.with_status(404));
    }

    // Build transcript + subject + people set.
    let mut transcript_parts: Vec<String> = Vec::with_capacity(messages.len());
    let mut people: HashSet<String> = HashSet::new();
    let subject = messages
        .first()
        .and_then(|m| m.get("subject").and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_string();
    for m in &messages {
        let date = m.get("date").and_then(|v| v.as_str()).unwrap_or("");
        let sender = m.get("sender").and_then(|v| v.as_str()).unwrap_or("");
        let recipient = m.get("recipient").and_then(|v| v.as_str()).unwrap_or("");
        let body = m.get("body").and_then(|v| v.as_str()).unwrap_or("");
        let text = strip_html_to_text(body);
        let truncated: String = text.chars().take(2000).collect();
        transcript_parts.push(format!("[{date}] {sender} -> {recipient}: {truncated}"));

        if !sender.is_empty() {
            people.insert(sender.to_lowercase());
        }
        if !recipient.is_empty() {
            for r in recipient.split(',') {
                let r = r.trim().to_lowercase();
                if !r.is_empty() {
                    people.insert(r);
                }
            }
        }
    }
    let transcript = transcript_parts.join("\n\n");

    let ai = ctx.env.ai("AI")?;
    let text = run_ai(&ai, AGENT_MODEL, SYS_SUMMARIZE, &transcript, 800, 0.2).await?;
    let parsed: Option<SummarizeAiShape> = safe_parse_json(&text);

    // Coerce summary to Vec<{text, isActionItem}>.
    let summary_out: Vec<Value> = match parsed.as_ref().and_then(|p| p.summary.as_ref()) {
        Some(SummaryField::List(items)) => items
            .iter()
            .map(|i| match i {
                SummaryItem::Object {
                    text,
                    is_action_item,
                } => json!({ "text": text, "isActionItem": *is_action_item }),
                SummaryItem::Text(s) => json!({ "text": s, "isActionItem": false }),
            })
            .collect(),
        Some(SummaryField::Text(s)) => vec![json!({ "text": s, "isActionItem": false })],
        None => {
            if text.is_empty() {
                Vec::new()
            } else {
                let truncated: String = text.chars().take(500).collect();
                vec![json!({ "text": truncated, "isActionItem": false })]
            }
        }
    };

    let mut suggested = parsed.and_then(|p| p.suggested_replies).unwrap_or_default();
    if suggested.len() > 3 {
        suggested.truncate(3);
    }

    Response::from_json(&json!({
        "thread": {
            "subject": subject,
            "messageCount": messages.len(),
            "peopleCount": people.len(),
        },
        "summary": summary_out,
        "suggestedReplies": suggested,
    }))
}

// ── /draft ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct DraftBody {
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default, rename = "threadId")]
    thread_id: Option<String>,
}

#[derive(Deserialize)]
struct DraftAiShape {
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    body: Option<String>,
}

const SYS_DRAFT: &str = r#"You draft business emails. Output STRICT JSON: {"to":"...","subject":"...","body":"..."}.
Body is plain text — no markdown, no commentary, no greetings like "Here is your draft"."#;

pub async fn draft(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let mailbox_id = match check_route(&req, &ctx).await {
        Ok(Some(id)) => id,
        Ok(None) => return not_found("Not found"),
        Err(e) => return auth_error_response(e),
    };

    let body: DraftBody = match req.json().await {
        Ok(b) => b,
        Err(_) => return bad_request("invalid_json"),
    };
    let prompt = match body.prompt {
        Some(p) if !p.is_empty() => p,
        _ => return bad_request("missing_prompt"),
    };

    let mut context = String::new();
    let mut original_to: Option<String> = None;
    let mut original_subject: Option<String> = None;

    if let Some(tid) = body.thread_id.as_deref() {
        let thread_val = rpc_call(
            &ctx.env,
            &mailbox_id,
            "/rpc/get_thread_emails",
            &json!({ "thread_id": tid }),
        )
        .await?;
        let messages = thread_val.as_array().cloned().unwrap_or_default();
        if let Some(last) = messages.last() {
            original_to = last
                .get("sender")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let subj = last
                .get("subject")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            original_subject = Some(if subj.starts_with("Re:") {
                subj
            } else {
                format!("Re: {subj}")
            });
            let parts: Vec<String> = messages
                .iter()
                .map(|m| {
                    let sender = m.get("sender").and_then(|v| v.as_str()).unwrap_or("");
                    let body_html = m.get("body").and_then(|v| v.as_str()).unwrap_or("");
                    let text = strip_html_to_text(body_html);
                    let truncated: String = text.chars().take(1500).collect();
                    format!("{sender}: {truncated}")
                })
                .collect();
            context = parts.join("\n\n");
        }
    }

    let user_text = if context.is_empty() {
        format!("Instruction: {prompt}")
    } else {
        format!("Instruction: {prompt}\n\nThread context:\n{context}")
    };

    let ai = ctx.env.ai("AI")?;
    let text = run_ai(&ai, AGENT_MODEL, SYS_DRAFT, &user_text, 1200, 0.4).await?;
    let parsed: Option<DraftAiShape> = safe_parse_json(&text);
    let parsed_body = match parsed.as_ref().and_then(|p| p.body.as_ref()) {
        Some(b) if !b.is_empty() => b.clone(),
        _ => return server_error("draft_generation_failed", 502),
    };

    // FIXME(workflow-D): persist via toolDraft once verifyDraft ports. TS
    // calls toolDraftReply/toolDraftEmail here so a draft row exists; the
    // CLI only reads {to, subject, body} from the response, so v1 skips
    // the round-trip and returns the AI shape directly.
    let to = parsed
        .as_ref()
        .and_then(|p| p.to.clone())
        .filter(|s| !s.is_empty())
        .or(original_to)
        .unwrap_or_default();
    let subject = parsed
        .as_ref()
        .and_then(|p| p.subject.clone())
        .filter(|s| !s.is_empty())
        .or(original_subject)
        .unwrap_or_default();

    Response::from_json(&json!({
        "to": to,
        "subject": subject,
        "body": parsed_body,
    }))
}

// ── /ask ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct AskBody {
    #[serde(default)]
    query: Option<String>,
}

#[derive(Deserialize, Serialize, Default)]
struct AskFilters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    query: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    folder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    date_start: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    date_end: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    is_read: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    is_starred: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    has_attachment: Option<bool>,
}

fn sys_ask(today: &str) -> String {
    format!(
        r#"Translate a natural-language email search into a structured filter. Output STRICT JSON with these optional fields (omit ones you don't infer):
{{"query":"...","folder":"inbox|sent|draft|archive|trash","from":"...","to":"...","subject":"...","date_start":"ISO","date_end":"ISO","is_read":bool,"is_starred":bool,"has_attachment":bool}}
Today is {today}. No commentary."#
    )
}

pub async fn ask(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let mailbox_id = match check_route(&req, &ctx).await {
        Ok(Some(id)) => id,
        Ok(None) => return not_found("Not found"),
        Err(e) => return auth_error_response(e),
    };

    let body: AskBody = match req.json().await {
        Ok(b) => b,
        Err(_) => return bad_request("invalid_json"),
    };
    let query = match body.query {
        Some(q) if !q.is_empty() => q,
        _ => return bad_request("missing_query"),
    };

    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let sys = sys_ask(&today);

    let ai = ctx.env.ai("AI")?;
    let text = run_ai(&ai, FAST_MODEL, &sys, &query, 400, 0.0).await?;
    let mut filters: AskFilters = safe_parse_json(&text).unwrap_or_default();
    if filters.query.is_none() {
        filters.query = Some(query.clone());
    }

    // Search call: filters + page/limit.
    let mut search_body = serde_json::to_value(&filters).unwrap_or_else(|_| json!({}));
    if let Some(obj) = search_body.as_object_mut() {
        obj.insert("page".into(), json!(1));
        obj.insert("limit".into(), json!(50));
    }
    let emails_val = rpc_call(&ctx.env, &mailbox_id, "/rpc/search_emails", &search_body).await?;
    let emails = emails_val.as_array().cloned().unwrap_or_default();

    // Count call: filters only (no page/limit).
    let count_body = serde_json::to_value(&filters).unwrap_or_else(|_| json!({}));
    let count_val = rpc_call(
        &ctx.env,
        &mailbox_id,
        "/rpc/count_search_results",
        &count_body,
    )
    .await?;
    let count = count_val
        .get("count")
        .and_then(|v| v.as_i64())
        .unwrap_or(0)
        .max(0);

    let results: Vec<Value> = emails
        .iter()
        .map(|e| {
            let id = e.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let thread_id = e
                .get("thread_id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(id);
            json!({
                "sender": e.get("sender").and_then(|v| v.as_str()).unwrap_or(""),
                "subject": e.get("subject").and_then(|v| v.as_str()).unwrap_or(""),
                "date": e.get("date").and_then(|v| v.as_str()).unwrap_or(""),
                "threadId": thread_id,
            })
        })
        .collect();

    let summary = if count == 0 {
        format!("No matches for \"{query}\"")
    } else {
        let plural = if count == 1 { "" } else { "es" };
        format!("{count} match{plural} for \"{query}\"")
    };

    Response::from_json(&json!({
        "summary": summary,
        "results": results,
    }))
}

// ── /triage ───────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
struct TriageAiShape {
    #[serde(default)]
    important: Option<Vec<String>>,
    #[serde(default)]
    updates: Option<Vec<String>>,
    #[serde(default)]
    promotions: Option<Vec<String>>,
}

const SYS_TRIAGE: &str = r#"Classify each email into exactly one of: important, updates, promotions.
- important: human-to-human, requires a reply or action from the inbox owner
- updates: notifications, receipts, system mail, automated status
- promotions: marketing, newsletters, sales
Output STRICT JSON: {"important":["id",...],"updates":["id",...],"promotions":["id",...]}. Use only ids from the input. No commentary."#;

pub async fn triage(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let mailbox_id = match check_route(&req, &ctx).await {
        Ok(Some(id)) => id,
        Ok(None) => return not_found("Not found"),
        Err(e) => return auth_error_response(e),
    };

    // Triage body is `{}` — we don't bother parsing it.

    let emails_val = rpc_call(
        &ctx.env,
        &mailbox_id,
        "/rpc/get_emails",
        &json!({
            "folder": "inbox",
            "page": 1,
            "limit": 50,
            "sort_column": "date",
            "sort_direction": "DESC",
        }),
    )
    .await?;
    let emails = emails_val.as_array().cloned().unwrap_or_default();

    if emails.is_empty() {
        return Response::from_json(&json!({
            "categories": [
                { "label": "important", "glyph": "!", "count": 0, "threadIds": [] },
                { "label": "updates", "glyph": "●", "count": 0, "threadIds": [] },
                { "label": "promotions", "glyph": "○", "count": 0, "threadIds": [] },
            ]
        }));
    }

    let lines: Vec<String> = emails
        .iter()
        .map(|e| {
            let id = e.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let sender = e.get("sender").and_then(|v| v.as_str()).unwrap_or("");
            let subject = e.get("subject").and_then(|v| v.as_str()).unwrap_or("");
            format!("{id} | from={sender} | subj={subject}")
        })
        .collect();
    let user_text = lines.join("\n");

    let ai = ctx.env.ai("AI")?;
    let text = run_ai(&ai, AGENT_MODEL, SYS_TRIAGE, &user_text, 1500, 0.0).await?;
    let parsed: TriageAiShape = safe_parse_json(&text).unwrap_or_default();

    // id -> thread_id (fallback to id when thread_id missing).
    let mut id_to_thread: HashMap<String, String> = HashMap::with_capacity(emails.len());
    for e in &emails {
        let id = e.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let thread = e
            .get("thread_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(id);
        if !id.is_empty() {
            id_to_thread.insert(id.to_string(), thread.to_string());
        }
    }

    let to_thread_ids = |ids: Option<Vec<String>>| -> Vec<String> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut out: Vec<String> = Vec::new();
        for id in ids.unwrap_or_default() {
            if let Some(tid) = id_to_thread.get(&id) {
                if seen.insert(tid.clone()) {
                    out.push(tid.clone());
                }
            }
        }
        out
    };

    let important = to_thread_ids(parsed.important);
    let updates = to_thread_ids(parsed.updates);
    let promotions = to_thread_ids(parsed.promotions);

    Response::from_json(&json!({
        "categories": [
            { "label": "important", "glyph": "!", "count": important.len(), "threadIds": important },
            { "label": "updates", "glyph": "●", "count": updates.len(), "threadIds": updates },
            { "label": "promotions", "glyph": "○", "count": promotions.len(), "threadIds": promotions },
        ]
    }))
}
