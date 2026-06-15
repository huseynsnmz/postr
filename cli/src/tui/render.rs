use chrono::{DateTime, Datelike, Duration, Local, TimeZone};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::state::{
    AiKind, AiResultState, AppState, CommandState, ComposeField, ComposeKind, ComposeState,
    Feedback, FeedbackKind, LoadingKind, Mode, PriorMode, ReadingState, TuiMessage,
};
use crate::tui::app::App;
use crate::tui::command::{CmdStatus, SLASH_COMMANDS};
use crate::tui::theme;

pub fn draw(frame: &mut Frame, app: &App) {
    match &app.state.mode {
        Mode::Loading(kind) => draw_loading(frame, app, kind),
        Mode::Error(msg) => draw_error(frame, app, msg),
        Mode::Reading => draw_reading(frame, app),
        Mode::Inbox => draw_inbox(frame, app),
        Mode::Composing => draw_compose(frame, app),
        Mode::ComposeDiscardConfirm => {
            draw_compose(frame, app);
            draw_discard_overlay(frame);
        }
        Mode::Command { prior } => {
            match prior {
                PriorMode::Inbox => draw_inbox(frame, app),
                PriorMode::Reading => draw_reading(frame, app),
            }
            // Only float the popover when the buffer looks like a slash
            // command — typing plain text just edits the input box.
            if let Some(cmd) = app.state.command.as_ref() {
                if cmd.query.starts_with('/') {
                    draw_command_menu(frame, app, cmd, *prior);
                }
            }
        }
        Mode::AiPending { kind, prior } => {
            match prior {
                PriorMode::Inbox => draw_inbox(frame, app),
                PriorMode::Reading => draw_reading(frame, app),
            }
            draw_ai_pending(frame, *kind);
        }
        Mode::AiResult { kind, .. } => match kind {
            AiKind::Summarize => draw_ai_summarize(frame, app),
            AiKind::Draft => draw_ai_draft(frame, app),
            AiKind::Ask => draw_ai_ask(frame, app),
            AiKind::Triage => draw_ai_triage(frame, app),
        },
    }
    if app.state.show_shortcuts {
        draw_shortcuts_overlay(frame, &app.state.mode);
    }
    if app.state.pending_confirm.is_some() {
        draw_confirm_overlay(frame);
    }
    if let Some(picker) = app.state.mailbox_picker.as_ref() {
        draw_mailbox_picker(frame, picker, &app.mailbox_id);
    }
}

// ── Mailbox picker (/switch) ─────────────────────────────────────────────────

fn draw_mailbox_picker(
    frame: &mut Frame,
    picker: &crate::state::MailboxPickerState,
    current: &str,
) {
    let area = frame.area();
    let w = 60u16.min(area.width.saturating_sub(4));
    let visible_rows = picker.mailboxes.len().max(1) as u16;
    let h = (visible_rows + 4).min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let panel = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    frame.render_widget(Clear, panel);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::HAIRLINE))
        .style(Style::default().bg(theme::RECESSED_WELL))
        .title(Span::styled(
            " SWITCH MAILBOX ",
            Style::default().fg(theme::MUTED),
        ));
    let inner = block.inner(panel);
    frame.render_widget(block, panel);

    let muted = Style::default().fg(theme::MUTED).bg(theme::RECESSED_WELL);
    let text = Style::default().fg(theme::TEXT).bg(theme::RECESSED_WELL);
    let signal_bold = Style::default()
        .fg(theme::SIGNAL_LIGHT)
        .bg(theme::RECESSED_WELL)
        .add_modifier(Modifier::BOLD);

    let mut lines: Vec<Line> = Vec::new();
    if picker.loading {
        lines.push(Line::from(Span::styled(" Loading…", muted)));
    } else if picker.mailboxes.is_empty() {
        lines.push(Line::from(Span::styled(" No mailboxes registered.", muted)));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(" Add one with ", muted),
            Span::styled("postr mailbox add <addr>", signal_bold),
        ]));
    } else {
        for (i, mb) in picker.mailboxes.iter().enumerate() {
            let selected = i == picker.selected;
            let active = mb.id.eq_ignore_ascii_case(current);
            let row_bg = if selected {
                theme::ROW_SELECT
            } else {
                theme::RECESSED_WELL
            };
            let bar = if selected { "▌" } else { " " };
            let bar_span = Span::styled(
                bar.to_string(),
                Style::default()
                    .fg(theme::SIGNAL_LIGHT)
                    .bg(row_bg)
                    .add_modifier(Modifier::BOLD),
            );
            let addr = format!(" {}", mb.address);
            let addr_style = if selected {
                signal_bold.bg(row_bg)
            } else {
                text.bg(row_bg)
            };
            let suffix = match (&mb.display_name, active) {
                (Some(n), true) if !n.is_empty() => format!("  ({n}) · active"),
                (Some(n), false) if !n.is_empty() => format!("  ({n})"),
                (_, true) => "  · active".to_string(),
                _ => String::new(),
            };
            lines.push(
                Line::from(vec![
                    bar_span,
                    Span::styled(addr, addr_style),
                    Span::styled(suffix, muted.bg(row_bg)),
                ])
                .style(Style::default().bg(row_bg)),
            );
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(" ", muted),
        Span::styled("↑↓", signal_bold),
        Span::styled(" pick   ", muted),
        Span::styled("⏎", signal_bold),
        Span::styled(" switch   ", muted),
        Span::styled("esc", signal_bold),
        Span::styled(" cancel", muted),
    ]));

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(theme::RECESSED_WELL)),
        inner,
    );
}

// ── Destructive-action confirm overlay ────────────────────────────────────────

