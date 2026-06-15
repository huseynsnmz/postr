//! UI-side state. Wraps wire types (`EmailMeta`, `EmailFull`, `ThreadFull`)
//! with the few adornments the TUI cares about (selection index, scroll,
//! "n more", quoted-block collapse). No network here — `tui::app` drives
//! transitions in response to `AppEvent`s.

use std::time::{Duration, Instant};

use tui_textarea::TextArea;

use crate::api::types::{
    AskResult, EmailFull, EmailList, EmailMeta, SummaryBullet, ThreadFull, TriageCategory,
};
use crate::tui::body;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadingKind {
    Inbox,
    Thread,
    #[allow(dead_code)] // reserved for star/archive feedback in a follow-up
    Action,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriorMode {
    Inbox,
    Reading,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiKind {
    Summarize,
    Draft,
    Ask,
    Triage,
}

impl AiKind {
    pub fn name(self) -> &'static str {
        match self {
            AiKind::Summarize => "summarize",
            AiKind::Draft => "draft",
            AiKind::Ask => "ask",
            AiKind::Triage => "triage",
        }
    }
}

#[derive(Debug, Clone)]
pub enum Mode {
    Loading(LoadingKind),
    Error(String),
    Inbox,
    Reading,
    /// Slash menu overlay. `prior` records which screen to render underneath.
    Command {
        prior: PriorMode,
    },
    /// Compose / Reply / Forward editor (full-screen).
    Composing,
    /// "Discard draft? y/n" inline confirmation over the Composing screen.
    ComposeDiscardConfirm,
    /// In-flight AI request. `prior` records what to paint underneath and
    /// where Esc returns to on failure.
    AiPending {
        kind: AiKind,
        prior: PriorMode,
    },
    /// AI result panel. `kind` selects the renderer; the body lives in
    /// `AppState.ai`.
    AiResult {
        kind: AiKind,
        prior: PriorMode,
    },
}

/// Inbox row: wire shape + a couple of computed booleans + which mailbox
/// the row was fetched from (used by the unified-inbox `/switch all` view
/// so per-row operations like delete/star route to the right DO).
#[derive(Debug, Clone)]
pub struct TuiMessage {
    pub meta: EmailMeta,
    /// Mailbox the row came from — always set; in single-mailbox mode it's
    /// just the active mailbox's id; in unified mode it varies row to row.
    pub mailbox_id: String,
    /// TODO(phase4): `EmailMeta` has no attachment hint; hydrate by opening
    /// the row or by extending the Worker list response with a
    /// `has_attachments` bool.
    pub has_attachment: bool,
    /// Heuristic placeholder — design's "urgent" glyph is for system signals
    /// (failed builds etc.), not user-detected severity. Always false for v1.
    pub urgent: bool,
}

impl TuiMessage {
    pub fn from_meta(meta: EmailMeta, mailbox_id: String) -> Self {
        Self {
            meta,
            mailbox_id,
            has_attachment: false,
            urgent: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Account {
    pub email: String,
    pub unread_count: u32,
    pub total_count: u32,
    pub last_synced: String,
    pub mailbox_id: String,
}

#[derive(Debug, Clone)]
pub struct ReadingState {
    pub thread: ThreadFull,
    pub message_idx: usize,
    /// Mailbox the open thread belongs to — set when the inbox row was
    /// opened. Reply/forward/archive/delete all route through this id, not
    /// through the global active mailbox, so the unified-inbox view operates
    /// on the right DO.
    pub mailbox_id: String,
    pub body_lines: Vec<String>,
    pub quoted_collapsed: bool,
    pub quoted_lines: Vec<String>,
    pub scroll: u16,
}

// ── Feedback / flash line ──────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub enum FeedbackKind {
    Success,
    #[allow(dead_code)]
    Warning,
    Error,
    Info,
}

#[derive(Debug, Clone)]
pub struct Feedback {
    pub text: String,
    pub kind: FeedbackKind,
    pub shown_at: Instant,
}

// ── Undoable action ────────────────────────────────────────────────

/// Most recent reversible action. Only `Archive` is undoable in v1; delete
/// is destructive (Worker also nukes R2 blobs) and send is committed before
/// the user sees the flash. Cleared after the TTL window or when a
/// conflicting action would overwrite it.
#[derive(Debug, Clone)]
pub enum UndoableAction {
    Archive {
        email_id: String,
        /// Mailbox the email belonged to at archive time — needed because
        /// the unified inbox view can stage archives from arbitrary mailboxes.
        mailbox_id: String,
        prior_folder: String,
        recorded_at: Instant,
    },
}

impl UndoableAction {
    /// Match the design's 30-second undo window.
    pub fn is_expired(&self) -> bool {
        let UndoableAction::Archive { recorded_at, .. } = self;
        recorded_at.elapsed() > Duration::from_secs(30)
    }
}

// ── Slash command menu ─────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct CommandState {
    pub query: String,
    pub cursor: usize,
    /// Indices into `SLASH_COMMANDS`.
    pub filtered: Vec<usize>,
    /// Index into `filtered`.
    pub selected: usize,
}

impl CommandState {
    pub fn empty(prior: PriorMode) -> Self {
        let filtered = crate::tui::command::filter("", prior);
        Self {
            query: String::new(),
            cursor: 0,
            filtered,
            selected: 0,
        }
    }

    pub fn push_char(&mut self, ch: char, prior: PriorMode) {
        self.query.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
        self.recompute(prior);
    }

    pub fn backspace(&mut self, prior: PriorMode) {
        if self.cursor == 0 || self.query.is_empty() {
            return;
        }
        // Remove the char immediately before the cursor.
        let mut new_cursor = self.cursor;
        let bytes = self.query.as_bytes();
        // Walk back one UTF-8 codepoint.
        while new_cursor > 0 {
            new_cursor -= 1;
            if (bytes[new_cursor] & 0b1100_0000) != 0b1000_0000 {
                break;
            }
        }
        self.query.replace_range(new_cursor..self.cursor, "");
        self.cursor = new_cursor;
        self.recompute(prior);
    }

    fn recompute(&mut self, prior: PriorMode) {
        self.filtered = crate::tui::command::filter(&self.query, prior);
        if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
        }
    }
}

// ── Compose state ──────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum ComposeKind {
    New,
    Reply {
        /// `EmailFull.id` of the source message — the Worker `send_draft` flow
        /// inspects `in_reply_to` on the draft to choose the reply route.
        in_reply_to: String,
        thread_id: Option<String>,
        /// Sender label for the "↳ replying to X" hint.
        source_sender: String,
    },
    Forward {
        #[allow(dead_code)]
        source_email_id: String,
        #[allow(dead_code)]
        thread_id: Option<String>,
        source_sender: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeField {
    To,
    Subject,
    Body,
}

pub struct ComposeState {
    pub kind: ComposeKind,
    pub to: String,
    pub to_cursor: usize,
    pub subject: String,
    pub subject_cursor: usize,
    pub body: TextArea<'static>,
    pub focused: ComposeField,
    pub submitting: bool,
}

impl std::fmt::Debug for ComposeState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComposeState")
            .field("kind", &self.kind)
            .field("to", &self.to)
            .field("subject", &self.subject)
            .field("focused", &self.focused)
            .field("submitting", &self.submitting)
            .finish()
    }
}

