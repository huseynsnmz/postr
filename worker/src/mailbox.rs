//! DO-stub + R2-listing helpers.
//!
//! Mirrors:
//!   * `getMailboxStub`  in worker/workers/lib/email-helpers.ts:23-30
//!   * `listMailboxes`   in worker/workers/lib/email-helpers.ts:37-45
//!   * `requireMailbox`  in worker/workers/lib/mailbox.ts:21-41 (R2 head check)

use worker::*;

use crate::types::MailboxBrief;

/// Resolve a `MailboxDO` stub by mailbox id (email address).
pub fn mailbox_stub(env: &Env, mailbox_id: &str) -> Result<Stub> {
    let ns = env.durable_object("MAILBOX")?;
    let id = ns.id_from_name(mailbox_id)?;
    id.get_stub()
}

/// Return `Ok(true)` if `mailboxes/{mailbox_id}.json` exists in R2.
pub async fn require_mailbox(env: &Env, mailbox_id: &str) -> Result<bool> {
    let bucket = env.bucket("BUCKET")?;
    let key = format!("mailboxes/{mailbox_id}.json");
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
