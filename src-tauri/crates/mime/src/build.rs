//! RFC 5322 / MIME message construction with `mail-builder`.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use mail_builder::headers::address::Address as BuilderAddress;
use mail_builder::headers::content_type::ContentType as BuilderContentType;
use mail_builder::headers::date::Date as BuilderDate;
use mail_builder::headers::message_id::MessageId as BuilderMessageId;
use mail_builder::headers::text::Text as BuilderText;
use mail_builder::headers::Header;
use mail_builder::mime::MimePart;
use mail_builder::MessageBuilder;
use nuncio_core::models::{Address, Importance};
use nuncio_core::text::normalize_email;
use nuncio_core::{AppError, Result};
use uuid::Uuid;

use crate::addresses::format_address;
use crate::parse::PartPath;
use crate::threading::normalize_message_id;

/// A message to build and send.
#[derive(Debug, Clone, PartialEq)]
pub struct OutgoingMessage {
    pub from: Address,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    pub reply_to: Vec<Address>,
    pub subject: String,
    /// Editor HTML; inline images are referenced as `cid:<content-id>`.
    pub html: String,
    /// `(content-id, mime-type, file path)` for inline images.
    pub inline_images: Vec<(String, String, PathBuf)>,
    /// `(filename, mime-type, file path)` for regular attachments.
    pub attachments: Vec<(String, String, PathBuf)>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub importance: Importance,
    pub request_read_receipt: bool,
    pub message_id: Option<String>,
    pub date: Option<DateTime<Utc>>,
}

impl OutgoingMessage {
    pub fn new(from: Address, subject: impl Into<String>, html: impl Into<String>) -> Self {
        Self {
            from,
            to: Vec::new(),
            cc: Vec::new(),
            bcc: Vec::new(),
            reply_to: Vec::new(),
            subject: subject.into(),
            html: html.into(),
            inline_images: Vec::new(),
            attachments: Vec::new(),
            in_reply_to: None,
            references: Vec::new(),
            importance: Importance::Normal,
            request_read_receipt: false,
            message_id: None,
            date: None,
        }
    }
}

/// Input for [`build_raw_with_attachment_eml`].
#[derive(Debug, Clone, PartialEq)]
pub struct ForwardAsAttachment {
    pub from: Address,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub subject: String,
    /// Optional introductory HTML body.
    pub html: Option<String>,
    /// Raw RFC 822 bytes of the original message.
    pub source: Vec<u8>,
    pub source_filename: Option<String>,
    pub message_id: Option<String>,
    pub date: Option<DateTime<Utc>>,
}

impl ForwardAsAttachment {
    pub fn new(
        from: Address,
        to: Vec<Address>,
        subject: impl Into<String>,
        source: Vec<u8>,
    ) -> Self {
        Self {
            from,
            to,
            cc: Vec::new(),
            subject: subject.into(),
            html: None,
            source,
            source_filename: None,
            message_id: None,
            date: None,
        }
    }
}