// ── AI result state ────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum AiResultState {
    Summarize {
        thread_subject: String,
        message_count: u64,
        people_count: u64,
        bullets: Vec<SummaryBullet>,
        suggested_replies: Vec<String>,
        selected_reply: Option<usize>,
        /// Captured at request time so Compose Reply can be wired up
        /// without leaning on ReadingState that the user may have closed.
        source_email_id: String,
        thread_id: Option<String>,
        source_sender: String,
    },
    Draft {
        /// Original prompt — kept so `⌃r regenerate` can re-spawn.
        echo_prompt: String,
        thread_id: Option<String>,
        to: String,
        subject: String,
        body: String,
    },
    Ask {
        echo_query: String,
        summary: String,
        results: Vec<AskResult>,
        selected_index: usize,
    },
    Triage {
        categories: Vec<TriageCategory>,
    },
}

#[derive(Debug)]
pub struct AppState {
    pub mode: Mode,
    pub selected_index: usize,
    pub messages: Vec<TuiMessage>,
    pub account: Account,
    pub reading: Option<ReadingState>,
    pub command: Option<CommandState>,
    pub compose: Option<ComposeState>,
    pub feedback: Option<Feedback>,
    pub more_count: u32,
    /// Latest terminal width — refreshed every draw tick. Used by the body
    /// wrapper so we don't hardcode 80 columns. Defaults to 80 until first
    /// draw.
    pub body_wrap_width: u16,
    /// AI panel state for `Mode::AiResult` (Summarize, Draft, Ask, Triage).
    pub ai: Option<AiResultState>,
    /// Most recent reversible action (currently only `Archive`). Cleared
    /// after 30 s via `clear_undo_if_expired` or when a conflicting flow
    /// (e.g. opening a different email) overwrites it.
    pub last_undoable: Option<UndoableAction>,
    /// `?` shortcuts overlay. Floats on top of whatever the current mode is
    /// drawing; closes on `?` again or `Esc`. Stays a flat bool instead of a
    /// new Mode variant because every screen can show it without otherwise
    /// changing modes.
    pub show_shortcuts: bool,
    /// Pending destructive action awaiting `y` confirmation. Drawn as a
    /// small inline prompt at the bottom of the screen.
    pub pending_confirm: Option<PendingConfirm>,
    /// `true` after the user pressed `q` on the inbox once. A second `q`
    /// quits; any other key clears the flag. Surfaced as a flash hint.
    pub quit_armed: bool,
    /// `/switch` mailbox picker. `Some` while the centered overlay is open;
    /// resolved by `j/k/↑/↓` + `Enter` (or `Esc` to cancel).
    pub mailbox_picker: Option<MailboxPickerState>,
}

