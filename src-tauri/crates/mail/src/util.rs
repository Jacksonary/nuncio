//! Shared helpers for the mail engine: deterministic ids, IMAP formatting,
//! flag handling and error mapping.
//!
//! The engine deliberately never generates random ids for server-backed
//! messages: a message's local id is derived from `(account, mailbox identity,
//! uidvalidity, uid)` so that repeated syncs update the same row instead of
//! creating duplicates. The same applies to POP3 (uid + UIDL).

use std::time::Duration;

use async_imap::types::Flag;
use chrono::{DateTime, Utc};
use nuncio_core::AppError;
use uuid::Uuid;

/// Namespace used for deterministic UUIDv5 message/folder ids.
const NUNCIO_NAMESPACE: Uuid = Uuid::from_u128(0x6e75_6e63_696f_4d41_494c_0000_0000_0001);

/// Deterministic UUIDv5 for a namespaced key.
pub fn stable_id(scope: &str, key: &str) -> String {
    Uuid::new_v5(&NUNCIO_NAMESPACE, format!("{scope}:{key}").as_bytes()).to_string()
}

/// Local id of an IMAP message.
pub fn imap_message_id(
    account_id: &str,
    folder_id: &str,
    uidvalidity: Option<i64>,
    uid: i64,
) -> String {
    stable_id(
        "imap-message",
        &format!(
            "{account_id}|{folder_id}|{}|{uid}",
            uidvalidity.unwrap_or(0)
        ),
    )
}

/// Local id of a POP3 message (UIDL based).
pub fn pop3_message_id(account_id: &str, uidl: &str) -> String {
    stable_id("pop3-message", &format!("{account_id}|{uidl}"))
}

/// Local message row projected for a composer draft. The same id is reused for
/// every save so the Drafts folder has exactly one entry per draft.
pub fn draft_message_id(draft_id: &str) -> String {
    stable_id("draft-message", draft_id)
}

/// Message-ID header used for drafts. It lets a server-synced draft (the APPEND
/// copy) be matched back to the local row.
pub fn draft_header_id(draft_id: &str) -> String {
    format!("nuncio-draft-{draft_id}@nuncio.local")
}

/// Recover the local draft id from a draft Message-ID header.
pub fn draft_id_from_header(header: &str) -> Option<&str> {
    header
        .trim()
        .strip_prefix("nuncio-draft-")
        .and_then(|rest| rest.strip_suffix("@nuncio.local"))
        .filter(|value| !value.is_empty())
}

/// `true` for loopback hosts, where plaintext connections only reach a local
/// relay and never leave the machine.
pub fn is_loopback_host(host: &str) -> bool {
    let host = host.trim().trim_matches(['[', ']']).to_ascii_lowercase();
    host == "localhost"
        || host == "::1"
        || host == "0:0:0:0:0:0:0:1"
        || host
            .parse::<std::net::IpAddr>()
            .map(|address| address.is_loopback())
            .unwrap_or(false)
}

/// Conversation key shared by metadata-only and fully parsed messages.
///
/// `nuncio-mime` prefixes its keys (`msg:`/`ref:`), which means a message and
/// its reply would not match (the reply is keyed by the referenced id, the
/// original by its own id). The mail engine uses one unprefixed root key so
/// both sides resolve to the same conversation.
pub fn thread_id(
    message_id: Option<&str>,
    in_reply_to: Option<&str>,
    references: &[String],
    subject: &str,
    gm_thrid: Option<&str>,
) -> String {
    if let Some(gm_thrid) = gm_thrid {
        let gm_thrid = gm_thrid.trim();
        if !gm_thrid.is_empty() {
            return format!("gm:{gm_thrid}");
        }
    }
    let root = nuncio_mime::threading::root_reference(references)
        .or_else(|| {
            in_reply_to
                .map(nuncio_mime::normalize_message_id)
                .filter(|id| !id.is_empty())
        })
        .or_else(|| {
            message_id
                .map(nuncio_mime::normalize_message_id)
                .filter(|id| !id.is_empty())
        });
    if let Some(root) = root {
        return format!("thread:{root}");
    }
    let normalized = nuncio_core::text::normalize_subject(subject);
    if normalized.is_empty() {
        "unthreaded".to_string()
    } else {
        format!("subj:{normalized}")
    }
}

/// Local id for a draft's attachment, stable across body re-parses.
pub fn attachment_id(message_id: &str, index: usize, content_id: Option<&str>) -> String {
    stable_id(
        "attachment",
        &format!("{message_id}|{index}|{}", content_id.unwrap_or("")),
    )
}

