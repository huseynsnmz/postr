//! Async TUI driver: owns the `App` value, the crossterm event stream, and
//! the channel that worker tasks (inbox/thread loads, archive/star/delete,
//! compose send/save) use to feed results back into the render loop.

use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::event::{
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
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
    ComposeState, LoadingKind, Mode, PendingConfirm, PriorMode,
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
    /// `(mailbox_id, thread)` — mailbox the open thread belongs to, so
    /// ReadingState can store it for routing per-message ops.
    ThreadLoaded(String, ThreadFull),
    ActionDone(String),
    /// Same as ActionDone but without a flash — used by batch ops so the
    /// per-row completion doesn't overwrite the batch summary.
    ActionDoneQuiet,
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
    /// `/switch` opened the picker; the background `/cli/me` round-trip
    /// returned with the current mailbox list.
    MailboxesLoaded(Vec<crate::api::types::CliMailbox>),
    /// `/switch all` finished fetching every mailbox's inbox. The vec is
    /// `(mailbox_id, EmailList)` — order isn't significant since the merge
    /// step sorts by date.
    UnifiedInboxLoaded(Vec<(String, EmailList)>),
    /// `/switch all` resolved the mailbox-id list via `/cli/me`; the next
    /// step is to fan out the per-mailbox inbox fetches.
    AllMailboxesResolved(Vec<String>),
    /// Read-only flash from a background task (e.g. `/whoami`, `/mailbox-list`)
    /// — shows as an info flash without triggering an inbox reload.
    Notice(String),
}

/// Active mailbox scope: a single named mailbox, or the `/switch all`
/// unified view across every registered mailbox.
#[derive(Debug, Clone)]
pub enum ActiveScope {
    Single,
    /// Mailbox ids included in the unified view. The label shown in the
    /// inbox frame title is also derived from this.
    All(Vec<String>),
}

pub struct App {
    pub state: AppState,
    pub should_quit: bool,
    pub client: Arc<ApiClient>,
    /// Operative mailbox for "where do new messages come from" — used for
    /// compose-from and any flow that needs a single mailbox even when the
    /// inbox view is unified. In `ActiveScope::Single` this is the active
    /// one; in `ActiveScope::All` it stays at the last single-mode value.
    pub mailbox_id: String,
    pub scope: ActiveScope,
    pub tx: UnboundedSender<AppEvent>,
    pub cfg: Config,
    /// Transient context captured at `/summarize` spawn-time so the result
    /// arm can wire up Compose-Reply without re-reading `ReadingState`
    /// (which the user may have closed). Tuple is
    /// `(source_email_id, thread_id, sender)`.
    summarize_context: Option<(String, Option<String>, String)>,
}

impl App {
    pub fn new(
        client: Arc<ApiClient>,
        account: Account,
        cfg: Config,
        tx: UnboundedSender<AppEvent>,
    ) -> Self {
        let mailbox_id = account.mailbox_id.clone();
        Self {
            state: AppState::empty(account),
            should_quit: false,
            client,
            mailbox_id,
            scope: ActiveScope::Single,
            tx,
            cfg,
            summarize_context: None,
        }
    }

    // ── Background spawns ────────────────────────────────────────

