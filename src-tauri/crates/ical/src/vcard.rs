//! vCard parsing (2.1 / 3.0 / 4.0) and writing (vCard 3.0).
//!
//! Parsing is intentionally forgiving: unknown properties are ignored,
//! malformed blocks are skipped, and vCard 2.1 quoted-printable values with a
//! `CHARSET` parameter are decoded through `encoding_rs`.

use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use base64::Engine;
use chrono::{DateTime, Utc};
use nuncio_core::models::{Contact, ContactField, ContactFieldType, Id, PostalAddress};

use crate::ics::lines::{unfold, ContentLine};
use crate::value::{
    decode_caret, escape_text, format_date, parse_date_value, push_line, split_escaped, split_list,
    unescape_text,
};

/// A parsed vCard plus the group names from `CATEGORIES`.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedContact {
    /// `UID` (empty when the card has none).
    pub uid: String,
    /// Contact with empty id/addressBookId/accountId placeholders.
    pub contact: Contact,
    /// `CATEGORIES` values; the caller maps them to [`nuncio_core::models::ContactGroup`] rows.
    pub group_names: Vec<String>,
}

impl ParsedContact {
    /// Fill ids and return a persistable contact.
    pub fn into_contact(
        self,
        id: impl Into<Id>,
        address_book_id: impl Into<Id>,
        account_id: impl Into<Id>,
    ) -> Contact {
        let mut contact = self.contact;
        contact.id = id.into();
        contact.address_book_id = address_book_id.into();
        contact.account_id = account_id.into();
        if contact.display_name.trim().is_empty() {
            contact.display_name = contact.label();
        }
        contact
    }
}

/// Parse every vCard in `text`.
pub fn parse_vcards(text: &str) -> Vec<ParsedContact> {
    let lines = unfold(text);
    let mut blocks: Vec<Vec<String>> = Vec::new();
    let mut current: Option<Vec<String>> = None;
    let mut saw_begin = false;

    for line in lines {
        let upper = line.trim().to_ascii_uppercase();
        if upper.starts_with("BEGIN:VCARD") {
            saw_begin = true;
            current = Some(Vec::new());
            continue;
        }
        if upper.starts_with("END:VCARD") {
            if let Some(block) = current.take() {
                blocks.push(block);
            }
            continue;
        }
        if let Some(block) = current.as_mut() {
            block.push(line);
        } else if !saw_begin {
            // No BEGIN markers at all: treat the whole text as one card.
            if blocks.is_empty() {
                blocks.push(Vec::new());
            }
            if let Some(block) = blocks.last_mut() {
                block.push(line);
            }
        }
    }
    if let Some(block) = current.take() {
        blocks.push(block);
    }

    blocks
        .into_iter()
        .filter_map(|block| parse_block(&block))
        .collect()
}