fn draw_confirm_overlay(frame: &mut Frame) {
    let area = frame.area();
    let w = 50u16.min(area.width.saturating_sub(4));
    let h = 5u16;
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let panel = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    frame.render_widget(Clear, panel);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::RED))
        .style(Style::default().bg(theme::RECESSED_WELL))
        .title(Span::styled(
            " DELETE ",
            Style::default().fg(theme::RED).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(panel);
    frame.render_widget(block, panel);

    let muted = Style::default().fg(theme::MUTED).bg(theme::RECESSED_WELL);
    let text = Style::default().fg(theme::TEXT).bg(theme::RECESSED_WELL);
    let key = Style::default()
        .fg(theme::SIGNAL_LIGHT)
        .bg(theme::RECESSED_WELL)
        .add_modifier(Modifier::BOLD);

    let lines = vec![
        Line::from(Span::styled(" Permanently delete this message?", text)),
        Line::from(""),
        Line::from(vec![
            Span::styled(" ", text),
            Span::styled("y", key),
            Span::styled(" confirm   ", muted),
            Span::styled("any other key", key),
            Span::styled(" cancel", muted),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(theme::RECESSED_WELL)),
        inner,
    );
}

// ── Shortcuts overlay (?) ────────────────────────────────────────────────────

fn draw_shortcuts_overlay(frame: &mut Frame, mode: &Mode) {
    let area = frame.area();

    let max_w = 80u16.min(area.width.saturating_sub(4));
    let max_h = 22u16.min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(max_w)) / 2;
    let y = area.y + (area.height.saturating_sub(max_h)) / 2;
    let panel = Rect {
        x,
        y,
        width: max_w,
        height: max_h,
    };
    frame.render_widget(Clear, panel);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::HAIRLINE))
        .style(Style::default().bg(theme::RECESSED_WELL))
        .title(Span::styled(
            " SHORTCUTS ",
            Style::default().fg(theme::MUTED),
        ));
    let inner = block.inner(panel);
    frame.render_widget(block, panel);

    let muted = Style::default().fg(theme::MUTED).bg(theme::RECESSED_WELL);
    let key = Style::default()
        .fg(theme::SIGNAL_LIGHT)
        .bg(theme::RECESSED_WELL)
        .add_modifier(Modifier::BOLD);
    let text = Style::default().fg(theme::TEXT).bg(theme::RECESSED_WELL);

    let kv = |k: &'static str, v: &'static str| -> Line<'static> {
        Line::from(vec![
            Span::styled(format!("  {k:<14}"), key),
            Span::styled(v.to_string(), text),
        ])
    };
    let hdr =
        |s: &'static str| -> Line<'static> { Line::from(Span::styled(format!(" {s}"), muted)) };

    let in_reading = matches!(mode, Mode::Reading)
        || matches!(
            mode,
            Mode::Command {
                prior: PriorMode::Reading
            }
        );
    let in_compose = matches!(mode, Mode::Composing | Mode::ComposeDiscardConfirm);

    let mut lines: Vec<Line> = vec![
        hdr("Inbox"),
        kv("j / ↓", "next message"),
        kv("k / ↑", "previous message"),
        kv("g / G", "jump to top / bottom"),
        kv("1–9", "jump to row"),
        kv("⏎", "open selected"),
        kv("c", "compose new"),
        kv("s", "toggle star"),
        kv("e", "archive"),
        kv("d", "delete"),
        kv("u", "undo last archive"),
        kv("r", "refresh"),
        kv("/", "open command popover"),
        kv("?", "this overlay"),
    ];

    if in_reading || in_compose {
        lines.push(Line::from(""));
    }
    if in_reading {
        lines.push(hdr("Reading"));
        lines.push(kv("Esc", "back to inbox"));
        lines.push(kv("j / k", "scroll / next message"));
        lines.push(kv("z", "expand / collapse quoted"));
        lines.push(kv("r / a", "reply / reply all"));
        lines.push(kv("f", "forward"));
        lines.push(kv("e / d / s", "archive / delete / star"));
    }
    if in_compose {
        lines.push(hdr("Compose"));
        lines.push(kv("Tab / ⇧Tab", "next / previous field"));
        lines.push(kv("⌃⏎", "send (also ⌃S, ⌥⏎)"));
        lines.push(kv("⌃d", "save draft"));
        lines.push(kv("Esc", "discard"));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  esc / ?", key),
        Span::styled(" close", muted),
    ]));

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(theme::RECESSED_WELL)),
        inner,
    );
}

// ── Inbox ────────────────────────────────────────────────────────────────────

fn draw_inbox(frame: &mut Frame, app: &App) {
    let area = frame.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),  // welcome block
            Constraint::Length(1),  // spacer
            Constraint::Length(10), // bordered rows block (title + header + 7 rows + border)
            Constraint::Length(1),  // …N more line
            Constraint::Length(1),  // spacer
            Constraint::Length(3),  // input box
            Constraint::Length(1),  // hint / feedback
            Constraint::Min(0),
        ])
        .split(area);

    draw_welcome(frame, chunks[0], &app.state);
    draw_rows(frame, chunks[2], &app.state);
    draw_more(frame, chunks[3], app.state.more_count);
    draw_input(frame, chunks[5], &app.state);
    if let Some(fb) = app.state.feedback.as_ref() {
        draw_feedback(frame, chunks[6], fb);
    } else {
        draw_hint(frame, chunks[6]);
    }
}

// ── Welcome block ────────────────────────────────────────────────────────────

fn draw_welcome(frame: &mut Frame, area: Rect, state: &AppState) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::HAIRLINE))
        .style(Style::default().bg(theme::RECESSED_WELL));
    frame.render_widget(block, area);

    let muted = Style::default().fg(theme::MUTED).bg(theme::RECESSED_WELL);
    let text = Style::default().fg(theme::TEXT).bg(theme::RECESSED_WELL);
    let text_bold = text.add_modifier(Modifier::BOLD);

    let line1 = Line::from(vec![Span::styled("✉  postr v0.1.0", text_bold)]);
    let line2 = Line::from(vec![Span::styled(
        format!(
            "{} · {} unread · {} total · synced {}",
            state.account.email,
            state.account.unread_count,
            state.account.total_count,
            state.account.last_synced
        ),
        muted,
    )]);
    let line3 = Line::from(vec![
        Span::styled("Type a ", muted),
        Span::styled("number", text_bold),
        Span::styled(" to open · ", muted),
        Span::styled("/", text_bold),
        Span::styled(" for commands · ", muted),
        Span::styled("?", text_bold),
        Span::styled(" for shortcuts", muted),
    ]);

    let inner = Rect {
        x: area.x + 2,
        y: area.y + 1,
        width: area.width.saturating_sub(4),
        height: area.height.saturating_sub(2),
    };
    frame.render_widget(
        Paragraph::new(vec![line1, line2, line3]).style(Style::default().bg(theme::RECESSED_WELL)),
        inner,
    );
}

// ── Message rows ─────────────────────────────────────────────────────────────

fn draw_rows(frame: &mut Frame, area: Rect, state: &AppState) {
    // Block title carries the account email so a future multi-mailbox switch
    // surfaces in the same place.
    let title = format!(" INBOX · {} ", state.account.email);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::HAIRLINE))
        .title(Span::styled(title, Style::default().fg(theme::MUTED)));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let total_w = inner.width as usize;

    const W_MARKER: usize = 2;
    const W_NUMBER: usize = 3;
    const W_SENDER: usize = 16;
    const W_GLYPH: usize = 2;
    const TIME_GUTTER: usize = 1;

    // Header row: # · Sender · Subject · Date. Same column widths as the
    // data rows so they line up vertically.
    let muted = Style::default().fg(theme::MUTED);
    let header = {
        let time_w = "Date".len();
        let fixed = W_MARKER + W_NUMBER + W_SENDER + W_GLYPH + TIME_GUTTER + time_w;
        let subject_w = total_w.saturating_sub(fixed);
        Line::from(vec![
            Span::styled("  ", muted),
            Span::styled(format!("{:>2} ", "#"), muted),
            Span::styled(pad_right("Sender", W_SENDER), muted),
            Span::styled("  ", muted),
            Span::styled(pad_right("Subject", subject_w), muted),
            Span::styled(" Date".to_string(), muted),
        ])
    };

    let take_n = (inner.height as usize).saturating_sub(1);
    let mut lines: Vec<Line> = Vec::with_capacity(take_n + 1);
    lines.push(header);
    lines.extend(
        state
            .messages
            .iter()
            .take(take_n)
            .enumerate()
            .map(|(i, m)| {
                row_line(
                    i,
                    m,
                    state.selected_index == i,
                    total_w,
                    W_MARKER,
                    W_NUMBER,
                    W_SENDER,
                    W_GLYPH,
                    TIME_GUTTER,
                )
            }),
    );

    frame.render_widget(Paragraph::new(lines), inner);
}

