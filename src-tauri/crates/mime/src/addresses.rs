//! Recipient parsing and formatting for the composer and mailto handling.

use nuncio_core::models::Address;

use crate::util::decode_header_phrase;

/// Parse a recipient input string into addresses.
///
/// Accepts the common user-facing forms:
///
/// * `Name <a@b>` and `<a@b>`
/// * bare `a@b`
/// * `a@b (Name)`
/// * lists separated by `,`, `;`, the full-width `，` / `；` or `、`
///
/// Quoted display names may contain separators. Tokens without an `@` are
/// ignored so partially typed input never produces bogus recipients.
pub fn parse_address_list(input: &str) -> Vec<Address> {
    let mut addresses = Vec::new();
    let tokens = split_list(input);
    let mut index = 0usize;
    while index < tokens.len() {
        let token = tokens[index].trim();
        let next = tokens.get(index + 1).map(|value| value.trim());
        // Unquoted display names containing a comma ("Doe, John <j@x>") are
        // split by the scanner; rejoin them when the next token is an address.
        if !token.is_empty()
            && !token.contains('@')
            && !token.contains('<')
            && next.is_some_and(|value| value.contains('@') && value.contains('<'))
        {
            let merged = format!("{token}, {}", next.unwrap_or_default());
            if let Some(address) = parse_one(&merged) {
                addresses.push(address);
                index += 2;
                continue;
            }
        }
        if let Some(address) = parse_one(token) {
            addresses.push(address);
        }
        index += 1;
    }
    dedupe_addresses(addresses)
}

/// Parse exactly one address, returning `None` when the token is not one.
pub fn parse_one_address(input: &str) -> Option<Address> {
    parse_one(input.trim())
}

/// Parse an RFC 5322 address list (comma separated), keeping group members.
pub fn parse_rfc5322_list(input: &str) -> Vec<Address> {
    parse_address_list(input)
}

/// Format an address the way it is shown in recipient fields.
pub fn format_address(address: &Address) -> String {
    let email = address.email.trim();
    let name = address.name.as_deref().unwrap_or("").trim();
    if name.is_empty() {
        return email.to_string();
    }
    if needs_quotes(name) {
        format!(
            "\"{}\" <{email}>",
            name.replace('\\', "\\\\").replace('"', "\\\"")
        )
    } else {
        format!("{name} <{email}>")
    }
}

/// Format an address list for a recipient field (`a@b, "Doe, John" <c@d>`).
pub fn format_address_list(addresses: &[Address]) -> String {
    addresses
        .iter()
        .map(format_address)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Remove duplicates by normalized e-mail address, preserving input order.
pub fn dedupe_addresses(addresses: Vec<Address>) -> Vec<Address> {
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::with_capacity(addresses.len());
    for address in addresses {
        let key = nuncio_core::text::normalize_email(&address.email);
        if key.is_empty() || !seen.insert(key) {
            continue;
        }
        result.push(address);
    }
    result
}

fn split_list(input: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut start = 0usize;
    let mut in_quote = false;
    let mut escaped = false;
    let mut angle_depth = 0usize;
    for (index, character) in input.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' if in_quote => escaped = true,
            '"' => in_quote = !in_quote,
            '<' if !in_quote => angle_depth += 1,
            '>' if !in_quote && angle_depth > 0 => angle_depth -= 1,
            ',' | ';' | '，' | '；' | '、' if !in_quote && angle_depth == 0 => {
                tokens.push(&input[start..index]);
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    tokens.push(&input[start..]);
    tokens
}

fn parse_one(token: &str) -> Option<Address> {
    let token = token.trim();
    if token.is_empty()
        || token
            .chars()
            .any(|character| character == '\r' || character == '\n')
    {
        return None;
    }

    let (name_part, email_part) = if let Some(open) = token.rfind('<') {
        let close = token.rfind('>')?;
        if close <= open {
            return None;
        }
        (Some(&token[..open]), &token[open + 1..close])
    } else if token.ends_with(')') {
        if let Some(open) = token.rfind('(') {
            (Some(&token[open + 1..token.len() - 1]), &token[..open])
        } else {
            (None, token)
        }
    } else {
        (None, token)
    };

    let email = strip_mailto(email_part.trim())
        .trim_matches(['<', '>', '"', '\'', ' ', '\t'])
        .to_string();
    if email.is_empty() || !email.contains('@') || email.chars().any(char::is_whitespace) {
        return None;
    }
    if email.starts_with("mailto:") || email.starts_with("MAILTO:") {
        return None;
    }

    let name = name_part.and_then(|part| {
        let part = part.trim().trim_matches('"').trim();
        if part.is_empty() {
            None
        } else {
            let unescaped = part.replace("\\\"", "\"").replace("\\\\", "\\");
            let decoded = decode_header_phrase(&unescaped);
            if decoded.trim().is_empty() {
                None
            } else {
                Some(decoded.trim().to_string())
            }
        }
    });

    Some(Address { name, email })
}

fn strip_mailto(value: &str) -> &str {
    match value.get(..7) {
        Some(prefix) if prefix.eq_ignore_ascii_case("mailto:") => &value[7..],
        _ => value,
    }
}

fn needs_quotes(name: &str) -> bool {
    name.chars().any(|character| {
        matches!(
            character,
            ',' | ';' | '<' | '>' | '(' | ')' | ':' | '@' | '"'
        )
    }) || name.chars().next().is_some_and(char::is_whitespace)
        || name.chars().last().is_some_and(char::is_whitespace)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_forms() {
        let addresses = parse_address_list(
            "Alice <alice@example.com>; bob@example.com, \"Doe, John\" <john@example.com>",
        );
        assert_eq!(addresses.len(), 3);
        assert_eq!(addresses[0].name.as_deref(), Some("Alice"));
        assert_eq!(addresses[0].email, "alice@example.com");
        assert_eq!(addresses[1].name, None);
        assert_eq!(addresses[2].name.as_deref(), Some("Doe, John"));
    }

    #[test]
    fn parses_chinese_separators() {
        let addresses = parse_address_list(
            "张三 <zhang@example.cn>，李四 <li@example.cn>；王五 <wang@example.cn>",
        );
        assert_eq!(addresses.len(), 3);
        assert_eq!(addresses[0].name.as_deref(), Some("张三"));
        assert_eq!(addresses[2].email, "wang@example.cn");
    }

    #[test]
    fn parses_comment_form_and_ignores_partial_input() {
        let addresses = parse_address_list("a@b.com (Alice), not-an-address, <c@d.com>");
        assert_eq!(addresses.len(), 2);
        assert_eq!(addresses[0].name.as_deref(), Some("Alice"));
        assert_eq!(addresses[1].email, "c@d.com");
    }

    #[test]
    fn dedupes_case_insensitively() {
        let addresses = parse_address_list("A <a@b.com>, a@B.com");
        assert_eq!(addresses.len(), 1);
    }

    #[test]
    fn formats_names_that_need_quotes() {
        let address = Address::with_name("Doe, John", "john@example.com");
        assert_eq!(format_address(&address), "\"Doe, John\" <john@example.com>");
        let plain = Address::with_name("Alice", "alice@example.com");
        assert_eq!(format_address(&plain), "Alice <alice@example.com>");
    }
}
