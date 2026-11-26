//! Conversion of IMAP `FETCH` responses into core persistence models.
//!
//! All helpers here are pure so they can be unit-tested without a server.

use async_imap::imap_proto::{Address as ImapAddress, BodyStructure, ContentEncoding};
use async_imap::types::{Fetch, Flag};
use chrono::{DateTime, TimeZone, Utc};
use nuncio_core::models::{Address, Importance, MessageSummary, StoredMessage};
use nuncio_core::Result;
use tracing::debug;

use crate::util::{flag_matches, imap_message_id, lossy};

/// Metadata extracted from one `FETCH` response.
#[derive(Debug, Clone)]
pub struct MessageMeta {
    pub stored: StoredMessage,
    /// Suggested section for the first textual part ("TEXT" or "1.2").
    pub text_part: Option<String>,
    /// Content-Transfer-Encoding of `text_part` (`base64`, `quoted-printable`, ...).
    pub text_part_encoding: Option<String>,
    /// Charset parameter of `text_part` (e.g. `gb2312`, `utf-8`).
    pub text_part_charset: Option<String>,
}

/// Message flags decoded from a FETCH response.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FlagSnapshot {
    pub seen: bool,
    pub flagged: bool,
    pub answered: bool,
    pub forwarded: bool,
    pub is_draft: bool,
}

impl FlagSnapshot {
    pub fn from_fetch(fetch: &Fetch) -> Self {
        let flags: Vec<Flag<'_>> = fetch.flags().collect();
        let has = |name: &str| flags.iter().any(|flag| flag_matches(flag, name));
        Self {
            seen: has("\\Seen"),
            flagged: has("\\Flagged"),
            answered: has("\\Answered"),
            forwarded: has("$Forwarded") || has("\\Forwarded"),
            is_draft: has("\\Draft"),
        }
    }
}

/// Build a `StoredMessage` (metadata only) from a FETCH response.
pub fn meta_from_fetch(
    fetch: &Fetch,
    account_id: &str,
    folder_id: &str,
    uidvalidity: Option<i64>,
    fallback_date: DateTime<Utc>,
    gm_thrid: Option<&str>,
) -> Result<Option<MessageMeta>> {
    let Some(uid) = fetch.uid else {
        return Ok(None);
    };
    let id = imap_message_id(account_id, folder_id, uidvalidity, uid as i64);
    let flags = FlagSnapshot::from_fetch(fetch);

    let envelope = fetch.envelope();
    let subject = envelope
        .and_then(|env| env.subject.as_deref())
        .map(|bytes| nuncio_mime::decode_header_phrase(&lossy(bytes)))
        .unwrap_or_default();

    let from = envelope
        .and_then(|env| env.from.as_ref())
        .and_then(|list| list.first())
        .and_then(convert_address);
    let to = envelope
        .and_then(|env| env.to.as_ref())
        .map(|list| convert_addresses(list.as_slice()))
        .unwrap_or_default();
    let cc = envelope
        .and_then(|env| env.cc.as_ref())
        .map(|list| convert_addresses(list.as_slice()))
        .unwrap_or_default();
    let bcc = envelope
        .and_then(|env| env.bcc.as_ref())
        .map(|list| convert_addresses(list.as_slice()))
        .unwrap_or_default();
    let reply_to = envelope
        .and_then(|env| env.reply_to.as_ref())
        .map(|list| convert_addresses(list.as_slice()))
        .unwrap_or_default();

    let date = envelope
        .and_then(|env| env.date.as_deref())
        .and_then(parse_header_date)
        .or_else(|| fetch.internal_date().map(|date| date.with_timezone(&Utc)))
        .unwrap_or(fallback_date);

    let internal_date = fetch.internal_date().map(|date| date.with_timezone(&Utc));

    let header_bytes = fetch.section(&async_imap::imap_proto::SectionPath::Full(
        async_imap::imap_proto::MessageSection::Header,
    ));
    let parsed_headers = header_bytes.and_then(|bytes| {
        nuncio_mime::parse::parse_headers_only(bytes)
            .map_err(|error| debug!(%error, "failed to parse fetched header block"))
            .ok()
    });

    let message_id_header = parsed_headers
        .as_ref()
        .and_then(|headers| headers.message_id.clone())
        .or_else(|| {
            envelope
                .and_then(|env| env.message_id.as_deref())
                .map(|raw| nuncio_mime::normalize_message_id(&lossy(raw)))
                .filter(|value| !value.is_empty())
        });
    let in_reply_to = parsed_headers
        .as_ref()
        .and_then(|headers| headers.in_reply_to.clone())
        .or_else(|| {
            envelope
                .and_then(|env| env.in_reply_to.as_deref())
                .map(|raw| nuncio_mime::normalize_message_id(&lossy(raw)))
                .filter(|value| !value.is_empty())
        });
    let mut references = parsed_headers
        .as_ref()
        .map(|headers| headers.references.clone())
        .unwrap_or_default();
    if references.is_empty() {
        if let Some(parent) = in_reply_to.clone() {
            references.push(parent);
        }
    }

    let importance = parsed_headers
        .as_ref()
        .map(|headers| headers.importance)
        .unwrap_or(Importance::Normal);

    let gm_msgid = fetch.gmail_msg_id().map(|value| value.to_string());
    let labels = fetch
        .gmail_labels()
        .map(|values| values.iter().map(|value| value.to_string()).collect())
        .unwrap_or_default();

    let bodystructure = fetch.bodystructure();
    let has_attachments = bodystructure.map(has_attachments).unwrap_or(false);
    let text_part = bodystructure.and_then(first_text_part);
    let (text_part_encoding, text_part_charset) = bodystructure
        .and_then(first_text_part_header)
        .map(|(encoding, charset)| (Some(encoding), charset))
        .unwrap_or((None, None));

    let thread_id = crate::util::thread_id(
        message_id_header.as_deref(),
        in_reply_to.as_deref(),
        &references,
        &subject,
        gm_thrid,
    );

    let mut summary = MessageSummary::new(id, account_id, folder_id, thread_id, date);
    summary.subject = subject;
    summary.r#from = from;
    summary.to = to;
    summary.cc = cc;
    summary.seen = flags.seen;
    summary.flagged = flags.flagged;
    summary.answered = flags.answered;
    summary.forwarded = flags.forwarded;
    summary.is_draft = flags.is_draft;
    summary.has_attachments = has_attachments;
    summary.importance = importance;
    summary.size = fetch.size.unwrap_or_default() as u64;
    summary.thread_count = 1;
    summary.thread_unread = if flags.seen { 0 } else { 1 };

    let mut stored = StoredMessage::new(summary);
    stored.bcc = bcc;
    stored.reply_to = reply_to;
    stored.message_id_header = message_id_header;
    stored.in_reply_to = in_reply_to;
    stored.references = references;
    stored.uid = Some(uid as i64);
    stored.uidvalidity = uidvalidity;
    stored.gm_msgid = gm_msgid;
    stored.gm_thrid = gm_thrid
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    stored.labels = labels;
    stored.raw_headers = header_bytes.map(lossy);
    stored.internal_date = internal_date;

    Ok(Some(MessageMeta {
        stored,
        text_part,
        text_part_encoding,
        text_part_charset,
    }))
}

