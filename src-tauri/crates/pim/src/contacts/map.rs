//! Shared contact mapping helpers.
//!
//! Provider payloads agree on the concept of `Contact` but not on field names,
//! phone/address types or photo encodings.  Everything reusable lives here so
//! the Google, Graph and CardDAV adapters stay small.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use nuncio_core::models::{ContactField, ContactFieldType};

/// Largest photo kept inline (matches the IPC contract: small `<256px` data URLs).
pub const MAX_PHOTO_BYTES: usize = 200 * 1024;

/// Map a provider type label onto the contract's fixed set.
pub fn field_type(value: &str) -> ContactFieldType {
    match value.trim().to_ascii_lowercase().as_str() {
        "home" => ContactFieldType::Home,
        "work" | "business" | "company" => ContactFieldType::Work,
        "mobile" | "cell" | "cellphone" => ContactFieldType::Mobile,
        "fax" | "faxhome" | "faxwork" | "otherfax" => ContactFieldType::Fax,
        "pager" => ContactFieldType::Pager,
        "main" | "primary" => ContactFieldType::Main,
        _ => ContactFieldType::Other,
    }
}

/// Build a contact field, marking the first value primary when the provider
/// does not tell us otherwise.
pub fn contact_field(value: impl Into<String>, kind: &str, primary: bool) -> ContactField {
    ContactField {
        r#type: field_type(kind),
        value: value.into(),
        is_primary: primary,
    }
}

/// JSON array with a single primary field for providers that return a plain
/// string (Graph phones, CardDAV TEL handled by nuncio-ical).
pub fn fields_from_values(values: &[String], kind: &str) -> Vec<ContactField> {
    values
        .iter()
        .enumerate()
        .filter(|(_, value)| !value.trim().is_empty())
        .map(|(index, value)| contact_field(value.trim(), kind, index == 0))
        .collect()
}

/// Encode image bytes as a `data:` URL, skipping anything above
/// [`MAX_PHOTO_BYTES`].
pub fn photo_data_url(mime: &str, bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() || bytes.len() > MAX_PHOTO_BYTES {
        return None;
    }
    let mime = normalize_image_mime(mime, bytes);
    let encoded = STANDARD.encode(bytes);
    Some(format!("data:{mime};base64,{encoded}"))
}

/// Decode a `data:image/...;base64,...` URL into `(mime, bytes)` for provider
/// uploads.  Returns `None` for anything that is not a small base64 image.
pub fn decode_photo(data_url: &str) -> Option<(String, Vec<u8>)> {
    let data_url = data_url.trim();
    let rest = data_url.strip_prefix("data:")?;
    let (meta, encoded) = rest.split_once(',')?;
    let mut parts = meta.split(';');
    let mime = parts.next()?.trim().to_ascii_lowercase();
    if !mime.starts_with("image/") || !parts.any(|part| part.eq_ignore_ascii_case("base64")) {
        return None;
    }
    let bytes = STANDARD.decode(encoded.trim()).ok()?;
    if bytes.is_empty() || bytes.len() > MAX_PHOTO_BYTES {
        return None;
    }
    Some((normalize_image_mime(&mime, &bytes), bytes))
}

/// Keep an existing data URL only when it is small enough.
pub fn sanitize_photo(value: Option<String>) -> Option<String> {
    let value = value?;
    let trimmed = value.trim();
    if !trimmed.starts_with("data:image/") {
        return None;
    }
    if trimmed.len() > MAX_PHOTO_BYTES * 4 / 3 + 64 {
        return None;
    }
    Some(trimmed.to_string())
}

fn normalize_image_mime(mime: &str, bytes: &[u8]) -> String {
    let mime = mime.trim().to_ascii_lowercase();
    if mime.starts_with("image/") {
        return mime;
    }
    // Sniff the common magic numbers.
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png".into()
    } else if bytes.starts_with(&[0xFF, 0xD8]) {
        "image/jpeg".into()
    } else if bytes.starts_with(b"GIF8") {
        "image/gif".into()
    } else if bytes.len() > 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "image/webp".into()
    } else if bytes.starts_with(b"BM") {
        "image/bmp".into()
    } else {
        "image/jpeg".into()
    }
}