    pub fn spawn_load_mailboxes(&self) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match client.me().await {
                Ok(me) => {
                    let _ = tx.send(AppEvent::MailboxesLoaded(me.mailboxes));
                }
                Err(e) => {
                    let _ = tx.send(AppEvent::Error {
                        kind: (&e).into(),
                        message: format!("mailboxes: {e}"),
                    });
                }
            }
        });
    }

    /// Switch the active mailbox at runtime: update state, persist the
    /// config, and reload the inbox. Idempotent if `new_id` matches the
    /// current mailbox **and** we're already in `ActiveScope::Single`
    /// (otherwise we still need to flip out of unified mode).
    pub fn set_active_mailbox(
        &mut self,
        new_id: String,
        display_name: Option<String>,
    ) -> anyhow::Result<()> {
        let same_single = matches!(self.scope, ActiveScope::Single) && new_id == self.mailbox_id;
        if same_single {
            return Ok(());
        }
        self.scope = ActiveScope::Single;
        self.mailbox_id = new_id.clone();
        self.state.account.mailbox_id = new_id.clone();
        self.state.account.email = new_id.clone();
        self.state.account.unread_count = 0;
        self.state.account.total_count = 0;
        self.state.account.last_synced = "—".into();
        self.state.messages.clear();
        self.state.selected_index = 0;
        self.state.more_count = 0;
        self.state.reading = None;
        self.state.mode = Mode::Loading(LoadingKind::Inbox);
        self.cfg.default_mailbox_id = Some(new_id.clone());
        self.cfg.email = Some(new_id);
        self.cfg.save()?;
        let label = display_name
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|n| format!("{n} <{}>", self.mailbox_id))
            .unwrap_or_else(|| self.mailbox_id.clone());
        self.state.flash_info(format!("Switched to {label}"));
        self.spawn_load_inbox();
        Ok(())
    }

    pub fn spawn_load_inbox(&self) {
        match &self.scope {
            ActiveScope::Single => self.spawn_load_single_inbox(self.mailbox_id.clone()),
            ActiveScope::All(ids) => self.spawn_load_all_inboxes(ids.clone()),
        }
    }

    fn spawn_load_single_inbox(&self, mb: String) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        let folder = self.state.folder.clone();
        tokio::spawn(async move {
            match client.get_inbox_list(&mb, &folder).await {
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

    fn spawn_load_all_inboxes(&self, ids: Vec<String>) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        let folder = self.state.folder.clone();
        tokio::spawn(async move {
            // Fan out: each mailbox fetched independently; we tag each with
            // its id so the merge step downstream can attribute rows.
            let futures = ids.into_iter().map(|id| {
                let client = client.clone();
                let folder = folder.clone();
                async move {
                    let result = client.get_inbox_list(&id, &folder).await;
                    (id, result)
                }
            });
            let results = futures_util::future::join_all(futures).await;
            let mut chunks: Vec<(String, EmailList)> = Vec::new();
            let mut first_err: Option<String> = None;
            for (id, res) in results {
                match res {
                    Ok(list) => chunks.push((id, list)),
                    Err(e) if first_err.is_none() => first_err = Some(format!("{id}: {e}")),
                    Err(_) => {} // skip per-mailbox errors after the first surface
                }
            }
            if chunks.is_empty() {
                let _ = tx.send(AppEvent::Error {
                    kind: ErrorKind::Network,
                    message: first_err.unwrap_or_else(|| "all inbox fetches failed".into()),
                });
            } else {
                let _ = tx.send(AppEvent::UnifiedInboxLoaded(chunks));
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
        let mb = m.mailbox_id.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = match m.meta.thread_id.as_deref() {
                Some(tid) => client.get_thread(&mb, tid).await,
                None => client.get_email(&mb, &m.meta.id).await.map(|e| vec![e]),
            };
            match result {
                Ok(t) => {
                    let _ = tx.send(AppEvent::ThreadLoaded(mb.clone(), t));
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

    pub fn spawn_mark_read(&self, email_id: String, read: bool, mailbox_id: String) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match client.mark_read(&mailbox_id, &email_id, read).await {
                Ok(_) => {
                    let _ = tx.send(AppEvent::ActionDone(
                        if read { "Marked read" } else { "Marked unread" }.into(),
                    ));
                }
                Err(e) => {
                    let _ = tx.send(AppEvent::Error {
                        kind: (&e).into(),
                        message: format!("read flag: {e}"),
                    });
                }
            }
        });
    }

    pub fn spawn_mark_all_read(&mut self) {
        let folder = self.state.folder.clone();
        let mailbox_id = self.mailbox_id.clone();
        let client = self.client.clone();
        let tx = self.tx.clone();
        self.state.flash_info("Marking all read…");
        tokio::spawn(async move {
            match client.mark_all_read(&mailbox_id, &folder).await {
                Ok(n) => {
                    let summary = if n == 0 {
                        "Nothing to mark".to_string()
                    } else {
                        format!("Marked {n} message(s) read")
                    };
                    let _ = tx.send(AppEvent::ActionDone(summary));
                }
                Err(e) => {
                    let _ = tx.send(AppEvent::Error {
                        kind: (&e).into(),
                        message: format!("mark-all-read: {e}"),
                    });
                }
            }
        });
    }

    /// Toggle the read flag on the inbox row currently in scope. Routes
    /// over the multi-selection if one is active. `target_read` is the
    /// desired state — true=read, false=unread.
    pub fn toggle_read_on_target(&mut self, target_read: bool) {
        if !self.state.multi_selected.is_empty() {
            self.batch_mark_read(target_read);
            return;
        }
        if let Some(r) = self.state.reading.as_ref() {
            let msg = &r.thread[r.message_idx];
            let id = msg.id.clone();
            let mb = r.mailbox_id.clone();
            self.spawn_mark_read(id, target_read, mb);
            return;
        }
        if let Some(m) = self.state.selected_meta().cloned() {
            if let Some(row) = self.state.messages.get_mut(self.state.selected_index) {
                row.meta.read = target_read;
            }
            self.spawn_mark_read(m.meta.id, target_read, m.mailbox_id);
        }
    }

    fn batch_mark_read(&mut self, target_read: bool) {
        let rows = self.drain_selection();
        let n = rows.len();
        if n == 0 {
            return;
        }
        for (email_id, mailbox_id) in rows {
            self.spawn_mark_read_quiet(email_id, target_read, mailbox_id);
        }
        self.state.flash_success(if target_read {
            format!("Marked {n} message(s) read")
        } else {
            format!("Marked {n} message(s) unread")
        });
    }

    fn spawn_mark_read_quiet(&self, email_id: String, read: bool, mailbox_id: String) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let _ = client.mark_read(&mailbox_id, &email_id, read).await;
            let _ = tx.send(AppEvent::ActionDoneQuiet);
        });
    }

    pub fn spawn_star(&self, email_id: String, on: bool, mailbox_id: String) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match client.star(&mailbox_id, &email_id, on).await {
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

    pub fn spawn_archive(&mut self, email_id: String, mailbox_id: String) {
        // Record the undo handle *before* the move, so a fast `u` press
        // after the flash can restore the row. Worker errors leave the
        // stale entry in place — at worst, `u` tries to move an email
        // that never moved and the user sees an error flash.
        self.state.last_undoable = Some(crate::state::UndoableAction::Archive {
            email_id: email_id.clone(),
            mailbox_id: mailbox_id.clone(),
            prior_folder: "inbox".to_string(),
            recorded_at: std::time::Instant::now(),
        });
        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match client.archive(&mailbox_id, &email_id).await {
                Ok(_) => {
                    let _ = tx.send(AppEvent::ActionDone("Archived · u to undo".into()));
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

    pub fn spawn_delete(&self, email_id: String, mailbox_id: String) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match client.delete_email(&mailbox_id, &email_id).await {
                Ok(_) => {
                    let _ = tx.send(AppEvent::ActionDone("deleted".into()));
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
            mailbox_id,
            prior_folder,
            ..
        } = action;
        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match client
                .move_email(&mailbox_id, &email_id, &prior_folder)
                .await
            {
                Ok(_) => {
                    let _ = tx.send(AppEvent::ActionDone("Restored".into()));
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
            AppEvent::InboxLoaded(list) => self.state.set_inbox(list, &self.mailbox_id),
            AppEvent::UnifiedInboxLoaded(chunks) => {
                self.state.set_unified_inbox(chunks);
                self.state.account.email = "all mailboxes".into();
            }
            AppEvent::AllMailboxesResolved(ids) => {
                if ids.is_empty() {
                    self.state.flash_error("No mailboxes to merge");
                    self.state.mode = Mode::Inbox;
                    return;
                }
                self.scope = ActiveScope::All(ids.clone());
                self.spawn_load_all_inboxes(ids);
            }
            AppEvent::ThreadLoaded(mb, t) => self.state.set_thread(t, mb),
            AppEvent::ActionDone(s) => {
                self.state.flash_success(s);
                // Refresh the inbox so the affected row reflects the new state.
                // Uses the current scope (single or unified) automatically.
                self.spawn_load_inbox();
            }
            AppEvent::ActionDoneQuiet => {
                self.spawn_load_inbox();
            }
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
            AppEvent::Notice(s) => self.state.flash_info(s),
            AppEvent::MailboxesLoaded(list) => {
                if let Some(picker) = self.state.mailbox_picker.as_mut() {
                    picker.loading = false;
                    let cur = self.mailbox_id.clone();
                    let cur_is_unified = matches!(self.scope, ActiveScope::All(_));
                    picker.mailboxes = list;
                    picker.refilter();
                    picker.selected = picker
                        .filtered
                        .iter()
                        .position(|entry| match entry {
                            crate::state::MailboxPickerEntry::All => cur_is_unified,
                            crate::state::MailboxPickerEntry::Mailbox(mb) => {
                                mb.id.eq_ignore_ascii_case(&cur) && !cur_is_unified
                            }
                        })
                        .unwrap_or(0);
                }
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
        // Picker overlays eat every key so the user can't accidentally
        // interact with the screen behind them.
        if self.state.mailbox_picker.is_some() {
            self.handle_key_mailbox_picker(key);
            return;
        }
        if self.state.folder_picker.is_some() {
            self.handle_key_folder_picker(key);
            return;
        }
        // Bare `q` is the soft-exit key: from non-inbox screens it returns to
        // the inbox; from the inbox itself, the first `q` arms a "press q
        // again to quit" prompt and the second actually exits. Typing-modes
        // pass the keystroke through to the buffer.
        let is_typing = matches!(
            self.state.mode,
            Mode::Composing | Mode::Command { .. } | Mode::ComposeDiscardConfirm
        );
        if !is_typing && matches!(key.code, KeyCode::Char('q')) {
            if matches!(self.state.mode, Mode::Inbox) {
                if self.state.quit_armed {
                    self.should_quit = true;
                } else {
                    self.state.quit_armed = true;
                    self.state.flash_info("Press q again to quit");
                }
            } else {
                // Anywhere else: ferry the user back to the inbox.
                self.state.reading = None;
                self.state.compose = None;
                self.state.command = None;
                self.state.ai = None;
                self.state.show_shortcuts = false;
                self.state.pending_confirm = None;
                self.state.mailbox_picker = None;
                self.state.mode = Mode::Inbox;
            }
            return;
        }
        // Any non-`q` keystroke while we're on the inbox disarms the prompt.
        if self.state.quit_armed && matches!(self.state.mode, Mode::Inbox) {
            self.state.quit_armed = false;
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
                    let _ = tx.send(AppEvent::ThreadLoaded(mb.clone(), t));
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
        if self.state.show_shortcuts {
            // Any key dismisses the overlay; '?' re-opens, so swallow it
            // here too and let the user toggle.
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('?') | KeyCode::Enter) {
                self.state.show_shortcuts = false;
            }
            return;
        }
        if let Some(confirm) = self.state.pending_confirm.clone() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.state.pending_confirm = None;
                    self.execute_pending_confirm(confirm);
                }
                _ => {
                    self.state.pending_confirm = None;
                    self.state.flash_info("Cancelled");
                }
            }
            return;
        }
        if let KeyCode::Char('?') = key.code {
            self.state.show_shortcuts = true;
            return;
        }
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
            // Space toggles multi-selection on the highlighted row. The
            // cursor stays put — auto-advance felt jumpy when picking a
            // single row at a time.
            KeyCode::Char(' ') => {
                if let Some(m) = self.state.selected_meta().cloned() {
                    let id = m.meta.id;
                    if self.state.multi_selected.remove(&id).is_none() {
                        self.state.multi_selected.insert(id, m.mailbox_id);
                    }
                }
            }
            // Esc clears the multi-selection. (Inbox has no other Esc binding.)
            KeyCode::Esc if !self.state.multi_selected.is_empty() => {
                self.state.multi_selected.clear();
                self.state.flash_info("Selection cleared");
            }
            KeyCode::Enter => self.spawn_open_selected(),
            KeyCode::Char('s') => {
                if !self.state.multi_selected.is_empty() {
                    self.batch_toggle_star();
                } else if let Some(m) = self.state.selected_meta().cloned() {
                    let new_val = !m.meta.starred;
                    if let Some(row) = self.state.messages.get_mut(self.state.selected_index) {
                        row.meta.starred = new_val;
                    }
                    self.spawn_star(m.meta.id, new_val, m.mailbox_id);
                }
            }
            KeyCode::Char('e') => {
                if !self.state.multi_selected.is_empty() {
                    self.batch_archive();
                } else if let Some(m) = self.state.selected_meta().cloned() {
                    self.spawn_archive(m.meta.id, m.mailbox_id);
                }
            }
            KeyCode::Char('d') => {
                if !self.state.multi_selected.is_empty() {
                    self.batch_delete();
                } else if let Some(m) = self.state.selected_meta().cloned() {
                    self.arm_delete_confirm(m.meta.id, m.mailbox_id, false);
                }
            }
            KeyCode::Char('u') => self.spawn_undo(),
            // `m` toggles read state. With a multi-selection, batches over
            // every row using the highlighted row's state to pick direction.
            KeyCode::Char('m') => {
                let direction = self
                    .state
                    .selected_meta()
                    .map(|m| !m.meta.read)
                    .unwrap_or(true);
                self.toggle_read_on_target(direction);
            }
            KeyCode::Char('/') => self.open_command_menu(PriorMode::Inbox),
            KeyCode::Char('c') => self.enter_compose_new(),
            KeyCode::Char('r') => {
                self.state.flash_info("Refreshing…");
                self.spawn_load_inbox();
            }
            KeyCode::Char('a') | KeyCode::Char('f') => {
                self.state.flash_info("Open a message first");
            }
            _ => {}
        }
    }

    fn handle_key_reading(&mut self, key: KeyEvent) {
        if self.state.show_shortcuts {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('?') | KeyCode::Enter) {
                self.state.show_shortcuts = false;
            }
            return;
        }
        if let Some(confirm) = self.state.pending_confirm.clone() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.state.pending_confirm = None;
                    self.execute_pending_confirm(confirm);
                }
                _ => {
                    self.state.pending_confirm = None;
                    self.state.flash_info("Cancelled");
                }
            }
            return;
        }
        if let KeyCode::Char('?') = key.code {
            self.state.show_shortcuts = true;
            return;
        }
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
                    let mb = r.mailbox_id.clone();
                    if let Some(r_mut) = self.state.reading.as_mut() {
                        r_mut.thread[r_mut.message_idx].starred = new_val;
                    }
                    self.spawn_star(id, new_val, mb);
                }
            }
            KeyCode::Char('e') => {
                if let Some(r) = self.state.reading.as_ref() {
                    let id = r.thread[r.message_idx].id.clone();
                    let mb = r.mailbox_id.clone();
                    self.spawn_archive(id, mb);
                    self.state.mode = Mode::Inbox;
                    self.state.reading = None;
                }
            }
            KeyCode::Char('d') => {
                if let Some(r) = self.state.reading.as_ref() {
                    let id = r.thread[r.message_idx].id.clone();
                    let mb = r.mailbox_id.clone();
                    self.arm_delete_confirm(id, mb, true);
                }
            }
            KeyCode::Char('/') => self.open_command_menu(PriorMode::Reading),
            KeyCode::Char('r') | KeyCode::Char('a') => self.enter_compose_reply(),
            KeyCode::Char('f') => self.enter_compose_forward(),
            KeyCode::Char('m') => {
                let direction = self
                    .state
                    .reading
                    .as_ref()
                    .map(|r| !r.thread[r.message_idx].read)
                    .unwrap_or(true);
                self.toggle_read_on_target(direction);
            }
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
            let is_slash = cmd.query.starts_with('/');
            match key.code {
                KeyCode::Esc => action = Action::Close,
                KeyCode::Enter if is_slash && !cmd.filtered.is_empty() => {
                    action = Action::Run {
                        idx: cmd.filtered[cmd.selected],
                        args: crate::tui::command::split_args(&cmd.query).to_string(),
                    };
                }
                KeyCode::Down if is_slash && !cmd.filtered.is_empty() => {
                    cmd.selected = (cmd.selected + 1).min(cmd.filtered.len() - 1);
                }
                KeyCode::Up if is_slash => {
                    cmd.selected = cmd.selected.saturating_sub(1);
                }
                KeyCode::Backspace => {
                    cmd.backspace(prior);
                    if cmd.query.is_empty() {
                        action = Action::Close;
                    }
                }
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
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        // Ctrl+Enter is the canonical send; many terminals can't disambiguate
        // it from plain Enter, so we also accept Ctrl+S and Alt+Enter as
        // aliases that the same terminals do deliver distinctly.
        if (ctrl && matches!(key.code, KeyCode::Enter | KeyCode::Char('s')))
            || (alt && matches!(key.code, KeyCode::Enter))
        {
            self.submit_compose();
            return;
        }
        if ctrl {
            match key.code {
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
        self.open_prompt(prior, '/');
    }

    /// Start typing into the always-live prompt with `seed` as the first char.
    /// `/` seeds the slash command popover; any other char drops the user into
    /// a plain text buffer (no popover yet — placeholder for future "ask
    /// across mail" entry).
    fn open_prompt(&mut self, prior: PriorMode, seed: char) {
        let mut cmd = CommandState::empty(prior);
        cmd.push_char(seed, prior);
        self.state.command = Some(cmd);
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
                if let Some((id, mb)) = self.current_target() {
                    self.spawn_archive(id, mb);
                }
            }
            "delete" => {
                if let Some((id, mb)) = self.current_target() {
                    let from_reading = self.state.reading.is_some();
                    self.arm_delete_confirm(id, mb, from_reading);
                }
            }
            "star" => self.toggle_star_on_target(),
            "summarize" => self.run_summarize(),
            "draft" => self.run_draft(args),
            "ask" => self.run_ask(args),
            "triage" => self.run_triage(),
            "refresh" => {
                self.state.flash_info("Refreshing…");
                self.spawn_load_inbox();
            }
            "switch" => self.run_switch(args),
            "folder" => self.run_folder(args),
            "read" => self.toggle_read_on_target(true),
            "unread" => self.toggle_read_on_target(false),
            "mark-all-read" => self.spawn_mark_all_read(),
            "logout" => self.run_logout(),
            "login" => self.run_login(),
            "whoami" => self.run_whoami(),
            "mailbox-list" => self.run_mailbox_list(),
            "mailbox-add" => self.run_mailbox_add(args),
            "mailbox-update" => self.run_mailbox_update(args),
            "mailbox-remove" => self.run_mailbox_remove(args),
            "demo-seed" => self.run_demo_seed(args),
            _ => {}
        }
    }

    // ── Identity / mailbox CRUD slash commands ───────────────────────
    //
    // Thin wrappers around the same `ApiClient` + keyring/config helpers
    // that `main.rs` uses for the CLI subcommands, exposed as `/whoami`,
    // `/mailbox-*`, etc. Output one-line — multi-line CLI output is
    // collapsed into a `·`-separated flash.

    fn run_logout(&mut self) {
        let _ = crate::config::keyring::delete_token();
        let _ = crate::config::Config::clear();
        self.should_quit = true;
    }

    fn run_login(&mut self) {
        self.state
            .flash_info("Run `postr login <url>` from the shell to (re-)authenticate.");
    }

    fn run_whoami(&mut self) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        let url = self.cfg.worker_base_url.clone();
        tokio::spawn(async move {
            match client.me().await {
                Ok(me) => {
                    let mbs: Vec<String> = me.mailboxes.iter().map(format_mailbox_inline).collect();
                    let msg = format!("{} · {} · {}", url, me.email, mbs.join(", "));
                    let _ = tx.send(AppEvent::Notice(msg));
                }
                Err(e) => {
                    let _ = tx.send(AppEvent::Error {
                        kind: (&e).into(),
                        message: e.to_string(),
                    });
                }
            }
        });
    }

    fn run_mailbox_list(&mut self) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match client.list_mailboxes().await {
                Ok(list) if list.is_empty() => {
                    let _ = tx.send(AppEvent::Notice("No mailboxes".into()));
                }
                Ok(list) => {
                    let joined = list
                        .iter()
                        .map(format_mailbox_inline)
                        .collect::<Vec<_>>()
                        .join(", ");
                    let _ = tx.send(AppEvent::Notice(joined));
                }
                Err(e) => {
                    let _ = tx.send(AppEvent::Error {
                        kind: (&e).into(),
                        message: e.to_string(),
                    });
                }
            }
        });
    }

    fn run_mailbox_add(&mut self, args: &str) {
        let parsed = match SlashArgs::parse(args) {
            Ok(p) => p,
            Err(e) => return self.flash_args_error(e),
        };
        let Some(address) = parsed.address else {
            return self.flash_args_error("address required: /mailbox-add <addr>");
        };
        let name = parsed.name;
        let alias = parsed.alias;
        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match client
                .create_mailbox(&address, name.as_deref(), alias.as_deref())
                .await
            {
                Ok(mb) => {
                    let _ = tx.send(AppEvent::Notice(format!(
                        "Added {}",
                        format_mailbox_inline(&mb)
                    )));
                }
                Err(e) => {
                    let _ = tx.send(AppEvent::Error {
                        kind: (&e).into(),
                        message: e.to_string(),
                    });
                }
            }
        });
    }

    fn run_mailbox_update(&mut self, args: &str) {
        let parsed = match SlashArgs::parse(args) {
            Ok(p) => p,
            Err(e) => return self.flash_args_error(e),
        };
        let Some(address) = parsed.address else {
            return self.flash_args_error("address required: /mailbox-update <addr> …");
        };
        let name_payload: Option<Option<String>> = match (parsed.name, parsed.clear_name) {
            (None, false) => None,
            (None, true) => Some(None),
            (Some(n), _) => Some(Some(n)),
        };
        let alias_payload: Option<Option<String>> = match (parsed.alias, parsed.clear_alias) {
            (None, false) => None,
            (None, true) => Some(None),
            (Some(a), _) => Some(Some(a)),
        };
        if name_payload.is_none() && alias_payload.is_none() {
            return self.flash_args_error(
                "nothing to update — pass --name/--alias or --clear-name/--clear-alias",
            );
        }
        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let name_ref = name_payload.as_ref().map(|o| o.as_deref());
            let alias_ref = alias_payload.as_ref().map(|o| o.as_deref());
            match client.update_mailbox(&address, name_ref, alias_ref).await {
                Ok(mb) => {
                    let _ = tx.send(AppEvent::Notice(format!(
                        "Updated {}",
                        format_mailbox_inline(&mb)
                    )));
                }
                Err(e) => {
                    let _ = tx.send(AppEvent::Error {
                        kind: (&e).into(),
                        message: e.to_string(),
                    });
                }
            }
        });
    }

    fn run_mailbox_remove(&mut self, args: &str) {
        let parsed = match SlashArgs::parse(args) {
            Ok(p) => p,
            Err(e) => return self.flash_args_error(e),
        };
        let Some(address) = parsed.address else {
            return self.flash_args_error("address required: /mailbox-remove <addr>");
        };
        // If we're removing the default mailbox, clear the marker so the next
        // restart doesn't try to open a dead mailbox. Mirrors `main.rs`.
        let was_default = self.cfg.default_mailbox_id.as_deref() == Some(address.as_str());
        if was_default {
            self.cfg.default_mailbox_id = None;
            let _ = self.cfg.save();
        }
        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match client.delete_mailbox(&address).await {
                Ok(()) => {
                    let msg = if was_default {
                        format!("Removed {address} (was default — cleared)")
                    } else {
                        format!("Removed {address}")
                    };
                    let _ = tx.send(AppEvent::Notice(msg));
                }
                Err(e) => {
                    let _ = tx.send(AppEvent::Error {
                        kind: (&e).into(),
                        message: e.to_string(),
                    });
                }
            }
        });
    }

    fn run_demo_seed(&mut self, args: &str) {
        let parsed = match SlashArgs::parse(args) {
            Ok(p) => p,
            Err(e) => return self.flash_args_error(e),
        };
        let Some(address) = parsed.address else {
            return self.flash_args_error("address required: /demo-seed <addr>");
        };
        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match client.seed_demo(&address).await {
                Ok(n) => {
                    let _ = tx.send(AppEvent::Notice(format!(
                        "Seeded {n} demo emails into {address}"
                    )));
                }
                Err(e) => {
                    let _ = tx.send(AppEvent::Error {
                        kind: (&e).into(),
                        message: e.to_string(),
                    });
                }
            }
        });
    }

    fn flash_args_error(&mut self, msg: impl Into<String>) {
        self.state.flash_error(msg);
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

    /// `/switch <addr>` swaps directly when `<addr>` is a full email; for any
    /// other token (an alias or fragment) the picker opens pre-filtered to
    /// that string so fuzzy match resolves it. `/switch all` activates the
    /// unified inbox across every registered mailbox.
    fn run_switch(&mut self, args: &str) {
        let arg = args.trim();
        if arg.eq_ignore_ascii_case("all") {
            self.enter_unified_inbox();
            return;
        }
        if arg.contains('@') {
            let target = arg.to_lowercase();
            if let Err(e) = self.set_active_mailbox(target, None) {
                self.state.flash_error(format!("Switch failed: {e}"));
            }
            return;
        }
        self.open_mailbox_picker_with_query(arg);
    }

    /// Activate the unified inbox view across all registered mailboxes.
    /// Fetches the mailbox list on its own, then flips scope to All and
    /// kicks off the parallel inbox fetches.
    fn enter_unified_inbox(&mut self) {
        self.state.messages.clear();
        self.state.selected_index = 0;
        self.state.more_count = 0;
        self.state.reading = None;
        self.state.mode = Mode::Loading(LoadingKind::Inbox);
        self.state.flash_info("Loading all inboxes…");

        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match client.me().await {
                Ok(me) => {
                    let ids: Vec<String> = me.mailboxes.into_iter().map(|m| m.id).collect();
                    let _ = tx.send(AppEvent::AllMailboxesResolved(ids));
                }
                Err(e) => {
                    let _ = tx.send(AppEvent::Error {
                        kind: (&e).into(),
                        message: format!("mailboxes: {e}"),
                    });
                }
            }
        });
    }

    fn open_mailbox_picker_with_query(&mut self, query: &str) {
        let mut picker = crate::state::MailboxPickerState::new();
        picker.query = query.to_string();
        self.state.mailbox_picker = Some(picker);
        self.spawn_load_mailboxes();
    }

    /// `/folder <name>` switches the folder directly; `/folder` (no args)
    /// opens a centered picker.
    fn run_folder(&mut self, args: &str) {
        let arg = args.trim().to_lowercase();
        if arg.is_empty() {
            self.state.folder_picker =
                Some(crate::state::FolderPickerState::new(&self.state.folder));
            return;
        }
        if !crate::tui::command::FOLDERS.iter().any(|f| f.name == arg) {
            self.state.flash_error(format!(
                "Unknown folder '{arg}' — try inbox/archive/sent/drafts/trash"
            ));
            return;
        }
        self.switch_folder(arg);
    }

    fn switch_folder(&mut self, folder: String) {
        if folder == self.state.folder {
            return;
        }
        self.state.folder = folder.clone();
        self.state.messages.clear();
        self.state.selected_index = 0;
        self.state.more_count = 0;
        self.state.reading = None;
        self.state.mode = Mode::Loading(LoadingKind::Inbox);
        self.state.flash_info(format!("Folder: {folder}"));
        self.spawn_load_inbox();
    }

    fn handle_key_folder_picker(&mut self, key: KeyEvent) {
        let folders = crate::tui::command::FOLDERS;
        let Some(picker) = self.state.folder_picker.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                self.state.folder_picker = None;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                picker.selected = (picker.selected + 1) % folders.len();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                picker.selected = if picker.selected == 0 {
                    folders.len() - 1
                } else {
                    picker.selected - 1
                };
            }
            KeyCode::Char(ch @ '1'..='9') => {
                let idx = (ch as u8 - b'1') as usize;
                if idx < folders.len() {
                    picker.selected = idx;
                }
            }
            KeyCode::Enter => {
                let name = folders[picker.selected].name.to_string();
                self.state.folder_picker = None;
                self.switch_folder(name);
            }
            _ => {}
        }
    }

    fn close_mailbox_picker(&mut self) {
        self.state.mailbox_picker = None;
    }

    fn handle_key_mailbox_picker(&mut self, key: KeyEvent) {
        // Pull the chosen mailbox out so we don't hold a borrow across self.*.
        let chosen = {
            let Some(picker) = self.state.mailbox_picker.as_mut() else {
                return;
            };
            if picker.loading {
                if matches!(key.code, KeyCode::Esc) {
                    self.close_mailbox_picker();
                }
                return;
            }
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Esc => {
                    self.close_mailbox_picker();
                    return;
                }
                KeyCode::Down => {
                    if !picker.filtered.is_empty() {
                        picker.selected = (picker.selected + 1) % picker.filtered.len();
                    }
                    return;
                }
                KeyCode::Up => {
                    if !picker.filtered.is_empty() {
                        picker.selected = if picker.selected == 0 {
                            picker.filtered.len() - 1
                        } else {
                            picker.selected - 1
                        };
                    }
                    return;
                }
                // Ctrl+N/P as arrow-key aliases for keyboards that swap them.
                KeyCode::Char('n') if ctrl => {
                    if !picker.filtered.is_empty() {
                        picker.selected = (picker.selected + 1) % picker.filtered.len();
                    }
                    return;
                }
                KeyCode::Char('p') if ctrl => {
                    if !picker.filtered.is_empty() {
                        picker.selected = if picker.selected == 0 {
                            picker.filtered.len() - 1
                        } else {
                            picker.selected - 1
                        };
                    }
                    return;
                }
                KeyCode::Backspace => {
                    picker.query.pop();
                    picker.refilter();
                    return;
                }
                KeyCode::Char(ch) if !ctrl => {
                    picker.query.push(ch);
                    picker.refilter();
                    return;
                }
                KeyCode::Enter => picker.filtered.get(picker.selected).cloned(),
                _ => return,
            }
        };
        if let Some(entry) = chosen {
            self.close_mailbox_picker();
            match entry {
                crate::state::MailboxPickerEntry::All => self.enter_unified_inbox(),
                crate::state::MailboxPickerEntry::Mailbox(mb) => {
                    if let Err(e) = self.set_active_mailbox(mb.id, mb.display_name) {
                        self.state.flash_error(format!("Switch failed: {e}"));
                    }
                }
            }
        }
    }

    /// Decide whether `d` should soft-delete (move to trash) or hard-delete.
    /// In any folder except `trash` the row is moved; inside `trash` we
    /// purge it for good.
    fn arm_delete_confirm(&mut self, email_id: String, mailbox_id: String, from_reading: bool) {
        let action = if self.state.folder.eq_ignore_ascii_case("trash") {
            crate::state::ConfirmAction::HardDelete
        } else {
            crate::state::ConfirmAction::MoveToTrash
        };
        self.state.pending_confirm = Some(PendingConfirm {
            email_id,
            mailbox_id,
            action,
            from_reading,
        });
    }

    fn execute_pending_confirm(&mut self, confirm: PendingConfirm) {
        match confirm.action {
            crate::state::ConfirmAction::MoveToTrash => {
                self.spawn_move_to_trash(confirm.email_id, confirm.mailbox_id);
            }
            crate::state::ConfirmAction::HardDelete => {
                self.spawn_delete(confirm.email_id, confirm.mailbox_id);
            }
        }
        if confirm.from_reading {
            self.state.mode = Mode::Inbox;
            self.state.reading = None;
        }
    }

    // ── Batch ops (apply over `multi_selected`) ─────────────────

    fn drain_selection(&mut self) -> Vec<(String, String)> {
        let out: Vec<(String, String)> = self
            .state
            .multi_selected
            .iter()
            .map(|(id, mb)| (id.clone(), mb.clone()))
            .collect();
        self.state.multi_selected.clear();
        out
    }

    fn batch_archive(&mut self) {
        let rows = self.drain_selection();
        let n = rows.len();
        if n == 0 {
            return;
        }
        for (email_id, mailbox_id) in rows {
            self.spawn_archive_quiet(email_id, mailbox_id);
        }
        self.state.flash_success(format!("Archived {n} message(s)"));
    }

    fn batch_delete(&mut self) {
        let rows = self.drain_selection();
        let n = rows.len();
        if n == 0 {
            return;
        }
        // Soft-delete from inbox/etc, hard-delete from inside trash. Match
        // the single-row UX without a per-row confirm — the batch flash
        // (and the row count) is the confirmation surface.
        let permanent = self.state.folder.eq_ignore_ascii_case("trash");
        for (email_id, mailbox_id) in rows {
            if permanent {
                self.spawn_delete_quiet(email_id, mailbox_id);
            } else {
                self.spawn_move_to_trash_quiet(email_id, mailbox_id);
            }
        }
        self.state.flash_success(if permanent {
            format!("Deleted {n} message(s)")
        } else {
            format!("Moved {n} message(s) to trash")
        });
    }

    fn batch_toggle_star(&mut self) {
        // Direction is decided by the currently-highlighted row's state —
        // if it's starred we unstar everything, otherwise we star everything.
        // Matches the way email clients usually batch the star action.
        let direction = self
            .state
            .selected_meta()
            .map(|m| !m.meta.starred)
            .unwrap_or(true);
        let rows = self.drain_selection();
        let n = rows.len();
        if n == 0 {
            return;
        }
        for (email_id, mailbox_id) in rows {
            self.spawn_star_quiet(email_id, direction, mailbox_id);
        }
        self.state.flash_success(if direction {
            format!("Starred {n} message(s)")
        } else {
            format!("Unstarred {n} message(s)")
        });
    }

    // Quiet variants emit no per-op flash so the batch summary stays clean.
    fn spawn_archive_quiet(&self, email_id: String, mailbox_id: String) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let _ = client.archive(&mailbox_id, &email_id).await;
            let _ = tx.send(AppEvent::ActionDoneQuiet);
        });
    }

    fn spawn_delete_quiet(&self, email_id: String, mailbox_id: String) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let _ = client.delete_email(&mailbox_id, &email_id).await;
            let _ = tx.send(AppEvent::ActionDoneQuiet);
        });
    }

    fn spawn_move_to_trash_quiet(&self, email_id: String, mailbox_id: String) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let _ = client.move_email(&mailbox_id, &email_id, "trash").await;
            let _ = tx.send(AppEvent::ActionDoneQuiet);
        });
    }

    fn spawn_star_quiet(&self, email_id: String, on: bool, mailbox_id: String) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let _ = client.star(&mailbox_id, &email_id, on).await;
            let _ = tx.send(AppEvent::ActionDoneQuiet);
        });
    }

    pub fn spawn_move_to_trash(&mut self, email_id: String, mailbox_id: String) {
        // Stage an undo so `u` brings the row back to the prior folder.
        self.state.last_undoable = Some(crate::state::UndoableAction::Archive {
            email_id: email_id.clone(),
            mailbox_id: mailbox_id.clone(),
            prior_folder: self.state.folder.clone(),
            recorded_at: std::time::Instant::now(),
        });
        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match client.move_email(&mailbox_id, &email_id, "trash").await {
                Ok(_) => {
                    let _ = tx.send(AppEvent::ActionDone("Moved to trash · u to undo".into()));
                }
                Err(e) => {
                    let _ = tx.send(AppEvent::Error {
                        kind: (&e).into(),
                        message: format!("trash: {e}"),
                    });
                }
            }
        });
    }

    /// Reading view takes precedence; otherwise the selected inbox row.
    /// Returns `(email_id, mailbox_id)` so callers can route ops to the
    /// right DO even in the unified inbox view.
    fn current_target(&self) -> Option<(String, String)> {
        if let Some(r) = self.state.reading.as_ref() {
            return Some((r.thread[r.message_idx].id.clone(), r.mailbox_id.clone()));
        }
        self.state
            .selected_meta()
            .map(|m| (m.meta.id.clone(), m.mailbox_id.clone()))
    }

    fn toggle_star_on_target(&mut self) {
        if let Some(r) = self.state.reading.as_ref() {
            let msg = &r.thread[r.message_idx];
            let new_val = !msg.starred;
            let id = msg.id.clone();
            let mb = r.mailbox_id.clone();
            if let Some(r_mut) = self.state.reading.as_mut() {
                r_mut.thread[r_mut.message_idx].starred = new_val;
            }
            self.spawn_star(id, new_val, mb);
            return;
        }
        if let Some(m) = self.state.selected_meta().cloned() {
            let new_val = !m.meta.starred;
            if let Some(row) = self.state.messages.get_mut(self.state.selected_index) {
                row.meta.starred = new_val;
            }
            self.spawn_star(m.meta.id, new_val, m.mailbox_id);
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
        email: cfg.email.clone().unwrap_or(mailbox_id.clone()),
        unread_count: 0,
        total_count: 0,
        last_synced: "—".into(),
        mailbox_id,
    };
    let (tx, mut rx) = unbounded_channel::<AppEvent>();
    let mut app = App::new(client, account, cfg, tx.clone());
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
    // Best-effort: ask the terminal for kitty-protocol modifier reporting so
    // Ctrl+Enter, Alt+Enter etc. arrive as distinct key events instead of
    // collapsing to plain Enter. Capable terminals (kitty, WezTerm, recent
    // iTerm2, Ghostty, Alacritty) honor this; older ones (Terminal.app)
    // silently ignore it. Either way crossterm falls back gracefully.
    let _ = execute!(
        stdout,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS,
        )
    );
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> anyhow::Result<()> {
    let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
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

// ── Slash-command arg parsing ────────────────────────────────────────
//
// The slash command popover hands handlers a single `args: &str` (the
// substring after the command name). For mailbox CRUD we accept a
// shell-like form: `<addr> [--name N] [--alias A] [--clear-name]
// [--clear-alias]`. No quoting — values stop at the next whitespace.
// Multi-word display names need to be set from the CLI for now.

#[derive(Default)]
struct SlashArgs {
    address: Option<String>,
    name: Option<String>,
    alias: Option<String>,
    clear_name: bool,
    clear_alias: bool,
}

impl SlashArgs {
    fn parse(args: &str) -> Result<Self, &'static str> {
        let mut out = SlashArgs::default();
        let mut it = args.split_whitespace();
        while let Some(tok) = it.next() {
            match tok {
                "--name" => {
                    let v = it.next().ok_or("--name needs a value")?;
                    out.name = Some(v.to_string());
                }
                "--alias" => {
                    let v = it.next().ok_or("--alias needs a value")?;
                    out.alias = Some(v.to_string());
                }
                "--clear-name" => out.clear_name = true,
                "--clear-alias" => out.clear_alias = true,
                s if s.starts_with("--") => return Err("unknown flag"),
                s => {
                    if out.address.is_some() {
                        return Err("unexpected extra positional arg");
                    }
                    out.address = Some(s.to_string());
                }
            }
        }
        Ok(out)
    }
}

fn format_mailbox_inline(mb: &crate::api::types::CliMailbox) -> String {
    let alias = mb
        .alias
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|a| format!(" [{a}]"))
        .unwrap_or_default();
    match mb.display_name.as_deref().filter(|s| !s.is_empty()) {
        Some(n) => format!("{} <{}>{alias}", n, mb.address),
        None => format!("{}{alias}", mb.address),
    }
}