#[derive(Debug, Clone)]
pub struct MailboxPickerState {
    pub mailboxes: Vec<crate::api::types::CliMailbox>,
    pub selected: usize,
    /// `true` while the initial `/cli/me` round-trip is still in flight.
    pub loading: bool,
    /// Free-text filter; substring-matched against address, alias, and
    /// display name (all case-insensitive).
    pub query: String,
    /// Indices into `mailboxes` that match `query`. Empty `query` ⇒ all.
    pub filtered: Vec<usize>,
}

impl Default for MailboxPickerState {
    fn default() -> Self {
        Self::new()
    }
}

impl MailboxPickerState {
    pub fn new() -> Self {
        Self {
            mailboxes: Vec::new(),
            selected: 0,
            loading: true,
            query: String::new(),
            filtered: Vec::new(),
        }
    }

    /// Recompute `filtered` from the current `query`. Idempotent.
    pub fn refilter(&mut self) {
        let q = self.query.trim().to_lowercase();
        self.filtered = self
            .mailboxes
            .iter()
            .enumerate()
            .filter(|(_, mb)| {
                if q.is_empty() {
                    return true;
                }
                let addr = mb.address.to_lowercase();
                let alias = mb.alias.as_deref().unwrap_or("").to_lowercase();
                let name = mb.display_name.as_deref().unwrap_or("").to_lowercase();
                addr.contains(&q) || alias.contains(&q) || name.contains(&q)
            })
            .map(|(i, _)| i)
            .collect();
        if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
        }
    }
}

