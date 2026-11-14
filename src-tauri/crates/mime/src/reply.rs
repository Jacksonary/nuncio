//! Reply / reply-all / forward draft construction.

use std::collections::HashSet;

use nuncio_core::models::{Address, ReplyMode};
use nuncio_core::text::normalize_email;

use crate::addresses::{dedupe_addresses, format_address_list};
use crate::parse::{ParsedAttachment, ParsedHeaders, ParsedMessage};
use crate::threading::references_with;
use crate::util::escape_html;

/// Values the composer needs to open a reply / forward draft.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplyParts {
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub subject: String,
    pub html: String,
    /// Message-ID of the source message, without angle brackets.
    pub in_reply_to: Option<String>,
    /// References for the new message, without angle brackets.
    pub references: Vec<String>,
    /// Attachments of the source message (filled for forward modes).
    pub attachments_to_forward: Vec<ParsedAttachment>,
}

const REPLY_PREFIXES: &[&str] = &["re", "aw", "antw", "sv", "vs", "回复", "答复", "回覆"];
const FORWARD_PREFIXES: &[&str] = &["fw", "fwd", "转发", "轉發"];

/// Build the recipient set, subject and quoted HTML for a reply mode.
///
/// `me` contains all of the account's addresses (identities); those are
/// excluded from Cc in reply-all and used to pick a sensible recipient when
/// replying to one's own message.
pub fn build_reply(
    parsed: &ParsedMessage,
    mode: ReplyMode,
    me: &[String],
    lang: &str,
    quote: bool,
) -> ReplyParts {
    let headers = &parsed.headers;
    let me_set: HashSet<String> = me.iter().map(|address| normalize_email(address)).collect();
    let is_me = |address: &Address| me_set.contains(&normalize_email(&address.email));

    let mut to = Vec::new();
    let mut cc = Vec::new();
    let mut attachments_to_forward = Vec::new();
    let mut in_reply_to = None;
    let mut references = Vec::new();

    match mode {
        ReplyMode::Reply | ReplyMode::ReplyAll => {
            if let Some(primary) = primary_recipient(headers, &is_me) {
                to.push(primary);
            }
            if mode == ReplyMode::ReplyAll {
                let mut candidates = headers.to.clone();
                candidates.extend(headers.cc.clone());
                candidates.retain(|address| {
                    !is_me(address)
                        && to.iter().all(|target| {
                            normalize_email(&target.email) != normalize_email(&address.email)
                        })
                });
                cc = dedupe_addresses(candidates);
            }
            in_reply_to = headers.message_id.clone();
            references = references_with(&headers.references, headers.message_id.as_deref());
        }
        ReplyMode::Forward => {
            attachments_to_forward = parsed.attachments.clone();
        }
        ReplyMode::ForwardAsAttachment => {}
        ReplyMode::EditAsNew => {
            to = dedupe_addresses(headers.to.clone());
            cc = dedupe_addresses(headers.cc.clone());
            attachments_to_forward = parsed.attachments.clone();
        }
    }

    let base_subject = headers.subject.trim();
    let subject = match mode {
        ReplyMode::Reply | ReplyMode::ReplyAll => ensure_prefix(base_subject, REPLY_PREFIXES, "Re"),
        ReplyMode::Forward | ReplyMode::ForwardAsAttachment => {
            ensure_prefix(base_subject, FORWARD_PREFIXES, "Fw")
        }
        ReplyMode::EditAsNew => base_subject.to_string(),
    };

    let html = match mode {
        ReplyMode::EditAsNew => {
            let body = parsed.body_html();
            crate::sanitize::sanitize_fragment(&body)
        }
        _ => {
            let mut html = String::from("<p><br></p>");
            if quote {
                if matches!(mode, ReplyMode::Forward | ReplyMode::ForwardAsAttachment) {
                    html.push_str("<hr>");
                }
                html.push_str(&quote_header(headers, lang));
                html.push_str("<blockquote class=\"nuncio-quote\">");
                html.push_str(&crate::sanitize::sanitize_fragment(&parsed.body_html()));
                html.push_str("</blockquote>");
            }
            html
        }
    };

    ReplyParts {
        to,
        cc,
        subject,
        html,
        in_reply_to,
        references,
        attachments_to_forward,
    }
}

/// Ensure `subject` starts with `prefix`, without duplicating an existing
/// localized or English reply/forward prefix.
pub fn ensure_prefix(subject: &str, prefixes: &[&str], prefix: &str) -> String {
    let subject = subject.trim();
    if subject.is_empty() {
        return format!("{prefix}:");
    }
    if has_any_prefix(subject, prefixes) {
        subject.to_string()
    } else {
        format!("{prefix}: {subject}")
    }
}

fn has_any_prefix(subject: &str, prefixes: &[&str]) -> bool {
    let colon = match subject.find([':', '：']) {
        Some(index) => index,
        None => return false,
    };
    let candidate = subject[..colon].trim();
    if candidate.chars().count() > 12 {
        return false;
    }
    let mut base = candidate.to_ascii_lowercase();
    if let Some(open) = base.find(['[', '(']) {
        let close = if base[open..].starts_with('[') {
            ']'
        } else {
            ')'
        };
        if base.ends_with(close)
            && base[open + 1..base.len().saturating_sub(1)]
                .chars()
                .all(|character| character.is_ascii_digit())
        {
            base.truncate(open);
        }
    }
    let base = base.trim();
    prefixes.contains(&base)
}

