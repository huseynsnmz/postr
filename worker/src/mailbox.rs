//! DO-stub + R2-listing helpers.
//!
//! Mirrors:
//!   * `getMailboxStub`  in worker/workers/lib/email-helpers.ts:23-30
//!   * `listMailboxes`   in worker/workers/lib/email-helpers.ts:37-45
//!   * `requireMailbox`  in worker/workers/lib/mailbox.ts:21-41 (R2 head check)

use worker::*;

use crate::types::MailboxBrief;

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

/// List all mailboxes by scanning the `mailboxes/` prefix in R2 and
/// stripping the `.json` suffix to recover the email address.
pub async fn list_mailboxes(env: &Env) -> Result<Vec<MailboxBrief>> {
    let bucket = env.bucket("BUCKET")?;
    let listing = bucket.list().prefix("mailboxes/").execute().await?;
    let mut out = Vec::new();
    for object in listing.objects() {
        let key = object.key();
        if let Some(email) = key
            .strip_prefix("mailboxes/")
            .and_then(|s| s.strip_suffix(".json"))
        {
            out.push(MailboxBrief {
                id: email.to_string(),
                address: email.to_string(),
            });
        }
    }
    Ok(out)
}