fn parse_block(lines: &[String]) -> Option<ParsedContact> {
    let parsed: Vec<ContentLine> = lines
        .iter()
        .filter_map(|line| ContentLine::parse(line))
        .collect();
    if parsed.is_empty() {
        return None;
    }
    let version = parsed
        .iter()
        .find(|line| line.name == "VERSION")
        .map(|line| line.value.trim().to_string())
        .unwrap_or_else(|| "3.0".into());

    let mut contact = Contact::new("", "", "");
    let mut uid = String::new();
    let mut group_names: Vec<String> = Vec::new();
    let mut emails: Vec<ContactField> = Vec::new();
    let mut phones: Vec<ContactField> = Vec::new();
    let mut addresses: Vec<PostalAddress> = Vec::new();
    let mut websites: Vec<String> = Vec::new();

    for line in &parsed {
        match line.name.as_str() {
            "VERSION" | "BEGIN" | "END" | "PRODID" | "KIND" => {}
            "FN" => {
                contact.display_name = text_value(line, &version);
            }
            "N" => {
                let value = decode_text(line, &version);
                let parts = split_escaped(&value, ';');
                contact.family_name = parts
                    .first()
                    .map(|part| unescape_text(part))
                    .unwrap_or_default();
                contact.given_name = parts
                    .get(1)
                    .map(|part| unescape_text(part))
                    .unwrap_or_default();
                contact.middle_name = parts
                    .get(2)
                    .map(|part| unescape_text(part))
                    .unwrap_or_default();
            }
            "NICKNAME" => {
                let value = text_value(line, &version);
                contact.nickname = split_list(&value, ',')
                    .into_iter()
                    .next()
                    .unwrap_or_default();
            }
            "ORG" => {
                let value = decode_text(line, &version);
                let parts = split_escaped(&value, ';');
                contact.company = parts
                    .first()
                    .map(|part| unescape_text(part))
                    .unwrap_or_default();
                contact.department = parts
                    .iter()
                    .skip(1)
                    .map(|part| unescape_text(part))
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join(" / ");
            }
            "TITLE" | "ROLE" => {
                if contact.job_title.is_empty() {
                    contact.job_title = text_value(line, &version);
                }
            }
            "EMAIL" => {
                let value = decode_text(line, &version);
                let value = value.trim().to_string();
                if !value.is_empty() {
                    let kind = field_type(&line.type_values());
                    let primary = is_pref(line);
                    emails.push(ContactField {
                        r#type: kind,
                        value,
                        is_primary: primary,
                    });
                }
            }
            "TEL" => {
                let value = decode_text(line, &version);
                let value = value.trim().to_string();
                if !value.is_empty() {
                    let kind = phone_type(&line.type_values());
                    let primary = is_pref(line);
                    phones.push(ContactField {
                        r#type: kind,
                        value,
                        is_primary: primary,
                    });
                }
            }
            "ADR" => {
                let value = decode_text(line, &version);
                let parts: Vec<String> = split_escaped(&value, ';')
                    .into_iter()
                    .map(|part| unescape_text(&part))
                    .collect();
                let street = match (parts.get(1), parts.get(2)) {
                    (Some(extended), Some(street))
                        if !extended.trim().is_empty() && !street.trim().is_empty() =>
                    {
                        format!("{}\n{}", extended.trim(), street.trim())
                    }
                    (Some(extended), Some(_)) if !extended.trim().is_empty() => {
                        extended.trim().to_string()
                    }
                    (_, Some(street)) => street.trim().to_string(),
                    _ => String::new(),
                };
                let address = PostalAddress {
                    r#type: field_type(&line.type_values()),
                    street,
                    city: parts
                        .get(3)
                        .map(|part| part.trim().to_string())
                        .unwrap_or_default(),
                    region: parts
                        .get(4)
                        .map(|part| part.trim().to_string())
                        .unwrap_or_default(),
                    postal_code: parts
                        .get(5)
                        .map(|part| part.trim().to_string())
                        .unwrap_or_default(),
                    country: parts
                        .get(6)
                        .map(|part| part.trim().to_string())
                        .unwrap_or_default(),
                };
                if !address.street.is_empty()
                    || !address.city.is_empty()
                    || !address.region.is_empty()
                    || !address.postal_code.is_empty()
                    || !address.country.is_empty()
                {
                    addresses.push(address);
                }
            }
            "URL" => {
                let value = decode_text(line, &version);
                let value = value.trim().to_string();
                if !value.is_empty() && !websites.contains(&value) {
                    websites.push(value);
                }
            }
            "BDAY" => {
                let value = decode_text(line, &version);
                if let Some(birthday) = parse_birthday(&value) {
                    contact.birthday = Some(birthday);
                }
            }
            "NOTE" => {
                if contact.notes.is_empty() {
                    contact.notes = text_value(line, &version);
                }
            }
            "CATEGORIES" => {
                let value = decode_text(line, &version);
                for group in split_list(&value, ',') {
                    if !group.is_empty() && !group_names.contains(&group) {
                        group_names.push(group);
                    }
                }
            }
            "UID" => {
                uid = decode_text(line, &version).trim().to_string();
            }
            "REV" => {
                if let Some(updated) = parse_rev(&decode_text(line, &version)) {
                    contact.updated_at = Some(updated);
                }
            }
            "PHOTO" | "LOGO" => {
                if contact.photo.is_none() {
                    contact.photo = decode_photo(line);
                }
            }
            "X-NUNCIO-FAVORITE" => {
                let value = line.value.trim();
                contact.is_favorite = value == "1" || value.eq_ignore_ascii_case("true");
            }
            _ => {}
        }
    }

    if contact.display_name.trim().is_empty() {
        let joined = [contact.given_name.as_str(), contact.family_name.as_str()]
            .iter()
            .filter(|part| !part.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(" ");
        if !joined.is_empty() {
            contact.display_name = joined;
        } else if let Some(first) = emails.first() {
            contact.display_name = first.value.clone();
        }
    }

    if !emails.iter().any(|field| field.is_primary) {
        if let Some(first) = emails.first_mut() {
            first.is_primary = true;
        }
    }
    if !phones.iter().any(|field| field.is_primary) {
        if let Some(first) = phones.first_mut() {
            first.is_primary = true;
        }
    }

    contact.emails = emails;
    contact.phones = phones;
    contact.addresses = addresses;
    contact.websites = websites;

    Some(ParsedContact {
        uid,
        contact,
        group_names,
    })
}

/// Decode a property value to text and remove vCard backslash escapes.
fn text_value(line: &ContentLine, version: &str) -> String {
    unescape_text(&decode_text(line, version))
}

/// Decode a property value to text, honouring QUOTED-PRINTABLE / CHARSET.
fn decode_text(line: &ContentLine, _version: &str) -> String {
    let encoding = line
        .param("ENCODING")
        .map(|value| value.trim().to_ascii_uppercase());
    let charset = line.param("CHARSET");
    match encoding.as_deref() {
        Some("QUOTED-PRINTABLE") | Some("Q") => decode_charset(&qp_decode(&line.value), charset),
        Some("B") | Some("BASE64") => {
            let bytes = decode_base64(&line.value).unwrap_or_default();
            decode_charset(&bytes, charset)
        }
        _ => {
            let value = decode_caret(&line.value);
            if let Some(charset) = charset {
                if !charset.eq_ignore_ascii_case("utf-8") && !charset.eq_ignore_ascii_case("utf8") {
                    return decode_charset(value.as_bytes(), Some(charset));
                }
            }
            value
        }
    }
}

fn decode_charset(bytes: &[u8], charset: Option<&str>) -> String {
    let label = charset.unwrap_or("UTF-8").trim();
    if label.eq_ignore_ascii_case("utf-8")
        || label.eq_ignore_ascii_case("utf8")
        || label.eq_ignore_ascii_case("us-ascii")
        || label.is_empty()
    {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    if let Some(encoding) = encoding_rs::Encoding::for_label(label.as_bytes()) {
        let (text, _, _) = encoding.decode(bytes);
        text.into_owned()
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

fn qp_decode(value: &str) -> Vec<u8> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] == b'=' && idx + 2 < bytes.len() {
            let hex = &value[idx + 1..idx + 3];
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                idx += 3;
                continue;
            }
        }
        out.push(bytes[idx]);
        idx += 1;
    }
    out
}