fn primary_recipient<F>(headers: &ParsedHeaders, is_me: &F) -> Option<Address>
where
    F: Fn(&Address) -> bool,
{
    let candidates = headers
        .reply_to
        .iter()
        .chain(headers.from.iter())
        .chain(headers.to.iter());
    let mut fallback = None;
    for address in candidates {
        if !is_me(address) {
            return Some(address.clone());
        }
        if fallback.is_none() {
            fallback = Some(address.clone());
        }
    }
    fallback
}

fn quote_header(headers: &ParsedHeaders, lang: &str) -> String {
    let (from_label, sent_label, to_label, cc_label, subject_label) =
        if lang.eq_ignore_ascii_case("en-US") {
            ("From", "Sent", "To", "Cc", "Subject")
        } else {
            ("发件人", "发送时间", "收件人", "抄送", "主题")
        };

    let mut rows = Vec::new();
    if let Some(from) = headers.from.as_ref() {
        push_row(
            &mut rows,
            from_label,
            &format_address_list(std::slice::from_ref(from)),
        );
    }
    if let Some(date) = headers.date.as_ref() {
        push_row(
            &mut rows,
            sent_label,
            &date.format("%Y-%m-%d %H:%M").to_string(),
        );
    }
    if !headers.to.is_empty() {
        push_row(&mut rows, to_label, &format_address_list(&headers.to));
    }
    if !headers.cc.is_empty() {
        push_row(&mut rows, cc_label, &format_address_list(&headers.cc));
    }
    if !headers.subject.trim().is_empty() {
        push_row(&mut rows, subject_label, headers.subject.trim());
    }
    format!("<div class=\"nuncio-quote-header\">{}</div>", rows.join(""))
}

fn push_row(rows: &mut Vec<String>, label: &str, value: &str) {
    rows.push(format!(
        "<div><b>{}:</b> {}</div>",
        escape_html(label),
        escape_html(value)
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    fn sample() -> ParsedMessage {
        let raw = b"From: Alice <alice@example.com>\r\nTo: Me <me@example.com>, Bob <bob@example.com>\r\nCc: Carol <carol@example.com>\r\nSubject: Project update\r\nMessage-ID: <id1@example.com>\r\nReferences: <root@example.com>\r\nDate: Tue, 12 Nov 2026 10:00:00 +0800\r\nContent-Type: text/html\r\n\r\n<p>Original <b>body</b></p>";
        parse(raw).expect("message should parse")
    }

    #[test]
    fn reply_goes_to_sender_and_quotes() {
        let parsed = sample();
        let reply = build_reply(
            &parsed,
            ReplyMode::Reply,
            &["me@example.com".to_string()],
            "zh-CN",
            true,
        );
        assert_eq!(reply.to.len(), 1);
        assert_eq!(reply.to[0].email, "alice@example.com");
        assert!(reply.cc.is_empty());
        assert_eq!(reply.subject, "Re: Project update");
        assert_eq!(reply.in_reply_to.as_deref(), Some("id1@example.com"));
        assert_eq!(
            reply.references,
            vec!["root@example.com", "id1@example.com"]
        );
        assert!(reply.html.contains("nuncio-quote-header"));
        assert!(reply.html.contains("发件人"));
        assert!(reply.html.contains("<blockquote"));
        assert!(reply.html.contains("Original <b>body</b>"));
        assert!(reply.attachments_to_forward.is_empty());
    }

    #[test]
    fn reply_all_excludes_me() {
        let parsed = sample();
        let reply = build_reply(
            &parsed,
            ReplyMode::ReplyAll,
            &["me@example.com".to_string()],
            "en-US",
            false,
        );
        assert_eq!(reply.to[0].email, "alice@example.com");
        assert_eq!(reply.cc.len(), 2);
        assert!(reply
            .cc
            .iter()
            .any(|address| address.email == "bob@example.com"));
        assert!(reply
            .cc
            .iter()
            .any(|address| address.email == "carol@example.com"));
        assert!(reply
            .cc
            .iter()
            .all(|address| address.email != "me@example.com"));
        assert!(!reply.html.contains("nuncio-quote-header"));
    }

    #[test]
    fn does_not_duplicate_prefixes() {
        assert_eq!(ensure_prefix("Re: x", REPLY_PREFIXES, "Re"), "Re: x");
        assert_eq!(ensure_prefix("RE[2]: x", REPLY_PREFIXES, "Re"), "RE[2]: x");
        assert_eq!(ensure_prefix("回复：x", REPLY_PREFIXES, "Re"), "回复：x");
        assert_eq!(ensure_prefix("Fwd: x", FORWARD_PREFIXES, "Fw"), "Fwd: x");
        assert_eq!(ensure_prefix("x", REPLY_PREFIXES, "Re"), "Re: x");
        assert_eq!(ensure_prefix("", REPLY_PREFIXES, "Re"), "Re:");
    }

    #[test]
    fn forward_keeps_attachments_and_clears_recipients() {
        let parsed = sample();
        let forward = build_reply(
            &parsed,
            ReplyMode::Forward,
            &["me@example.com".to_string()],
            "en-US",
            true,
        );
        assert!(forward.to.is_empty());
        assert!(forward.cc.is_empty());
        assert_eq!(forward.subject, "Fw: Project update");
        assert_eq!(
            forward.attachments_to_forward.len(),
            parsed.attachments.len()
        );
        assert!(!forward.html.contains("Forwarded"));
        assert!(forward.html.contains("From:"));
    }
}