/// Compact IMAP UID set (`1,2,3,7:9`). Extremely large sets are chunked by the
/// caller; this only compresses consecutive runs.
pub fn uid_set(uids: &[i64]) -> String {
    let mut sorted: Vec<i64> = uids.iter().copied().filter(|uid| *uid > 0).collect();
    sorted.sort_unstable();
    sorted.dedup();
    let mut parts: Vec<String> = Vec::new();
    let mut index = 0;
    while index < sorted.len() {
        let start = sorted[index];
        let mut end = start;
        while index + 1 < sorted.len() && sorted[index + 1] == end + 1 {
            index += 1;
            end = sorted[index];
        }
        if start == end {
            parts.push(start.to_string());
        } else {
            parts.push(format!("{start}:{end}"));
        }
        index += 1;
    }
    parts.join(",")
}

/// IMAP date used by `SEARCH SINCE` (`12-Nov-2026`).
pub fn imap_date(date: DateTime<Utc>) -> String {
    date.format("%d-%b-%Y").to_string()
}

/// IMAP `INTERNALDATE` value (`12-Nov-2026 08:00:00 +0000`).
pub fn imap_internal_date(date: DateTime<Utc>) -> String {
    date.format("%d-%b-%Y %H:%M:%S +0000").to_string()
}

/// Decode an IMAP `nstring` from envelope/header data.
pub fn lossy(value: &[u8]) -> String {
    String::from_utf8_lossy(value).to_string()
}

/// `true` when the flag list contains the given system flag.
pub fn has_flag<'a, I>(flags: I, name: &str) -> bool
where
    I: IntoIterator<Item = &'a Flag<'a>>,
{
    flags.into_iter().any(|flag| flag_matches(flag, name))
}

/// Compare an IMAP flag to a protocol name.
pub fn flag_matches(flag: &Flag<'_>, name: &str) -> bool {
    match name {
        "\\Seen" => matches!(flag, Flag::Seen),
        "\\Answered" => matches!(flag, Flag::Answered),
        "\\Flagged" => matches!(flag, Flag::Flagged),
        "\\Deleted" => matches!(flag, Flag::Deleted),
        "\\Draft" => matches!(flag, Flag::Draft),
        "\\Recent" => matches!(flag, Flag::Recent),
        _ => matches!(flag, Flag::Custom(value) if value.eq_ignore_ascii_case(name)),
    }
}

/// Map an async-imap error to a core error, preserving the auth/network split.
pub fn imap_error(error: async_imap::error::Error) -> AppError {
    use async_imap::error::Error;
    match error {
        Error::Io(inner) => {
            if inner.kind() == std::io::ErrorKind::TimedOut {
                AppError::network(format!("IMAP timeout: {inner}"))
            } else {
                AppError::network(format!("IMAP I/O error: {inner}"))
            }
        }
        Error::ConnectionLost => AppError::network("IMAP connection lost"),
        Error::No(message) | Error::Bad(message) => {
            let lower = message.to_ascii_lowercase();
            if lower.contains("auth")
                || lower.contains("login")
                || lower.contains("credential")
                || lower.contains("password")
                || lower.contains("token")
            {
                AppError::auth(format!("IMAP authentication failed: {message}"))
            } else {
                AppError::protocol(format!("IMAP command failed: {message}"))
            }
        }
        Error::Parse(inner) => AppError::protocol(format!("IMAP parse error: {inner}")),
        Error::Validate(inner) => {
            AppError::invalid_input(format!("invalid IMAP argument: {inner}"))
        }
        Error::Append => AppError::protocol("IMAP APPEND failed"),
        other => AppError::protocol(format!("IMAP error: {other}")),
    }
}

/// Prefix shared by every error emitted when an IMAP command or socket
/// watchdog expires. Such an error always means the connection was poisoned
/// (the response stream is half-consumed), so the worker must rebuild it.
pub const COMMAND_TIMEOUT_MARKER: &str = "IMAP command timeout";

/// `true` when an error was raised by a command or socket timeout.
///
/// This is deliberately narrower than "the message contains the word
/// timeout": connect-phase timeouts (`IMAP connect ... timed out`) also report
/// a network error but leave no session to invalidate, so treating them as a
/// lost command would make the worker retry a dial immediately instead of
/// honouring its backoff.
pub fn is_command_timeout(error: &AppError) -> bool {
    if error.code() != "network" {
        return false;
    }
    let message = error.message();
    message.starts_with(COMMAND_TIMEOUT_MARKER) || message.starts_with("IMAP timeout:")
}