fn decode_base64(value: &str) -> Option<Vec<u8>> {
    let cleaned: String = value.chars().filter(|ch| !ch.is_whitespace()).collect();
    STANDARD
        .decode(&cleaned)
        .or_else(|_| STANDARD_NO_PAD.decode(cleaned.trim_end_matches('=')))
        .ok()
}

fn is_pref(line: &ContentLine) -> bool {
    if line
        .type_values()
        .iter()
        .any(|value| value.eq_ignore_ascii_case("PREF"))
    {
        return true;
    }
    if line.has_param("PREF") {
        match line.param("PREF") {
            Some(value) => {
                let value = value.trim();
                value == "1"
                    || value.eq_ignore_ascii_case("true")
                    || value.eq_ignore_ascii_case("yes")
            }
            None => true,
        }
    } else {
        false
    }
}

fn field_type(types: &[String]) -> ContactFieldType {
    let upper: Vec<String> = types
        .iter()
        .map(|value| value.to_ascii_uppercase())
        .collect();
    let has = |needle: &str| upper.iter().any(|value| value == needle);
    if has("MOBILE") || has("CELL") {
        ContactFieldType::Mobile
    } else if has("FAX") {
        ContactFieldType::Fax
    } else if has("PAGER") {
        ContactFieldType::Pager
    } else if has("HOME") {
        ContactFieldType::Home
    } else if has("WORK") {
        ContactFieldType::Work
    } else if has("MAIN") || has("VOICE") {
        ContactFieldType::Main
    } else {
        ContactFieldType::Other
    }
}

fn phone_type(types: &[String]) -> ContactFieldType {
    field_type(types)
}