#[allow(clippy::too_many_arguments)]
fn row_line(
    idx: usize,
    msg: &TuiMessage,
    selected: bool,
    total_w: usize,
    w_marker: usize,
    w_number: usize,
    w_sender: usize,
    w_glyph: usize,
    time_gutter: usize,
) -> Line<'static> {
    let time_str = format_time(&msg.meta.date);
    let sender_str = if msg.meta.sender.trim().is_empty() {
        "(no sender)".to_string()
    } else {
        msg.meta.sender.clone()
    };
    let subject_str = if msg.meta.subject.trim().is_empty() {
        "(no subject)".to_string()
    } else {
        msg.meta.subject.clone()
    };
    let unread = !msg.meta.read;

    let time_w = UnicodeWidthStr::width(time_str.as_str()).max(1);
    let fixed = w_marker + w_number + w_sender + w_glyph + time_gutter + time_w;
    let subject_w = total_w.saturating_sub(fixed);

    let row_bg = if selected {
        Some(theme::ROW_SELECT)
    } else {
        None
    };
    let apply_bg = |s: Style| -> Style {
        if let Some(bg) = row_bg {
            s.bg(bg)
        } else {
            s
        }
    };

    let signal_bold = apply_bg(
        Style::default()
            .fg(theme::SIGNAL_LIGHT)
            .add_modifier(Modifier::BOLD),
    );
    let muted = apply_bg(Style::default().fg(theme::MUTED));
    let text_bold = apply_bg(
        Style::default()
            .fg(theme::TEXT)
            .add_modifier(Modifier::BOLD),
    );
    let text_dim = apply_bg(Style::default().fg(theme::MUTED));

    let marker_span = if selected {
        Span::styled("❯ ".to_string(), signal_bold)
    } else {
        Span::styled("  ".to_string(), apply_bg(Style::default()))
    };

    let n = idx + 1;
    let number_str = format!("{:>2} ", n);
    let number_style = if selected { signal_bold } else { muted };
    let number_span = Span::styled(number_str, number_style);

    let sender_disp = truncate_cells(&sender_str, w_sender.saturating_sub(1));
    let sender_padded = pad_right(&sender_disp, w_sender);
    let sender_style = if unread { text_bold } else { text_dim };
    let sender_span = Span::styled(sender_padded, sender_style);

    let (glyph_char, glyph_color) = if msg.urgent {
        (theme::G_URGENT, theme::RED)
    } else if unread {
        (theme::G_UNREAD, theme::SIGNAL_LIGHT)
    } else if msg.meta.starred {
        (theme::G_STARRED, theme::AMBER)
    } else if msg.has_attachment {
        (theme::G_ATTACHMENT, theme::TEAL)
    } else {
        (theme::G_READ, theme::FAINT)
    };
    let glyph_span = Span::styled(
        format!("{} ", glyph_char),
        apply_bg(Style::default().fg(glyph_color)),
    );

    let subj_disp = truncate_cells(&subject_str, subject_w.saturating_sub(1).max(1));
    let subj_padded = pad_right(&subj_disp, subject_w);
    let subj_style = if unread {
        apply_bg(Style::default().fg(theme::TEXT))
    } else {
        apply_bg(Style::default().fg(theme::MUTED))
    };
    let subject_span = Span::styled(subj_padded, subj_style);

    let time_span = Span::styled(
        format!(" {}", time_str),
        apply_bg(Style::default().fg(theme::MUTED)),
    );

    let mut line = Line::from(vec![
        marker_span,
        number_span,
        sender_span,
        glyph_span,
        subject_span,
        time_span,
    ]);
    if let Some(bg) = row_bg {
        line = line.style(Style::default().bg(bg));
    }
    line
}

// ── More count line ──────────────────────────────────────────────────────────

