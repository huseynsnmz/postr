use worker::*;

mod auth;
mod do_mailbox;
mod inbound;
mod mailbox;
mod routes;
mod types;

// Re-export the DO class so worker-build/wasm-bindgen can find it at the
// top level. The `#[durable_object]` macro on the struct (do_mailbox.rs)
// generates the `new`/`fetch` glue that the runtime invokes.
pub use do_mailbox::MailboxDO;

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    console_error_panic_hook::set_once();
    Router::new()
        .get("/api/v1/health", |_req, _ctx| {
            Response::from_json(&serde_json::json!({
                "ok": true,
                "worker": "postr-worker"
            }))
        })
        .get_async("/api/v1/cli/me", routes::cli::me)
        .post_async("/api/v1/cli/mailboxes", routes::cli::create_mailbox)
        .put_async(
            "/api/v1/cli/mailboxes/:mailboxId",
            routes::cli::update_mailbox,
        )
        .delete_async(
            "/api/v1/cli/mailboxes/:mailboxId",
            routes::cli::delete_mailbox,
        )
        .get_async("/api/v1/mailboxes/:mailboxId/emails", routes::emails::list)
        .get_async(
            "/api/v1/mailboxes/:mailboxId/emails/:emailId",
            routes::emails::get_one,
        )
        .put_async(
            "/api/v1/mailboxes/:mailboxId/emails/:emailId",
            routes::emails::update,
        )
        .delete_async(
            "/api/v1/mailboxes/:mailboxId/emails/:emailId",
            routes::emails::remove,
        )
        .post_async(
            "/api/v1/mailboxes/:mailboxId/emails/:emailId/move",
            routes::emails::move_to,
        )
        .post_async(
            "/api/v1/mailboxes/:mailboxId/emails",
            routes::send::send_fresh,
        )
        .post_async(
            "/api/v1/mailboxes/:mailboxId/emails/:emailId/reply",
            routes::send::send_reply,
        )
        .post_async(
            "/api/v1/mailboxes/:mailboxId/drafts",
            routes::drafts::create,
        )
        .get_async(
            "/api/v1/mailboxes/:mailboxId/threads/:threadId",
            routes::threads::get_one,
        )
        .post_async(
            "/api/v1/mailboxes/:mailboxId/threads/:threadId/read",
            routes::threads::mark_read,
        )
        .post_async(
            "/api/v1/mailboxes/:mailboxId/summarize",
            routes::ai::summarize,
        )
        .post_async("/api/v1/mailboxes/:mailboxId/draft", routes::ai::draft)
        .post_async("/api/v1/mailboxes/:mailboxId/ask", routes::ai::ask)
        .post_async("/api/v1/mailboxes/:mailboxId/triage", routes::ai::triage)
        .run(req, env)
        .await
}

#[event(email)]
async fn email_handler(message: ForwardableEmailMessage, env: Env, ctx: Context) -> Result<()> {
    console_error_panic_hook::set_once();
    inbound::receive_email(message, env, ctx).await
}
