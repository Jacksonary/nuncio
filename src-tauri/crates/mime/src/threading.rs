//! Conversation threading key derivation.

use nuncio_core::text::normalize_subject;

/// Normalize a Message-ID / Content-ID: trim whitespace and `<` `>` brackets.
pub fn normalize_message_id(raw: &str) -> String {
    crate::util::strip_angle_brackets(raw)
}

/// Split a References header value into normalized message ids.
///
/// Values may be a single id or a whitespace separated list, with or without
/// angle brackets. Duplicates are removed while keeping the original order.
pub fn normalize_references(references: &[String]) -> Vec<String> {
    let mut result = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for value in references {
        for token in value.split_whitespace() {
            let id = normalize_message_id(token);
            if !id.is_empty() && seen.insert(id.clone()) {
                result.push(id);
            }
        }
    }
    result
}

/// The root (oldest) reference id, when available.
pub fn root_reference(references: &[String]) -> Option<String> {
    normalize_references(references).into_iter().next()
}

/// Compute a stable conversation key.
///
/// Priority:
/// 1. Gmail `X-GM-THRID` (`gm:<id>`)
/// 2. root of `References` (`ref:<message-id>`)
/// 3. `In-Reply-To` (`ref:<message-id>`)
/// 4. own `Message-ID` (`msg:<message-id>`)
/// 5. normalized subject (`subj:<subject>`) or `unthreaded`
pub fn thread_key(
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
    if let Some(root) = root_reference(references) {
        return format!("ref:{root}");
    }
    if let Some(in_reply_to) = in_reply_to {
        let id = normalize_message_id(in_reply_to);
        if !id.is_empty() {
            return format!("ref:{id}");
        }
    }
    if let Some(message_id) = message_id {
        let id = normalize_message_id(message_id);
        if !id.is_empty() {
            return format!("msg:{id}");
        }
    }
    let normalized = normalize_subject(subject);
    if normalized.is_empty() {
        "unthreaded".to_string()
    } else {
        format!("subj:{normalized}")
    }
}

/// Append `message_id` to a reference list when it is missing.
pub fn references_with(references: &[String], message_id: Option<&str>) -> Vec<String> {
    let mut list = normalize_references(references);
    if let Some(message_id) = message_id {
        let id = normalize_message_id(message_id);
        if !id.is_empty() && !list.iter().any(|existing| existing == &id) {
            list.push(id);
        }
    }
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_brackets() {
        assert_eq!(normalize_message_id(" <abc@def> "), "abc@def");
    }

    #[test]
    fn prefers_gmail_thread_then_root_reference() {
        assert_eq!(thread_key(None, None, &[], "hi", Some("123")), "gm:123");
        assert_eq!(
            thread_key(
                Some("own@x"),
                Some("parent@x"),
                &["<root@x> <mid@x>".to_string()],
                "hi",
                None
            ),
            "ref:root@x"
        );
    }

    #[test]
    fn falls_back_to_in_reply_to_and_subject() {
        assert_eq!(
            thread_key(Some("own@x"), Some("<parent@x>"), &[], "hi", None),
            "ref:parent@x"
        );
        assert_eq!(thread_key(None, None, &[], "Re: Hello", None), "subj:Hello");
        assert_eq!(thread_key(None, None, &[], "", None), "unthreaded");
    }

    #[test]
    fn appends_own_message_id() {
        let references = references_with(&["<a@x>".to_string()], Some("b@x"));
        assert_eq!(references, vec!["a@x".to_string(), "b@x".to_string()]);
    }
}