fn convert_addresses(list: &[ImapAddress<'_>]) -> Vec<Address> {
    list.iter().filter_map(convert_address).collect()
}

fn convert_address(address: &ImapAddress<'_>) -> Option<Address> {
    let mailbox = address.mailbox.as_deref().map(lossy).unwrap_or_default();
    let host = address.host.as_deref().map(lossy).unwrap_or_default();
    if mailbox.is_empty() {
        // Group start/end markers have an empty mailbox; they are not addresses.
        return None;
    }
    let email = if host.is_empty() {
        mailbox
    } else {
        format!("{mailbox}@{host}")
    };
    let name = address
        .name
        .as_deref()
        .map(|bytes| nuncio_mime::decode_header_phrase(&lossy(bytes)))
        .filter(|value| !value.trim().is_empty());
    Some(Address { name, email })
}

fn parse_header_date(raw: &[u8]) -> Option<DateTime<Utc>> {
    let text = lossy(raw);
    let text = text.trim();
    DateTime::parse_from_rfc2822(text)
        .map(|date| date.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            // Some servers return an IMAP-style date stripped of its timezone.
            chrono::NaiveDateTime::parse_from_str(text, "%d-%b-%Y %H:%M:%S")
                .ok()
                .and_then(|naive| Utc.from_local_datetime(&naive).single())
        })
}

/* ───────────────────────── body structure help ─────────────────────── */

/// Whether a body structure contains at least one attachment-like part.
pub fn has_attachments(structure: &BodyStructure<'_>) -> bool {
    match structure {
        BodyStructure::Multipart { bodies, .. } => bodies.iter().any(has_attachments),
        BodyStructure::Basic { common, .. } => {
            disposition_is_attachment(common) || !type_is(&common.ty, "text")
        }
        BodyStructure::Text { common, .. } => disposition_is_attachment(common),
        BodyStructure::Message { common, .. } => disposition_is_attachment(common) || true,
    }
}

fn disposition_is_attachment(common: &async_imap::imap_proto::BodyContentCommon<'_>) -> bool {
    common
        .disposition
        .as_ref()
        .map(|disposition| disposition.ty.eq_ignore_ascii_case("attachment"))
        .unwrap_or(false)
        || common
            .disposition
            .as_ref()
            .and_then(|disposition| disposition.params.as_ref())
            .map(|params| has_param(params, "filename"))
            .unwrap_or(false)
}

