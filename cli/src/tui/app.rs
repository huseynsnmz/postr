//! Async TUI driver: owns the `App` value, the crossterm event stream, and
//! the channel that worker tasks (inbox/thread loads, archive/star/delete,
//! compose send/save) use to feed results back into the render loop.

use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

use crate::api::types::{
    AiDraftResponse, AskResponse, EmailList, SummarizeResponse, ThreadFull, TriageResponse,
};
use crate::api::ApiClient;
use crate::config::Config;
use crate::state::{
    Account, AiKind, AiResultState, AppState, CommandState, ComposeField, ComposeKind,
    ComposeState, LoadingKind, Mode, PriorMode,
};
use crate::tui::command::SLASH_COMMANDS;
use crate::tui::render;

/// Coarse classification of an `ApiError` so the render loop can branch on
/// remediation (e.g. `Unauthorized` wipes the keyring) without re-matching
/// on the underlying `ApiError` variant.
#[derive(Debug, Clone, Copy)]
pub enum ErrorKind {
    Unauthorized,
    NotFound,
    Server,
    Network,
    Decode,
    #[allow(dead_code)] // reserved for non-ApiError failures (e.g. send pipeline glue)
    Generic,
}

impl From<&crate::api::client::ApiError> for ErrorKind {
    fn from(e: &crate::api::client::ApiError) -> Self {
        use crate::api::client::ApiError as E;
        match e {
            E::Unauthorized => ErrorKind::Unauthorized,
            E::NotFound => ErrorKind::NotFound,
            E::Server(_, _) => ErrorKind::Server,
            E::Network(_) => ErrorKind::Network,
            E::Decode(_) => ErrorKind::Decode,
        }
    }
}

/// Messages from background tasks back into the render loop.
#[derive(Debug)]
pub enum AppEvent {
    InboxLoaded(EmailList),
    ThreadLoaded(ThreadFull),
    ActionDone(String),
    Error {
        kind: ErrorKind,
        message: String,
    },
    Sent {
        to: String,
    },
    SendFailed {
        err: String,
    },
    DraftSaved,
    AiSummarizeDone(SummarizeResponse),
    /// `prompt` echoed back so `⌃r regenerate` can re-spawn; `thread_id`
    /// preserved for the same reason.
    AiDraftDone(AiDraftResponse, String, Option<String>),
    /// `query` echoed back so the result panel can render the prompt.
    AiAskDone(AskResponse, String),
    AiTriageDone(TriageResponse),
    AiFailed {
        kind: AiKind,
        err: String,
    },
    /// Periodic tick so the feedback line can clear itself after its TTL
    /// elapses without polling per-render.
    FeedbackExpired,
}

pub struct App {
    pub state: AppState,
    pub should_quit: bool,
    pub client: Arc<ApiClient>,
    pub mailbox_id: String,
    pub tx: UnboundedSender<AppEvent>,
    /// Transient context captured at `/summarize` spawn-time so the result
    /// arm can wire up Compose-Reply without re-reading `ReadingState`
    /// (which the user may have closed). Tuple is
    /// `(source_email_id, thread_id, sender)`.
    summarize_context: Option<(String, Option<String>, String)>,
}

impl App {
    pub fn new(client: Arc<ApiClient>, account: Account, tx: UnboundedSender<AppEvent>) -> Self {
        let mailbox_id = account.mailbox_id.clone();
        Self {
            state: AppState::empty(account),
            should_quit: false,
            client,
            mailbox_id,
            tx,
            summarize_context: None,
        }
    }

    // ── Background spawns ────────────────────────────────────────