/// `YYYY-MM-DD` for a known year, `--MM-DD` when the year is unknown.
pub fn birthday(year: Option<i32>, month: u32, day: u32) -> Option<String> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    match year.filter(|year| *year > 1) {
        Some(year) => Some(format!("{year:04}-{month:02}-{day:02}")),
        // `0001-01-01` is the "unknown" sentinel used by Graph; other
        // providers simply omit the year and get the `--MM-DD` form.
        None if year.is_some() => None,
        None => Some(format!("--{month:02}-{day:02}")),
    }
}

/// Parse the provider date forms (`1990-04-15`, `1990-04-15T00:00:00Z`,
/// `--04-15`) into the contract representation.
pub fn parse_birthday(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Some(rest) = value.strip_prefix("--") {
        let (month, day) = rest.split_once('-')?;
        return birthday(None, month.parse().ok()?, day.parse().ok()?);
    }
    let date = value.split('T').next().unwrap_or(value);
    let mut parts = date.split('-');
    let year: i32 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    if year <= 1 {
        // `0001-01-01` and year zero mean "unknown" in Graph / vCard.
        return None;
    }
    birthday(Some(year), month, day)
}

/// A display name fallback used when the provider sends no `FN`.
pub fn display_name_from_parts(given: &str, family: &str) -> String {
    let joined = [given.trim(), family.trim()]
        .iter()
        .filter(|part| !part.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" ");
    joined
}

/// Preferred email or empty string.
pub fn primary_email(contact: &nuncio_core::models::Contact) -> String {
    contact.primary_email().unwrap_or_default()
}

/// Ensure exactly one primary field when the provider gave none.
pub fn ensure_primary(mut fields: Vec<ContactField>) -> Vec<ContactField> {
    if fields.is_empty() || fields.iter().any(|field| field.is_primary) {
        return fields;
    }
    if let Some(first) = fields.first_mut() {
        first.is_primary = true;
    }
    fields
}

/// Collapse multiple primaries down to the first one (Graph merges phone lists).
pub fn single_primary(mut fields: Vec<ContactField>) -> Vec<ContactField> {
    let mut seen = false;
    for field in fields.iter_mut() {
        if field.is_primary {
            if seen {
                field.is_primary = false;
            } else {
                seen = true;
            }
        }
    }
    ensure_primary(fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_field_types() {
        assert_eq!(field_type("WORK"), ContactFieldType::Work);
        assert_eq!(field_type("cell"), ContactFieldType::Mobile);
        assert_eq!(field_type("unknown"), ContactFieldType::Other);
    }

    #[test]
    fn builds_birthdays() {
        assert_eq!(birthday(Some(1815), 12, 10).unwrap(), "1815-12-10");
        assert_eq!(birthday(None, 4, 15).unwrap(), "--04-15");
        assert_eq!(birthday(Some(0), 2, 30), None);
        assert_eq!(birthday(Some(2000), 13, 1), None);
    }

    #[test]
    fn parses_birthdays() {
        assert_eq!(
            parse_birthday("1990-04-15T00:00:00Z").unwrap(),
            "1990-04-15"
        );
        assert_eq!(parse_birthday("--04-15").unwrap(), "--04-15");
        assert_eq!(parse_birthday("0001-01-01T00:00:00Z"), None);
    }

    #[test]
    fn caps_photo_size() {
        assert!(photo_data_url("image/png", &[0u8; 16]).is_some());
        assert!(photo_data_url("image/png", &vec![0u8; MAX_PHOTO_BYTES + 1]).is_none());
    }

    #[test]
    fn sniffs_image_type() {
        let png = [0x89, b'P', b'N', b'G', 0, 0];
        assert!(photo_data_url("application/octet-stream", &png)
            .unwrap()
            .starts_with("data:image/png"));
    }
}
