//! DO-stub + R2-listing helpers.
//!
//! Mirrors:
//!   * `getMailboxStub`  in worker/workers/lib/email-helpers.ts:23-30
//!   * `listMailboxes`   in worker/workers/lib/email-helpers.ts:37-45
//!   * `requireMailbox`  in worker/workers/lib/mailbox.ts:21-41 (R2 head check)

use serde::{Deserialize, Serialize};
use worker::*;

use crate::types::MailboxBrief;

/// Body of `mailboxes/{address}.json` in R2. `display_name` and `alias`
/// were added after v1 — older objects parse fine because both fields are
/// optional.
#[derive(Debug, Serialize, Deserialize)]
pub struct MailboxRecord {
    pub address: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Short handle used by `/switch <alias>` in the TUI; doesn't affect
    /// outbound mail. Unique per worker (enforced by callers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
}

/// Normalize a path-parameter mailbox id: percent-decode, then lowercase.
///
/// `worker::Router` hands back path params *as captured* — `me%40x.com` arrives
/// literally with `%40`, not `@`. Without this normalization R2 keys won't
/// match the canonical `mailboxes/me@x.com.json` that `create_mailbox`
/// writes. Lowercasing matches the same canonicalization `create_mailbox`
/// applies on write.
pub fn normalize_mailbox_id(raw: &str) -> String {
    percent_decode(raw).to_lowercase()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Resolve a `MailboxDO` stub by mailbox id (email address).
pub fn mailbox_stub(env: &Env, mailbox_id: &str) -> Result<Stub> {
    let ns = env.durable_object("MAILBOX")?;
    let id = ns.id_from_name(&normalize_mailbox_id(mailbox_id))?;
    id.get_stub()
}

/// Return `Ok(true)` if `mailboxes/{mailbox_id}.json` exists in R2.
pub async fn require_mailbox(env: &Env, mailbox_id: &str) -> Result<bool> {
    let bucket = env.bucket("BUCKET")?;
    let key = format!("mailboxes/{}.json", normalize_mailbox_id(mailbox_id));
    Ok(bucket.head(key).await?.is_some())
}

/// Read and parse `mailboxes/{id}.json` from R2.
///
/// Returns `Ok(None)` when the object doesn't exist; `Ok(Some(rec))` with
/// a fresh record (filled-in `address` if the stored JSON is malformed or
/// older than the `MailboxRecord` shape).
pub async fn load_record(env: &Env, mailbox_id: &str) -> Result<Option<MailboxRecord>> {
    let bucket = env.bucket("BUCKET")?;
    let address = normalize_mailbox_id(mailbox_id);
    let key = format!("mailboxes/{address}.json");
    let Some(obj) = bucket.get(&key).execute().await? else {
        return Ok(None);
    };
    let Some(body) = obj.body() else {
        return Ok(Some(MailboxRecord {
            address,
            display_name: None,
            alias: None,
        }));
    };
    let bytes = body.bytes().await?;
    let rec: MailboxRecord = serde_json::from_slice(&bytes).unwrap_or(MailboxRecord {
        address: address.clone(),
        display_name: None,
        alias: None,
    });
    Ok(Some(rec))
}

/// Write the record back to R2, overwriting any existing object.
pub async fn save_record(env: &Env, rec: &MailboxRecord) -> Result<()> {
    let bucket = env.bucket("BUCKET")?;
    let key = format!("mailboxes/{}.json", rec.address);
    let body = serde_json::to_string(rec).map_err(|e| Error::RustError(e.to_string()))?;
    bucket.put(&key, body).execute().await?;
    Ok(())
}

/// Delete the R2 marker. Idempotent — silent if the key was already gone.
pub async fn delete_record(env: &Env, mailbox_id: &str) -> Result<()> {
    let bucket = env.bucket("BUCKET")?;
    let key = format!("mailboxes/{}.json", normalize_mailbox_id(mailbox_id));
    bucket.delete(&key).await?;
    Ok(())
}

/// List all mailboxes by scanning the `mailboxes/` prefix in R2 and
/// reading each object body to recover `display_name`.
pub async fn list_mailboxes(env: &Env) -> Result<Vec<MailboxBrief>> {
    let bucket = env.bucket("BUCKET")?;
    let listing = bucket.list().prefix("mailboxes/").execute().await?;
    let mut out = Vec::new();
    for object in listing.objects() {
        let key = object.key();
        let email = match key
            .strip_prefix("mailboxes/")
            .and_then(|s| s.strip_suffix(".json"))
        {
            Some(s) => s.to_string(),
            None => continue,
        };
        // Best-effort fetch of the body for the display_name + alias; if it
        // fails or the object is empty we still surface the mailbox bare.
        let rec = match bucket.get(&key).execute().await {
            Ok(Some(obj)) => match obj.body() {
                Some(body) => body
                    .bytes()
                    .await
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<MailboxRecord>(&bytes).ok()),
                None => None,
            },
            _ => None,
        };
        let (display_name, alias) = rec
            .map(|r| (r.display_name, r.alias))
            .unwrap_or((None, None));
        out.push(MailboxBrief {
            id: email.clone(),
            address: email,
            display_name,
            alias,
        });
    }
    Ok(out)
}