/// Build an RFC 822 message and return `(bytes, Message-ID)`.
///
/// The body is `multipart/alternative` (plain text + HTML); when inline images
/// are present the HTML part becomes `multipart/related`, and when regular
/// attachments are present the whole body is wrapped in `multipart/mixed`.
///
/// These bytes are the ones handed to SMTP, so `Bcc` recipients are
/// deliberately **not** written to a `Bcc` header: a `Bcc` header would be
/// transmitted verbatim and disclose every blind recipient to the `To`/`Cc`
/// recipients. The caller must add the blind recipients to the SMTP envelope
/// (they are available from the outbox item recipients) and use
/// [`build_message_with_bcc`] only for stored copies (Drafts/Sent) that stay
/// on the sender's side.
pub fn build_message(input: OutgoingMessage) -> Result<(Vec<u8>, String)> {
    let message_id = input
        .message_id
        .as_deref()
        .map(normalize_message_id)
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| generate_message_id(&input.from.email));

    let mut builder = MessageBuilder::new()
        .message_id(message_id.clone())
        .from(to_builder_address(&input.from))
        .subject(input.subject.clone());

    if let Some(to) = to_builder_list(&input.to) {
        builder = builder.to(to);
    }
    if let Some(cc) = to_builder_list(&input.cc) {
        builder = builder.cc(cc);
    }
    // No `Bcc` header here on purpose; see the function documentation. Blind
    // recipients travel in the SMTP envelope, and stored copies get them via
    // `add_bcc_header` / `build_message_with_bcc`.
    if let Some(reply_to) = to_builder_list(&input.reply_to) {
        builder = builder.reply_to(reply_to);
    }

    if let Some(in_reply_to) = input
        .in_reply_to
        .as_deref()
        .map(normalize_message_id)
        .filter(|id| !id.is_empty())
    {
        builder = builder.in_reply_to(in_reply_to);
    }
    if !input.references.is_empty() {
        let references = crate::threading::normalize_references(&input.references);
        if !references.is_empty() {
            builder = builder.references(BuilderMessageId::new_list(references.into_iter()));
        }
    }

    let (importance, priority) = match input.importance {
        Importance::High => ("High", "1"),
        Importance::Normal => ("Normal", "3"),
        Importance::Low => ("Low", "5"),
    };
    builder = builder
        .header("Importance", BuilderText::new(importance))
        .header("X-Priority", BuilderText::new(priority))
        .header(
            "User-Agent",
            BuilderText::new(format!("Nuncio/{}", env!("CARGO_PKG_VERSION"))),
        );

    if input.request_read_receipt {
        builder = builder.header(
            "Disposition-Notification-To",
            BuilderText::new(input.from.email.clone()),
        );
    }
    if let Some(date) = input.date {
        builder = builder.date(BuilderDate::new(date.timestamp()));
    }

    let body = build_body(&input)?;
    builder = builder.body(body);

    let bytes = builder
        .write_to_vec()
        .map_err(|error| AppError::io(format!("failed to build MIME message: {error}")))?;

    tracing::debug!(
        message_id = %message_id,
        attachments = input.attachments.len(),
        inline_images = input.inline_images.len(),
        bcc_recipients = input.bcc.len(),
        size = bytes.len(),
        "built outgoing MIME message"
    );
    Ok((bytes, message_id))
}

/// Build a message for local storage (Drafts/Sent) that includes a `Bcc`
/// header so the sender can see the blind recipients when reopening it.
///
/// **Never transmit the returned bytes over SMTP** — the `Bcc` header would be
/// delivered to every recipient. Use [`build_message`] for the wire.
pub fn build_message_with_bcc(mut input: OutgoingMessage) -> Result<(Vec<u8>, String)> {
    let bcc = std::mem::take(&mut input.bcc);
    let (raw, message_id) = build_message(input)?;
    let raw = add_bcc_header(&raw, &bcc)?;
    Ok((raw, message_id))
}

/// Insert (or replace) a `Bcc` header in raw RFC 822 bytes.
///
/// Only meaningful for stored copies (Drafts/Sent); see
/// [`build_message_with_bcc`]. The header is encoded and folded with the same
/// `mail-builder` routines used for the other address headers. Existing `Bcc`
/// headers are removed first so the function is idempotent, and addresses
/// containing control characters are rejected to prevent header injection.
pub fn add_bcc_header(raw: &[u8], bcc: &[Address]) -> Result<Vec<u8>> {
    if bcc.is_empty() {
        return Ok(raw.to_vec());
    }
    for address in bcc {
        if has_control_characters(&address.email)
            || address.name.as_deref().is_some_and(has_control_characters)
        {
            return Err(AppError::invalid_input(
                "refusing to write an address with control characters to the Bcc header",
            ));
        }
    }

    let raw = strip_bcc_header(raw);
    let insert_at = header_end(&raw).ok_or_else(|| {
        AppError::invalid_input("cannot add a Bcc header to a message without a header block")
    })?;
    let header = format_bcc_header(bcc)?;
    let mut out = Vec::with_capacity(raw.len() + header.len());
    out.extend_from_slice(&raw[..insert_at]);
    out.extend_from_slice(&header);
    out.extend_from_slice(&raw[insert_at..]);
    Ok(out)
}

