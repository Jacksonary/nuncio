//! MIME message parsing built on `mail-parser`.
//!
//! `mail-parser` is compiled with the `full_encoding` feature, so body parts
//! and RFC 2047 headers are decoded from GBK/GB2312/GB18030, Big5,
//! ISO-2022-JP, UTF-7 and the other charsets listed in the task.

use std::collections::HashMap;
use std::fmt;

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveTime, TimeZone, Utc};
use mail_parser::{
    Addr, ContentType, GetHeader, HeaderName, HeaderValue, Message, MessageParser, MessagePart,
    PartType,
};
use nuncio_core::models::{Address, Importance};
use nuncio_core::{AppError, Result};
use serde::{Deserialize, Serialize};

use crate::addresses::{dedupe_addresses, parse_address_list};
use crate::util::{decode_header_phrase, header_block, strip_angle_brackets};

/// Path of a MIME part inside a message, using 1-based multipart indexes
/// (`1.2` is the second part of the first top-level part).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PartPath(pub Vec<u32>);

impl PartPath {
    pub fn new(segments: Vec<u32>) -> Self {
        Self(segments)
    }

    /// Parse a dotted part path such as `1.2.1`.
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim().trim_matches('.');
        if value.is_empty() {
            return None;
        }
        let mut segments = Vec::new();
        for segment in value.split('.') {
            let segment: u32 = segment.parse().ok()?;
            if segment == 0 {
                return None;
            }
            segments.push(segment);
        }
        Some(Self(segments))
    }

    pub fn as_string(&self) -> String {
        self.0
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(".")
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Display for PartPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.as_string())
    }
}

impl std::str::FromStr for PartPath {
    type Err = AppError;

    fn from_str(value: &str) -> Result<Self> {
        Self::parse(value)
            .ok_or_else(|| AppError::invalid_input(format!("invalid MIME part path: {value}")))
    }
}

/// Header fields decoded into `nuncio-core` types.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedHeaders {
    /// Raw RFC 822 header block, without the final empty line.
    pub raw: String,
    pub subject: String,
    pub from: Option<Address>,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    pub reply_to: Vec<Address>,
    pub date: Option<DateTime<Utc>>,
    /// Message-ID without surrounding brackets.
    pub message_id: Option<String>,
    /// In-Reply-To without surrounding brackets.
    pub in_reply_to: Option<String>,
    /// References without surrounding brackets.
    pub references: Vec<String>,
    pub importance: Importance,
    pub list_unsubscribe: Option<String>,
    pub disposition_notification_to: Vec<Address>,
}

/// Attachment metadata and a stable way to extract its bytes later.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedAttachment {
    pub filename: String,
    pub mime_type: String,
    pub size: u64,
    /// Content-ID without surrounding brackets.
    pub content_id: Option<String>,
    pub is_inline: bool,
    /// `attachment`, `inline`, or `None` when the part has no disposition.
    pub disposition: Option<String>,
    pub part: PartPath,
    /// Flat index in the parser part list; accepted by [`extract_part_by_index`].
    pub index: u32,
}

/// A `text/calendar` body part.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CalendarPart {
    /// iTIP METHOD (REQUEST, CANCEL, REPLY, ...), upper-case or empty.
    pub method: String,
    pub text: String,
    pub part: PartPath,
    pub index: u32,
}

/// Fully parsed message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedMessage {
    pub headers: ParsedHeaders,
    pub html: Option<String>,
    pub text: Option<String>,
    /// First ~200 characters of visible text, whitespace collapsed.
    pub preview: String,
    pub attachments: Vec<ParsedAttachment>,
    pub calendar_parts: Vec<CalendarPart>,
    pub size: u64,
}

impl ParsedMessage {
    pub fn subject(&self) -> &str {
        &self.headers.subject
    }

    pub fn from(&self) -> Option<&Address> {
        self.headers.from.as_ref()
    }

    pub fn message_id(&self) -> Option<&str> {
        self.headers.message_id.as_deref()
    }

    pub fn has_html(&self) -> bool {
        self.html
            .as_deref()
            .is_some_and(|html| !html.trim().is_empty())
    }