#[derive(Debug, Clone)]
pub enum PendingConfirm {
    /// Hard-delete an email from the inbox view.
    DeleteFromInbox {
        email_id: String,
        mailbox_id: String,
    },
    /// Hard-delete the currently open thread message.
    DeleteFromReading {
        email_id: String,
        mailbox_id: String,
    },
}

impl AppState {
    pub fn empty(account: Account) -> Self {
        Self {
            mode: Mode::Loading(LoadingKind::Inbox),
            selected_index: 0,
            messages: Vec::new(),
            account,
            reading: None,
            command: None,
            compose: None,
            feedback: None,
            more_count: 0,
            body_wrap_width: 80,
            ai: None,
            last_undoable: None,
            show_shortcuts: false,
            pending_confirm: None,
            mailbox_picker: None,
            quit_armed: false,
        }
    }

    pub fn clear_ai(&mut self) {
        self.ai = None;
    }

    pub fn clear_undo_if_expired(&mut self) {
        if let Some(u) = &self.last_undoable {
            if u.is_expired() {
                self.last_undoable = None;
            }
        }
    }

    pub fn set_inbox(&mut self, list: EmailList, mailbox_id: &str) {
        let metas = list.emails;
        let total = list.total_count as u32;
        let unread = metas.iter().filter(|m| !m.read).count() as u32;
        let len = metas.len() as u32;
        self.messages = metas
            .into_iter()
            .map(|m| TuiMessage::from_meta(m, mailbox_id.to_string()))
            .collect();
        // Clamp selection.
        if self.selected_index >= self.messages.len() && !self.messages.is_empty() {
            self.selected_index = self.messages.len() - 1;
        } else if self.messages.is_empty() {
            self.selected_index = 0;
        }
        self.account.unread_count = unread;
        self.account.total_count = total.max(len);
        self.account.last_synced = "just now".into();
        self.more_count = self.account.total_count.saturating_sub(len);
        self.mode = Mode::Inbox;
    }

    /// Sort + flatten merged inboxes for the unified `/switch all` view.
    /// `chunks` is `[(mailbox_id, EmailList), …]`. Each row remembers its
    /// owning mailbox so single-row ops route to the right DO.
    pub fn set_unified_inbox(&mut self, chunks: Vec<(String, EmailList)>) {
        let mut rows: Vec<TuiMessage> = Vec::new();
        let mut total: u64 = 0;
        let mut unread: u64 = 0;
        for (mailbox_id, list) in chunks {
            total = total.saturating_add(list.total_count);
            for m in list.emails {
                if !m.read {
                    unread += 1;
                }
                rows.push(TuiMessage::from_meta(m, mailbox_id.clone()));
            }
        }
        // Sort newest-first by RFC date string. Falls back to lexicographic
        // for malformed dates — the worker only emits RFC 3339 so this
        // matters in practice for empty strings only.
        rows.sort_by(|a, b| b.meta.date.cmp(&a.meta.date));
        let len = rows.len() as u32;
        self.messages = rows;
        if self.selected_index >= self.messages.len() && !self.messages.is_empty() {
            self.selected_index = self.messages.len() - 1;
        } else if self.messages.is_empty() {
            self.selected_index = 0;
        }
        self.account.unread_count = unread as u32;
        self.account.total_count = (total as u32).max(len);
        self.account.last_synced = "just now".into();
        self.more_count = self.account.total_count.saturating_sub(len);
        self.mode = Mode::Inbox;
    }

    pub fn set_thread(&mut self, thread: ThreadFull, mailbox_id: String) {
        if thread.is_empty() {
            self.set_error("empty thread".into());
            return;
        }
        // Open the most recent message by default.
        let message_idx = thread.len() - 1;
        let raw = thread[message_idx].body.as_deref().unwrap_or("");
        let width = self.body_wrap_width as usize;
        let (visible, quoted) = body::parse_body(raw, width);
        self.reading = Some(ReadingState {
            thread,
            message_idx,
            mailbox_id,
            body_lines: visible,
            quoted_collapsed: true,
            quoted_lines: quoted,
            scroll: 0,
        });
        self.mode = Mode::Reading;
    }