/// `true` when an error describes a transport-level connection loss rather
/// than a protocol rejection. Only such errors are retried transparently (and
/// only for idempotent commands): a stale socket that a server closed while it
/// was idle shows up as EPIPE, ECONNRESET or an unexpected EOF.
pub fn is_connection_error(error: &AppError) -> bool {
    if error.code() != "network" {
        return false;
    }
    let message = error.message().to_ascii_lowercase();
    const NEEDLES: [&str; 11] = [
        "broken pipe",
        "connection reset",
        "connection aborted",
        "unexpected eof",
        "unexpected end of file",
        "connection closed",
        "connection lost",
        "closed connection",
        "close_notify",
        "not connected",
        "end of file",
    ];
    NEEDLES.iter().any(|needle| message.contains(needle))
}

/// Wrap a future in a timeout, mapping expiry to a network error.
pub async fn with_timeout<T, F>(timeout: Duration, what: &str, future: F) -> nuncio_core::Result<T>
where
    F: std::future::Future<Output = nuncio_core::Result<T>>,
{
    match tokio::time::timeout(timeout, future).await {
        Ok(result) => result,
        Err(_) => Err(AppError::network(format!("{what} timed out"))),
    }
}

/// Install the rustls ring provider once. Safe to call repeatedly.
pub fn ensure_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Convert a folder count i64 into u32 for the notifier.
pub fn clamp_count(value: i64) -> u32 {
    value.clamp(0, u32::MAX as i64) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uid_set_compresses_ranges() {
        assert_eq!(uid_set(&[3, 1, 2, 7, 9]), "1:3,7,9");
        assert_eq!(uid_set(&[]), "");
        assert_eq!(uid_set(&[0, 4]), "4");
    }

    #[test]
    fn ids_are_stable_and_scoped() {
        let a = imap_message_id("acc", "folder", Some(7), 42);
        let b = imap_message_id("acc", "folder", Some(7), 42);
        let c = imap_message_id("acc", "folder", Some(8), 42);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 36);
    }

    #[test]
    fn command_timeouts_are_retryable_but_connect_timeouts_are_not() {
        let command = AppError::network(format!(
            "{COMMAND_TIMEOUT_MARKER}: SELECT timed out after 30.0s; connection marked unusable"
        ));
        assert!(is_command_timeout(&command));
        // A socket watchdog error is surfaced with this prefix.
        assert!(is_command_timeout(&AppError::network(
            "IMAP timeout: IMAP read timeout: no data for 180s; connection abandoned"
        )));
        // Connect-phase budgets do not invalidate a session and must keep
        // their backoff instead of triggering an immediate retry.
        assert!(!is_command_timeout(&AppError::network(
            "IMAP connect to example.com timed out after 40s"
        )));
        assert!(!is_command_timeout(&AppError::protocol("IMAP command failed")));
    }

    #[test]
    fn connection_errors_are_classified() {
        assert!(is_connection_error(&AppError::network(
            "IMAP I/O error: Broken pipe (os error 32)"
        )));
        assert!(is_connection_error(&AppError::network(
            "IMAP I/O error: Connection reset by peer (os error 54)"
        )));
        assert!(is_connection_error(&AppError::network(
            "IMAP I/O error: unexpected end of file"
        )));
        assert!(is_connection_error(&AppError::network(
            "IMAP connection lost"
        )));
        assert!(!is_connection_error(&AppError::network(
            "IMAP connect to example.com timed out"
        )));
        assert!(!is_connection_error(&AppError::protocol(
            "IMAP command failed: NO such mailbox"
        )));
        assert!(!is_connection_error(&AppError::auth("bad password")));
    }

    #[test]
    fn modified_utf7_decodes_chinese_folder_names() {
        // RFC 3501 §5.1.3 examples plus what 163 and Gmail actually return.
        assert_eq!(decode_modified_utf7("&XfJT0ZAB-"), "已发送");
        assert_eq!(decode_modified_utf7("&g0l6P3ux-"), "草稿箱");
        assert_eq!(decode_modified_utf7("&XfJSIJZk-"), "已删除");
        assert_eq!(decode_modified_utf7("&V4NXPpCuTvY-"), "垃圾邮件");
        assert_eq!(decode_modified_utf7("&XfJSIJZkkK5O9g-"), "已删除邮件");
        assert_eq!(decode_modified_utf7("&ZeVnLIqe-"), "日本語");
        assert_eq!(
            decode_modified_utf7("~peter/mail/&U,BTFw-/&ZeVnLIqe-"),
            "~peter/mail/台北/日本語"
        );
    }

    #[test]
    fn modified_utf7_passes_ascii_and_edge_cases_through() {
        assert_eq!(decode_modified_utf7("INBOX"), "INBOX");
        assert_eq!(
            decode_modified_utf7("[Gmail]/Sent Mail"),
            "[Gmail]/Sent Mail"
        );
        // "&-" is a literal ampersand.
        assert_eq!(decode_modified_utf7("A&-B"), "A&B");
        // Malformed or unterminated sequences are preserved verbatim.
        assert_eq!(decode_modified_utf7("A&"), "A&");
        assert_eq!(decode_modified_utf7("a&!!-b"), "a&!!-b");
        assert_eq!(decode_modified_utf7(""), "");
    }

    #[test]
    fn mailbox_names_decode_utf7_and_rfc2047() {
        // UTF-7 alone.
        assert_eq!(decode_mailbox_name("&XfJT0ZAB-"), "已发送");
        // RFC 2047 wrapped names: some servers never emit UTF-7 for these.
        assert_eq!(decode_mailbox_name("=?UTF-8?B?5bey5Y+R6YCB?="), "已发送");
        assert_eq!(
            decode_mailbox_name("=?utf-8?Q?=E8=8D=89=E7=A8=BF?="),
            "草稿"
        );
        // ASCII passes through untouched either way.
        assert_eq!(decode_mailbox_name("INBOX"), "INBOX");
        // A literal "=?" that is not an encoded word is preserved.
        assert_eq!(decode_mailbox_name("a =? b"), "a =? b");
    }

    #[test]
    fn imap_dates_are_protocol_shaped() {
        let date = DateTime::parse_from_rfc3339("2026-11-12T08:09:10Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(imap_date(date), "12-Nov-2026");
        assert_eq!(imap_internal_date(date), "12-Nov-2026 08:09:10 +0000");
    }
}

