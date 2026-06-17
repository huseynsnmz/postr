//! Minimal RFC 5322 / 2822 MIME builder for outbound text/plain emails.
//!
//! v1 scope (matches Workflow B plan):
//!   * text/plain only — no HTML, no attachments.
//!   * `to`/`cc` are comma-separated strings (already joined by caller).
//!   * `bcc` is recorded on the DO row but never added as a header (RFC).
//!   * Subject is RFC 2047 base64-encoded when it contains non-ASCII bytes.
//!   * Date is RFC 2822 (`chrono::Utc::now().to_rfc2822()`).
//!
//! No workers-rs imports here so the module compiles outside the wasm
//! target if needed for unit tests in the future.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;

/// Build a text/plain RFC 5322 message ready for `EmailMessage::new`.
/// `message_id` is the bare id; we wrap it in `<>` here.
#[allow(clippy::too_many_arguments)]
pub fn build_text_mime(
    from: &str,
    to: &str,
    cc: Option<&str>,
    subject: &str,
    date_rfc2822: &str,
    message_id: &str,
    in_reply_to: Option<&str>,
    references: &[String],
    body: &str,
) -> String {
    let mut out = String::with_capacity(body.len() + 512);
    push_header(&mut out, "From", from);
    push_header(&mut out, "To", to);
    if let Some(cc_val) = cc {
        if !cc_val.is_empty() {
            push_header(&mut out, "Cc", cc_val);
        }
    }
    push_header(&mut out, "Subject", &encode_subject(subject));
    push_header(&mut out, "Date", date_rfc2822);
    push_header(&mut out, "Message-ID", &format!("<{message_id}>"));
    if let Some(irt) = in_reply_to {
        if !irt.is_empty() {
            push_header(&mut out, "In-Reply-To", &format!("<{irt}>"));
        }
    }
    if !references.is_empty() {
        let refs = references
            .iter()
            .map(|r| format!("<{r}>"))
            .collect::<Vec<_>>()
            .join(" ");
        push_header(&mut out, "References", &refs);
    }
    push_header(&mut out, "MIME-Version", "1.0");
    push_header(&mut out, "Content-Type", "text/plain; charset=utf-8");
    push_header(&mut out, "Content-Transfer-Encoding", "8bit");
    out.push_str("\r\n");
    out.push_str(&normalize_body(body));
    out
}

fn push_header(out: &mut String, key: &str, value: &str) {
    out.push_str(key);
    out.push_str(": ");
    out.push_str(value);
    out.push_str("\r\n");
}

/// RFC 2047 encode the whole subject as one B-word when it contains
/// non-printable-ASCII bytes; otherwise emit verbatim. Good enough for
/// v1 — the alternative is per-word Q-encoding which adds complexity
/// without changing deliverability.
fn encode_subject(subject: &str) -> String {
    if subject.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        return subject.to_string();
    }
    format!("=?utf-8?B?{}?=", B64.encode(subject.as_bytes()))
}

/// Build an RFC 5322 address: `name <local@domain>` if a display name is
/// present, else the bare address.
///
/// Encoding rules:
///   * Empty/whitespace name → bare address.
///   * ASCII name with RFC 5322 "specials" (`(`, `)`, `<`, `>`, `,`, etc.)
///     → quoted-string with `\\`/`"` escaping.
///   * ASCII name without specials → bare.
///   * Non-ASCII name → RFC 2047 base64 encoded-word.
///
/// Strict receivers down-rank messages where the From header isn't a
/// well-formed RFC 5322 address (this is the spec-compliance side of
/// reputation — receivers can't validate alignment if they can't parse
/// the header).
pub fn format_from(name: &str, email: &str) -> String {
    let n = name.trim();
    if n.is_empty() {
        return email.to_string();
    }
    let encoded = if n.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        if n.bytes().any(is_address_special) {
            format!("\"{}\"", n.replace('\\', "\\\\").replace('"', "\\\""))
        } else {
            n.to_string()
        }
    } else {
        format!("=?utf-8?B?{}?=", B64.encode(n.as_bytes()))
    };
    format!("{encoded} <{email}>")
}

/// RFC 5322 §3.2.3 "specials" (subset that matters in a display-name
/// context — the rest are already handled by the ASCII range check).
fn is_address_special(b: u8) -> bool {
    matches!(
        b,
        b'(' | b')'
            | b'<'
            | b'>'
            | b'['
            | b']'
            | b':'
            | b';'
            | b'@'
            | b'\\'
            | b','
            | b'"'
            | b'.'
    )
}

/// Normalise newlines to CRLF. RFC 5322 forbids lines >998 chars but the
/// CLI body comes from a TUI editor — practical bodies don't hit that.
fn normalize_body(body: &str) -> String {
    // Replace lone LFs with CRLF, leaving existing CRLFs alone.
    let mut out = String::with_capacity(body.len());
    let mut prev = '\0';
    for c in body.chars() {
        if c == '\n' && prev != '\r' {
            out.push('\r');
        }
        out.push(c);
        prev = c;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_subject_passthrough() {
        assert_eq!(encode_subject("Hello world"), "Hello world");
    }

    #[test]
    fn non_ascii_subject_encoded() {
        let s = encode_subject("Héllo");
        assert!(s.starts_with("=?utf-8?B?"));
        assert!(s.ends_with("?="));
    }

    #[test]
    fn lf_normalized_to_crlf() {
        assert_eq!(normalize_body("a\nb"), "a\r\nb");
        assert_eq!(normalize_body("a\r\nb"), "a\r\nb");
    }

    #[test]
    fn from_bare_ascii_passthrough() {
        assert_eq!(
            format_from("Hüseyin", "h@example.com"),
            "=?utf-8?B?SMO8c2V5aW4=?= <h@example.com>"
        );
        assert_eq!(format_from("Jane Doe", "j@x.com"), "Jane Doe <j@x.com>");
        assert_eq!(format_from("Smith, John", "j@x.com"), "\"Smith, John\" <j@x.com>");
        assert_eq!(format_from("", "j@x.com"), "j@x.com");
        assert_eq!(format_from("   ", "j@x.com"), "j@x.com");
        assert_eq!(format_from("a\"b", "j@x.com"), "\"a\\\"b\" <j@x.com>");
    }

    #[test]
    fn build_includes_required_headers() {
        let m = build_text_mime(
            "a@example.com",
            "b@example.com",
            None,
            "Hi",
            "Mon, 01 Jan 2024 00:00:00 +0000",
            "abc@example.com",
            None,
            &[],
            "body",
        );
        assert!(m.contains("From: a@example.com\r\n"));
        assert!(m.contains("To: b@example.com\r\n"));
        assert!(m.contains("Subject: Hi\r\n"));
        assert!(m.contains("Message-ID: <abc@example.com>\r\n"));
        assert!(m.contains("MIME-Version: 1.0\r\n"));
        assert!(m.ends_with("body"));
    }
}