    pub fn set_error(&mut self, msg: String) {
        self.mode = Mode::Error(msg);
    }

    pub fn dismiss_error(&mut self) {
        self.mode = Mode::Inbox;
    }

    pub fn selected_next(&mut self) {
        if self.messages.is_empty() {
            return;
        }
        let last = self.messages.len() - 1;
        if self.selected_index < last {
            self.selected_index += 1;
        }
    }

    pub fn selected_prev(&mut self) {
        if self.selected_index > 0 {
            self.selected_index -= 1;
        }
    }

    pub fn selected_meta(&self) -> Option<&TuiMessage> {
        self.messages.get(self.selected_index)
    }

    // ── Feedback helpers ───────────────────────────────────────

    pub fn flash(&mut self, text: impl Into<String>, kind: FeedbackKind) {
        self.feedback = Some(Feedback {
            text: text.into(),
            kind,
            shown_at: Instant::now(),
        });
    }

    pub fn flash_info(&mut self, text: impl Into<String>) {
        self.flash(text, FeedbackKind::Info);
    }

    pub fn flash_success(&mut self, text: impl Into<String>) {
        self.flash(text, FeedbackKind::Success);
    }

    pub fn flash_error(&mut self, text: impl Into<String>) {
        self.flash(text, FeedbackKind::Error);
    }

    pub fn clear_feedback_if_expired(&mut self) {
        if let Some(fb) = &self.feedback {
            if fb.shown_at.elapsed() > Duration::from_secs(5) {
                self.feedback = None;
            }
        }
    }
}

// ── ComposeState constructors ──────────────────────────────────────

impl ComposeState {
    pub fn new_blank() -> Self {
        Self {
            kind: ComposeKind::New,
            to: String::new(),
            to_cursor: 0,
            subject: String::new(),
            subject_cursor: 0,
            body: TextArea::default(),
            focused: ComposeField::To,
            submitting: false,
        }
    }

    pub fn new_reply(src: &EmailFull) -> Self {
        let subject = strip_re_prefix(&src.subject);
        let subject = format!("Re: {subject}");
        let to = src.sender.clone();
        let to_cursor = to.len();
        let subject_cursor = subject.len();
        Self {
            kind: ComposeKind::Reply {
                in_reply_to: src.id.clone(),
                thread_id: src.thread_id.clone(),
                source_sender: src.sender.clone(),
            },
            to,
            to_cursor,
            subject,
            subject_cursor,
            body: TextArea::default(),
            focused: ComposeField::Body,
            submitting: false,
        }
    }

    pub fn new_forward(src: &EmailFull) -> Self {
        let subject = strip_fwd_prefix(&src.subject);
        let subject = format!("Fwd: {subject}");
        let subject_cursor = subject.len();
        // Build quoted body: "On <date>, <sender> wrote:\n" + "> " prefixed
        // body lines. Reuse the body parser to flatten HTML and re-wrap.
        let raw = src.body.as_deref().unwrap_or("");
        let (visible, _quoted) = body::parse_body(raw, 72);
        let mut quoted = String::new();
        quoted.push('\n');
        quoted.push('\n');
        quoted.push_str(&format!("On {}, {} wrote:\n", src.date, src.sender));
        for line in visible {
            quoted.push_str("> ");
            quoted.push_str(&line);
            quoted.push('\n');
        }
        let mut body = TextArea::default();
        body.insert_str(&quoted);
        Self {
            kind: ComposeKind::Forward {
                source_email_id: src.id.clone(),
                thread_id: src.thread_id.clone(),
                source_sender: src.sender.clone(),
            },
            to: String::new(),
            to_cursor: 0,
            subject,
            subject_cursor,
            body,
            focused: ComposeField::To,
            submitting: false,
        }
    }