/// Decode a mailbox name as delivered by `LIST`.
///
/// Mailbox names are modified UTF-7 (RFC 3501 §5.1.3). A few servers also wrap
/// non-ASCII names in RFC 2047 encoded words (`=?utf-8?B?...?=`), which most
/// clients never see because they decode the header form; without this the
/// sidebar shows the raw escape sequence and the folder loses its role.
pub fn decode_mailbox_name(input: &str) -> String {
    let decoded = decode_modified_utf7(input);
    if decoded.contains("=?") {
        nuncio_mime::decode_header_phrase(&decoded)
    } else {
        decoded
    }
}

/// Decode an IMAP mailbox name in modified UTF-7 (RFC 3501 §5.1.3).
///
/// Servers that do not negotiate UTF8=ACCEPT return non-ASCII mailbox names as
/// modified UTF-7: printable ASCII passes through unchanged, `&` stands for
/// itself (`&-`), and `&...-` holds UTF-16BE code units in modified base64
/// (`,` instead of `/`). Anything malformed is left untouched so a broken name
/// still round-trips to the server instead of being mangled.
pub fn decode_modified_utf7(input: &str) -> String {
    fn decode_base64(input: &str) -> Option<Vec<u16>> {
        fn value(byte: u8) -> Option<u32> {
            match byte {
                b'A'..=b'Z' => Some((byte - b'A') as u32),
                b'a'..=b'z' => Some((byte - b'a') as u32 + 26),
                b'0'..=b'9' => Some((byte - b'0') as u32 + 52),
                b'+' => Some(62),
                b',' => Some(63),
                _ => None,
            }
        }
        let bytes = input.as_bytes();
        let mut units = Vec::new();
        let mut buffer: u32 = 0;
        let mut bits = 0u32;
        for byte in bytes {
            let value = value(*byte)?;
            buffer = (buffer << 6) | value;
            bits += 6;
            if bits >= 16 {
                bits -= 16;
                units.push(((buffer >> bits) & 0xFFFF) as u16);
            }
        }
        Some(units)
    }

    if !input.contains('&') {
        return input.to_string();
    }
    let mut output = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(position) = rest.find('&') {
        output.push_str(&rest[..position]);
        rest = &rest[position + 1..];
        // "&-" is a literal ampersand.
        if let Some(tail) = rest.strip_prefix('-') {
            output.push('&');
            rest = tail;
            continue;
        }
        let Some(end) = rest.find('-') else {
            // Unterminated shift sequence: keep the rest verbatim.
            output.push('&');
            output.push_str(rest);
            return output;
        };
        match decode_base64(&rest[..end]) {
            Some(units) => {
                output.extend(char::decode_utf16(units).map(|c| c.unwrap_or('\u{FFFD}')));
                rest = &rest[end + 1..];
            }
            None => {
                // Not valid modified base64: keep the shift sequence verbatim,
                // including its terminating "-".
                output.push('&');
                output.push_str(&rest[..end]);
                output.push('-');
                rest = &rest[end + 1..];
            }
        }
    }
    output.push_str(rest);
    output
}