fn draw_more(frame: &mut Frame, area: Rect, count: u32) {
    let muted = Style::default().fg(theme::MUTED);
    let line = Line::from(vec![
        Span::styled(format!("  …{} more · ", count), muted),
        Span::styled(
            "G",
            Style::default()
                .fg(theme::TEXT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" to jump to end", muted),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

// ── Input box ────────────────────────────────────────────────────────────────

/// Always-live prompt. The buffer comes from `state.command` when typing is
/// active; otherwise the input shows just the prompt glyph with a blinking
/// cursor — no placeholder. Matches the Claude Code prompt model: typing is
/// always captured, `/` triggers an autocomplete popover (rendered separately).
fn draw_input(frame: &mut Frame, area: Rect, state: &AppState) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::HAIRLINE))
        .style(Style::default().bg(theme::RECESSED_WELL));
    frame.render_widget(block, area);

    let inner = Rect {
        x: area.x + 2,
        y: area.y + 1,
        width: area.width.saturating_sub(4),
        height: 1,
    };
    let prompt = "› ";
    let (buffer, cursor) = match &state.command {
        Some(cmd) => (cmd.query.as_str(), cmd.cursor),
        None => ("", 0),
    };
    let line = Line::from(vec![
        Span::styled(
            prompt,
            Style::default()
                .fg(theme::SIGNAL_LIGHT)
                .bg(theme::RECESSED_WELL)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            buffer.to_string(),
            Style::default()
                .fg(theme::TEXT)
                .bg(theme::RECESSED_WELL)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(line).style(Style::default().bg(theme::RECESSED_WELL)),
        inner,
    );

    let prefix_cells = UnicodeWidthStr::width(prompt) as u16;
    let typed_so_far = &buffer[..cursor.min(buffer.len())];
    let typed_cells = UnicodeWidthStr::width(typed_so_far) as u16;
    frame.set_cursor_position((inner.x + prefix_cells + typed_cells, inner.y));
}

// ── Hint / status row ────────────────────────────────────────────────────────

fn draw_hint(frame: &mut Frame, area: Rect) {
    let key = Style::default()
        .fg(theme::TEXT)
        .add_modifier(Modifier::BOLD);
    let label = Style::default().fg(theme::MUTED);
    let sep = Span::styled("   ", label);
    let line = Line::from(vec![
        Span::styled(" ", label),
        Span::styled("/", key),
        Span::styled(" commands", label),
        sep.clone(),
        Span::styled("↑↓", key),
        Span::styled(" select", label),
        sep.clone(),
        Span::styled("⏎", key),
        Span::styled(" open", label),
        sep.clone(),
        Span::styled("c", key),
        Span::styled(" compose", label),
        sep.clone(),
        Span::styled("r", key),
        Span::styled(" refresh", label),
        sep,
        Span::styled("?", key),
        Span::styled(" shortcuts", label),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_feedback(frame: &mut Frame, area: Rect, fb: &Feedback) {
    let (glyph, color) = match fb.kind {
        FeedbackKind::Success => (theme::G_SUCCESS, theme::GREEN),
        FeedbackKind::Warning => (theme::G_WARNING, theme::AMBER),
        FeedbackKind::Error => (theme::G_ERROR, theme::RED),
        FeedbackKind::Info => (theme::G_PROMPT, theme::SIGNAL_LIGHT),
    };
    let line = Line::from(vec![
        Span::styled(
            format!(" {} ", glyph),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(fb.text.clone(), Style::default().fg(theme::TEXT)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

// ── Loading screen ──────────────────────────────────────────────────────────

fn draw_loading(frame: &mut Frame, app: &App, kind: &LoadingKind) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);
    draw_welcome(frame, chunks[0], &app.state);

    let msg = match kind {
        LoadingKind::Inbox => "Loading inbox…",
        LoadingKind::Thread => "Loading thread…",
        LoadingKind::Action => "Working…",
    };
    let line = Line::from(Span::styled(msg, Style::default().fg(theme::MUTED)));
    let p = Paragraph::new(line).alignment(Alignment::Center);
    frame.render_widget(p, vertical_center(chunks[2], 1));

    draw_hint(frame, chunks[3]);
}

// ── Error screen ────────────────────────────────────────────────────────────

fn draw_error(frame: &mut Frame, app: &App, msg: &str) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);
    draw_welcome(frame, chunks[0], &app.state);

    let body = Line::from(vec![
        Span::styled(
            format!("{} ", theme::G_ERROR),
            Style::default().fg(theme::RED).add_modifier(Modifier::BOLD),
        ),
        Span::styled(msg.to_string(), Style::default().fg(theme::TEXT)),
    ]);
    let p = Paragraph::new(body).alignment(Alignment::Center);
    frame.render_widget(p, vertical_center(chunks[2], 1));

    let key = Style::default()
        .fg(theme::TEXT)
        .add_modifier(Modifier::BOLD);
    let label = Style::default().fg(theme::MUTED);
    let hint = Line::from(vec![
        Span::styled(" ", label),
        Span::styled("esc", key),
        Span::styled(" dismiss   ", label),
        Span::styled("⏎", key),
        Span::styled(" retry", label),
    ]);
    frame.render_widget(Paragraph::new(hint).alignment(Alignment::Center), chunks[3]);
}

// ── Reading screen ──────────────────────────────────────────────────────────

fn draw_reading(frame: &mut Frame, app: &App) {
    let Some(reading) = app.state.reading.as_ref() else {
        return;
    };
    let area = frame.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // echo "› {n}"
            Constraint::Length(1), // subject
            Constraint::Length(1), // meta
            Constraint::Length(1), // hairline
            Constraint::Min(0),    // body
            Constraint::Length(1), // spacer
            Constraint::Length(1), // chips
            Constraint::Length(1), // hint / status
        ])
        .split(area);

    let msg = &reading.thread[reading.message_idx];

    let echo = Line::from(vec![
        Span::styled(
            "› ",
            Style::default()
                .fg(theme::SIGNAL_LIGHT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "message {} of {}",
                reading.message_idx + 1,
                reading.thread.len()
            ),
            Style::default().fg(theme::MUTED),
        ),
    ]);
    frame.render_widget(Paragraph::new(echo), chunks[0]);

    let subj_str = if msg.subject.trim().is_empty() {
        "(no subject)".to_string()
    } else {
        msg.subject.clone()
    };
    let subj = Line::from(Span::styled(
        subj_str,
        Style::default()
            .fg(theme::TEXT)
            .add_modifier(Modifier::BOLD),
    ));
    frame.render_widget(Paragraph::new(subj), chunks[1]);

    let muted = Style::default().fg(theme::MUTED);
    let text = Style::default().fg(theme::TEXT);
    let mut meta_spans = vec![
        Span::styled("from ", muted),
        Span::styled(
            if msg.sender.trim().is_empty() {
                "(no sender)".to_string()
            } else {
                msg.sender.clone()
            },
            text,
        ),
        Span::styled(" · ", muted),
        Span::styled(format_time(&msg.date), muted),
    ];
    if let Some(label) = msg
        .folder_id
        .as_deref()
        .filter(|f| !f.is_empty() && *f != "inbox")
    {
        meta_spans.push(Span::styled(" · ", muted));
        meta_spans.push(Span::styled(
            format!("{} ", theme::G_LABEL),
            Style::default().fg(theme::VIOLET),
        ));
        meta_spans.push(Span::styled(
            label.to_string(),
            Style::default().fg(theme::VIOLET),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(meta_spans)), chunks[2]);

    // chunks[3] is reserved as a hairline gap before the body frame.
    // We render nothing there — the frame's top border provides the divider.

    let body_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::HAIRLINE))
        .title(Span::styled(" MESSAGE ", Style::default().fg(theme::MUTED)));
    let body_area = body_block.inner(chunks[4]);
    frame.render_widget(body_block, chunks[4]);

    let body_lines = build_body_lines(reading);
    frame.render_widget(
        Paragraph::new(body_lines).scroll((reading.scroll, 0)),
        body_area,
    );

    frame.render_widget(Paragraph::new(chip_line()), chunks[6]);

    if let Some(fb) = app.state.feedback.as_ref() {
        draw_feedback(frame, chunks[7], fb);
    } else {
        draw_reading_hint(frame, chunks[7]);
    }
}

fn build_body_lines(reading: &ReadingState) -> Vec<Line<'static>> {
    let text = Style::default().fg(theme::BODY);
    let muted = Style::default().fg(theme::MUTED);
    let key = Style::default()
        .fg(theme::SIGNAL_LIGHT)
        .add_modifier(Modifier::BOLD);

    let mut out: Vec<Line> = reading
        .body_lines
        .iter()
        .map(|l| Line::from(Span::styled(l.clone(), text)))
        .collect();

    if !reading.quoted_lines.is_empty() {
        if reading.quoted_collapsed {
            out.push(Line::from(""));
            out.push(Line::from(vec![
                Span::styled(format!("{} ", theme::G_TREE_CONT), muted),
                Span::styled(
                    format!("{} quoted lines hidden · ", reading.quoted_lines.len()),
                    muted,
                ),
                Span::styled("z", key),
                Span::styled(" to expand", muted),
            ]));
        } else {
            out.push(Line::from(""));
            for line in &reading.quoted_lines {
                out.push(Line::from(Span::styled(line.clone(), muted)));
            }
        }
    }
    out
}

fn chip_line() -> Line<'static> {
    let text_bold = Style::default()
        .fg(theme::TEXT)
        .add_modifier(Modifier::BOLD);
    let signal_chip = Style::default()
        .fg(theme::TEXT)
        .bg(theme::SIGNAL)
        .add_modifier(Modifier::BOLD);
    let signal_key = Style::default()
        .fg(theme::SIGNAL_LIGHT)
        .add_modifier(Modifier::BOLD);
    let green_key = Style::default()
        .fg(theme::GREEN)
        .add_modifier(Modifier::BOLD);
    let amber_key = Style::default()
        .fg(theme::AMBER)
        .add_modifier(Modifier::BOLD);
    let red_key = Style::default().fg(theme::RED).add_modifier(Modifier::BOLD);
    let gap = Span::styled("  ", Style::default());

    Line::from(vec![
        Span::styled(" r reply ", signal_chip),
        gap.clone(),
        Span::styled("a", signal_key),
        Span::styled(" reply all", text_bold),
        gap.clone(),
        Span::styled("f", signal_key),
        Span::styled(" forward", text_bold),
        gap.clone(),
        Span::styled("e", green_key),
        Span::styled(" archive", text_bold),
        gap.clone(),
        Span::styled("s", amber_key),
        Span::styled(" star", text_bold),
        gap,
        Span::styled("d", red_key),
        Span::styled(" delete", text_bold),
    ])
}

fn draw_reading_hint(frame: &mut Frame, area: Rect) {
    let key = Style::default()
        .fg(theme::TEXT)
        .add_modifier(Modifier::BOLD);
    let label = Style::default().fg(theme::MUTED);
    let line = Line::from(vec![
        Span::styled(" ", label),
        Span::styled("/", key),
        Span::styled(" commands   ", label),
        Span::styled("j/k", key),
        Span::styled(" prev/next msg   ", label),
        Span::styled("esc", key),
        Span::styled(" back to inbox", label),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

// ── Slash menu overlay ──────────────────────────────────────────────────────

/// Render the slash-command hints **above** the input row as a flat list —
/// no border, no popover box, no background fill. Mirrors Claude Code's
/// `PromptInputHelpMenu`: each row is `name<pad>command · description`, and
/// the whole list dissolves into the surrounding screen instead of floating
/// in a windowed panel.
///
/// Only invoked when the prompt buffer starts with `/` (the caller checks).
fn draw_command_menu(frame: &mut Frame, _app: &App, cmd: &CommandState, prior: PriorMode) {
    let area = frame.area();

    // Anchor at the input box. Layout for inbox: chunks[5] starts at
    //   y = welcome(5) + spacer(1) + rows(10) + more(1) + spacer(1) = 18,
    //   height 3. For reading, no dedicated input row — anchor near the
    //   bottom (3 rows above the bottom edge).
    let input_top = match prior {
        PriorMode::Inbox => area.y + 18,
        PriorMode::Reading => area.y + area.height.saturating_sub(4),
    };

    if cmd.filtered.is_empty() {
        return;
    }

    let rows = cmd.filtered.len() as u16;
    let bottom_y = input_top.saturating_sub(1);
    let mut top_y = bottom_y.saturating_sub(rows.saturating_sub(1));
    if top_y < area.y {
        top_y = area.y;
    }
    let height = bottom_y.saturating_sub(top_y).saturating_add(1);
    let list_area = Rect {
        x: area.x + 1,
        y: top_y,
        width: area.width.saturating_sub(2),
        height,
    };

    // Two-column layout: `/name` left-aligned, "command · desc" after a
    // gutter. Width tuned to the longest existing command name + a couple
    // of cells of breathing room.
    let name_col_w = SLASH_COMMANDS
        .iter()
        .map(|c| c.name.len() + 1) // +1 for the leading '/'
        .max()
        .unwrap_or(12)
        + 4;

    let muted = Style::default().fg(theme::MUTED);
    let text = Style::default().fg(theme::TEXT);
    let phase5 = Style::default().fg(theme::FAINT);

    let lines: Vec<Line> = cmd
        .filtered
        .iter()
        .enumerate()
        .map(|(i, &cmd_idx)| {
            let sc = SLASH_COMMANDS[cmd_idx];
            let selected = i == cmd.selected;
            let name_style = if sc.status == CmdStatus::Phase5 {
                phase5
            } else if selected {
                text.add_modifier(Modifier::BOLD)
            } else {
                muted
            };
            let desc_style = if sc.status == CmdStatus::Phase5 {
                phase5
            } else if selected {
                text
            } else {
                muted
            };

            let name = format!("/{:<w$}", sc.name, w = name_col_w - 1);
            Line::from(vec![
                Span::styled(name, name_style),
                Span::styled("command ", muted),
                Span::styled("· ", muted),
                Span::styled(sc.desc.to_string(), desc_style),
            ])
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), list_area);
}

// ── Compose screen ──────────────────────────────────────────────────────────

fn draw_compose(frame: &mut Frame, app: &App) {
    let Some(c) = app.state.compose.as_ref() else {
        return;
    };
    let area = frame.area();

    let has_marker = !matches!(c.kind, ComposeKind::New);
    let marker_rows = if has_marker { 1 } else { 0 };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),           // echo
            Constraint::Length(marker_rows), // reply marker (0 if New)
            Constraint::Length(1),           // To
            Constraint::Length(1),           // Subject
            Constraint::Length(1),           // hairline
            Constraint::Min(0),              // body
            Constraint::Length(1),           // spacer
            Constraint::Length(1),           // chips
            Constraint::Length(1),           // feedback / hint
        ])
        .split(area);

    // Echo row.
    let echo_label = match c.kind {
        ComposeKind::New => "› /compose",
        ComposeKind::Reply { .. } => "› /reply",
        ComposeKind::Forward { .. } => "› /forward",
    };
    let echo = Line::from(vec![Span::styled(
        echo_label,
        Style::default().fg(theme::MUTED),
    )]);
    frame.render_widget(Paragraph::new(echo), chunks[0]);

    // Reply / Forward marker.
    if has_marker {
        let verb = match c.kind {
            ComposeKind::Reply { .. } => "replying to",
            ComposeKind::Forward { .. } => "forwarding from",
            ComposeKind::New => unreachable!(),
        };
        let sender = c.source_sender().unwrap_or("");
        let marker = Line::from(vec![Span::styled(
            format!("↳ {verb} {sender}"),
            Style::default().fg(theme::GREEN),
        )]);
        frame.render_widget(Paragraph::new(marker), chunks[1]);
    }

    let muted = Style::default().fg(theme::MUTED);
    let text = Style::default().fg(theme::TEXT);

    const LABEL_W: u16 = 9;

    let to_line = Line::from(vec![
        Span::styled("To       ", muted),
        Span::styled(c.to.clone(), text),
    ]);
    frame.render_widget(Paragraph::new(to_line), chunks[2]);

    let subj_line = Line::from(vec![
        Span::styled("Subject  ", muted),
        Span::styled(c.subject.clone(), text),
    ]);
    frame.render_widget(Paragraph::new(subj_line), chunks[3]);

    // chunks[4] is reserved as the hairline gap before the body frame.
    // The frame's top border replaces the explicit rule that used to live here.

    let body_title = match c.kind {
        ComposeKind::New => " COMPOSE ",
        ComposeKind::Reply { .. } => " REPLY ",
        ComposeKind::Forward { .. } => " FORWARD ",
    };
    let body_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::HAIRLINE))
        .title(Span::styled(body_title, Style::default().fg(theme::MUTED)));
    let body_area = body_block.inner(chunks[5]);
    frame.render_widget(body_block, chunks[5]);

    // Body via tui_textarea. Hide cursor when body is not focused.
    let mut body_clone = c.body.clone();
    if c.focused == ComposeField::Body {
        body_clone.set_cursor_style(
            Style::default()
                .fg(theme::INK)
                .bg(theme::SIGNAL_LIGHT)
                .add_modifier(Modifier::BOLD),
        );
    } else {
        body_clone.set_cursor_style(Style::default());
    }
    frame.render_widget(&body_clone, body_area);

    // Chip row.
    frame.render_widget(Paragraph::new(compose_chip_line(c)), chunks[7]);

    // Feedback / hint.
    if let Some(fb) = app.state.feedback.as_ref() {
        draw_feedback(frame, chunks[8], fb);
    } else {
        draw_compose_hint(frame, chunks[8]);
    }

    // Cursor — body cursor is owned by tui_textarea, so only place ours for
    // the single-line fields.
    match c.focused {
        ComposeField::To => {
            let typed = &c.to[..c.to_cursor.min(c.to.len())];
            let cells = UnicodeWidthStr::width(typed) as u16;
            frame.set_cursor_position((chunks[2].x + LABEL_W + cells, chunks[2].y));
        }
        ComposeField::Subject => {
            let typed = &c.subject[..c.subject_cursor.min(c.subject.len())];
            let cells = UnicodeWidthStr::width(typed) as u16;
            frame.set_cursor_position((chunks[3].x + LABEL_W + cells, chunks[3].y));
        }
        ComposeField::Body => { /* tui_textarea handles its own cursor */ }
    }
}