fn parse_birthday(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix("--") {
        let digits: String = rest.chars().filter(|ch| ch.is_ascii_digit()).collect();
        if digits.len() >= 4 {
            return Some(format!("--{}-{}", &digits[0..2], &digits[2..4]));
        }
        return None;
    }
    // vCard 4.0 `19960415` / `1996-04-15` / date-time.
    parse_date_value(trimmed).map(|parsed| format_date(parsed.date))
}

fn parse_rev(value: &str) -> Option<DateTime<Utc>> {
    let parsed = parse_date_value(value)?;
    Some(
        parsed
            .resolve(crate::value::TzSpec::utc())
            .with_timezone(&Utc),
    )
}

fn decode_photo(line: &ContentLine) -> Option<String> {
    let raw = line.value.trim();
    if raw.starts_with("data:") {
        return Some(raw.to_string());
    }
    if raw.starts_with("http://") || raw.starts_with("https://") {
        return Some(raw.to_string());
    }
    let encoding = line
        .param("ENCODING")
        .map(|value| value.trim().to_ascii_uppercase());
    let bytes = match encoding.as_deref() {
        Some("B") | Some("BASE64") => decode_base64(raw)?,
        Some("QUOTED-PRINTABLE") | Some("Q") => qp_decode(raw),
        _ => {
            // Some producers omit ENCODING for base64 photos.
            decode_base64(raw)?
        }
    };
    if bytes.is_empty() {
        return None;
    }
    let mime = photo_media_type(line, &bytes);
    Some(format!("data:{mime};base64,{}", STANDARD.encode(bytes)))
}

fn photo_media_type(line: &ContentLine, bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return "image/jpeg".into();
    }
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        return "image/png".into();
    }
    if bytes.starts_with(b"GIF8") {
        return "image/gif".into();
    }
    if bytes.len() > 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return "image/webp".into();
    }
    for value in line.type_values() {
        match value.to_ascii_uppercase().as_str() {
            "JPEG" | "JPG" => return "image/jpeg".into(),
            "PNG" => return "image/png".into(),
            "GIF" => return "image/gif".into(),
            "WEBP" => return "image/webp".into(),
            _ => {}
        }
    }
    "image/jpeg".into()
}

// ─────────────────────────────── Writing ───────────────────────────────

