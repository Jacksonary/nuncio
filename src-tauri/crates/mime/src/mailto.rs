//! `mailto:` URL parsing (RFC 6068).

use nuncio_core::models::Address;
use nuncio_core::{AppError, Result};

use crate::addresses::parse_address_list;
use crate::util::percent_decode;

/// Values extracted from a `mailto:` URL.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MailtoData {
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    pub subject: String,
    pub body: String,
}

/// Parse a `mailto:` URL.
///
/// Header fields are matched case-insensitively. `to`, `cc` and `bcc` accept a
/// comma separated address list; `subject` and `body` keep line breaks.
pub fn parse_mailto(url: &str) -> Result<MailtoData> {
    let url = url.trim();
    let colon = url
        .find(':')
        .ok_or_else(|| AppError::invalid_input("not a mailto URL"))?;
    let scheme = &url[..colon];
    if !scheme.eq_ignore_ascii_case("mailto") {
        return Err(AppError::invalid_input(format!(
            "unsupported URI scheme: {scheme}"
        )));
    }
    let rest = &url[colon + 1..];
    let (path, query) = match rest.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (rest, None),
    };

    let mut data = MailtoData::default();
    let path = percent_decode(path);
    data.to.extend(parse_address_list(&path));

    if let Some(query) = query {
        let query = query.replace("&amp;", "&");
        for pair in query.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (name, value) = match pair.split_once('=') {
                Some((name, value)) => (name, value),
                None => (pair, ""),
            };
            let name = percent_decode(name);
            let value = percent_decode(value);
            match name.trim().to_ascii_lowercase().as_str() {
                "to" => data.to.extend(parse_address_list(&value)),
                "cc" => data.cc.extend(parse_address_list(&value)),
                "bcc" => data.bcc.extend(parse_address_list(&value)),
                "subject" => data.subject = value,
                "body" => data.body = value,
                // Unknown or unsupported fields (in-reply-to, ...) are ignored.
                _ => {}
            }
        }
    }

    data.to = crate::addresses::dedupe_addresses(std::mem::take(&mut data.to));
    data.cc = crate::addresses::dedupe_addresses(std::mem::take(&mut data.cc));
    data.bcc = crate::addresses::dedupe_addresses(std::mem::take(&mut data.bcc));
    Ok(data)
}

/// True when the string looks like a `mailto:` URL.
pub fn is_mailto(url: &str) -> bool {
    let trimmed = url.trim_start();
    match trimmed.get(..7) {
        Some(prefix) => prefix.eq_ignore_ascii_case("mailto:"),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_addresses_and_headers() {
        let data = parse_mailto(
            "mailto:alice@example.com,bob@example.com?cc=carol%40example.com&subject=Hello%20World&body=Line%201%0ALine%202",
        )
        .expect("mailto should parse");
        assert_eq!(data.to.len(), 2);
        assert_eq!(data.to[1].email, "bob@example.com");
        assert_eq!(data.cc[0].email, "carol@example.com");
        assert_eq!(data.subject, "Hello World");
        assert_eq!(data.body, "Line 1\nLine 2");
    }

    #[test]
    fn header_names_are_case_insensitive() {
        let data = parse_mailto("MAILTO:?To=a@b.com&Cc=c%40d.com&Bcc=e%40f.com&Subject=x")
            .expect("mailto should parse");
        assert_eq!(data.to[0].email, "a@b.com");
        assert_eq!(data.cc[0].email, "c@d.com");
        assert_eq!(data.bcc[0].email, "e@f.com");
        assert_eq!(data.subject, "x");
    }

    #[test]
    fn treats_plus_literally() {
        let data = parse_mailto("mailto:a@b.com?subject=1+1%3D2").expect("mailto should parse");
        assert_eq!(data.subject, "1+1=2");
    }

    #[test]
    fn rejects_other_schemes() {
        assert!(parse_mailto("https://example.com").is_err());
        assert!(is_mailto("mailto:a@b.com"));
        assert!(!is_mailto("https://example.com"));
    }
}