fn compose_chip_line(c: &ComposeState) -> Line<'static> {
    let text_bold = Style::default()
        .fg(theme::TEXT)
        .add_modifier(Modifier::BOLD);
    let signal_chip = Style::default()
        .fg(theme::TEXT)
        .bg(theme::SIGNAL)
        .add_modifier(Modifier::BOLD);
    let cyan_key = Style::default()
        .fg(theme::CYAN)
        .add_modifier(Modifier::BOLD);
    let red_key = Style::default().fg(theme::RED).add_modifier(Modifier::BOLD);
    let gap = Span::styled("  ", Style::default());

    let send_label = if c.submitting {
        " sending… "
    } else {
        " ⌃⏎ send "
    };

    Line::from(vec![
        Span::styled(send_label.to_string(), signal_chip),
        gap.clone(),
        Span::styled("⌃d", cyan_key),
        Span::styled(" draft", text_bold),
        gap.clone(),
        Span::styled("⌃a", cyan_key),
        Span::styled(" attach", text_bold),
        gap,
        Span::styled("esc", red_key),
        Span::styled(" discard", text_bold),
    ])
}

fn draw_compose_hint(frame: &mut Frame, area: Rect) {
    let key = Style::default()
        .fg(theme::TEXT)
        .add_modifier(Modifier::BOLD);
    let label = Style::default().fg(theme::MUTED);
    let line = Line::from(vec![
        Span::styled(" ", label),
        Span::styled("Tab", key),
        Span::styled(" next field   ", label),
        Span::styled("⌃⏎", key),
        Span::styled(" send   ", label),
        Span::styled("⌃d", key),
        Span::styled(" save draft", label),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_discard_overlay(frame: &mut Frame) {
    let area = frame.area();
    let w = 28u16.min(area.width.saturating_sub(4));
    let h = 5u16.min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let rect = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    frame.render_widget(Clear, rect);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::AMBER))
        .style(Style::default().bg(theme::RECESSED_WELL));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let text = Style::default().fg(theme::TEXT).bg(theme::RECESSED_WELL);
    let muted = Style::default().fg(theme::MUTED).bg(theme::RECESSED_WELL);
    let key = Style::default()
        .fg(theme::TEXT)
        .bg(theme::RECESSED_WELL)
        .add_modifier(Modifier::BOLD);

    let lines = vec![
        Line::from(Span::styled(
            "Discard draft?",
            text.add_modifier(Modifier::BOLD),
        ))
        .alignment(Alignment::Center),
        Line::from(""),
        Line::from(vec![
            Span::styled("y", key),
            Span::styled(" discard  ", muted),
            Span::styled("n", key),
            Span::styled(" keep", muted),
        ])
        .alignment(Alignment::Center),
    ];
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(theme::RECESSED_WELL)),
        inner,
    );
}

