//! Background spawns for the AI slash commands (`/summarize`, `/draft`,
//! `/ask`, `/triage`). Mirrors `compose::spawn_send` in style — each
//! function `tokio::spawn`s, calls the matching `ApiClient` method, and
//! drops the result into the channel as an `AppEvent`. The render loop
//! decides what to do with success/failure based on the current `Mode`.

use std::sync::Arc;

use tokio::sync::mpsc::UnboundedSender;

use crate::api::client::ApiError;
use crate::api::ApiClient;
use crate::state::AiKind;
use crate::tui::app::{AppEvent, ErrorKind};

/// Route an `ApiError` to the right event: `Unauthorized` goes through the
/// canonical auth-failure flow (clears the keyring) instead of leaving the
/// AI panel stuck on a generic "failed" line.
fn send_ai_error(tx: &UnboundedSender<AppEvent>, kind: AiKind, err: ApiError) {
    if matches!(err, ApiError::Unauthorized) {
        let _ = tx.send(AppEvent::Error {
            kind: ErrorKind::Unauthorized,
            message: format!("/{}: {err}", kind.name()),
        });
        return;
    }
    let _ = tx.send(AppEvent::AiFailed {
        kind,
        err: err.to_string(),
    });
}

pub fn spawn_summarize(
    client: Arc<ApiClient>,
    mailbox_id: String,
    thread_id: String,
    tx: UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        match client.summarize(&mailbox_id, &thread_id).await {
            Ok(resp) => {
                let _ = tx.send(AppEvent::AiSummarizeDone(resp));
            }
            Err(e) => send_ai_error(&tx, AiKind::Summarize, e),
        }
    });
}

pub fn spawn_draft(
    client: Arc<ApiClient>,
    mailbox_id: String,
    prompt: String,
    thread_id: Option<String>,
    tx: UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        match client
            .draft(&mailbox_id, &prompt, thread_id.as_deref())
            .await
        {
            Ok(resp) => {
                let _ = tx.send(AppEvent::AiDraftDone(resp, prompt, thread_id));
            }
            Err(e) => send_ai_error(&tx, AiKind::Draft, e),
        }
    });
}

pub fn spawn_ask(
    client: Arc<ApiClient>,
    mailbox_id: String,
    query: String,
    tx: UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        match client.ask(&mailbox_id, &query).await {
            Ok(resp) => {
                let _ = tx.send(AppEvent::AiAskDone(resp, query));
            }
            Err(e) => send_ai_error(&tx, AiKind::Ask, e),
        }
    });
}

pub fn spawn_triage(client: Arc<ApiClient>, mailbox_id: String, tx: UnboundedSender<AppEvent>) {
    tokio::spawn(async move {
        match client.triage(&mailbox_id).await {
            Ok(resp) => {
                let _ = tx.send(AppEvent::AiTriageDone(resp));
            }
            Err(e) => send_ai_error(&tx, AiKind::Triage, e),
        }
    });
}