    pub fn cycle_focus_forward(&mut self) {
        self.focused = match self.focused {
            ComposeField::To => ComposeField::Subject,
            ComposeField::Subject => ComposeField::Body,
            ComposeField::Body => ComposeField::To,
        };
    }

    pub fn cycle_focus_back(&mut self) {
        self.focused = match self.focused {
            ComposeField::To => ComposeField::Body,
            ComposeField::Subject => ComposeField::To,
            ComposeField::Body => ComposeField::Subject,
        };
    }

    pub fn focused_string_mut(&mut self) -> Option<(&mut String, &mut usize)> {
        match self.focused {
            ComposeField::To => Some((&mut self.to, &mut self.to_cursor)),
            ComposeField::Subject => Some((&mut self.subject, &mut self.subject_cursor)),
            ComposeField::Body => None,
        }
    }

    pub fn validate(&self) -> Result<Vec<String>, &'static str> {
        let to: Vec<String> = self
            .to
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if to.is_empty() {
            return Err("Add at least one recipient");
        }
        Ok(to)
    }

    pub fn body_text(&self) -> String {
        self.body.lines().join("\n")
    }

    pub fn has_unsaved(&self) -> bool {
        !self.to.is_empty() || !self.subject.is_empty() || !self.body_text().is_empty()
    }

    /// Sender label for "↳ replying to X" / "↳ forwarding from X".
    pub fn source_sender(&self) -> Option<&str> {
        match &self.kind {
            ComposeKind::Reply { source_sender, .. } => Some(source_sender.as_str()),
            ComposeKind::Forward { source_sender, .. } => Some(source_sender.as_str()),
            ComposeKind::New => None,
        }
    }
}

fn strip_re_prefix(subject: &str) -> String {
    let trimmed = subject.trim_start();
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("re:") {
        trimmed[3..].trim_start().to_string()
    } else {
        trimmed.to_string()
    }
}