    pub fn body_html(&self) -> String {
        if let Some(html) = self.html.as_deref() {
            if !html.trim().is_empty() {
                return html.to_string();
            }
        }
        crate::text::plain_text_to_html(self.text.as_deref().unwrap_or(""))
    }

    pub fn attachment_count(&self) -> usize {
        self.attachments.len()
    }
}

/// Parse a complete RFC 5322 message.
pub fn parse(raw: &[u8]) -> Result<ParsedMessage> {
    if raw.is_empty() {
        return Err(AppError::invalid_input("empty message"));
    }
    let parser = MessageParser::new();
    let message = parser
        .parse(raw)
        .ok_or_else(|| AppError::protocol("failed to parse MIME message"))?;

    let headers = headers_from_message(&message, raw);
    let mut html = collect_html(&message);
    let mut text = collect_text(&message);
    if text.is_none() {
        text = salvage_bodies(&message, false);
    }
    if html.is_none() {
        html = salvage_bodies(&message, true);
    }
    let preview = make_preview(html.as_deref(), text.as_deref());
    let (attachments, calendar_parts) = collect_attachments(&message);

    tracing::debug!(
        subject_len = headers.subject.chars().count(),
        attachments = attachments.len(),
        calendars = calendar_parts.len(),
        "parsed MIME message"
    );

    Ok(ParsedMessage {
        headers,
        html,
        text,
        preview,
        attachments,
        calendar_parts,
        size: raw.len() as u64,
    })
}

/// Decode an IMAP ENVELOPE-like header block into core types.
///
/// `raw_headers` may include the body; only the header block is read.
pub fn parse_headers_only(raw_headers: &[u8]) -> Result<ParsedHeaders> {
    if raw_headers.is_empty() {
        return Err(AppError::invalid_input("empty header block"));
    }
    let parser = MessageParser::new();
    let message = parser
        .parse_headers(raw_headers)
        .ok_or_else(|| AppError::protocol("failed to parse message headers"))?;
    Ok(headers_from_message(&message, raw_headers))
}

/// Extract the decoded bytes of a MIME part addressed by [`PartPath`].
pub fn extract_part(raw: &[u8], part: &PartPath) -> Result<Vec<u8>> {
    let parser = MessageParser::new();
    let message = parser
        .parse(raw)
        .ok_or_else(|| AppError::protocol("failed to parse MIME message"))?;
    let part = part_at_path(&message, &part.0)
        .ok_or_else(|| AppError::not_found(format!("MIME part {part}")))?;
    Ok(part.contents().to_vec())
}

/// Extract the decoded bytes of a MIME part by its flat parser index.
pub fn extract_part_by_index(raw: &[u8], index: u32) -> Result<Vec<u8>> {
    let parser = MessageParser::new();
    let message = parser
        .parse(raw)
        .ok_or_else(|| AppError::protocol("failed to parse MIME message"))?;
    let part = message
        .part(index)
        .ok_or_else(|| AppError::not_found(format!("MIME part index {index}")))?;
    Ok(part.contents().to_vec())
}

fn headers_from_message(message: &Message<'_>, raw: &[u8]) -> ParsedHeaders {
    let subject = message.subject().unwrap_or_default().trim().to_string();
    let from = message
        .from()
        .and_then(|address| address.iter().next())
        .and_then(convert_addr);
    let to = header_addresses(message, HeaderName::To);
    let cc = header_addresses(message, HeaderName::Cc);
    let bcc = header_addresses(message, HeaderName::Bcc);
    let reply_to = header_addresses(message, HeaderName::ReplyTo);
    let date = message.date().and_then(convert_date);
    let message_id = message
        .message_id()
        .map(strip_angle_brackets)
        .filter(|id| !id.is_empty());
    let in_reply_to = message
        .header_values(HeaderName::InReplyTo)
        .flat_map(extract_ids)
        .next();
    let references = message
        .header_values(HeaderName::References)
        .flat_map(extract_ids)
        .collect::<Vec<_>>();
    let importance = importance_from_message(message);
    let list_unsubscribe = list_unsubscribe_from_message(message);
    let disposition_notification_to =
        header_addresses(message, HeaderName::DispositionNotificationTo);

    ParsedHeaders {
        raw: header_block(raw),
        subject,
        from,
        to,
        cc,
        bcc,
        reply_to,
        date,
        message_id,
        in_reply_to,
        references,
        importance,
        list_unsubscribe,
        disposition_notification_to,
    }
}