    pub fn spawn_load_inbox(&self) {
        let client = self.client.clone();
        let mb = self.mailbox_id.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match client.get_inbox_list(&mb).await {
                Ok(list) => {
                    let _ = tx.send(AppEvent::InboxLoaded(list));
                }
                Err(e) => {
                    let _ = tx.send(AppEvent::Error {
                        kind: (&e).into(),
                        message: format!("inbox: {e}"),
                    });
                }
            }
        });
    }

    pub fn spawn_open_selected(&mut self) {
        let Some(m) = self.state.selected_meta().cloned() else {
            return;
        };
        // Don't let `u` resurrect a stale archive after the user navigates.
        self.state.last_undoable = None;
        self.state.mode = Mode::Loading(LoadingKind::Thread);
        let client = self.client.clone();
        let mb = self.mailbox_id.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = match m.meta.thread_id.as_deref() {
                Some(tid) => client.get_thread(&mb, tid).await,
                None => client.get_email(&mb, &m.meta.id).await.map(|e| vec![e]),
            };
            match result {
                Ok(t) => {
                    let _ = tx.send(AppEvent::ThreadLoaded(t));
                }
                Err(e) => {
                    let _ = tx.send(AppEvent::Error {
                        kind: (&e).into(),
                        message: format!("thread: {e}"),
                    });
                }
            }
        });
    }

    pub fn spawn_star(&self, email_id: String, on: bool) {
        let client = self.client.clone();
        let mb = self.mailbox_id.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match client.star(&mb, &email_id, on).await {
                Ok(_) => {
                    let _ = tx.send(AppEvent::ActionDone(if on {
                        "starred".into()
                    } else {
                        "unstarred".into()
                    }));
                }
                Err(e) => {
                    let _ = tx.send(AppEvent::Error {
                        kind: (&e).into(),
                        message: format!("star: {e}"),
                    });
                }
            }
        });
    }

    pub fn spawn_archive(&mut self, email_id: String) {
        // Record the undo handle *before* the move, so a fast `u` press
        // after the flash can restore the row. Worker errors leave the
        // stale entry in place — at worst, `u` tries to move an email
        // that never moved and the user sees an error flash.
        self.state.last_undoable = Some(crate::state::UndoableAction::Archive {
            email_id: email_id.clone(),
            prior_folder: "inbox".to_string(),
            recorded_at: std::time::Instant::now(),
        });
        let client = self.client.clone();
        let mb = self.mailbox_id.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match client.archive(&mb, &email_id).await {
                Ok(_) => {
                    let _ = tx.send(AppEvent::ActionDone("Archived · u to undo".into()));
                    match client.get_inbox_list(&mb).await {
                        Ok(list) => {
                            let _ = tx.send(AppEvent::InboxLoaded(list));
                        }
                        Err(e) => {
                            let _ = tx.send(AppEvent::Error {
                                kind: (&e).into(),
                                message: format!("inbox: {e}"),
                            });
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.send(AppEvent::Error {
                        kind: (&e).into(),
                        message: format!("archive: {e}"),
                    });
                }
            }
        });
    }

    pub fn spawn_delete(&self, email_id: String) {
        let client = self.client.clone();
        let mb = self.mailbox_id.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match client.delete_email(&mb, &email_id).await {
                Ok(_) => {
                    let _ = tx.send(AppEvent::ActionDone("deleted".into()));
                    match client.get_inbox_list(&mb).await {
                        Ok(list) => {
                            let _ = tx.send(AppEvent::InboxLoaded(list));
                        }
                        Err(e) => {
                            let _ = tx.send(AppEvent::Error {
                                kind: (&e).into(),
                                message: format!("inbox: {e}"),
                            });
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.send(AppEvent::Error {
                        kind: (&e).into(),
                        message: format!("delete: {e}"),
                    });
                }
            }
        });
    }

    /// Restore the last archived email (within the 30s undo window).
    /// Currently the only undoable action is `Archive`; delete/send are
    /// not reversible from the CLI side.
    pub fn spawn_undo(&mut self) {
        let Some(action) = self.state.last_undoable.take() else {
            self.state.flash_info("Nothing to undo");
            return;
        };
        if action.is_expired() {
            self.state.flash_info("Undo expired");
            return;
        }
        let crate::state::UndoableAction::Archive {
            email_id,
            prior_folder,
            ..
        } = action;
        let client = self.client.clone();
        let mb = self.mailbox_id.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match client.move_email(&mb, &email_id, &prior_folder).await {
                Ok(_) => {
                    let _ = tx.send(AppEvent::ActionDone("Restored".into()));
                    match client.get_inbox_list(&mb).await {
                        Ok(list) => {
                            let _ = tx.send(AppEvent::InboxLoaded(list));
                        }
                        Err(e) => {
                            let _ = tx.send(AppEvent::Error {
                                kind: (&e).into(),
                                message: format!("inbox: {e}"),
                            });
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.send(AppEvent::Error {
                        kind: (&e).into(),
                        message: format!("undo: {e}"),
                    });
                }
            }
        });
    }

    // ── Event handlers ───────────────────────────────────────────

    pub fn on_event(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::InboxLoaded(list) => self.state.set_inbox(list),
            AppEvent::ThreadLoaded(t) => self.state.set_thread(t),
            AppEvent::ActionDone(s) => self.state.flash_success(s),
            AppEvent::Error { kind, message } => match kind {
                ErrorKind::Unauthorized => self.handle_auth_failure(),
                _ => self.state.set_error(message),
            },
            AppEvent::Sent { to } => {
                self.state.compose = None;
                self.state.mode = Mode::Inbox;
                self.state.flash_success(format!("Sent to {to}"));
                self.spawn_load_inbox();
            }
            AppEvent::SendFailed { err } => {
                if let Some(c) = self.state.compose.as_mut() {
                    c.submitting = false;
                }
                self.state.flash_error(format!("Send failed: {err}"));
            }
            AppEvent::DraftSaved => self.state.flash_success("Draft saved"),
            AppEvent::AiSummarizeDone(r) => self.on_ai_summarize_done(r),
            AppEvent::AiDraftDone(r, prompt, tid) => self.on_ai_draft_done(r, prompt, tid),
            AppEvent::AiAskDone(r, q) => self.on_ai_ask_done(r, q),
            AppEvent::AiTriageDone(r) => self.on_ai_triage_done(r),
            AppEvent::AiFailed { kind, err } => self.on_ai_failed(kind, err),
            AppEvent::FeedbackExpired => {
                self.state.clear_feedback_if_expired();
                self.state.clear_undo_if_expired();
            }
        }
    }

    /// Token rejected by the Worker. Wipe the keyring entry and lock the
    /// TUI on an Error screen with explicit remediation. Don't auto-quit —
    /// let the user read it, then press Esc/q.
    fn handle_auth_failure(&mut self) {
        // Best-effort: if keyring access fails we still want the error
        // screen, so swallow the result.
        let _ = crate::config::keyring::delete_token();
        self.state.mode = Mode::Error(
            "Token rejected. Quit (q) and run `postr login <url>` again to re-authenticate."
                .to_string(),
        );
    }

    /// Re-parse the open message's body at the new terminal width so the
    /// wrapped lines don't stay frozen against the prior column count.
    /// Only meaningful while Reading; other modes recompute on next draw.
    pub fn handle_resize(&mut self, width: u16) {
        // Cache for the very next draw too — render normally writes this
        // but resize events can arrive between draws.
        self.state.body_wrap_width = width;
        if !matches!(self.state.mode, Mode::Reading) {
            return;
        }
        let Some(r) = self.state.reading.as_mut() else {
            return;
        };
        let raw = r.thread[r.message_idx].body.as_deref().unwrap_or("");
        let (visible, quoted) = crate::tui::body::parse_body(raw, width as usize);
        r.body_lines = visible;
        r.quoted_lines = quoted;
        // Scroll offset stays in old line-count units; clamp to the new max
        // so we don't render past EOF.
        let max_scroll = r.body_lines.len().saturating_sub(1) as u16;
        if r.scroll > max_scroll {
            r.scroll = max_scroll;
        }
    }

    fn on_ai_summarize_done(&mut self, r: SummarizeResponse) {
        let prior = match self.state.mode {
            Mode::AiPending {
                kind: AiKind::Summarize,
                prior,
            } => prior,
            // User dismissed the pending panel — drop the late result.
            _ => return,
        };
        let (source_email_id, thread_id, source_sender) =
            self.summarize_context.take().unwrap_or_default();
        self.state.ai = Some(AiResultState::Summarize {
            thread_subject: r.thread.subject,
            message_count: r.thread.message_count,
            people_count: r.thread.people_count,
            bullets: r.summary,
            suggested_replies: r.suggested_replies,
            selected_reply: None,
            source_email_id,
            thread_id,
            source_sender,
        });
        self.state.mode = Mode::AiResult {
            kind: AiKind::Summarize,
            prior,
        };
    }

    fn on_ai_draft_done(&mut self, r: AiDraftResponse, prompt: String, thread_id: Option<String>) {
        let prior = match self.state.mode {
            Mode::AiPending {
                kind: AiKind::Draft,
                prior,
            } => prior,
            _ => return,
        };
        self.state.ai = Some(AiResultState::Draft {
            echo_prompt: prompt,
            thread_id,
            to: r.to,
            subject: r.subject,
            body: r.body,
        });
        self.state.mode = Mode::AiResult {
            kind: AiKind::Draft,
            prior,
        };
    }

    fn on_ai_ask_done(&mut self, r: AskResponse, query: String) {
        let prior = match self.state.mode {
            Mode::AiPending {
                kind: AiKind::Ask,
                prior,
            } => prior,
            _ => return,
        };
        self.state.ai = Some(AiResultState::Ask {
            echo_query: query,
            summary: r.summary,
            results: r.results,
            selected_index: 0,
        });
        self.state.mode = Mode::AiResult {
            kind: AiKind::Ask,
            prior,
        };
    }

    fn on_ai_triage_done(&mut self, r: TriageResponse) {
        let prior = match self.state.mode {
            Mode::AiPending {
                kind: AiKind::Triage,
                prior,
            } => prior,
            _ => return,
        };
        self.state.ai = Some(AiResultState::Triage {
            categories: r.categories,
        });
        self.state.mode = Mode::AiResult {
            kind: AiKind::Triage,
            prior,
        };
    }

    fn on_ai_failed(&mut self, kind: AiKind, err: String) {
        let prior = match self.state.mode {
            Mode::AiPending { prior, .. } | Mode::AiResult { prior, .. } => prior,
            _ => return, // late failure for a command the user already dismissed
        };
        self.state.ai = None;
        self.summarize_context = None;
        self.state.mode = match prior {
            PriorMode::Inbox => Mode::Inbox,
            PriorMode::Reading => Mode::Reading,
        };
        self.state
            .flash_error(format!("/{} failed: {err}", kind.name()));
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        // Ctrl+C is always a hard exit, even from compose/menu.
        if matches!(key.code, KeyCode::Char('c')) && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.should_quit = true;
            return;
        }
        // Bare `q` only quits when we're not in a typing-mode (compose,
        // command menu, discard confirm) — otherwise it's just a character.
        let is_typing = matches!(
            self.state.mode,
            Mode::Composing | Mode::Command { .. } | Mode::ComposeDiscardConfirm
        );
        if !is_typing && matches!(key.code, KeyCode::Char('q')) {
            self.should_quit = true;
            return;
        }

        let mode = self.state.mode.clone();
        match mode {
            Mode::Loading(_) => {}
            Mode::Error(_) => match key.code {
                KeyCode::Esc | KeyCode::Enter => {
                    self.state.dismiss_error();
                    if self.state.messages.is_empty() {
                        // If the token was just wiped (auth failure), the
                        // next inbox load would fail immediately. Quit
                        // instead so the user can re-login.
                        match crate::config::keyring::load_token() {
                            Ok(Some(_)) => {
                                self.state.mode = Mode::Loading(LoadingKind::Inbox);
                                self.spawn_load_inbox();
                            }
                            _ => self.should_quit = true,
                        }
                    }
                }
                _ => {}
            },
            Mode::Inbox => self.handle_key_inbox(key),
            Mode::Reading => self.handle_key_reading(key),
            Mode::Command { .. } => self.handle_key_command(key),
            Mode::Composing => self.handle_key_compose(key),
            Mode::ComposeDiscardConfirm => self.handle_key_discard_confirm(key),
            Mode::AiPending { prior, .. } => self.handle_key_ai_pending(key, prior),
            Mode::AiResult { kind, prior } => self.handle_key_ai_result(kind, prior, key),
        }
    }

    fn handle_key_ai_pending(&mut self, key: KeyEvent, prior: PriorMode) {
        // Esc cancels — the spawned task keeps running but its `*Done` arm
        // will no-op because the mode no longer matches `AiPending`.
        if matches!(key.code, KeyCode::Esc) {
            self.summarize_context = None;
            self.state.mode = match prior {
                PriorMode::Inbox => Mode::Inbox,
                PriorMode::Reading => Mode::Reading,
            };
        }
    }

    fn handle_key_ai_result(&mut self, kind: AiKind, prior: PriorMode, key: KeyEvent) {
        if matches!(key.code, KeyCode::Esc) {
            self.dismiss_ai_result(prior);
            return;
        }
        match kind {
            AiKind::Summarize => self.handle_key_ai_summarize(key, prior),
            AiKind::Draft => self.handle_key_ai_draft(key, prior),
            AiKind::Ask => self.handle_key_ai_ask(key, prior),
            AiKind::Triage => {
                if matches!(key.code, KeyCode::Enter) {
                    self.dismiss_ai_result(prior);
                }
            }
        }
    }

    fn dismiss_ai_result(&mut self, prior: PriorMode) {
        self.state.clear_ai();
        self.state.mode = match prior {
            PriorMode::Inbox => Mode::Inbox,
            PriorMode::Reading => Mode::Reading,
        };
    }

    fn handle_key_ai_summarize(&mut self, key: KeyEvent, _prior: PriorMode) {
        match key.code {
            KeyCode::Char(ch @ '1'..='3') => {
                if let Some(AiResultState::Summarize {
                    suggested_replies,
                    selected_reply,
                    ..
                }) = self.state.ai.as_mut()
                {
                    let idx = (ch as u8 - b'1') as usize;
                    if idx < suggested_replies.len() {
                        *selected_reply = Some(idx);
                    }
                }
            }
            KeyCode::Char('e') | KeyCode::Enter => self.enter_compose_from_summary(),
            _ => {}
        }
    }

    fn handle_key_ai_draft(&mut self, key: KeyEvent, _prior: PriorMode) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter if ctrl => self.send_ai_draft_now(),
            KeyCode::Char('r') if ctrl => self.regenerate_ai_draft(),
            KeyCode::Char('e') => self.open_ai_draft_in_compose(),
            _ => {}
        }
    }

    fn handle_key_ai_ask(&mut self, key: KeyEvent, _prior: PriorMode) {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                if let Some(AiResultState::Ask {
                    results,
                    selected_index,
                    ..
                }) = self.state.ai.as_mut()
                {
                    if !results.is_empty() {
                        *selected_index = (*selected_index + 1).min(results.len() - 1);
                    }
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if let Some(AiResultState::Ask { selected_index, .. }) = self.state.ai.as_mut() {
                    *selected_index = selected_index.saturating_sub(1);
                }
            }
            KeyCode::Enter => self.open_selected_ask_result(),
            _ => {}
        }
    }

    fn open_selected_ask_result(&mut self) {
        let thread_id = match self.state.ai.as_ref() {
            Some(AiResultState::Ask {
                results,
                selected_index,
                ..
            }) => results.get(*selected_index).map(|r| r.thread_id.clone()),
            _ => None,
        };
        let Some(tid) = thread_id else {
            return;
        };
        self.state.clear_ai();
        self.state.mode = Mode::Loading(LoadingKind::Thread);
        let client = self.client.clone();
        let mb = self.mailbox_id.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match client.get_thread(&mb, &tid).await {
                Ok(t) => {
                    let _ = tx.send(AppEvent::ThreadLoaded(t));
                }
                Err(e) => {
                    let _ = tx.send(AppEvent::Error {
                        kind: (&e).into(),
                        message: format!("thread: {e}"),
                    });
                }
            }
        });
    }

    fn enter_compose_from_summary(&mut self) {
        // Build a Reply ComposeState directly from the captured summary
        // context — we don't necessarily still have an EmailFull on hand.
        let Some(AiResultState::Summarize {
            thread_subject,
            suggested_replies,
            selected_reply,
            source_email_id,
            thread_id,
            source_sender,
            ..
        }) = self.state.ai.as_ref()
        else {
            return;
        };
        let body_text = selected_reply
            .and_then(|i| suggested_replies.get(i))
            .cloned();
        if body_text.is_none() {
            self.state.flash_info("Press 1-3 to choose a reply first");
            return;
        }
        let body_text = body_text.unwrap_or_default();
        let subject = format!("Re: {thread_subject}");
        let subject_cursor = subject.len();
        let to = source_sender.clone();
        let to_cursor = to.len();
        let kind = ComposeKind::Reply {
            in_reply_to: source_email_id.clone(),
            thread_id: thread_id.clone(),
            source_sender: source_sender.clone(),
        };
        let mut body = tui_textarea::TextArea::default();
        body.insert_str(&body_text);
        let compose = ComposeState {
            kind,
            to,
            to_cursor,
            subject,
            subject_cursor,
            body,
            focused: ComposeField::Body,
            submitting: false,
        };
        self.state.clear_ai();
        self.state.compose = Some(compose);
        self.state.mode = Mode::Composing;
    }

    fn open_ai_draft_in_compose(&mut self) {
        let Some(AiResultState::Draft {
            to, subject, body, ..
        }) = self.state.ai.take()
        else {
            return;
        };
        // The Worker /draft response doesn't expose draftId (see worker
        // routes/ai.ts), so we always route through compose-state. Reply
        // wiring is impossible here without the source email id — treat
        // every AI draft as a fresh send.
        let mut c = ComposeState::new_blank();
        c.to = to;
        c.to_cursor = c.to.len();
        c.subject = subject;
        c.subject_cursor = c.subject.len();
        c.body.insert_str(&body);
        c.focused = ComposeField::Body;
        self.state.compose = Some(c);
        self.state.mode = Mode::Composing;
    }

    fn send_ai_draft_now(&mut self) {
        self.open_ai_draft_in_compose();
        self.submit_compose();
    }

    fn regenerate_ai_draft(&mut self) {
        let (prompt, tid, prior) = match self.state.mode {
            Mode::AiResult {
                kind: AiKind::Draft,
                prior,
            } => match self.state.ai.as_ref() {
                Some(AiResultState::Draft {
                    echo_prompt,
                    thread_id,
                    ..
                }) => (echo_prompt.clone(), thread_id.clone(), prior),
                _ => return,
            },
            _ => return,
        };
        self.state.clear_ai();
        self.state.mode = Mode::AiPending {
            kind: AiKind::Draft,
            prior,
        };
        crate::tui::ai::spawn_draft(
            self.client.clone(),
            self.mailbox_id.clone(),
            prompt,
            tid,
            self.tx.clone(),
        );
    }

    fn handle_key_inbox(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.state.selected_next(),
            KeyCode::Char('k') | KeyCode::Up => self.state.selected_prev(),
            KeyCode::Char('g') => self.state.selected_index = 0,
            KeyCode::Char('G') if !self.state.messages.is_empty() => {
                self.state.selected_index = self.state.messages.len() - 1;
            }
            KeyCode::Char(ch @ '1'..='9') => {
                let idx = (ch as u8 - b'1') as usize;
                if idx < self.state.messages.len() {
                    self.state.selected_index = idx;
                }
            }
            KeyCode::Enter => self.spawn_open_selected(),
            KeyCode::Char('s') => {
                if let Some(m) = self.state.selected_meta().cloned() {
                    let new_val = !m.meta.starred;
                    if let Some(row) = self.state.messages.get_mut(self.state.selected_index) {
                        row.meta.starred = new_val;
                    }
                    self.spawn_star(m.meta.id, new_val);
                }
            }
            KeyCode::Char('e') => {
                if let Some(m) = self.state.selected_meta().cloned() {
                    self.spawn_archive(m.meta.id);
                }
            }
            KeyCode::Char('d') => {
                if let Some(m) = self.state.selected_meta().cloned() {
                    self.spawn_delete(m.meta.id);
                }
            }
            KeyCode::Char('u') => self.spawn_undo(),
            KeyCode::Char('/') => self.open_command_menu(PriorMode::Inbox),
            KeyCode::Char('c') => self.enter_compose_new(),
            KeyCode::Char('r') | KeyCode::Char('a') | KeyCode::Char('f') => {
                self.state.flash_info("Open a message first");
            }
            _ => {}
        }
    }

    fn handle_key_reading(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.state.mode = Mode::Inbox;
                self.state.reading = None;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                let width = self.state.body_wrap_width as usize;
                if let Some(r) = self.state.reading.as_mut() {
                    let last = r.thread.len().saturating_sub(1);
                    if r.message_idx < last {
                        r.message_idx += 1;
                        let raw = r.thread[r.message_idx].body.as_deref().unwrap_or("");
                        let (visible, quoted) = crate::tui::body::parse_body(raw, width);
                        r.body_lines = visible;
                        r.quoted_lines = quoted;
                        r.quoted_collapsed = true;
                        r.scroll = 0;
                    } else {
                        r.scroll = r.scroll.saturating_add(1);
                    }
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                let width = self.state.body_wrap_width as usize;
                if let Some(r) = self.state.reading.as_mut() {
                    if r.scroll > 0 {
                        r.scroll -= 1;
                    } else if r.message_idx > 0 {
                        r.message_idx -= 1;
                        let raw = r.thread[r.message_idx].body.as_deref().unwrap_or("");
                        let (visible, quoted) = crate::tui::body::parse_body(raw, width);
                        r.body_lines = visible;
                        r.quoted_lines = quoted;
                        r.quoted_collapsed = true;
                        r.scroll = 0;
                    }
                }
            }
            KeyCode::Char('z') => {
                if let Some(r) = self.state.reading.as_mut() {
                    r.quoted_collapsed = !r.quoted_collapsed;
                }
            }
            KeyCode::Char('s') => {
                if let Some(r) = self.state.reading.as_ref() {
                    let msg = &r.thread[r.message_idx];
                    let new_val = !msg.starred;
                    let id = msg.id.clone();
                    if let Some(r_mut) = self.state.reading.as_mut() {
                        r_mut.thread[r_mut.message_idx].starred = new_val;
                    }
                    self.spawn_star(id, new_val);
                }
            }
            KeyCode::Char('e') => {
                if let Some(r) = self.state.reading.as_ref() {
                    let id = r.thread[r.message_idx].id.clone();
                    self.spawn_archive(id);
                    self.state.mode = Mode::Inbox;
                    self.state.reading = None;
                }
            }
            KeyCode::Char('d') => {
                if let Some(r) = self.state.reading.as_ref() {
                    let id = r.thread[r.message_idx].id.clone();
                    self.spawn_delete(id);
                    self.state.mode = Mode::Inbox;
                    self.state.reading = None;
                }
            }
            KeyCode::Char('/') => self.open_command_menu(PriorMode::Reading),
            KeyCode::Char('r') | KeyCode::Char('a') => self.enter_compose_reply(),
            KeyCode::Char('f') => self.enter_compose_forward(),
            KeyCode::Char('o') => {
                self.state.flash_info("Link open lands later");
            }
            _ => {}
        }
    }

    fn handle_key_command(&mut self, key: KeyEvent) {
        let prior = match &self.state.mode {
            Mode::Command { prior } => *prior,
            _ => return,
        };
        // We need to mutate `cmd` and sometimes call helpers on `self` — peel
        // the mutable borrow apart by extracting an action first.
        enum Action {
            Run { idx: usize, args: String },
            Close,
            None,
        }
        let mut action = Action::None;
        if let Some(cmd) = self.state.command.as_mut() {
            match key.code {
                KeyCode::Esc => action = Action::Close,
                KeyCode::Enter if !cmd.filtered.is_empty() => {
                    action = Action::Run {
                        idx: cmd.filtered[cmd.selected],
                        args: crate::tui::command::split_args(&cmd.query).to_string(),
                    };
                }
                KeyCode::Down if !cmd.filtered.is_empty() => {
                    cmd.selected = (cmd.selected + 1).min(cmd.filtered.len() - 1);
                }
                KeyCode::Up => cmd.selected = cmd.selected.saturating_sub(1),
                KeyCode::Char('j')
                    if cmd.query.is_empty()
                        && key.modifiers.is_empty()
                        && !cmd.filtered.is_empty() =>
                {
                    cmd.selected = (cmd.selected + 1).min(cmd.filtered.len() - 1);
                }
                KeyCode::Char('k') if cmd.query.is_empty() && key.modifiers.is_empty() => {
                    cmd.selected = cmd.selected.saturating_sub(1);
                }
                KeyCode::Backspace => cmd.backspace(prior),
                KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    cmd.push_char(ch, prior);
                }
                _ => {}
            }
        }
        match action {
            Action::Run { idx, args } => self.run_slash_command(idx, &args),
            Action::Close => self.close_command_menu(),
            Action::None => {}
        }
    }

    fn handle_key_compose(&mut self, key: KeyEvent) {
        // Compose-wide control hotkeys come first.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Enter | KeyCode::Char('s') => {
                    self.submit_compose();
                    return;
                }
                KeyCode::Char('d') => {
                    self.save_compose_draft();
                    return;
                }
                KeyCode::Char('a') => {
                    self.state.flash_info("Attachments land later");
                    return;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Esc => {
                let dirty = self
                    .state
                    .compose
                    .as_ref()
                    .map(|c| c.has_unsaved())
                    .unwrap_or(false);
                if dirty {
                    self.state.mode = Mode::ComposeDiscardConfirm;
                } else {
                    self.state.compose = None;
                    self.state.mode = Mode::Inbox;
                }
            }
            KeyCode::Tab => {
                if let Some(c) = self.state.compose.as_mut() {
                    c.cycle_focus_forward();
                }
            }
            KeyCode::BackTab => {
                if let Some(c) = self.state.compose.as_mut() {
                    c.cycle_focus_back();
                }
            }
            KeyCode::Enter => {
                if let Some(c) = self.state.compose.as_mut() {
                    if c.focused == ComposeField::Body {
                        c.body.input(tui_textarea::Input::from(key));
                    } else {
                        c.cycle_focus_forward();
                    }
                }
            }
            _ => {
                if let Some(c) = self.state.compose.as_mut() {
                    match c.focused {
                        ComposeField::Body => {
                            c.body.input(tui_textarea::Input::from(key));
                        }
                        ComposeField::To | ComposeField::Subject => {
                            if let Some((value, cursor)) = c.focused_string_mut() {
                                crate::tui::compose::edit_single_line(value, cursor, key);
                            }
                        }
                    }
                }
            }
        }
    }

    fn handle_key_discard_confirm(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.state.compose = None;
                self.state.mode = Mode::Inbox;
                self.state.flash_info("Discarded");
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.state.mode = Mode::Composing;
            }
            _ => {}
        }
    }

    // ── Mode transitions ─────────────────────────────────────────

    fn open_command_menu(&mut self, prior: PriorMode) {
        self.state.command = Some(CommandState::empty(prior));
        self.state.mode = Mode::Command { prior };
    }

    fn close_command_menu(&mut self) {
        let prior = match &self.state.mode {
            Mode::Command { prior } => *prior,
            _ => return,
        };
        self.state.command = None;
        self.state.mode = match prior {
            PriorMode::Inbox => Mode::Inbox,
            PriorMode::Reading => Mode::Reading,
        };
    }

    fn enter_compose_new(&mut self) {
        self.state.compose = Some(ComposeState::new_blank());
        self.state.mode = Mode::Composing;
    }

    fn enter_compose_reply(&mut self) {
        let Some(r) = self.state.reading.as_ref() else {
            self.state.flash_info("Open a message first");
            return;
        };
        let src = &r.thread[r.message_idx];
        self.state.compose = Some(ComposeState::new_reply(src));
        self.state.mode = Mode::Composing;
    }

    fn enter_compose_forward(&mut self) {
        let Some(r) = self.state.reading.as_ref() else {
            self.state.flash_info("Open a message first");
            return;
        };
        let src = &r.thread[r.message_idx];
        self.state.compose = Some(ComposeState::new_forward(src));
        self.state.mode = Mode::Composing;
    }

    fn run_slash_command(&mut self, idx: usize, args: &str) {
        let cmd = SLASH_COMMANDS[idx];
        self.close_command_menu();
        match cmd.name {
            "compose" => self.enter_compose_new(),
            "reply" => self.enter_compose_reply(),
            "forward" => self.enter_compose_forward(),
            "archive" => {
                if let Some(id) = self.current_target_id() {
                    self.spawn_archive(id);
                }
            }
            "delete" => {
                if let Some(id) = self.current_target_id() {
                    self.spawn_delete(id);
                }
            }
            "star" => self.toggle_star_on_target(),
            "summarize" => self.run_summarize(),
            "draft" => self.run_draft(args),
            "ask" => self.run_ask(args),
            "triage" => self.run_triage(),
            "logout" => {
                self.should_quit = true;
            }
            _ => {}
        }
    }

    fn run_summarize(&mut self) {
        let Some(r) = self.state.reading.as_ref() else {
            self.state.flash_info("Open a thread first to summarize");
            return;
        };
        let msg = &r.thread[r.message_idx];
        let thread_id = msg.thread_id.clone().unwrap_or_else(|| msg.id.clone());
        self.summarize_context = Some((msg.id.clone(), msg.thread_id.clone(), msg.sender.clone()));
        self.state.mode = Mode::AiPending {
            kind: AiKind::Summarize,
            prior: PriorMode::Reading,
        };
        crate::tui::ai::spawn_summarize(
            self.client.clone(),
            self.mailbox_id.clone(),
            thread_id,
            self.tx.clone(),
        );
    }

    fn run_draft(&mut self, args: &str) {
        let prompt = args.trim();
        if prompt.is_empty() {
            self.state.flash_info("/draft needs a prompt");
            return;
        }
        let (prior, thread_id) = if let Some(r) = self.state.reading.as_ref() {
            (
                PriorMode::Reading,
                r.thread[r.message_idx].thread_id.clone(),
            )
        } else {
            (PriorMode::Inbox, None)
        };
        self.state.mode = Mode::AiPending {
            kind: AiKind::Draft,
            prior,
        };
        crate::tui::ai::spawn_draft(
            self.client.clone(),
            self.mailbox_id.clone(),
            prompt.to_string(),
            thread_id,
            self.tx.clone(),
        );
    }

    fn run_ask(&mut self, args: &str) {
        let q = args.trim();
        if q.is_empty() {
            self.state.flash_info("/ask needs a query");
            return;
        }
        let prior = if self.state.reading.is_some() {
            PriorMode::Reading
        } else {
            PriorMode::Inbox
        };
        self.state.mode = Mode::AiPending {
            kind: AiKind::Ask,
            prior,
        };
        crate::tui::ai::spawn_ask(
            self.client.clone(),
            self.mailbox_id.clone(),
            q.to_string(),
            self.tx.clone(),
        );
    }

    fn run_triage(&mut self) {
        let prior = if self.state.reading.is_some() {
            PriorMode::Reading
        } else {
            PriorMode::Inbox
        };
        self.state.mode = Mode::AiPending {
            kind: AiKind::Triage,
            prior,
        };
        crate::tui::ai::spawn_triage(
            self.client.clone(),
            self.mailbox_id.clone(),
            self.tx.clone(),
        );
    }

    fn current_target_id(&self) -> Option<String> {
        if let Some(r) = self.state.reading.as_ref() {
            return Some(r.thread[r.message_idx].id.clone());
        }
        self.state.selected_meta().map(|m| m.meta.id.clone())
    }

    fn toggle_star_on_target(&mut self) {
        if let Some(r) = self.state.reading.as_ref() {
            let msg = &r.thread[r.message_idx];
            let new_val = !msg.starred;
            let id = msg.id.clone();
            if let Some(r_mut) = self.state.reading.as_mut() {
                r_mut.thread[r_mut.message_idx].starred = new_val;
            }
            self.spawn_star(id, new_val);
            return;
        }
        if let Some(m) = self.state.selected_meta().cloned() {
            let new_val = !m.meta.starred;
            if let Some(row) = self.state.messages.get_mut(self.state.selected_index) {
                row.meta.starred = new_val;
            }
            self.spawn_star(m.meta.id, new_val);
        }
    }

    // ── Submit / save helpers ───────────────────────────────────

    fn submit_compose(&mut self) {
        let Some(c) = self.state.compose.as_mut() else {
            return;
        };
        if c.submitting {
            return;
        }
        match c.validate() {
            Err(msg) => self.state.flash_error(msg.to_string()),
            Ok(_) => {
                c.submitting = true;
                let client = self.client.clone();
                let mb = self.mailbox_id.clone();
                let tx = self.tx.clone();
                crate::tui::compose::spawn_send(client, mb, c, tx);
            }
        }
    }

    fn save_compose_draft(&mut self) {
        let Some(c) = self.state.compose.as_ref() else {
            return;
        };
        if c.to.trim().is_empty() && c.subject.trim().is_empty() && c.body_text().trim().is_empty()
        {
            self.state.flash_error("Nothing to save".to_string());
            return;
        }
        let client = self.client.clone();
        let mb = self.mailbox_id.clone();
        let tx = self.tx.clone();
        crate::tui::compose::spawn_save_draft(client, mb, c, tx);
    }
}