fn strip_fwd_prefix(subject: &str) -> String {
    let trimmed = subject.trim_start();
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("fwd:") {
        trimmed[4..].trim_start().to_string()
    } else if lower.starts_with("fw:") {
        trimmed[3..].trim_start().to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::{EmailList, EmailMeta};

    fn account() -> Account {
        Account {
            email: "u@example.com".into(),
            unread_count: 0,
            total_count: 0,
            last_synced: "—".into(),
            mailbox_id: "mb".into(),
        }
    }

    fn meta(id: &str, read: bool) -> EmailMeta {
        EmailMeta {
            id: id.into(),
            subject: "s".into(),
            sender: "x@y".into(),
            recipient: "u@example.com".into(),
            cc: None,
            bcc: None,
            date: "2026-06-15T09:00:00Z".into(),
            read,
            starred: false,
            in_reply_to: None,
            email_references: None,
            thread_id: None,
            folder_id: Some("inbox".into()),
            snippet: None,
        }
    }

    #[test]
    fn ai_kind_name_round_trip() {
        assert_eq!(AiKind::Summarize.name(), "summarize");
        assert_eq!(AiKind::Draft.name(), "draft");
        assert_eq!(AiKind::Ask.name(), "ask");
        assert_eq!(AiKind::Triage.name(), "triage");
    }

    #[test]
    fn selected_next_clamps_at_last_row() {
        let mut s = AppState::empty(account());
        s.messages = vec![
            TuiMessage::from_meta(meta("a", true), "test@x.com".into()),
            TuiMessage::from_meta(meta("b", true), "test@x.com".into()),
        ];
        s.selected_index = 1;
        s.selected_next();
        assert_eq!(s.selected_index, 1);
    }

    #[test]
    fn selected_prev_clamps_at_zero() {
        let mut s = AppState::empty(account());
        s.selected_prev();
        assert_eq!(s.selected_index, 0);
    }

    #[test]
    fn selected_next_noop_when_empty() {
        let mut s = AppState::empty(account());
        s.selected_next();
        assert_eq!(s.selected_index, 0);
    }

    #[test]
    fn set_inbox_computes_unread_and_more() {
        let mut s = AppState::empty(account());
        s.set_inbox(
            EmailList {
                emails: vec![meta("a", false), meta("b", true), meta("c", false)],
                total_count: 10,
            },
            "test@x.com",
        );
        assert_eq!(s.account.unread_count, 2);
        assert_eq!(s.account.total_count, 10);
        assert_eq!(s.more_count, 7);
    }

    #[test]
    fn set_inbox_total_count_at_least_visible_len() {
        // Worker may report a stale total < page size; we floor at len.
        let mut s = AppState::empty(account());
        s.set_inbox(
            EmailList {
                emails: vec![meta("a", true), meta("b", true)],
                total_count: 0,
            },
            "test@x.com",
        );
        assert_eq!(s.account.total_count, 2);
        assert_eq!(s.more_count, 0);
    }

    #[test]
    fn set_inbox_clamps_selected_when_list_shrinks() {
        let mut s = AppState::empty(account());
        s.selected_index = 5;
        s.set_inbox(
            EmailList {
                emails: vec![meta("a", true), meta("b", true)],
                total_count: 2,
            },
            "test@x.com",
        );
        assert_eq!(s.selected_index, 1);
    }

    #[test]
    fn compose_validate_rejects_empty_to() {
        let c = ComposeState::new_blank();
        assert!(c.validate().is_err());
    }

    #[test]
    fn compose_validate_splits_recipients() {
        let mut c = ComposeState::new_blank();
        c.to = "a@x, b@y , c@z".into();
        let got = c.validate().unwrap();
        assert_eq!(got, vec!["a@x", "b@y", "c@z"]);
    }

    #[test]
    fn compose_has_unsaved_tracks_any_field() {
        let mut c = ComposeState::new_blank();
        assert!(!c.has_unsaved());
        c.subject = "hi".into();
        assert!(c.has_unsaved());
    }

    #[test]
    fn strip_re_prefix_removes_only_leading_re() {
        assert_eq!(strip_re_prefix("Re: hello"), "hello");
        assert_eq!(strip_re_prefix("hello"), "hello");
        // Single-pass intentional: nested replies keep the inner prefix
        // so threading downstream can still see them.
        assert_eq!(strip_re_prefix("Re: Re: hello"), "Re: hello");
    }

    #[test]
    fn undo_archive_expires_after_window() {
        // Use a recorded_at far in the past so we don't have to sleep.
        let action = UndoableAction::Archive {
            email_id: "e1".into(),
            mailbox_id: "test@x.com".into(),
            prior_folder: "inbox".into(),
            recorded_at: Instant::now() - Duration::from_secs(31),
        };
        assert!(action.is_expired());

        let fresh = UndoableAction::Archive {
            email_id: "e1".into(),
            mailbox_id: "test@x.com".into(),
            prior_folder: "inbox".into(),
            recorded_at: Instant::now(),
        };
        assert!(!fresh.is_expired());
    }

    #[test]
    fn clear_undo_if_expired_drops_stale() {
        let mut s = AppState::empty(account());
        s.last_undoable = Some(UndoableAction::Archive {
            email_id: "e1".into(),
            mailbox_id: "test@x.com".into(),
            prior_folder: "inbox".into(),
            recorded_at: Instant::now() - Duration::from_secs(31),
        });
        s.clear_undo_if_expired();
        assert!(s.last_undoable.is_none());
    }

    #[test]
    fn clear_undo_if_expired_keeps_fresh() {
        let mut s = AppState::empty(account());
        s.last_undoable = Some(UndoableAction::Archive {
            email_id: "e1".into(),
            mailbox_id: "test@x.com".into(),
            prior_folder: "inbox".into(),
            recorded_at: Instant::now(),
        });
        s.clear_undo_if_expired();
        assert!(s.last_undoable.is_some());
    }
}