fn has_control_characters(value: &str) -> bool {
    value
        .chars()
        .any(|character| matches!(character, '\r' | '\n' | '\0'))
}

fn format_bcc_header(addresses: &[Address]) -> Result<Vec<u8>> {
    let list = to_builder_list(addresses)
        .ok_or_else(|| AppError::invalid_input("a Bcc header needs at least one address"))?;
    let mut header = Vec::new();
    header.extend_from_slice(b"Bcc: ");
    // `Header::write_header` writes the folded value followed by CRLF; the
    // column argument is the length of the "Bcc: " prefix for line folding.
    list.write_header(&mut header, 5);
    if !header.ends_with(b"\r\n") {
        header.extend_from_slice(b"\r\n");
    }
    Ok(header)
}

/// Remove every `Bcc` header (including folded continuation lines).
fn strip_bcc_header(raw: &[u8]) -> Vec<u8> {
    let Some(end) = header_end(raw) else {
        return raw.to_vec();
    };
    let mut out = Vec::with_capacity(raw.len());
    let mut skipping = false;
    for line in raw[..end].split_inclusive(|byte| *byte == b'\n') {
        let is_continuation = matches!(line.first(), Some(b' ' | b'\t'));
        if is_continuation {
            if skipping {
                continue;
            }
        } else {
            skipping = header_name_is(line, b"bcc");
        }
        if skipping {
            continue;
        }
        out.extend_from_slice(line);
    }
    out.extend_from_slice(&raw[end..]);
    out
}

fn header_name_is(line: &[u8], name: &[u8]) -> bool {
    line.get(..name.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(name))
        && line.get(name.len()) == Some(&b':')
}

/// Byte offset of the start of the blank line that terminates the header
/// block (the bytes before it include the final header's line break).
fn header_end(raw: &[u8]) -> Option<usize> {
    raw.windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 2)
        .or_else(|| {
            raw.windows(2)
                .position(|window| window == b"\n\n")
                .map(|index| index + 1)
        })
}

/// Build a "forward as attachment" message with the original as `message/rfc822`.
pub fn build_raw_with_attachment_eml(input: ForwardAsAttachment) -> Result<(Vec<u8>, String)> {
    let message_id = input
        .message_id
        .as_deref()
        .map(normalize_message_id)
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| generate_message_id(&input.from.email));

    let filename = input
        .source_filename
        .as_deref()
        .map(|name| {
            name.rsplit(['/', '\\'])
                .next()
                .unwrap_or(name)
                .trim()
                .to_string()
        })
        .filter(|name| !name.is_empty())
        .map(|name| {
            if name.contains('.') {
                name
            } else {
                format!("{name}.eml")
            }
        })
        .unwrap_or_else(|| "forwarded-message.eml".to_string());

    let text = input
        .html
        .as_deref()
        .map(crate::text::html_to_text)
        .filter(|text| !text.trim().is_empty())
        .unwrap_or_else(|| "Forwarded message is attached.".to_string());

    let mut builder = MessageBuilder::new()
        .message_id(message_id.clone())
        .from(to_builder_address(&input.from))
        .subject(input.subject.clone())
        .text_body(text);
    if let Some(to) = to_builder_list(&input.to) {
        builder = builder.to(to);
    }
    if let Some(cc) = to_builder_list(&input.cc) {
        builder = builder.cc(cc);
    }
    builder = builder
        .header(
            "User-Agent",
            BuilderText::new(format!("Nuncio/{}", env!("CARGO_PKG_VERSION"))),
        )
        .attachment("message/rfc822", filename, input.source);
    if let Some(date) = input.date {
        builder = builder.date(BuilderDate::new(date.timestamp()));
    }

    let bytes = builder
        .write_to_vec()
        .map_err(|error| AppError::io(format!("failed to build forwarded message: {error}")))?;
    tracing::debug!(message_id = %message_id, size = bytes.len(), "built forward-as-attachment message");
    Ok((bytes, message_id))
}

