//! Compose-side helpers: single-line field editing, send/save-draft dispatch,
//! the `AppEvent` plumbing for the send pipeline.
//!
//! `ComposeState` constructors (`new_blank`/`new_reply`/`new_forward`) live in
//! `state::mod` next to the struct itself.

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::mpsc::UnboundedSender;

use crate::api::client::ApiError;
use crate::api::types::DraftInput;
use crate::api::ApiClient;
use crate::state::{ComposeKind, ComposeState};
use crate::tui::app::{AppEvent, ErrorKind};

/// Route compose-pipeline errors: `Unauthorized` falls through to the
/// canonical auth-failure flow; everything else surfaces as a flash on
/// the compose screen via `SendFailed`.
fn send_compose_error(
    tx: &tokio::sync::mpsc::UnboundedSender<AppEvent>,
    prefix: &str,
    err: ApiError,
) {
    if matches!(err, ApiError::Unauthorized) {
        let _ = tx.send(AppEvent::Error {
            kind: ErrorKind::Unauthorized,
            message: format!("{prefix}: {err}"),
        });
        return;
    }
    let _ = tx.send(AppEvent::SendFailed {
        err: format!("{prefix}: {err}"),
    });
}

/// Apply a key event to a single-line `(value, cursor)` pair. Cursor is a
/// byte offset into `value`. Only ASCII-friendly editing — same approach as
/// the rest of the TUI, which treats `to`/`subject` as simple text fields.
pub fn edit_single_line(value: &mut String, cursor: &mut usize, key: KeyEvent) {
    match key.code {
        KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            value.insert(*cursor, ch);
            *cursor += ch.len_utf8();
        }
        KeyCode::Backspace if *cursor > 0 => {
            let mut new_cursor = *cursor;
            let bytes = value.as_bytes();
            while new_cursor > 0 {
                new_cursor -= 1;
                if (bytes[new_cursor] & 0b1100_0000) != 0b1000_0000 {
                    break;
                }
            }
            value.replace_range(new_cursor..*cursor, "");
            *cursor = new_cursor;
        }
        KeyCode::Delete if *cursor < value.len() => {
            let bytes = value.as_bytes();
            let mut end = *cursor + 1;
            while end < bytes.len() && (bytes[end] & 0b1100_0000) == 0b1000_0000 {
                end += 1;
            }
            value.replace_range(*cursor..end, "");
        }
        KeyCode::Left if *cursor > 0 => {
            let bytes = value.as_bytes();
            let mut new_cursor = *cursor;
            while new_cursor > 0 {
                new_cursor -= 1;
                if (bytes[new_cursor] & 0b1100_0000) != 0b1000_0000 {
                    break;
                }
            }
            *cursor = new_cursor;
        }
        KeyCode::Right if *cursor < value.len() => {
            let bytes = value.as_bytes();
            let mut new_cursor = *cursor + 1;
            while new_cursor < bytes.len() && (bytes[new_cursor] & 0b1100_0000) == 0b1000_0000 {
                new_cursor += 1;
            }
            *cursor = new_cursor;
        }
        KeyCode::Home => *cursor = 0,
        KeyCode::End => *cursor = value.len(),
        _ => {}
    }
}

fn build_draft(compose: &ComposeState) -> DraftInput {
    DraftInput {
        to: Some(compose.to.clone()),
        subject: Some(compose.subject.clone()),
        body: compose.body_text(),
        in_reply_to: match &compose.kind {
            ComposeKind::Reply { in_reply_to, .. } => Some(in_reply_to.clone()),
            // Forward is treated as a fresh send in v1 — the Worker has no
            // /forward route exposed via send_draft and a Fwd: doesn't set
            // in_reply_to. TODO(phase5): plumb a dedicated forward route.
            _ => None,
        },
        thread_id: match &compose.kind {
            ComposeKind::Reply { thread_id, .. } => thread_id.clone(),
            ComposeKind::Forward { thread_id, .. } => thread_id.clone(),
            _ => None,
        },
        ..Default::default()
    }
}

pub fn spawn_send(
    client: Arc<ApiClient>,
    mailbox_id: String,
    compose: &ComposeState,
    tx: UnboundedSender<AppEvent>,
) {
    let draft = build_draft(compose);
    let first_to = compose
        .to
        .split(',')
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    tokio::spawn(async move {
        match client.save_draft(&mailbox_id, &draft).await {
            Ok(saved) => match client.send_draft(&mailbox_id, &saved.id).await {
                Ok(()) => {
                    let _ = tx.send(AppEvent::Sent { to: first_to });
                }
                Err(e) => send_compose_error(&tx, "send", e),
            },
            Err(e) => send_compose_error(&tx, "draft", e),
        }
    });
}

pub fn spawn_save_draft(
    client: Arc<ApiClient>,
    mailbox_id: String,
    compose: &ComposeState,
    tx: UnboundedSender<AppEvent>,
) {
    let draft = build_draft(compose);
    tokio::spawn(async move {
        match client.save_draft(&mailbox_id, &draft).await {
            Ok(_) => {
                let _ = tx.send(AppEvent::DraftSaved);
            }
            Err(e) => send_compose_error(&tx, "draft", e),
        }
    });
}
