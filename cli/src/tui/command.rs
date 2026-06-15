//! Slash command catalog + filter.
//!
//! Commands are scoped: some only make sense when a thread is open (`reply`,
//! `forward`), others work anywhere. Active commands run in Phase 4; Phase 5
//! placeholders render in muted style and flash "lands in Phase 5" on Enter.

use crate::state::PriorMode;

#[derive(Debug, Clone, Copy)]
pub struct SlashCommand {
    pub name: &'static str,
    pub desc: &'static str,
    pub scope: Scope,
    pub status: CmdStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Inbox,
    Reading,
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmdStatus {
    Active,
    #[allow(dead_code)] // reserved for future phased rollouts
    Phase5,
}

pub const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "compose",
        desc: "Write a new message",
        scope: Scope::Both,
        status: CmdStatus::Active,
    },
    SlashCommand {
        name: "reply",
        desc: "Reply to the open message",
        scope: Scope::Reading,
        status: CmdStatus::Active,
    },
    SlashCommand {
        name: "forward",
        desc: "Forward the open message",
        scope: Scope::Reading,
        status: CmdStatus::Active,
    },
    SlashCommand {
        name: "archive",
        desc: "Archive selected/open",
        scope: Scope::Both,
        status: CmdStatus::Active,
    },
    SlashCommand {
        name: "delete",
        desc: "Delete selected/open",
        scope: Scope::Both,
        status: CmdStatus::Active,
    },
    SlashCommand {
        name: "star",
        desc: "Toggle star on selected/open",
        scope: Scope::Both,
        status: CmdStatus::Active,
    },
    SlashCommand {
        name: "draft",
        desc: "Generate a draft with AI",
        scope: Scope::Both,
        status: CmdStatus::Active,
    },
    SlashCommand {
        name: "summarize",
        desc: "Summarize this thread",
        scope: Scope::Reading,
        status: CmdStatus::Active,
    },
    SlashCommand {
        name: "ask",
        desc: "Ask across mail",
        scope: Scope::Both,
        status: CmdStatus::Active,
    },
    SlashCommand {
        name: "triage",
        desc: "Auto-categorize new mail",
        scope: Scope::Both,
        status: CmdStatus::Active,
    },
    SlashCommand {
        name: "logout",
        desc: "Sign out",
        scope: Scope::Both,
        status: CmdStatus::Active,
    },
];

/// Substring match on the first whitespace-separated token of `query`,
/// scope-filtered. Returns indices into `SLASH_COMMANDS`. Trailing args
/// (after the first space) are ignored here — they're consumed by
/// `run_slash_command` via `split_args`.
pub fn filter(query: &str, prior: PriorMode) -> Vec<usize> {
    let first = query.split_whitespace().next().unwrap_or("");
    let q = first.trim_start_matches('/').to_ascii_lowercase();
    SLASH_COMMANDS
        .iter()
        .enumerate()
        .filter(|(_, c)| match c.scope {
            Scope::Both => true,
            Scope::Inbox => matches!(prior, PriorMode::Inbox),
            Scope::Reading => matches!(prior, PriorMode::Reading),
        })
        .filter(|(_, c)| q.is_empty() || c.name.contains(&q))
        .map(|(i, _)| i)
        .collect()
}

/// Returns the substring after the first whitespace run, trimmed.
/// `"draft  hi there"` → `"hi there"`. No first token → `""`.
pub fn split_args(query: &str) -> &str {
    match query.split_once(char::is_whitespace) {
        Some((_, rest)) => rest.trim_start(),
        None => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names_of(indices: &[usize]) -> Vec<&'static str> {
        indices.iter().map(|&i| SLASH_COMMANDS[i].name).collect()
    }

    #[test]
    fn filter_inbox_scope_omits_reading_only_commands() {
        let names = names_of(&filter("", PriorMode::Inbox));
        assert!(!names.contains(&"reply"));
        assert!(!names.contains(&"forward"));
        assert!(!names.contains(&"summarize"));
        assert!(names.contains(&"compose"));
    }

    #[test]
    fn filter_reading_scope_includes_reply_and_forward() {
        let names = names_of(&filter("", PriorMode::Reading));
        assert!(names.contains(&"reply"));
        assert!(names.contains(&"forward"));
        assert!(names.contains(&"summarize"));
    }

    #[test]
    fn filter_substring_matches_command_name() {
        assert_eq!(names_of(&filter("arch", PriorMode::Inbox)), vec!["archive"]);
    }

    #[test]
    fn filter_only_uses_first_token() {
        // "draft hi there" matches `draft`, ignoring the trailing args.
        assert_eq!(
            names_of(&filter("draft hi there", PriorMode::Inbox)),
            vec!["draft"]
        );
    }

    #[test]
    fn filter_strips_leading_slash() {
        assert_eq!(names_of(&filter("/ask", PriorMode::Inbox)), vec!["ask"]);
    }

    #[test]
    fn split_args_returns_rest_after_first_whitespace_run() {
        assert_eq!(split_args("draft  hi there"), "hi there");
    }

    #[test]
    fn split_args_empty_when_no_args() {
        assert_eq!(split_args("draft"), "");
        assert_eq!(split_args(""), "");
    }
}