/// Read a MIME part of an outgoing message back by path (useful for tests and
/// for attaching an existing message).
pub fn extract_attachment_bytes(raw: &[u8], part: &PartPath) -> Result<Vec<u8>> {
    crate::parse::extract_part(raw, part)
}

fn build_body(input: &OutgoingMessage) -> Result<MimePart<'static>> {
    let text_part = MimePart::new("text/plain", crate::text::html_to_text(&input.html));
    let html_leaf = MimePart::new("text/html", input.html.clone());

    let html_part = if input.inline_images.is_empty() {
        html_leaf
    } else {
        let mut parts = Vec::with_capacity(input.inline_images.len() + 1);
        parts.push(html_leaf);
        for (content_id, mime_type, path) in &input.inline_images {
            let bytes = read_file(path, "inline image")?;
            parts.push(
                MimePart::new(mime_type.clone(), bytes)
                    .inline()
                    .cid(content_id.clone()),
            );
        }
        let content_type =
            BuilderContentType::new("multipart/related").attribute("type", "text/html");
        MimePart::new(content_type, parts)
    };

    let body = MimePart::new("multipart/alternative", vec![text_part, html_part]);

    if input.attachments.is_empty() {
        Ok(body)
    } else {
        let mut parts = Vec::with_capacity(input.attachments.len() + 1);
        parts.push(body);
        for (filename, mime_type, path) in &input.attachments {
            let bytes = read_file(path, "attachment")?;
            parts.push(MimePart::new(mime_type.clone(), bytes).attachment(filename.clone()));
        }
        Ok(MimePart::new("multipart/mixed", parts))
    }
}

fn read_file(path: &PathBuf, kind: &str) -> Result<Vec<u8>> {
    std::fs::read(path)
        .map_err(|error| AppError::io(format!("failed to read {kind} {}: {error}", path.display())))
}

fn to_builder_address(address: &Address) -> BuilderAddress<'static> {
    let name = address
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned);
    BuilderAddress::new_address(name, address.email.clone())
}

fn to_builder_list(addresses: &[Address]) -> Option<BuilderAddress<'static>> {
    if addresses.is_empty() {
        return None;
    }
    Some(BuilderAddress::new_list(
        addresses.iter().map(to_builder_address).collect(),
    ))
}

fn generate_message_id(sender: &str) -> String {
    let domain = sender
        .rsplit_once('@')
        .map(|(_, domain)| domain.trim())
        .filter(|domain| {
            !domain.is_empty()
                && domain.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '.' | '-')
                })
        })
        .unwrap_or("nuncio.local");
    format!("{}@{}", Uuid::new_v4(), domain)
}