fn has_param(
    params: &[(std::borrow::Cow<'_, str>, std::borrow::Cow<'_, str>)],
    name: &str,
) -> bool {
    params
        .iter()
        .any(|(key, value)| key.eq_ignore_ascii_case(name) && !value.is_empty())
}

fn type_is(ty: &async_imap::imap_proto::ContentType<'_>, name: &str) -> bool {
    ty.ty.eq_ignore_ascii_case(name)
}

/// Section path of the preferred textual part: `TEXT` for a single-part
/// message, or a numeric part path (`1`, `1.2`, ...) inside a multipart body.
pub fn first_text_part(structure: &BodyStructure<'_>) -> Option<String> {
    let mut found: Vec<(Vec<u32>, bool)> = Vec::new();
    collect_text_parts(structure, &mut Vec::new(), &mut found);
    if found.is_empty() {
        return None;
    }
    let pick = found
        .iter()
        .find(|(_, html)| !*html)
        .or_else(|| found.first());
    pick.map(|(path, _)| {
        if path.is_empty() {
            "TEXT".to_string()
        } else {
            path.iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(".")
        }
    })
}

/// Transfer encoding and charset of the part that [`first_text_part`] selects.
pub fn first_text_part_header(structure: &BodyStructure<'_>) -> Option<(String, Option<String>)> {
    let mut found: Vec<(Vec<u32>, bool, String, Option<String>)> = Vec::new();
    collect_text_part_headers(structure, &mut Vec::new(), &mut found);
    if found.is_empty() {
        return None;
    }
    let pick = found
        .iter()
        .find(|(_, html, _, _)| !*html)
        .or_else(|| found.first())?;
    Some((pick.2.clone(), pick.3.clone()))
}

fn collect_text_part_headers(
    structure: &BodyStructure<'_>,
    prefix: &mut Vec<u32>,
    out: &mut Vec<(Vec<u32>, bool, String, Option<String>)>,
) {
    match structure {
        BodyStructure::Text { common, other, .. } => {
            let html = common.ty.subtype.eq_ignore_ascii_case("html");
            let encoding = match other.transfer_encoding {
                ContentEncoding::Base64 => "base64",
                ContentEncoding::QuotedPrintable => "quoted-printable",
                _ => "7bit",
            }
            .to_string();
            let charset = common.ty.params.as_ref().and_then(|params| {
                params
                    .iter()
                    .find(|(key, _)| key.eq_ignore_ascii_case("charset"))
                    .map(|(_, value)| value.to_string())
            });
            out.push((prefix.clone(), html, encoding, charset));
        }
        BodyStructure::Multipart { bodies, .. } => {
            for (index, body) in bodies.iter().enumerate() {
                prefix.push(index as u32 + 1);
                match body {
                    BodyStructure::Text { .. } | BodyStructure::Multipart { .. } => {
                        collect_text_part_headers(body, prefix, out);
                    }
                    _ => {}
                }
                prefix.pop();
            }
        }
        _ => {}
    }
}

fn collect_text_parts(
    structure: &BodyStructure<'_>,
    prefix: &mut Vec<u32>,
    out: &mut Vec<(Vec<u32>, bool)>,
) {
    match structure {
        BodyStructure::Text { common, .. } => {
            let html = common.ty.subtype.eq_ignore_ascii_case("html");
            out.push((prefix.clone(), html));
        }
        BodyStructure::Multipart { bodies, .. } => {
            for (index, body) in bodies.iter().enumerate() {
                prefix.push(index as u32 + 1);
                match body {
                    BodyStructure::Text { .. } | BodyStructure::Multipart { .. } => {
                        collect_text_parts(body, prefix, out);
                    }
                    _ => {}
                }
                prefix.pop();
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rfc2822_envelope_dates() {
        let date = parse_header_date(b"Thu, 12 Nov 2026 08:00:00 +0000").expect("date");
        assert_eq!(date.to_rfc3339(), "2026-11-12T08:00:00+00:00");
    }

    #[test]
    fn skips_group_markers_in_addresses() {
        let group = ImapAddress {
            name: None,
            adl: None,
            mailbox: Some(std::borrow::Cow::Borrowed(b"")),
            host: None,
        };
        assert!(convert_address(&group).is_none());
        let person = ImapAddress {
            name: Some(std::borrow::Cow::Borrowed(b"Ada")),
            adl: None,
            mailbox: Some(std::borrow::Cow::Borrowed(b"ada")),
            host: Some(std::borrow::Cow::Borrowed(b"example.com")),
        };
        assert_eq!(convert_address(&person).unwrap().email, "ada@example.com");
    }
}
