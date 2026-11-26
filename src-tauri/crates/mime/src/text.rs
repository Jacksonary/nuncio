//! Plain-text conversion, escaping and linkification helpers.

use mail_parser::decoders::html::html_to_text as fallback_html_to_text;

/// Convert an HTML fragment to readable plain text.
///
/// Wrapping is effectively disabled (very large width) because message text is
/// displayed in a UI that does its own layout. Falls back to the `mail-parser`
/// tag stripper if `html2text` rejects the input.
pub fn html_to_text(html: &str) -> String {
    // Conditional comments such as `<!--[if !mso]><!-->` survive the text
    // conversion and would otherwise show up verbatim in list previews.
    let cleaned = strip_html_comments(html);
    let converted = html2text::config::plain_no_decorate()
        .string_from_read(cleaned.as_bytes(), 100_000)
        .unwrap_or_else(|_| fallback_html_to_text(&cleaned));
    normalize_newlines(&converted)
}

/// Remove `<!-- ... -->` blocks (including Outlook conditional comments).
fn strip_html_comments(input: &str) -> std::borrow::Cow<'_, str> {
    if !input.contains("<!--") {
        return std::borrow::Cow::Borrowed(input);
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start + 4..].find("-->") {
            Some(end) => rest = &rest[start + 4 + end + 3..],
            // Unterminated comment: drop the remainder.
            None => return std::borrow::Cow::Owned(out),
        }
    }
    out.push_str(rest);
    std::borrow::Cow::Owned(out)
}

/// Normalize CRLF/CR to LF and trim trailing whitespace at the end.
pub fn normalize_newlines(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\0' => {}
            other => out.push(other),
        }
    }
    out.trim_end().to_string()
}

/// Escape a plain-text body and turn URLs / e-mail addresses into links.
///
/// Line breaks are preserved through the `white-space: pre-wrap` rule applied
/// to the generated `.nuncio-plain` wrapper.
pub fn plain_text_to_html(text: &str) -> String {
    let linked = linkify_html(&normalize_newlines(text));
    let with_breaks = linked.replace('\n', "<br>");
    format!("<div class=\"nuncio-plain\">{with_breaks}</div>")
}

/// Escape plain text and linkify URLs, `www.` hosts and e-mail addresses.
pub fn linkify_html(text: &str) -> String {
    let pattern = link_pattern();
    let mut out = String::with_capacity(text.len() + 32);
    let mut last = 0usize;
    for matched in pattern.find_iter(text) {
        if matched.start() < last {
            continue;
        }
        out.push_str(&crate::util::escape_html(&text[last..matched.start()]));
        let raw = matched.as_str();
        let (link, trailing) = split_trailing_punctuation(raw);
        if link.is_empty() {
            out.push_str(&crate::util::escape_html(raw));
        } else {
            out.push_str(&render_link(link));
        }
        out.push_str(&crate::util::escape_html(trailing));
        last = matched.end();
    }
    if last < text.len() {
        out.push_str(&crate::util::escape_html(&text[last..]));
    }
    out
}

fn render_link(link: &str) -> String {
    let lower = link.to_ascii_lowercase();
    let (href, display) = if lower.starts_with("mailto:") {
        (link.to_string(), link[7..].to_string())
    } else if lower.starts_with("http://") || lower.starts_with("https://") {
        (link.to_string(), link.to_string())
    } else if lower.starts_with("www.") {
        (format!("http://{link}"), link.to_string())
    } else if link.contains('@') {
        (format!("mailto:{link}"), link.to_string())
    } else {
        (link.to_string(), link.to_string())
    };
    format!(
        "<a href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\">{}</a>",
        crate::util::escape_attr(&href),
        crate::util::escape_html(&display)
    )
}

fn split_trailing_punctuation(value: &str) -> (&str, &str) {
    let trimmed = value.trim_end_matches([
        '.', ',', ';', ':', '!', '?', ')', ']', '}', '。', '，', '；', '：', '！', '？', '）', '】',
    ]);
    (&value[..trimmed.len()], &value[trimmed.len()..])
}

fn link_pattern() -> &'static regex::Regex {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| {
        regex::Regex::new(
            r#"(?i)(?:https?://[^\s<>"'）】]+|www\.[^\s<>"'）】]+|mailto:[^\s<>"']+|[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,})"#,
        )
        .expect("static link regex must compile")
    })
}

