//! Small shared helpers that do not belong to a single feature module.

use mail_parser::MessageParser;

/// Percent-decode a string as used by `mailto:` URLs (RFC 6068).
///
/// Unlike form encoding, `+` is kept literally. Invalid escapes are preserved.
pub(crate) fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
            {
                out.push((high << 4) | low);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encode every byte that is not an RFC 3986 unreserved character.
pub(crate) fn percent_encode(input: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(input.len());
    for &byte in input.as_bytes() {
        let unreserved = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~');
        if unreserved {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    out
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Escape text for insertion into HTML element content.
pub fn escape_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for character in input.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            '\0' => {}
            other => out.push(other),
        }
    }
    out
}

/// Escape text for insertion into a quoted HTML attribute value.
pub(crate) fn escape_attr(input: &str) -> String {
    escape_html(input)
}

/// Strip one layer of `<...>` around a Message-ID or Content-ID.
pub(crate) fn strip_angle_brackets(value: &str) -> String {
    let trimmed = value.trim();
    let trimmed = trimmed.strip_prefix('<').unwrap_or(trimmed);
    let trimmed = trimmed.strip_suffix('>').unwrap_or(trimmed);
    trimmed.trim().to_string()
}

/// Decode RFC 2047 encoded words in a free-form phrase (display name, file name).
///
/// `mail-parser` performs the character-set aware decoding, including GBK and
/// ISO-2022-JP. Values without encoded words are returned unchanged.
pub fn decode_header_phrase(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .map(|character| match character {
            '\r' | '\n' | '\0' => ' ',
            other => other,
        })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() || !trimmed.contains("=?") {
        return trimmed.to_string();
    }
    let header = format!("Subject: {trimmed}\r\n\r\n");
    match MessageParser::new().parse_headers(header.as_bytes()) {
        Some(message) => message
            .subject()
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| trimmed.to_string()),
        None => trimmed.to_string(),
    }
}

/// Extract the raw header block (without the terminating empty line).
pub(crate) fn header_block(raw: &[u8]) -> String {
    let end = find_header_separator(raw).unwrap_or(raw.len());
    String::from_utf8_lossy(&raw[..end])
        .trim_end_matches(['\r', '\n'])
        .to_string()
}

fn find_header_separator(raw: &[u8]) -> Option<usize> {
    raw.windows(4)
        .position(|window| window == b"\r\n\r\n")
        .or_else(|| raw.windows(2).position(|window| window == b"\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_round_trip() {
        let original = "中文 <a@b> + test";
        let encoded = percent_encode(original);
        assert!(!encoded.contains(' '));
        assert_eq!(percent_decode(&encoded), original);
    }

    #[test]
    fn percent_decode_keeps_plus() {
        assert_eq!(percent_decode("a+b%40c"), "a+b@c");
        assert_eq!(percent_decode("bad%zz"), "bad%zz");
    }

    #[test]
    fn escapes_html() {
        assert_eq!(
            escape_html("<a href=\"x\">&'"),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#39;"
        );
    }

    #[test]
    fn extracts_header_block() {
        let raw = b"From: a@b\r\nSubject: hi\r\n\r\nbody";
        assert_eq!(header_block(raw), "From: a@b\r\nSubject: hi");
    }

    #[test]
    fn decodes_rfc2047_phrase() {
        // "你好" as UTF-8 base64.
        assert_eq!(decode_header_phrase("=?UTF-8?B?5L2g5aW9?="), "你好");
        assert_eq!(decode_header_phrase("Plain Name"), "Plain Name");
    }
}