// ── AI screens ──────────────────────────────────────────────────────────────

fn draw_ai_pending(frame: &mut Frame, kind: AiKind) {
    let area = frame.area();
    let msg = match kind {
        AiKind::Summarize => "Reading thread…",
        AiKind::Draft => "Drafting…",
        AiKind::Ask => "Searching…",
        AiKind::Triage => "Sorting…",
    };
    let line = Line::from(vec![
        Span::styled(
            format!("{} ", theme::G_AI),
            Style::default()
                .fg(theme::VIOLET)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(msg, Style::default().fg(theme::MUTED)),
    ]);
    let p = Paragraph::new(line).alignment(Alignment::Center);
    frame.render_widget(p, vertical_center(area, 1));
}

fn draw_ai_summarize(frame: &mut Frame, app: &App) {
    let Some(AiResultState::Summarize {
        thread_subject,
        message_count,
        people_count,
        bullets,
        suggested_replies,
        selected_reply,
        ..
    }) = app.state.ai.as_ref()
    else {
        return;
    };
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // echo
            Constraint::Length(1), // subject header
            Constraint::Length(1), // tree meta
            Constraint::Length(1), // spacer
            Constraint::Min(0),    // bullets
            Constraint::Length(1), // spacer
            Constraint::Length(1), // "Suggested replies" label
            Constraint::Min(0),    // reply rows
            Constraint::Length(1), // spacer
            Constraint::Length(1), // hint
        ])
        .split(area);

    let muted = Style::default().fg(theme::MUTED);
    let text = Style::default().fg(theme::TEXT);
    let text_bold = text.add_modifier(Modifier::BOLD);
    let violet = Style::default()
        .fg(theme::VIOLET)
        .add_modifier(Modifier::BOLD);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled("› /summarize", muted))),
        chunks[0],
    );

    let header = Line::from(vec![
        Span::styled(format!("{} ", theme::G_AI), violet),
        Span::styled(thread_subject.clone(), text_bold),
    ]);
    frame.render_widget(Paragraph::new(header), chunks[1]);

    let meta = Line::from(vec![
        Span::styled(format!("{} ", theme::G_TREE), muted),
        Span::styled(
            format!("{message_count} messages · {people_count} people"),
            muted,
        ),
    ]);
    frame.render_widget(Paragraph::new(meta), chunks[2]);

    let body_w = chunks[4].width as usize;
    let mut bullet_lines: Vec<Line> = Vec::new();
    for b in bullets {
        let glyph_style = if b.is_action_item {
            Style::default()
                .fg(theme::AMBER)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::SIGNAL_LIGHT)
        };
        let txt_style = if b.is_action_item { text_bold } else { text };
        let glyph = if b.is_action_item { "! " } else { "• " };
        let wrap_w = body_w.saturating_sub(4).max(10);
        let wrapped: Vec<String> = textwrap::wrap(&b.text, wrap_w)
            .into_iter()
            .map(|c| c.into_owned())
            .collect();
        for (i, chunk) in wrapped.iter().enumerate() {
            if i == 0 {
                bullet_lines.push(Line::from(vec![
                    Span::styled("  ".to_string(), muted),
                    Span::styled(glyph.to_string(), glyph_style),
                    Span::styled(chunk.clone(), txt_style),
                ]));
            } else {
                bullet_lines.push(Line::from(vec![
                    Span::styled("    ".to_string(), muted),
                    Span::styled(chunk.clone(), txt_style),
                ]));
            }
        }
    }
    frame.render_widget(Paragraph::new(bullet_lines), chunks[4]);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled("  Suggested replies", muted))),
        chunks[6],
    );

    let mut reply_lines: Vec<Line> = Vec::new();
    for (i, r) in suggested_replies.iter().enumerate() {
        let is_selected = *selected_reply == Some(i);
        let num_str = format!(" [{}] ", i + 1);
        let num_style = if is_selected {
            Style::default()
                .fg(theme::TEXT)
                .bg(theme::SIGNAL)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(theme::SIGNAL_LIGHT)
                .add_modifier(Modifier::BOLD)
        };
        let body_style = if is_selected {
            text_bold
        } else {
            Style::default().fg(theme::TEXT)
        };
        let wrap_w = (chunks[7].width as usize).saturating_sub(6).max(10);
        let wrapped: Vec<String> = textwrap::wrap(r, wrap_w)
            .into_iter()
            .map(|c| c.into_owned())
            .collect();
        for (j, chunk) in wrapped.iter().enumerate() {
            if j == 0 {
                reply_lines.push(Line::from(vec![
                    Span::styled(num_str.clone(), num_style),
                    Span::styled(chunk.clone(), body_style),
                ]));
            } else {
                reply_lines.push(Line::from(vec![
                    Span::styled("     ".to_string(), muted),
                    Span::styled(chunk.clone(), body_style),
                ]));
            }
        }
    }
    if reply_lines.is_empty() {
        reply_lines.push(Line::from(Span::styled(
            "  (no suggestions)".to_string(),
            muted,
        )));
    }
    frame.render_widget(Paragraph::new(reply_lines), chunks[7]);

    if let Some(fb) = app.state.feedback.as_ref() {
        draw_feedback(frame, chunks[9], fb);
    } else {
        draw_ai_summarize_hint(frame, chunks[9]);
    }
}