/// Decode a raw body slice fetched straight from IMAP into readable text.
///
/// `BODY[]` returns the part exactly as stored on the server, so a snippet used
/// for list previews still carries its transfer encoding and charset. Without
/// this step a base64 newsletter shows up as base64 in the message list and a
/// GB2312 body arrives as mojibake.
pub fn decode_preview_part(
    bytes: &[u8],
    transfer_encoding: Option<&str>,
    charset: Option<&str>,
) -> String {
    let decoded: Vec<u8> = match transfer_encoding
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("base64") => {
            mail_parser::decoders::base64::base64_decode(bytes).unwrap_or_else(|| bytes.to_vec())
        }
        Some("quoted-printable") => {
            mail_parser::decoders::quoted_printable::quoted_printable_decode(bytes)
                .unwrap_or_else(|| bytes.to_vec())
        }
        _ => bytes.to_vec(),
    };

    let text = match charset.map(str::trim).filter(|value| !value.is_empty()) {
        Some(charset) if !charset.eq_ignore_ascii_case("utf-8") => {
            match encoding_rs::Encoding::for_label(charset.as_bytes()) {
                Some(encoding) if encoding != encoding_rs::UTF_8 => {
                    let (cow, _, _) = encoding.decode(&decoded);
                    cow.into_owned()
                }
                _ => String::from_utf8_lossy(&decoded).into_owned(),
            }
        }
        _ => String::from_utf8_lossy(&decoded).into_owned(),
    };

    strip_text(&text)
}

/// Plain text with HTML entities and tags removed, keeping readable line breaks.
pub fn strip_text(html_or_text: &str) -> String {
    if looks_like_html(html_or_text) {
        html_to_text(html_or_text)
    } else {
        normalize_newlines(html_or_text)
    }
}

/// Crude but cheap HTML detection used by preview generation.
pub fn looks_like_html(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("<html")
        || lower.contains("<body")
        || lower.contains("<div")
        || lower.contains("<p>")
        || lower.contains("<br")
        || lower.contains("<table")
        || lower.contains("<span")
        || lower.contains("</a>")
}

/// Re-exported for callers that build HTML documents.
pub use crate::util::escape_html;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_html_to_text() {
        let text = html_to_text("<p>Hello <b>world</b></p><p>Second line</p>");
        assert!(text.contains("Hello world"));
        assert!(text.contains("Second line"));
    }

    #[test]
    fn decodes_base64_and_quoted_printable_previews() {
        // base64("Hello world") — what a newsletter body looks like over IMAP.
        assert_eq!(
            decode_preview_part(b"SGVsbG8gd29ybGQ=", Some("base64"), Some("utf-8")).trim(),
            "Hello world"
        );
        assert_eq!(
            decode_preview_part(b"Hello=20world", Some("quoted-printable"), None).trim(),
            "Hello world"
        );
        // GB2312 body: "中文" encoded as GB2312 bytes, base64 of those bytes.
        assert_eq!(
            decode_preview_part(b"1tDOxA==", Some("base64"), Some("gb2312")).trim(),
            "中文"
        );
        // Plain 7bit text passes through.
        assert_eq!(
            decode_preview_part(b"plain text", Some("7bit"), Some("utf-8")).trim(),
            "plain text"
        );
    }

    #[test]
    fn drops_html_comments_from_previews() {
        let text = html_to_text("<p>Hello</p><!--[if !mso]><!--><p>World</p>");
        assert!(text.contains("Hello"), "got {text:?}");
        assert!(text.contains("World"), "got {text:?}");
        assert!(!text.contains("<!--"), "got {text:?}");
        assert!(!text.contains("[if !mso]"), "got {text:?}");
    }

    #[test]
    fn strips_html_markup_from_previews() {
        let text = decode_preview_part(b"<p>Hello <b>there</b></p>", None, None);
        assert!(text.contains("Hello"), "got {text:?}");
        assert!(!text.contains("<p>"), "got {text:?}");
    }

    #[test]
    fn escapes_and_keeps_line_breaks() {
        let html = plain_text_to_html("<b>hi</b>\nnext line");
        assert!(html.contains("&lt;b&gt;hi&lt;/b&gt;"));
        assert!(html.contains("<br>"));
        assert!(html.contains("nuncio-plain"));
    }

    #[test]
    fn linkifies_urls_and_emails() {
        let html = linkify_html("See https://example.com/a?b=1, or mail a@b.com.");
        assert!(html.contains("href=\"https://example.com/a?b=1\""));
        assert!(html.contains("href=\"mailto:a@b.com\""));
        assert!(html.contains(">a@b.com</a>"));
        assert!(html.contains(", or mail "));
    }

    #[test]
    fn linkifies_www() {
        let html = linkify_html("go to www.example.org now");
        assert!(html.contains("href=\"http://www.example.org\""));
    }
}