fn convert_addr(address: &Addr<'_>) -> Option<Address> {
    let email = address.address.as_deref()?.trim();
    if email.is_empty() {
        return None;
    }
    let email = strip_angle_brackets(email);
    let name = address
        .name
        .as_deref()
        .map(decode_header_phrase)
        .map(|name| name.trim_matches('"').trim().to_string())
        .filter(|name| !name.is_empty())
        .filter(|name| {
            nuncio_core::text::normalize_email(name) != nuncio_core::text::normalize_email(&email)
        });
    Some(Address { name, email })
}

fn header_addresses(message: &Message<'_>, name: HeaderName<'_>) -> Vec<Address> {
    let mut addresses = Vec::new();
    for value in message.header_values(name) {
        match value {
            HeaderValue::Address(address) => {
                addresses.extend(address.iter().filter_map(convert_addr));
            }
            HeaderValue::Text(text) => addresses.extend(parse_address_list(text)),
            HeaderValue::TextList(list) => {
                for text in list {
                    addresses.extend(parse_address_list(text));
                }
            }
            _ => {}
        }
    }
    dedupe_addresses(addresses)
}

fn extract_ids(value: &HeaderValue<'_>) -> Vec<String> {
    let mut ids = Vec::new();
    match value {
        HeaderValue::Text(text) => ids.extend(split_ids(text)),
        HeaderValue::TextList(list) => {
            for text in list {
                ids.extend(split_ids(text));
            }
        }
        _ => {}
    }
    ids
}

fn split_ids(value: &str) -> Vec<String> {
    value
        .split_whitespace()
        .map(strip_angle_brackets)
        .filter(|id| !id.is_empty())
        .collect()
}

fn convert_date(date: &mail_parser::DateTime) -> Option<DateTime<Utc>> {
    let naive_date = NaiveDate::from_ymd_opt(date.year as i32, date.month as u32, date.day as u32)?;
    let naive_time =
        NaiveTime::from_hms_opt(date.hour as u32, date.minute as u32, date.second as u32)?;
    let minutes = date.tz_hour as i32 * 60 + date.tz_minute as i32;
    let minutes = if date.tz_before_gmt {
        -minutes
    } else {
        minutes
    };
    let offset = FixedOffset::east_opt(minutes * 60)?;
    let local = offset
        .from_local_datetime(&naive_date.and_time(naive_time))
        .single()?;
    Some(local.with_timezone(&Utc))
}

fn collect_html(message: &Message<'_>) -> Option<String> {
    let mut parts = Vec::new();
    for part in message.html_bodies() {
        if let PartType::Html(html) = &part.body {
            parts.push(html.to_string());
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("<hr>"))
    }
}