fn draw_ai_summarize_hint(frame: &mut Frame, area: Rect) {
    let key = Style::default()
        .fg(theme::TEXT)
        .add_modifier(Modifier::BOLD);
    let label = Style::default().fg(theme::MUTED);
    let line = Line::from(vec![
        Span::styled(" ", label),
        Span::styled("1-3", key),
        Span::styled(" pick reply   ", label),
        Span::styled("e/⏎", key),
        Span::styled(" compose   ", label),
        Span::styled("esc", key),
        Span::styled(" back", label),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_ai_draft(frame: &mut Frame, app: &App) {
    let Some(AiResultState::Draft {
        echo_prompt,
        to,
        subject,
        body,
        ..
    }) = app.state.ai.as_ref()
    else {
        return;
    };
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // echo prompt
            Constraint::Length(1), // status header
            Constraint::Length(1), // To
            Constraint::Length(1), // Subject
            Constraint::Length(1), // hairline
            Constraint::Min(0),    // body
            Constraint::Length(1), // spacer
            Constraint::Length(1), // chips
            Constraint::Length(1), // hint / feedback
        ])
        .split(area);

    let muted = Style::default().fg(theme::MUTED);
    let text = Style::default().fg(theme::TEXT);
    let text_bold = text.add_modifier(Modifier::BOLD);
    let violet = Style::default()
        .fg(theme::VIOLET)
        .add_modifier(Modifier::BOLD);

    let echo = Line::from(vec![
        Span::styled("› /draft ", muted),
        Span::styled(echo_prompt.clone(), muted),
    ]);
    frame.render_widget(Paragraph::new(echo), chunks[0]);

    let header = Line::from(vec![
        Span::styled(format!("{} ", theme::G_AI), violet),
        Span::styled("Draft ready", text_bold),
    ]);
    frame.render_widget(Paragraph::new(header), chunks[1]);

    let to_line = Line::from(vec![
        Span::styled("To       ", muted),
        Span::styled(to.clone(), text),
    ]);
    frame.render_widget(Paragraph::new(to_line), chunks[2]);

    let subj_line = Line::from(vec![
        Span::styled("Subject  ", muted),
        Span::styled(subject.clone(), text),
    ]);
    frame.render_widget(Paragraph::new(subj_line), chunks[3]);

    let rule_w = chunks[4].width as usize;
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled("─".repeat(rule_w), muted))),
        chunks[4],
    );

    let wrap_w = (chunks[5].width as usize).max(10);
    let mut body_lines: Vec<Line> = Vec::new();
    for raw in body.split('\n') {
        if raw.is_empty() {
            body_lines.push(Line::from(""));
            continue;
        }
        for chunk in textwrap::wrap(raw, wrap_w) {
            body_lines.push(Line::from(Span::styled(chunk.into_owned(), text)));
        }
    }
    frame.render_widget(Paragraph::new(body_lines), chunks[5]);

    frame.render_widget(Paragraph::new(ai_draft_chip_line()), chunks[7]);

    if let Some(fb) = app.state.feedback.as_ref() {
        draw_feedback(frame, chunks[8], fb);
    } else {
        draw_ai_draft_hint(frame, chunks[8]);
    }
}

fn ai_draft_chip_line() -> Line<'static> {
    let text_bold = Style::default()
        .fg(theme::TEXT)
        .add_modifier(Modifier::BOLD);
    let signal_chip = Style::default()
        .fg(theme::TEXT)
        .bg(theme::SIGNAL)
        .add_modifier(Modifier::BOLD);
    let violet_key = Style::default()
        .fg(theme::VIOLET)
        .add_modifier(Modifier::BOLD);
    let cyan_key = Style::default()
        .fg(theme::CYAN)
        .add_modifier(Modifier::BOLD);
    let red_key = Style::default().fg(theme::RED).add_modifier(Modifier::BOLD);
    let gap = Span::styled("  ", Style::default());

    Line::from(vec![
        Span::styled(" ⌃⏎ send ".to_string(), signal_chip),
        gap.clone(),
        Span::styled("⌃r", violet_key),
        Span::styled(" regenerate", text_bold),
        gap.clone(),
        Span::styled("e", cyan_key),
        Span::styled(" edit", text_bold),
        gap,
        Span::styled("esc", red_key),
        Span::styled(" discard", text_bold),
    ])
}