/// Async entrypoint. Caller has already loaded `Config` and built the client.
pub async fn run(client: Arc<ApiClient>, cfg: Config) -> anyhow::Result<()> {
    install_panic_hook();
    let mut terminal = setup_terminal()?;
    let res = run_loop(&mut terminal, client, cfg).await;
    restore_terminal(&mut terminal)?;
    res
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    client: Arc<ApiClient>,
    cfg: Config,
) -> anyhow::Result<()> {
    let mailbox_id = cfg
        .default_mailbox_id
        .clone()
        .ok_or_else(|| anyhow::anyhow!("no default mailbox — run `postr login` again"))?;
    let account = Account {
        email: cfg.email.unwrap_or_default(),
        unread_count: 0,
        total_count: 0,
        last_synced: "—".into(),
        mailbox_id,
    };
    let (tx, mut rx) = unbounded_channel::<AppEvent>();
    let mut app = App::new(client, account, tx.clone());
    app.spawn_load_inbox();

    // Feedback expiry tick — pings every second so the flash line can clear
    // itself after its 5s TTL without per-render `Instant::now()` checks.
    let feedback_tx = tx.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            if feedback_tx.send(AppEvent::FeedbackExpired).is_err() {
                break;
            }
        }
    });

    let mut keys = EventStream::new();
    loop {
        terminal.draw(|f| {
            // Cache the terminal width for the body wrapper.
            app.state.body_wrap_width = f.area().width;
            render::draw(f, &app);
        })?;
        if app.should_quit {
            break;
        }
        tokio::select! {
            maybe_key = keys.next() => {
                match maybe_key {
                    Some(Ok(Event::Key(k))) => app.handle_key(k),
                    Some(Ok(Event::Resize(w, _h))) => app.handle_resize(w),
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e.into()),
                    None => break,
                }
            }
            Some(ev) = rx.recv() => app.on_event(ev),
        }
    }
    Ok(())
}

fn setup_terminal() -> anyhow::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> anyhow::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original(info);
    }));
}