/// Convenience helper used by the composer for a `Reply-To` header value.
pub fn reply_to_value(address: &Address) -> String {
    let email = normalize_email(&address.email);
    if email.is_empty() {
        format_address(address)
    } else {
        address.email.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nuncio-mime-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn builds_and_parses_round_trip() {
        let dir = temp_dir();
        let inline_path = dir.join("image.png");
        std::fs::write(
            &inline_path,
            [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0],
        )
        .expect("write image");
        let attachment_path = dir.join("report.txt");
        std::fs::write(&attachment_path, b"attachment body").expect("write attachment");

        let mut message = OutgoingMessage::new(
            Address::with_name("Alice", "alice@example.com"),
            "主题 with unicode",
            "<p>Hello <b>Bob</b></p><img src=\"cid:img1@nuncio\">",
        );
        message.to = vec![Address::with_name("Bob", "bob@example.com")];
        message.cc = vec![Address::new("carol@example.com")];
        message.inline_images = vec![(
            "img1@nuncio".to_string(),
            "image/png".to_string(),
            inline_path,
        )];
        message.attachments = vec![(
            "中文 报告.txt".to_string(),
            "text/plain".to_string(),
            attachment_path,
        )];
        message.importance = Importance::High;
        message.request_read_receipt = true;
        message.in_reply_to = Some("<parent@example.com>".to_string());
        message.references = vec!["<root@example.com>".to_string()];

        let (raw, message_id) = build_message(message).expect("build should succeed");
        let parsed = parse(&raw).expect("built message should parse");
        assert_eq!(parsed.subject(), "主题 with unicode");
        assert_eq!(
            parsed.from().map(|address| address.email.as_str()),
            Some("alice@example.com")
        );
        assert_eq!(
            parsed.headers.message_id.as_deref(),
            Some(message_id.as_str())
        );
        assert_eq!(
            parsed.headers.in_reply_to.as_deref(),
            Some("parent@example.com")
        );
        assert_eq!(parsed.headers.importance, Importance::High);
        assert!(!parsed.headers.disposition_notification_to.is_empty());
        assert!(parsed.html.as_deref().unwrap_or("").contains("<b>Bob</b>"));
        assert!(parsed.text.as_deref().unwrap_or("").contains("Hello Bob"));
        assert_eq!(parsed.attachments.len(), 2, "inline image + attachment");
        let attachment = parsed
            .attachments
            .iter()
            .find(|attachment| !attachment.is_inline)
            .expect("attachment");
        assert_eq!(attachment.filename, "中文 报告.txt");
        let bytes = crate::parse::extract_part(&raw, &attachment.part).expect("extract attachment");
        assert_eq!(bytes, b"attachment body");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn builds_forward_as_attachment() {
        let source = b"Subject: Original\r\nMessage-ID: <orig@example.com>\r\n\r\nOriginal body";
        let mut forward = ForwardAsAttachment::new(
            Address::new("a@example.com"),
            vec![Address::new("b@example.com")],
            "Fw: Original",
            source.to_vec(),
        );
        forward.source_filename = Some("Original".to_string());
        let (raw, _) = build_raw_with_attachment_eml(forward).expect("build forward");
        let parsed = parse(&raw).expect("forward should parse");
        assert!(parsed
            .text
            .as_deref()
            .unwrap_or("")
            .contains("Forwarded message"));
        assert_eq!(parsed.attachments.len(), 1);
        assert_eq!(parsed.attachments[0].mime_type, "message/rfc822");
        assert_eq!(parsed.attachments[0].filename, "Original.eml");
    }

    #[test]
    fn generates_message_id_from_sender_domain() {
        let id = generate_message_id("user@example.com");
        assert!(id.ends_with("@example.com"));
        assert!(normalize_message_id(&id).contains('@'));
    }

    fn message_with_bcc() -> OutgoingMessage {
        let mut message = OutgoingMessage::new(
            Address::with_name("Alice", "alice@example.com"),
            "Quarterly numbers",
            "<p>Hello Bob</p>",
        );
        message.to = vec![Address::with_name("Bob", "bob@example.com")];
        message.cc = vec![Address::new("carol@example.com")];
        message.bcc = vec![
            Address::with_name("戴夫 Dave", "dave@example.com"),
            Address::new("erin@example.com"),
        ];
        message
    }

    fn count_header_lines(raw: &[u8], name: &str) -> usize {
        let end = header_end(raw).unwrap_or(raw.len());
        raw[..end]
            .split_inclusive(|byte| *byte == b'\n')
            .filter(|line| {
                let line = line.strip_suffix(b"\n").unwrap_or(line);
                let line = line.strip_suffix(b"\r").unwrap_or(line);
                header_name_is(line, name.as_bytes()) && !matches!(line.first(), Some(b' ' | b'\t'))
            })
            .count()
    }

    #[test]
    fn bcc_is_never_written_to_transmitted_bytes() {
        let (raw, _) = build_message(message_with_bcc()).expect("build message");
        let text = String::from_utf8_lossy(&raw);
        assert!(
            !text.to_ascii_lowercase().contains("bcc:"),
            "Bcc header must not be part of the bytes sent over SMTP"
        );
        for email in ["dave@example.com", "erin@example.com"] {
            assert!(
                !text.contains(email),
                "blind recipient {email} leaked into the transmitted message"
            );
        }
        let parsed = parse(&raw).expect("parse");
        assert!(parsed.headers.bcc.is_empty());
        assert_eq!(parsed.headers.to.len(), 1);
        assert_eq!(parsed.headers.cc.len(), 1);
    }

    #[test]
    fn stored_copy_keeps_bcc_header() {
        let (raw, _) = build_message_with_bcc(message_with_bcc()).expect("build stored copy");
        assert_eq!(count_header_lines(&raw, "bcc"), 1);
        let parsed = parse(&raw).expect("parse");
        assert_eq!(parsed.headers.bcc.len(), 2);
        assert_eq!(parsed.headers.bcc[0].email, "dave@example.com");
        assert_eq!(parsed.headers.bcc[0].name.as_deref(), Some("戴夫 Dave"));
        assert_eq!(parsed.headers.bcc[1].email, "erin@example.com");
        assert_eq!(parsed.headers.to.len(), 1);
        assert_eq!(parsed.headers.cc.len(), 1);
        assert!(parsed.text.as_deref().unwrap_or("").contains("Hello Bob"));
    }

    #[test]
    fn adding_bcc_header_replaces_existing_header() {
        let (raw, _) = build_message_with_bcc(message_with_bcc()).expect("build stored copy");
        let replacement = vec![Address::with_name("Frank", "frank@example.com")];
        let updated = add_bcc_header(&raw, &replacement).expect("replace Bcc");
        assert_eq!(count_header_lines(&updated, "bcc"), 1);
        let parsed = parse(&updated).expect("parse");
        assert_eq!(parsed.headers.bcc.len(), 1);
        assert_eq!(parsed.headers.bcc[0].email, "frank@example.com");
        assert_eq!(parsed.headers.bcc[0].name.as_deref(), Some("Frank"));
    }

    #[test]
    fn adding_bcc_header_preserves_lf_only_messages() {
        let raw = b"From: a@example.com\nSubject: hi\n\nbody\n";
        let updated =
            add_bcc_header(raw, &[Address::new("b@example.com")]).expect("add Bcc to LF message");
        let parsed = parse(&updated).expect("parse");
        assert_eq!(parsed.headers.bcc.len(), 1);
        assert_eq!(parsed.headers.bcc[0].email, "b@example.com");
    }

    #[test]
    fn adding_bcc_header_rejects_control_characters() {
        let (raw, _) = build_message(message_with_bcc()).expect("build message");
        let injected = vec![Address::new("evil@example.com\r\nX-Injected: yes")];
        assert!(add_bcc_header(&raw, &injected).is_err());
        let injected_name = vec![Address::with_name(
            "Evil\r\nX-Injected: yes",
            "evil@example.com",
        )];
        assert!(add_bcc_header(&raw, &injected_name).is_err());
    }

    #[test]
    fn bcc_header_folds_long_lists() {
        let mut message = OutgoingMessage::new(
            Address::new("alice@example.com"),
            "many blind recipients",
            "<p>hi</p>",
        );
        message.to = vec![Address::new("bob@example.com")];
        message.bcc = (0..40)
            .map(|index| {
                Address::with_name(
                    format!("Recipient Number {index}"),
                    format!("recipient{index}@example.com"),
                )
            })
            .collect();
        let (raw, _) = build_message_with_bcc(message).expect("build");
        assert_eq!(count_header_lines(&raw, "bcc"), 1);
        let parsed = parse(&raw).expect("parse");
        assert_eq!(parsed.headers.bcc.len(), 40);
        assert_eq!(parsed.headers.bcc[39].email, "recipient39@example.com");
    }
}