/// Write a vCard 3.0 for `contact`.
pub fn write_vcard(contact: &Contact, group_names: &[String]) -> String {
    let mut out = String::new();
    push_line(&mut out, "BEGIN:VCARD");
    push_line(&mut out, "VERSION:3.0");
    push_line(&mut out, "PRODID:-//Nuncio//EN");

    let display = if !contact.display_name.trim().is_empty() {
        contact.display_name.clone()
    } else {
        contact.label()
    };
    push_line(&mut out, &format!("FN:{}", escape_text(&display)));
    push_line(
        &mut out,
        &format!(
            "N:{};{};{};;",
            escape_text(&contact.family_name),
            escape_text(&contact.given_name),
            escape_text(&contact.middle_name)
        ),
    );
    if !contact.nickname.is_empty() {
        push_line(
            &mut out,
            &format!("NICKNAME:{}", escape_text(&contact.nickname)),
        );
    }
    if !contact.company.is_empty() || !contact.department.is_empty() {
        push_line(
            &mut out,
            &format!(
                "ORG:{};{}",
                escape_text(&contact.company),
                escape_text(&contact.department)
            ),
        );
    }
    if !contact.job_title.is_empty() {
        push_line(
            &mut out,
            &format!("TITLE:{}", escape_text(&contact.job_title)),
        );
    }

    for field in &contact.emails {
        let mut line = String::from("EMAIL;TYPE=");
        line.push_str(email_type(field.r#type));
        if field.is_primary {
            line.push_str(";PREF=1");
        }
        line.push(':');
        line.push_str(&escape_text(&field.value));
        push_line(&mut out, &line);
    }
    for field in &contact.phones {
        let mut line = String::from("TEL;TYPE=");
        line.push_str(phone_type_label(field.r#type));
        if field.is_primary {
            line.push_str(";PREF=1");
        }
        line.push(':');
        line.push_str(&escape_text(&field.value));
        push_line(&mut out, &line);
    }
    for address in &contact.addresses {
        let line = format!(
            "ADR;TYPE={}:;;{};{};{};{};{}",
            address_type(address.r#type),
            escape_text(&address.street),
            escape_text(&address.city),
            escape_text(&address.region),
            escape_text(&address.postal_code),
            escape_text(&address.country)
        );
        push_line(&mut out, &line);
    }
    for website in &contact.websites {
        push_line(&mut out, &format!("URL:{}", escape_text(website)));
    }
    if let Some(birthday) = &contact.birthday {
        push_line(&mut out, &format!("BDAY:{}", birthday));
    }
    if !contact.notes.is_empty() {
        push_line(&mut out, &format!("NOTE:{}", escape_text(&contact.notes)));
    }
    if let Some(photo) = &contact.photo {
        if let Some(line) = photo_line(photo) {
            push_line(&mut out, &line);
        }
    }
    if !group_names.is_empty() {
        let escaped = group_names
            .iter()
            .map(|group| escape_text(group))
            .collect::<Vec<_>>()
            .join(",");
        push_line(&mut out, &format!("CATEGORIES:{escaped}"));
    }
    if !contact.id.is_empty() {
        push_line(&mut out, &format!("UID:{}", escape_text(&contact.id)));
    }
    if let Some(updated) = contact.updated_at {
        push_line(
            &mut out,
            &format!("REV:{}", updated.format("%Y%m%dT%H%M%SZ")),
        );
    }
    if contact.is_favorite {
        push_line(&mut out, "X-NUNCIO-FAVORITE:1");
    }
    push_line(&mut out, "END:VCARD");
    out
}

fn email_type(kind: ContactFieldType) -> &'static str {
    match kind {
        ContactFieldType::Home => "HOME",
        ContactFieldType::Work => "WORK",
        ContactFieldType::Mobile => "CELL",
        ContactFieldType::Fax => "FAX",
        ContactFieldType::Pager => "PAGER",
        ContactFieldType::Main => "VOICE",
        ContactFieldType::Other => "INTERNET",
    }
}

fn address_type(kind: ContactFieldType) -> &'static str {
    match kind {
        ContactFieldType::Home => "HOME",
        ContactFieldType::Work => "WORK",
        ContactFieldType::Other => "OTHER",
        ContactFieldType::Mobile => "CELL",
        ContactFieldType::Fax => "FAX",
        ContactFieldType::Pager => "PAGER",
        ContactFieldType::Main => "VOICE",
    }
}

fn phone_type_label(kind: ContactFieldType) -> &'static str {
    match kind {
        ContactFieldType::Home => "HOME",
        ContactFieldType::Work => "WORK",
        ContactFieldType::Mobile => "CELL",
        ContactFieldType::Fax => "FAX",
        ContactFieldType::Pager => "PAGER",
        ContactFieldType::Main => "VOICE",
        ContactFieldType::Other => "VOICE",
    }
}

fn photo_line(photo: &str) -> Option<String> {
    if let Some(rest) = photo.strip_prefix("data:") {
        let (meta, payload) = rest.split_once(',')?;
        if !meta.to_ascii_lowercase().contains("base64") {
            return None;
        }
        let mime = meta.split(';').next().unwrap_or("image/jpeg");
        let subtype = mime
            .split('/')
            .nth(1)
            .unwrap_or("jpeg")
            .to_ascii_uppercase();
        let subtype = if subtype == "JPG" {
            "JPEG".to_string()
        } else {
            subtype
        };
        return Some(format!("PHOTO;ENCODING=b;TYPE={subtype}:{payload}"));
    }
    if photo.starts_with("http://") || photo.starts_with("https://") {
        return Some(format!("PHOTO;VALUE=URI:{photo}"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuncio_core::models::ContactFieldType;

    #[test]
    fn parses_vcard_30_with_chinese_name() {
        let text = "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:张伟\r\nN:张;伟;;;\r\nORG:示例公司;研发部\r\nTITLE:工程师\r\nEMAIL;TYPE=WORK;PREF=1:zhang.wei@example.com\r\nTEL;TYPE=CELL:+86 138 0000 0000\r\nADR;TYPE=WORK:;;中关村大街 1 号;北京市;北京;100080;中国\r\nURL:https://example.com\r\nBDAY:1990-05-01\r\nNOTE:第一行\\n第二行\r\nCATEGORIES:同事,朋友\r\nUID:card-1\r\nEND:VCARD\r\n";
        let cards = parse_vcards(text);
        assert_eq!(cards.len(), 1);
        let card = &cards[0];
        assert_eq!(card.contact.display_name, "张伟");
        assert_eq!(card.contact.family_name, "张");
        assert_eq!(card.contact.given_name, "伟");
        assert_eq!(card.contact.company, "示例公司");
        assert_eq!(card.contact.department, "研发部");
        assert_eq!(card.contact.emails[0].value, "zhang.wei@example.com");
        assert!(card.contact.emails[0].is_primary);
        assert_eq!(card.contact.phones[0].r#type, ContactFieldType::Mobile);
        assert_eq!(card.contact.addresses[0].city, "北京市");
        assert_eq!(card.contact.birthday.as_deref(), Some("1990-05-01"));
        assert_eq!(card.contact.notes, "第一行\n第二行");
        assert_eq!(card.group_names, vec!["同事", "朋友"]);
        assert_eq!(card.uid, "card-1");
    }

    #[test]
    fn parses_vcard_21_quoted_printable_chinese() {
        let name_qp = "=E5=BC=A0=E4=BC=9F";
        let text = format!(
            "BEGIN:VCARD\nVERSION:2.1\nFN;CHARSET=UTF-8;ENCODING=QUOTED-PRINTABLE:{name_qp}\nN;CHARSET=UTF-8;ENCODING=QUOTED-PRINTABLE:=\n=E5=BC=A0;=E4=BC=9F;;;\nEMAIL;PREF;INTERNET:a@example.com\nTEL;HOME;VOICE:010-12345678\nEND:VCARD\n"
        );
        let cards = parse_vcards(&text);
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].contact.display_name, "张伟");
        assert_eq!(cards[0].contact.family_name, "张");
        assert_eq!(cards[0].contact.given_name, "伟");
        assert!(cards[0].contact.emails[0].is_primary);
        assert_eq!(cards[0].contact.phones[0].r#type, ContactFieldType::Home);
    }

    #[test]
    fn parses_vcard_40_birthday_without_year_and_photo() {
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAFgwJ/lJ0nWQAAAABJRU5ErkJggg==";
        let text = format!(
            "BEGIN:VCARD\nVERSION:4.0\nFN:Alice\nN:Smith;Alice;;;\nBDAY:--0203\nPHOTO:data:image/png;base64,{png}\nEMAIL:alice@example.com\nEND:VCARD\n"
        );
        let cards = parse_vcards(&text);
        assert_eq!(cards[0].contact.birthday.as_deref(), Some("--02-03"));
        assert!(cards[0]
            .contact
            .photo
            .as_deref()
            .unwrap_or_default()
            .starts_with("data:image/png;base64,"));
    }

    #[test]
    fn round_trips_chinese_contact() {
        let text = "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:李雷\r\nN:李;雷;;;\r\nORG:公司;部门\r\nEMAIL;TYPE=WORK;PREF=1:lilei@example.com\r\nTEL;TYPE=CELL:13900000000\r\nNOTE:备注\r\nCATEGORIES:朋友\r\nUID:c1\r\nEND:VCARD\r\n";
        let parsed = parse_vcards(text).remove(0);
        let groups = parsed.group_names.clone();
        let contact = parsed.into_contact("local-id", "book", "acct");
        let written = write_vcard(&contact, &groups);
        let reparsed = parse_vcards(&written).remove(0);
        assert_eq!(reparsed.contact.display_name, "李雷");
        assert_eq!(reparsed.contact.company, "公司");
        assert_eq!(reparsed.contact.emails[0].value, "lilei@example.com");
        assert_eq!(reparsed.group_names, vec!["朋友"]);
        assert!(written.contains("\r\n"));
    }

    #[test]
    fn multiple_cards_and_groups() {
        let text = "BEGIN:VCARD\nVERSION:3.0\nFN:A\nEND:VCARD\nBEGIN:VCARD\nVERSION:3.0\nFN:B\nEND:VCARD\n";
        let cards = parse_vcards(text);
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].contact.display_name, "A");
        assert_eq!(cards[1].contact.display_name, "B");
    }
}
