//! Scheduled-event helpers. Currently just the daily trash sweep — fans
//! out across every registered mailbox and asks each DO to purge rows
//! that have been sitting in the `trash` folder for > 30 days.
//!
//! Wired in `worker/src/lib.rs` under `#[event(scheduled)]`, scheduled by
//! `wrangler.jsonc#triggers.crons`.

use serde_json::json;
use worker::*;

use crate::mailbox;
use crate::routes::emails::do_rpc_request;

/// Iterate every mailbox marker in R2, ask its DO to delete trash rows
/// older than 30 days, and best-effort clean up the freed attachments
/// in R2. Errors per mailbox are logged but don't abort the sweep.
pub async fn purge_old_trash(env: Env) -> Result<()> {
    let mailboxes = mailbox::list_mailboxes(&env).await?;
    if mailboxes.is_empty() {
        return Ok(());
    }

    let mut total_emails: u64 = 0;
    let mut total_attachments: u64 = 0;

    for mb in mailboxes {
        match purge_one_mailbox(&env, &mb.address).await {
            Ok((emails, atts)) => {
                total_emails += emails;
                total_attachments += atts;
                if emails > 0 {
                    console_log!(
                        "trash sweep: {} purged {emails} email(s), {atts} attachment blob(s)",
                        mb.address
                    );
                }
            }
            Err(e) => {
                console_error!("trash sweep failed for {}: {e}", mb.address);
            }
        }
    }

    if total_emails > 0 {
        console_log!(
            "trash sweep: purged {total_emails} email(s) and {total_attachments} attachment blob(s) across {} mailbox(es)",
            mailboxes_len(&env).await?
        );
    }
    Ok(())
}

async fn mailboxes_len(env: &Env) -> Result<usize> {
    Ok(mailbox::list_mailboxes(env).await?.len())
}

async fn purge_one_mailbox(env: &Env, mailbox_id: &str) -> Result<(u64, u64)> {
    let stub = mailbox::mailbox_stub(env, mailbox_id)?;
    let do_req = do_rpc_request("/rpc/purge_old_trash", &json!({ "days": 30 }))?;
    let mut resp = stub.fetch_with_request(do_req).await?;
    let payload: serde_json::Value = resp.json().await?;

    let arr = payload.as_array().cloned().unwrap_or_default();
    if arr.is_empty() {
        return Ok((0, 0));
    }

    // Best-effort R2 cleanup. Same key shape as the regular delete route.
    let bucket = env.bucket("BUCKET")?;
    let mut attachments_deleted: u64 = 0;
    for entry in &arr {
        let email_id = entry.get("email_id").and_then(|v| v.as_str()).unwrap_or("");
        let Some(atts) = entry.get("attachments").and_then(|v| v.as_array()) else {
            continue;
        };
        for att in atts {
            let id = att.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let filename = att.get("filename").and_then(|v| v.as_str()).unwrap_or("");
            if email_id.is_empty() || id.is_empty() || filename.is_empty() {
                continue;
            }
            let key = format!("attachments/{email_id}/{id}/{filename}");
            if bucket.delete(key).await.is_ok() {
                attachments_deleted += 1;
            }
        }
    }
    Ok((arr.len() as u64, attachments_deleted))
}