fn collect_text(message: &Message<'_>) -> Option<String> {
    let mut parts = Vec::new();
    for part in message.text_bodies() {
        match &part.body {
            PartType::Text(text) => parts.push(text.to_string()),
            PartType::Html(html) => parts.push(crate::text::html_to_text(html)),
            _ => {}
        }
    }
    let text = parts
        .into_iter()
        .map(|part| part.trim_end().to_string())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Recover body parts from malformed messages whose boundaries prevented
/// `mail-parser` from classifying them as text/html bodies.
fn salvage_bodies(message: &Message<'_>, want_html: bool) -> Option<String> {
    let mut parts = Vec::new();
    for part in &message.parts {
        let (mime_type, _) = part_mime(part);
        if is_calendar_mime(&mime_type) {
            continue;
        }
        match (&part.body, want_html) {
            (PartType::Html(html), true) => parts.push(html.to_string()),
            (PartType::Text(text), false) => parts.push(text.to_string()),
            _ => {}
        }
    }
    let joined = parts
        .into_iter()
        .map(|part| part.trim_end().to_string())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(if want_html { "<hr>" } else { "\n\n" });
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}

fn make_preview(html: Option<&str>, text: Option<&str>) -> String {
    let visible = match text {
        Some(text) if !text.trim().is_empty() => text.to_string(),
        _ => html.map(crate::text::html_to_text).unwrap_or_default(),
    };
    nuncio_core::text::trim_preview(&visible, 200)
}

fn collect_attachments(message: &Message<'_>) -> (Vec<ParsedAttachment>, Vec<CalendarPart>) {
    let paths = part_paths(message);
    let mut attachments = Vec::new();
    let mut calendars = Vec::new();
    let mut calendar_indexes = std::collections::HashSet::new();

    // Calendar parts may be classified as text bodies or as attachments.
    for (index, part) in message.parts.iter().enumerate() {
        let index = index as u32;
        let (mime_type, content_type) = part_mime(part);
        if !is_calendar_mime(&mime_type) {
            continue;
        }
        let text = part_text(part);
        calendars.push(CalendarPart {
            method: calendar_method(content_type.as_ref(), &text),
            text,
            part: paths.get(&index).cloned().unwrap_or_default(),
            index,
        });
        calendar_indexes.insert(index);
    }

    for &index in &message.attachments {
        let part = match message.part(index) {
            Some(part) => part,
            None => continue,
        };
        let (mime_type, content_type) = part_mime(part);
        if is_calendar_mime(&mime_type) && calendar_indexes.contains(&index) {
            continue;
        }
        let disposition = part_disposition(part);
        let content_id = part_content_id(part);
        let filename =
            attachment_filename(part, content_type.as_ref(), &mime_type, &content_id, index);
        let is_inline = disposition.as_deref() == Some("inline")
            || (content_id.is_some()
                && disposition.as_deref() != Some("attachment")
                && (mime_type.starts_with("image/")
                    || mime_type.starts_with("audio/")
                    || mime_type.starts_with("video/")));
        let size = part.len() as u64;
        attachments.push(ParsedAttachment {
            filename,
            mime_type,
            size,
            content_id,
            is_inline,
            disposition,
            part: paths.get(&index).cloned().unwrap_or_default(),
            index,
        });
    }

    (attachments, calendars)
}

fn part_mime<'a>(part: &'a MessagePart<'a>) -> (String, Option<ContentType<'a>>) {
    let content_type = part
        .headers
        .header_value(&HeaderName::ContentType)
        .and_then(|value| value.as_content_type())
        .cloned();
    let mime_type = match &content_type {
        Some(content_type) => match content_type.subtype() {
            Some(subtype) => format!("{}/{}", content_type.ctype(), subtype),
            None => content_type.ctype().to_string(),
        },
        None => "text/plain".to_string(),
    };
    (mime_type.to_ascii_lowercase(), content_type)
}