fn draw_ai_draft_hint(frame: &mut Frame, area: Rect) {
    let key = Style::default()
        .fg(theme::TEXT)
        .add_modifier(Modifier::BOLD);
    let label = Style::default().fg(theme::MUTED);
    let line = Line::from(vec![
        Span::styled(" ", label),
        Span::styled("⌃⏎", key),
        Span::styled(" send   ", label),
        Span::styled("⌃r", key),
        Span::styled(" regenerate   ", label),
        Span::styled("e", key),
        Span::styled(" edit   ", label),
        Span::styled("esc", key),
        Span::styled(" discard", label),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_ai_ask(frame: &mut Frame, app: &App) {
    let Some(AiResultState::Ask {
        echo_query,
        summary,
        results,
        selected_index,
    }) = app.state.ai.as_ref()
    else {
        return;
    };
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // echo
            Constraint::Length(1), // summary
            Constraint::Length(1), // spacer
            Constraint::Min(0),    // results
            Constraint::Length(1), // hint
        ])
        .split(area);

    let muted = Style::default().fg(theme::MUTED);
    let violet = Style::default()
        .fg(theme::VIOLET)
        .add_modifier(Modifier::BOLD);

    let echo = Line::from(vec![
        Span::styled("› /ask ", muted),
        Span::styled(echo_query.clone(), muted),
    ]);
    frame.render_widget(Paragraph::new(echo), chunks[0]);

    let header = Line::from(vec![
        Span::styled(format!("{} ", theme::G_AI), violet),
        Span::styled(summary.clone(), Style::default().fg(theme::TEXT)),
    ]);
    frame.render_widget(Paragraph::new(header), chunks[1]);

    let total_w = chunks[3].width as usize;
    const W_MARKER: usize = 2;
    const W_SENDER: usize = 16;
    const W_GLYPH: usize = 2;
    const TIME_GUTTER: usize = 1;

    let lines: Vec<Line> = results
        .iter()
        .enumerate()
        .map(|(i, r)| {
            ask_row_line(
                r,
                i == *selected_index,
                total_w,
                W_MARKER,
                W_SENDER,
                W_GLYPH,
                TIME_GUTTER,
            )
        })
        .collect();
    if lines.is_empty() {
        let empty = Paragraph::new(Line::from(Span::styled("  (no results)", muted)));
        frame.render_widget(empty, chunks[3]);
    } else {
        frame.render_widget(Paragraph::new(lines), chunks[3]);
    }

    if let Some(fb) = app.state.feedback.as_ref() {
        draw_feedback(frame, chunks[4], fb);
    } else {
        draw_ai_ask_hint(frame, chunks[4]);
    }
}

fn ask_row_line(
    r: &crate::api::types::AskResult,
    selected: bool,
    total_w: usize,
    w_marker: usize,
    w_sender: usize,
    w_glyph: usize,
    time_gutter: usize,
) -> Line<'static> {
    let time_str = format_time(&r.date);
    let sender_str = if r.sender.trim().is_empty() {
        "(no sender)".to_string()
    } else {
        r.sender.clone()
    };
    let subject_str = if r.subject.trim().is_empty() {
        "(no subject)".to_string()
    } else {
        r.subject.clone()
    };

    let time_w = UnicodeWidthStr::width(time_str.as_str()).max(1);
    let fixed = w_marker + w_sender + w_glyph + time_gutter + time_w;
    let subject_w = total_w.saturating_sub(fixed);

    let row_bg = if selected {
        Some(theme::ROW_SELECT)
    } else {
        None
    };
    let apply_bg = |s: Style| -> Style {
        if let Some(bg) = row_bg {
            s.bg(bg)
        } else {
            s
        }
    };

    let signal_bold = apply_bg(
        Style::default()
            .fg(theme::SIGNAL_LIGHT)
            .add_modifier(Modifier::BOLD),
    );

    let marker_span = if selected {
        Span::styled(format!("{} ", theme::G_SELECTED), signal_bold)
    } else {
        Span::styled("  ".to_string(), apply_bg(Style::default()))
    };

    let sender_disp = truncate_cells(&sender_str, w_sender.saturating_sub(1));
    let sender_padded = pad_right(&sender_disp, w_sender);
    let sender_span = Span::styled(sender_padded, apply_bg(Style::default().fg(theme::MUTED)));

    let (glyph_char, glyph_color) = if r.glyph.as_deref() == Some("attachment") {
        (theme::G_ATTACHMENT, theme::TEAL)
    } else {
        (' ', theme::MUTED)
    };
    let glyph_span = Span::styled(
        format!("{} ", glyph_char),
        apply_bg(Style::default().fg(glyph_color)),
    );

    let subj_disp = truncate_cells(&subject_str, subject_w.saturating_sub(1).max(1));
    let subj_padded = pad_right(&subj_disp, subject_w);
    let subj_style = if selected {
        apply_bg(
            Style::default()
                .fg(theme::TEXT)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        apply_bg(Style::default().fg(theme::MUTED))
    };
    let subject_span = Span::styled(subj_padded, subj_style);

    let time_span = Span::styled(
        format!(" {}", time_str),
        apply_bg(Style::default().fg(theme::MUTED)),
    );

    let mut line = Line::from(vec![
        marker_span,
        sender_span,
        glyph_span,
        subject_span,
        time_span,
    ]);
    if let Some(bg) = row_bg {
        line = line.style(Style::default().bg(bg));
    }
    line
}

fn draw_ai_ask_hint(frame: &mut Frame, area: Rect) {
    let key = Style::default()
        .fg(theme::TEXT)
        .add_modifier(Modifier::BOLD);
    let label = Style::default().fg(theme::MUTED);
    let line = Line::from(vec![
        Span::styled(" ", label),
        Span::styled("j/k", key),
        Span::styled(" select   ", label),
        Span::styled("⏎", key),
        Span::styled(" open   ", label),
        Span::styled("esc", key),
        Span::styled(" back", label),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_ai_triage(frame: &mut Frame, app: &App) {
    let Some(AiResultState::Triage { categories }) = app.state.ai.as_ref() else {
        return;
    };
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // echo
            Constraint::Length(1), // header
            Constraint::Length(1), // spacer
            Constraint::Length(1), // chips
            Constraint::Min(0),    // breakdown lines
            Constraint::Length(1), // hint
        ])
        .split(area);

    let muted = Style::default().fg(theme::MUTED);
    let text = Style::default().fg(theme::TEXT);
    let violet = Style::default()
        .fg(theme::VIOLET)
        .add_modifier(Modifier::BOLD);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled("› /triage", muted))),
        chunks[0],
    );

    let total: u64 = categories.iter().map(|c| c.count).sum();
    let header = Line::from(vec![
        Span::styled(format!("{} ", theme::G_AI), violet),
        Span::styled(format!("Sorted {total} new messages"), text),
    ]);
    frame.render_widget(Paragraph::new(header), chunks[1]);

    frame.render_widget(Paragraph::new(triage_chip_line(categories)), chunks[3]);

    let mut breakdown: Vec<Line> = Vec::new();
    for cat in categories {
        let style = match cat.label.as_str() {
            "important" => Style::default().fg(theme::RED).add_modifier(Modifier::BOLD),
            "updates" => Style::default()
                .fg(theme::SIGNAL_LIGHT)
                .add_modifier(Modifier::BOLD),
            "promotions" => Style::default().fg(theme::MUTED),
            _ => Style::default().fg(theme::TEXT),
        };
        breakdown.push(Line::from(vec![
            Span::styled(format!("  {} ", cat.glyph), style),
            Span::styled(format!("{}  ", cat.label), text),
            Span::styled(format!("({})", cat.count), muted),
        ]));
    }
    frame.render_widget(Paragraph::new(breakdown), chunks[4]);

    if let Some(fb) = app.state.feedback.as_ref() {
        draw_feedback(frame, chunks[5], fb);
    } else {
        draw_ai_triage_hint(frame, chunks[5]);
    }
}

fn triage_chip_line(categories: &[crate::api::types::TriageCategory]) -> Line<'static> {
    let text_bold = Style::default()
        .fg(theme::TEXT)
        .add_modifier(Modifier::BOLD);
    let gap = Span::styled("  ", Style::default());
    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled(" ", Style::default()));
    for (i, cat) in categories.iter().enumerate() {
        let chip_style = match cat.label.as_str() {
            "important" => Style::default()
                .fg(theme::TEXT)
                .bg(theme::RED)
                .add_modifier(Modifier::BOLD),
            "updates" => Style::default()
                .fg(theme::TEXT)
                .bg(theme::SIGNAL)
                .add_modifier(Modifier::BOLD),
            "promotions" => Style::default()
                .fg(theme::MUTED)
                .bg(theme::HAIRLINE)
                .add_modifier(Modifier::BOLD),
            _ => text_bold,
        };
        spans.push(Span::styled(
            format!(" {} {} ({}) ", cat.glyph, cat.label, cat.count),
            chip_style,
        ));
        if i + 1 != categories.len() {
            spans.push(gap.clone());
        }
    }
    Line::from(spans)
}

fn draw_ai_triage_hint(frame: &mut Frame, area: Rect) {
    let key = Style::default()
        .fg(theme::TEXT)
        .add_modifier(Modifier::BOLD);
    let label = Style::default().fg(theme::MUTED);
    let line = Line::from(vec![
        Span::styled(" ", label),
        Span::styled("⏎/esc", key),
        Span::styled(" back to inbox", label),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

// ── helpers ─────────────────────────────────────────────────────────────────

fn vertical_center(area: Rect, h: u16) -> Rect {
    let h = h.min(area.height);
    let y_off = (area.height - h) / 2;
    Rect {
        x: area.x,
        y: area.y + y_off,
        width: area.width,
        height: h,
    }
}

fn format_time(date: &str) -> String {
    let parsed: Option<DateTime<Local>> = DateTime::parse_from_rfc3339(date)
        .ok()
        .map(|d| d.with_timezone(&Local))
        .or_else(|| {
            DateTime::parse_from_rfc2822(date)
                .ok()
                .map(|d| d.with_timezone(&Local))
        });
    let Some(dt) = parsed else {
        return date.to_string();
    };
    let now = Local::now();
    let today = Local
        .with_ymd_and_hms(now.year(), now.month(), now.day(), 0, 0, 0)
        .single();
    if let Some(today_start) = today {
        if dt >= today_start {
            return dt.format("%H:%M").to_string();
        }
    }
    if now.signed_duration_since(dt) < Duration::days(7) {
        return dt.format("%a").to_string();
    }
    dt.format("%b %-d").to_string()
}

fn truncate_cells(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(s) <= max {
        return s.to_string();
    }
    let budget = max.saturating_sub(1);
    let mut out = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw > budget {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

fn pad_right(s: &str, width: usize) -> String {
    let cur = UnicodeWidthStr::width(s);
    if cur >= width {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + (width - cur));
    out.push_str(s);
    for _ in 0..(width - cur) {
        out.push(' ');
    }
    out
}
