//! Message-body parsing for the Read view.
//!
//! Two jobs:
//!   1. Convert HTML bodies to terminal-friendly text (via `html2text`).
//!      Plain-text bodies pass through untouched — html2text re-wraps and
//!      escapes `&` if you feed it plain text, so we sniff for tags first.
//!   2. Split off a trailing quoted block (`> …`, `On … wrote:`, sigline
//!      `-- `). The visible portion is what we render; the quoted portion
//!      stays hidden until the user hits `z`.
//!
//! Heuristic — not a real RFC parser. Failure mode is "we show too much"
//! (under-collapse), which is harmless. Phase 4 can tighten this.

/// Returns `(visible_lines, quoted_lines)` already word-wrapped to `width`.
pub fn parse_body(raw: &str, width: usize) -> (Vec<String>, Vec<String>) {
    let width = width.max(40);
    let text = if looks_like_html(raw) {
        html2text::from_read(raw.as_bytes(), width).unwrap_or_else(|_| raw.to_string())
    } else {
        raw.to_string()
    };

    let (visible, quoted) = split_quoted(&text);
    let visible_wrapped = wrap_lines(&visible, width);
    let quoted_wrapped = wrap_lines(&quoted, width);
    (visible_wrapped, quoted_wrapped)
}

fn looks_like_html(s: &str) -> bool {
    s.contains('<') && s.contains('>')
}

/// Walk lines; once we hit a "quote marker", everything from there to EOF
/// is treated as quoted. This intentionally over-collapses rather than
/// trying to interleave quoted + reply chunks.
fn split_quoted(text: &str) -> (Vec<String>, Vec<String>) {
    let mut visible = Vec::new();
    let mut quoted = Vec::new();
    let mut in_quote = false;
    for line in text.lines() {
        if !in_quote && is_quote_marker(line) {
            in_quote = true;
        }
        if in_quote {
            quoted.push(line.to_string());
        } else {
            visible.push(line.to_string());
        }
    }
    // Trim trailing blank lines from visible for cleaner rendering.
    while visible.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
        visible.pop();
    }
    (visible, quoted)
}

fn is_quote_marker(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.starts_with('>') {
        return true;
    }
    // Sigline: "-- " (with trailing space) is the convention; many clients
    // also send the bare "--".
    if line == "-- " || line == "--" {
        return true;
    }
    // "On Mon, Jun 15, 2026 at 9:41 AM, So-and-so <…@…> wrote:" — common
    // Gmail-style attribution. Loose check: starts with "On " and ends
    // with "wrote:" (allowing trailing whitespace).
    let l = line.trim_end();
    if l.starts_with("On ") && l.ends_with("wrote:") {
        return true;
    }
    false
}

/// Word-wrap each line to `width` display columns via `textwrap`. Empty
/// lines pass through unchanged so paragraph breaks survive.
fn wrap_lines(lines: &[String], width: usize) -> Vec<String> {
    let mut out = Vec::with_capacity(lines.len());
    for raw in lines {
        if raw.is_empty() {
            out.push(String::new());
            continue;
        }
        for wrapped in textwrap::wrap(raw, width) {
            out.push(wrapped.into_owned());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_passes_through() {
        let (v, q) = parse_body("hello world", 80);
        assert_eq!(v, vec!["hello world".to_string()]);
        assert!(q.is_empty());
    }

    #[test]
    fn html_is_flattened_to_text() {
        let (v, _q) = parse_body("<p>hello <b>world</b></p>", 80);
        let joined = v.join(" ");
        assert!(joined.contains("hello"));
        assert!(joined.contains("world"));
        assert!(!joined.contains("<p>"));
    }

    #[test]
    fn greater_than_marker_splits_quoted() {
        let (v, q) = parse_body("reply text\n> old line", 80);
        assert_eq!(v, vec!["reply text".to_string()]);
        assert_eq!(q, vec!["> old line".to_string()]);
    }

    #[test]
    fn sigline_marker_splits_quoted() {
        let (v, q) = parse_body("body\n-- \nsig", 80);
        assert_eq!(v, vec!["body".to_string()]);
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn on_wrote_attribution_splits_quoted() {
        let raw = "thanks\nOn Mon, Jun 15, 2026, X wrote:\n> old";
        let (v, q) = parse_body(raw, 80);
        assert_eq!(v, vec!["thanks".to_string()]);
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn wrap_respects_width_floor() {
        // The 40-col floor in parse_body protects us from 0-width math.
        let raw = "aaaa bbbb cccc dddd eeee ffff gggg hhhh";
        let (v, _q) = parse_body(raw, 10);
        // floor is 40, so this short line stays one row.
        assert_eq!(v.len(), 1);
    }
}