fn part_disposition(part: &MessagePart<'_>) -> Option<String> {
    part.headers
        .header_value(&HeaderName::ContentDisposition)
        .and_then(|value| value.as_content_type())
        .map(|content_type| content_type.ctype().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
}

fn part_content_id(part: &MessagePart<'_>) -> Option<String> {
    part.headers
        .header_value(&HeaderName::ContentId)
        .and_then(|value| value.as_text())
        .map(strip_angle_brackets)
        .filter(|value| !value.is_empty())
}

fn attachment_filename(
    part: &MessagePart<'_>,
    content_type: Option<&ContentType<'_>>,
    mime_type: &str,
    content_id: &Option<String>,
    index: u32,
) -> String {
    let disposition = part
        .headers
        .header_value(&HeaderName::ContentDisposition)
        .and_then(|value| value.as_content_type());
    let raw_name = disposition
        .and_then(|value| value.attribute("filename"))
        .or_else(|| content_type.and_then(|value| value.attribute("name")))
        .or_else(|| {
            part.headers
                .header_value(&HeaderName::ContentDescription)
                .and_then(|value| value.as_text())
        })
        .or(content_id.as_deref());
    let name = raw_name
        .map(decode_header_phrase)
        .map(|name| {
            name.rsplit(['/', '\\'])
                .next()
                .unwrap_or(&name)
                .trim()
                .to_string()
        })
        .filter(|name| !name.is_empty());
    name.unwrap_or_else(|| crate::attachments::fallback_filename(index as usize + 1, mime_type))
}

fn part_text(part: &MessagePart<'_>) -> String {
    match &part.body {
        PartType::Text(text) | PartType::Html(text) => text.to_string(),
        PartType::Binary(bytes) | PartType::InlineBinary(bytes) => {
            String::from_utf8_lossy(bytes).into_owned()
        }
        PartType::Message(message) => String::from_utf8_lossy(message.raw_message()).into_owned(),
        PartType::Multipart(_) => String::new(),
    }
}

fn is_calendar_mime(mime_type: &str) -> bool {
    mime_type.eq_ignore_ascii_case("text/calendar")
        || mime_type.eq_ignore_ascii_case("application/ics")
}

fn calendar_method(content_type: Option<&ContentType<'_>>, text: &str) -> String {
    if let Some(method) = content_type.and_then(|value| value.attribute("method")) {
        let method = method.trim();
        if !method.is_empty() {
            return method.to_ascii_uppercase();
        }
    }
    for line in text.lines().take(40) {
        let line = line.trim();
        if let Some(method) = line
            .strip_prefix("METHOD:")
            .or_else(|| line.strip_prefix("method:"))
        {
            let method = method.trim();
            if !method.is_empty() {
                return method.to_ascii_uppercase();
            }
        }
    }
    String::new()
}

fn importance_from_message(message: &Message<'_>) -> Importance {
    for name in ["Importance", "X-Priority", "Priority", "X-MSMail-Priority"] {
        if let Some(value) = header_text(message, name) {
            if let Some(importance) = parse_importance(&value) {
                return importance;
            }
        }
    }
    Importance::Normal
}

fn header_text(message: &Message<'_>, name: &str) -> Option<String> {
    let headers = message.parts.first()?.headers.as_slice();
    for header in headers.iter().rev() {
        let matches = header.name.as_static_str().eq_ignore_ascii_case(name)
            || matches!(&header.name, HeaderName::Other(other) if other.eq_ignore_ascii_case(name));
        if !matches {
            continue;
        }
        if let Some(text) = header.value.as_text() {
            return Some(text.trim().to_string());
        }
        if let HeaderValue::TextList(list) = &header.value {
            let joined = list
                .iter()
                .map(|value| value.trim())
                .collect::<Vec<_>>()
                .join(" ");
            if !joined.is_empty() {
                return Some(joined);
            }
        }
    }
    None
}

fn parse_importance(value: &str) -> Option<Importance> {
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty() {
        return None;
    }
    let first_digit = value.chars().find(|character| character.is_ascii_digit());
    if value.starts_with("high") || value.contains("urgent") {
        return Some(Importance::High);
    }
    if value.starts_with("low") || value.contains("non-urgent") || value.contains("nonurgent") {
        return Some(Importance::Low);
    }
    if value.starts_with("normal") {
        return Some(Importance::Normal);
    }
    match first_digit {
        Some('1') | Some('2') => Some(Importance::High),
        Some('3') => Some(Importance::Normal),
        Some('4') | Some('5') => Some(Importance::Low),
        _ => None,
    }
}

fn list_unsubscribe_from_message(message: &Message<'_>) -> Option<String> {
    let mut mailto = None;
    for value in message.header_values(HeaderName::ListUnsubscribe) {
        match value {
            HeaderValue::Address(address) => {
                for address in address.iter() {
                    if let Some(url) = address.address.as_deref() {
                        if let Some(found) = classify_unsubscribe(url) {
                            match found {
                                Unsubscribe::Http(url) => return Some(url),
                                Unsubscribe::Mailto(url) => {
                                    if mailto.is_none() {
                                        mailto = Some(url);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            HeaderValue::Text(text) => {
                for token in text.split(['<', '>', ',', ' ', '\t', '\r', '\n']) {
                    if let Some(found) = classify_unsubscribe(token) {
                        match found {
                            Unsubscribe::Http(url) => return Some(url),
                            Unsubscribe::Mailto(url) => {
                                if mailto.is_none() {
                                    mailto = Some(url);
                                }
                            }
                        }
                    }
                }
            }
            HeaderValue::TextList(list) => {
                for text in list {
                    for token in text.split(['<', '>', ',', ' ', '\t', '\r', '\n']) {
                        if let Some(found) = classify_unsubscribe(token) {
                            match found {
                                Unsubscribe::Http(url) => return Some(url),
                                Unsubscribe::Mailto(url) => {
                                    if mailto.is_none() {
                                        mailto = Some(url);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    mailto
}

enum Unsubscribe {
    Http(String),
    Mailto(String),
}

fn has_prefix(value: &str, prefix: &str) -> bool {
    value
        .get(..prefix.len())
        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix))
}

fn classify_unsubscribe(value: &str) -> Option<Unsubscribe> {
    let value = value.trim();
    if has_prefix(value, "https://") || has_prefix(value, "http://") {
        Some(Unsubscribe::Http(value.to_string()))
    } else if has_prefix(value, "mailto:") {
        Some(Unsubscribe::Mailto(value.to_string()))
    } else {
        None
    }
}

fn part_paths(message: &Message<'_>) -> HashMap<u32, PartPath> {
    let mut paths = HashMap::new();
    let root = match message.parts.first() {
        Some(root) => root,
        None => return paths,
    };
    match root.sub_parts() {
        Some(children) => {
            for (index, child) in children.iter().enumerate() {
                walk_paths(message, *child, vec![index as u32 + 1], &mut paths);
            }
        }
        None => {
            paths.insert(0, PartPath(vec![1]));
        }
    }
    paths
}

fn walk_paths(
    message: &Message<'_>,
    index: u32,
    path: Vec<u32>,
    paths: &mut HashMap<u32, PartPath>,
) {
    paths.insert(index, PartPath(path.clone()));
    let part = match message.part(index) {
        Some(part) => part,
        None => return,
    };
    if let Some(children) = part.sub_parts() {
        for (child_index, child) in children.iter().enumerate() {
            let mut child_path = path.clone();
            child_path.push(child_index as u32 + 1);
            walk_paths(message, *child, child_path, paths);
        }
    }
}

fn part_at_path<'a>(message: &'a Message<'a>, path: &[u32]) -> Option<&'a MessagePart<'a>> {
    let root = message.parts.first()?;
    if path.is_empty() {
        return Some(root);
    }
    if root.sub_parts().is_none() {
        // Single-part message: path `1` addresses the root.
        return if path == [1] { Some(root) } else { None };
    }
    let mut current = root;
    for &segment in path {
        if segment == 0 {
            return None;
        }
        let children = current.sub_parts()?;
        let child = *children.get((segment - 1) as usize)?;
        current = message.part(child)?;
    }
    Some(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn part_path_round_trip() {
        let path = PartPath::parse("1.2.3").expect("path");
        assert_eq!(path.as_string(), "1.2.3");
        assert!(PartPath::parse("0.1").is_none());
        assert!(PartPath::parse("").is_none());
    }

    #[test]
    fn parses_a_minimal_message() {
        let raw = b"From: Alice <alice@example.com>\r\nTo: bob@example.com\r\nSubject: Hello\r\nDate: Tue, 12 Nov 2026 10:00:00 +0800\r\nMessage-ID: <id@example.com>\r\n\r\nHi Bob!\r\n";
        let parsed = parse(raw).expect("message should parse");
        assert_eq!(parsed.subject(), "Hello");
        assert_eq!(
            parsed.from().map(|address| address.email.as_str()),
            Some("alice@example.com")
        );
        assert_eq!(parsed.headers.message_id.as_deref(), Some("id@example.com"));
        assert_eq!(parsed.text.as_deref(), Some("Hi Bob!"));
        assert_eq!(parsed.preview, "Hi Bob!");
    }

    #[test]
    fn maps_importance_headers() {
        let raw = b"Subject: x\r\nX-Priority: 1 (Highest)\r\n\r\nbody";
        let parsed = parse(raw).expect("message should parse");
        assert_eq!(parsed.headers.importance, Importance::High);
    }

    #[test]
    fn prefers_https_unsubscribe() {
        let raw = b"Subject: x\r\nList-Unsubscribe: <mailto:unsub@example.com>, <https://example.com/unsub>\r\n\r\nbody";
        let parsed = parse(raw).expect("message should parse");
        assert_eq!(
            parsed.headers.list_unsubscribe.as_deref(),
            Some("https://example.com/unsub")
        );
    }
}
