//! Generic WebDAV `multistatus` parsing and request-body builders.
//!
//! The parser works on *local* element names (namespace prefixes are stripped)
//! because CalDAV/CardDAV servers use a wide variety of prefixes.  It is shared
//! by the CalDAV implementation here and the CardDAV implementation added in
//! stage 2.

use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::Event;
use quick_xml::Reader;

use crate::error::{protocol, Result};

pub const NS_DAV: &str = "DAV:";
pub const NS_CALDAV: &str = "urn:ietf:params:xml:ns:caldav";
pub const NS_CARDDAV: &str = "urn:ietf:params:xml:ns:carddav";
pub const NS_APPLE: &str = "http://apple.com/ns/ical/";
pub const NS_CS: &str = "http://calendarserver.org/ns/";

/// A parsed XML element.
#[derive(Debug, Clone, Default)]
pub struct Node {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub text: String,
    pub children: Vec<Node>,
}

impl Node {
    pub fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|child| child.name == name)
    }

    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> {
        self.children.iter().filter(move |child| child.name == name)
    }

    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// Depth-first search for the first descendant with `name`.
    pub fn find(&self, name: &str) -> Option<&Node> {
        if self.name == name {
            return Some(self);
        }
        for child in &self.children {
            if let Some(found) = child.find(name) {
                return Some(found);
            }
        }
        None
    }

    pub fn find_all<'a>(&'a self, name: &'a str, out: &mut Vec<&'a Node>) {
        if self.name == name {
            out.push(self);
        }
        for child in &self.children {
            child.find_all(name, out);
        }
    }
}

fn local_name(raw: &str) -> String {
    match raw.rsplit_once(':') {
        Some((_, local)) => local.to_ascii_lowercase(),
        None => raw.to_ascii_lowercase(),
    }
}

/// Decode the body of a numeric character reference (`#xD`, `#13`).
///
/// Servers escape the carriage returns of a CRLF-terminated vCard/iCalendar as
/// `&#xD;`. Left undecoded, the literal text ends up at the end of every field
/// (a contact shown as "Name&#xD;"). Only valid Unicode scalar values decode;
/// anything else stays as written.
fn numeric_char_reference(name: &str) -> Option<char> {
    let digits = name.strip_prefix('#')?;
    let code = match digits.strip_prefix(['x', 'X']) {
        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
        None => digits.parse::<u32>().ok()?,
    };
    char::from_u32(code)
}

/// Parse an XML document into a lightweight tree.
pub fn parse_node(xml: &str) -> Result<Node> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut root = Node {
        name: "#document".into(),
        ..Node::default()
    };
    let mut stack: Vec<Node> = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(start)) => {
                stack.push(Node {
                    name: local_name(start.name().as_ref()),
                    ..Node::default()
                });
                let attrs: Vec<(String, String)> = start
                    .attributes()
                    .filter_map(|attr| attr.ok())
                    .map(|attr| {
                        let key = local_name(attr.key.as_ref());
                        let value = attr
                            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                            .map(|value| value.into_owned())
                            .unwrap_or_default();
                        (key, value)
                    })
                    .collect();
                if let Some(node) = stack.last_mut() {
                    node.attrs = attrs;
                }
            }
            Ok(Event::Empty(start)) => {
                let attrs: Vec<(String, String)> = start
                    .attributes()
                    .filter_map(|attr| attr.ok())
                    .map(|attr| {
                        let key = local_name(attr.key.as_ref());
                        let value = attr
                            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                            .map(|value| value.into_owned())
                            .unwrap_or_default();
                        (key, value)
                    })
                    .collect();
                let node = Node {
                    name: local_name(start.name().as_ref()),
                    attrs,
                    ..Node::default()
                };
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(node);
                } else {
                    root.children.push(node);
                }
            }
            Ok(Event::End(_)) => {
                let Some(node) = stack.pop() else {
                    return Err(protocol("unbalanced XML end tag in DAV response"));
                };
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(node);
                } else {
                    root.children.push(node);
                }
            }
            Ok(Event::Text(text)) => {
                if let Some(node) = stack.last_mut() {
                    node.text.push_str(text.as_ref());
                }
            }
            Ok(Event::CData(cdata)) => {
                if let Some(node) = stack.last_mut() {
                    node.text.push_str(cdata.as_ref());
                }
            }
            Ok(Event::GeneralRef(reference)) => {
                if let Some(node) = stack.last_mut() {
                    let name = reference.as_ref();
                    if let Some(value) = resolve_predefined_entity(name) {
                        node.text.push_str(value);
                    } else if let Some(ch) = numeric_char_reference(name) {
                        node.text.push(ch);
                    } else {
                        node.text.push('&');
                        node.text.push_str(name);
                        node.text.push(';');
                    }
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => {
                return Err(protocol(format!("invalid XML in DAV response: {error}")));
            }
        }
    }
    if !stack.is_empty() {
        return Err(protocol("unterminated XML element in DAV response"));
    }
    Ok(root)
}

/// Properties extracted from one `response` element.
#[derive(Debug, Clone, Default)]
pub struct DavProp {
    pub etag: Option<String>,
    pub data: Option<String>,
    pub display_name: Option<String>,
    pub color: Option<String>,
    pub sync_token: Option<String>,
    pub ctag: Option<String>,
    pub is_collection: bool,
    pub is_calendar: bool,
    pub is_address_book: bool,
    pub read_only: bool,
    pub components: Vec<String>,
    pub supports_sync_collection: bool,
    pub content_type: Option<String>,
}

/// One `response` element.
#[derive(Debug, Clone, Default)]
pub struct DavResponse {
    pub href: String,
    pub status: Option<u16>,
    pub prop: DavProp,
    pub not_found: bool,
    pub ok: bool,
}

/// Parsed `multistatus`.
#[derive(Debug, Clone, Default)]
pub struct Multistatus {
    pub responses: Vec<DavResponse>,
    pub sync_token: Option<String>,
    pub current_user_principal: Option<String>,
    pub calendar_home_set: Option<String>,
    pub addressbook_home_set: Option<String>,
    pub display_name: Option<String>,
}

fn status_code(text: &str) -> Option<u16> {
    let text = text.trim();
    let token = text
        .split_whitespace()
        .find(|part| part.chars().all(|c| c.is_ascii_digit()))?;
    token.parse().ok()
}

fn prop_from_node(prop: &Node) -> DavProp {
    let mut out = DavProp::default();
    for child in &prop.children {
        match child.name.as_str() {
            "getetag" => out.etag = Some(child.text.trim().trim_matches('"').to_string()),
            "getcontenttype" => out.content_type = Some(child.text.trim().to_string()),
            "calendar-data" | "address-data" => out.data = Some(child.text.clone()),
            "displayname" => out.display_name = Some(child.text.trim().to_string()),
            "calendar-color" => {
                out.color = crate::util::normalize_color(child.text.trim());
            }
            "getctag" => out.ctag = Some(child.text.trim().to_string()),
            "sync-token" => out.sync_token = Some(child.text.trim().to_string()),
            "resourcetype" => {
                out.is_collection = child.children.iter().any(|c| c.name == "collection");
                out.is_calendar = child.children.iter().any(|c| c.name == "calendar");
                out.is_address_book = child.children.iter().any(|c| c.name == "addressbook");
            }
            "supported-calendar-component-set" => {
                out.components = child
                    .children
                    .iter()
                    .filter(|c| c.name == "comp")
                    .filter_map(|c| c.attr("name").map(|name| name.to_ascii_uppercase()))
                    .collect();
            }
            "current-user-privilege-set" => {
                let mut privileges = Vec::new();
                child.find_all("privilege", &mut privileges);
                let has_write = privileges.iter().any(|privilege| {
                    privilege
                        .children
                        .iter()
                        .any(|c| c.name == "write" || c.name == "write-content")
                });
                out.read_only = !has_write;
            }
            "supported-report-set" => {
                out.supports_sync_collection = child.find("sync-collection").is_some();
            }
            "current-user-principal" | "calendar-home-set" | "addressbook-home-set" => {
                // Handled at multistatus level.
            }
            _ => {}
        }
    }
    out
}

fn href_of(node: &Node) -> Option<String> {
    node.child("href")
        .map(|href| href.text.trim().to_string())
        .filter(|href| !href.is_empty())
        .or_else(|| {
            // Some servers wrap the href in a different structure.
            node.find("href")
                .map(|href| href.text.trim().to_string())
                .filter(|href| !href.is_empty())
        })
}

/// Parse a `207 Multi-Status` body.
pub fn parse_multistatus(xml: &str) -> Result<Multistatus> {
    let root = parse_node(xml)?;
    let multistatus = if root.name == "multistatus" {
        root
    } else {
        root.find("multistatus")
            .cloned()
            .ok_or_else(|| protocol("DAV response has no multistatus element"))?
    };

    let mut out = Multistatus {
        sync_token: multistatus
            .child("sync-token")
            .map(|node| node.text.trim().to_string())
            .filter(|value| !value.is_empty()),
        ..Multistatus::default()
    };

    for response in multistatus.children_named("response") {
        let href = href_of(response).unwrap_or_default();
        let direct_status = response
            .child("status")
            .and_then(|node| status_code(&node.text));
        let mut prop = DavProp::default();
        let mut not_found = direct_status == Some(404);
        let mut ok = direct_status.is_none_or(|code| (200..300).contains(&code));
        let mut principal = None;
        let mut calendar_home = None;
        let mut addressbook_home = None;

        for propstat in response.children_named("propstat") {
            let status = propstat
                .child("status")
                .and_then(|node| status_code(&node.text));
            if status == Some(404) {
                not_found = true;
            }
            if status.is_some_and(|code| (200..300).contains(&code)) {
                ok = true;
            }
            if let Some(prop_node) = propstat.child("prop") {
                let parsed = prop_from_node(prop_node);
                merge_prop(&mut prop, parsed);
                if let Some(node) = prop_node.child("current-user-principal") {
                    principal = href_of(node);
                }
                if let Some(node) = prop_node.child("calendar-home-set") {
                    calendar_home = href_of(node);
                }
                if let Some(node) = prop_node.child("addressbook-home-set") {
                    addressbook_home = href_of(node);
                }
            }
        }

        if principal.is_some() {
            out.current_user_principal = principal;
        }
        if calendar_home.is_some() {
            out.calendar_home_set = calendar_home;
        }
        if addressbook_home.is_some() {
            out.addressbook_home_set = addressbook_home;
        }
        if prop.display_name.is_some() {
            out.display_name = prop.display_name.clone();
        }
        out.responses.push(DavResponse {
            href,
            status: direct_status,
            prop,
            not_found,
            ok,
        });
    }
    Ok(out)
}

fn merge_prop(target: &mut DavProp, source: DavProp) {
    if source.etag.is_some() {
        target.etag = source.etag;
    }
    if source.data.is_some() {
        target.data = source.data;
    }
    if source.display_name.is_some() {
        target.display_name = source.display_name;
    }
    if source.color.is_some() {
        target.color = source.color;
    }
    if source.sync_token.is_some() {
        target.sync_token = source.sync_token;
    }
    if source.ctag.is_some() {
        target.ctag = source.ctag;
    }
    if source.content_type.is_some() {
        target.content_type = source.content_type;
    }
    target.is_collection |= source.is_collection;
    target.is_calendar |= source.is_calendar;
    target.is_address_book |= source.is_address_book;
    target.read_only |= source.read_only;
    target.supports_sync_collection |= source.supports_sync_collection;
    target.components.extend(source.components);
}

/* ───────────────────────────── request bodies ───────────────────────────── */

/// PROPFIND used to discover `current-user-principal`.
pub fn principal_propfind() -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="{NS_DAV}">
  <d:prop><d:current-user-principal/></d:prop>
</d:propfind>"#
    )
}

/// PROPFIND on the principal for calendar/address-book homes.
pub fn homes_propfind() -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="{NS_DAV}" xmlns:cal="{NS_CALDAV}" xmlns:card="{NS_CARDDAV}">
  <d:prop>
    <d:displayname/>
    <d:current-user-principal/>
    <cal:calendar-home-set/>
    <card:addressbook-home-set/>
  </d:prop>
</d:propfind>"#
    )
}

/// PROPFIND Depth: 1 listing used for calendars and address books.
pub fn collections_propfind(kind: &str) -> String {
    let extra = if kind == "addressbook" {
        "<card:addressbook-description/>".to_string()
    } else {
        "<cal:supported-calendar-component-set/><a:calendar-color/>".to_string()
    };
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="{NS_DAV}" xmlns:cal="{NS_CALDAV}" xmlns:card="{NS_CARDDAV}" xmlns:a="{NS_APPLE}" xmlns:cs="{NS_CS}">
  <d:prop>
    <d:resourcetype/>
    <d:displayname/>
    <d:getetag/>
    <d:current-user-privilege-set/>
    <d:supported-report-set/>
    <d:sync-token/>
    <cs:getctag/>
    {extra}
  </d:prop>
</d:propfind>"#
    )
}

/// PROPFIND Depth: 1 `getetag` listing (ctag fallback).
pub fn etags_propfind() -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="{NS_DAV}" xmlns:cs="{NS_CS}">
  <d:prop><d:getetag/><cs:getctag/><d:sync-token/></d:prop>
</d:propfind>"#
    )
}

/// RFC 6578 `sync-collection` REPORT body.
pub fn sync_collection_body(token: Option<&str>) -> String {
    let token = token.unwrap_or("");
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<d:sync-collection xmlns:d="{NS_DAV}">
  <d:sync-token>{token}</d:sync-token>
  <d:sync-level>1</d:sync-level>
  <d:prop><d:getetag/></d:prop>
</d:sync-collection>"#
    )
}

/// `calendar-multiget` / `addressbook-multiget` REPORT body.
pub fn multiget_body(kind: &str, hrefs: &[String]) -> String {
    let (report, data, ns) = if kind == "addressbook" {
        ("addressbook-multiget", "card:address-data", NS_CARDDAV)
    } else {
        ("calendar-multiget", "cal:calendar-data", NS_CALDAV)
    };
    let href_lines = hrefs
        .iter()
        .map(|href| format!("  <d:href>{}</d:href>", crate::util::xml_escape(href)))
        .collect::<Vec<_>>()
        .join("\n");
    let _ = ns;
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<d:{report} xmlns:d="{NS_DAV}" xmlns:cal="{NS_CALDAV}" xmlns:card="{NS_CARDDAV}">
  <d:prop><d:getetag/><{data}/></d:prop>
{href_lines}
</d:{report}>"#
    )
}

/// `MKCALENDAR` body.
pub fn mkcalendar_body(name: &str, color: Option<&str>) -> String {
    mkcalendar_body_with_component(name, color, "VEVENT")
}

/// `MKCALENDAR` body for a collection that supports `component` (`VEVENT` or
/// `VTODO`).
pub fn mkcalendar_body_with_component(name: &str, color: Option<&str>, component: &str) -> String {
    let component = component.to_ascii_uppercase();
    let color = color
        .map(|color| {
            format!(
                "<a:calendar-color>{}</a:calendar-color>",
                crate::util::xml_escape(color)
            )
        })
        .unwrap_or_default();
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<cal:mkcalendar xmlns:d="{NS_DAV}" xmlns:cal="{NS_CALDAV}" xmlns:a="{NS_APPLE}">
  <d:set>
    <d:prop>
      <d:displayname>{}</d:displayname>
      <cal:supported-calendar-component-set><cal:comp name="{component}"/></cal:supported-calendar-component-set>
      {color}
    </d:prop>
  </d:set>
</cal:mkcalendar>"#,
        crate::util::xml_escape(name)
    )
}

/// `PROPPATCH` body used to rename/recolor a collection.
pub fn proppatch_body(name: Option<&str>, color: Option<&str>) -> String {
    let mut props = String::new();
    if let Some(name) = name {
        props.push_str(&format!(
            "<d:displayname>{}</d:displayname>",
            crate::util::xml_escape(name)
        ));
    }
    if let Some(color) = color {
        props.push_str(&format!(
            "<a:calendar-color>{}</a:calendar-color>",
            crate::util::xml_escape(color)
        ));
    }
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<d:propertyupdate xmlns:d="{NS_DAV}" xmlns:a="{NS_APPLE}">
  <d:set><d:prop>{props}</d:prop></d:set>
</d:propertyupdate>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const ICLOUD_LIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<multistatus xmlns="DAV:">
  <response>
    <href>/123/calendars/</href>
    <propstat>
      <prop>
        <resourcetype><collection/><calendar/></resourcetype>
        <displayname>Home</displayname>
        <calendar-color xmlns="http://apple.com/ns/ical/">#FF2968FF</calendar-color>
        <supported-calendar-component-set xmlns="urn:ietf:params:xml:ns:caldav"><comp name="VEVENT"/></supported-calendar-component-set>
        <current-user-privilege-set><privilege><read/></privilege><privilege><write/></privilege></current-user-privilege-set>
        <sync-token>http://example.com/ns/sync/1234</sync-token>
      </prop>
      <status>HTTP/1.1 200 OK</status>
    </propstat>
  </response>
  <response>
    <href>/123/calendars/readonly/</href>
    <propstat>
      <prop>
        <resourcetype><collection/><calendar/></resourcetype>
        <displayname>Shared</displayname>
        <current-user-privilege-set><privilege><read/></privilege></current-user-privilege-set>
      </prop>
      <status>HTTP/1.1 200 OK</status>
    </propstat>
  </response>
</multistatus>"#;

    const FASTMAIL_SYNC: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/cal/event-1.ics</d:href>
    <d:propstat><d:prop><d:getetag>"e1"</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:response>
    <d:href>/cal/deleted.ics</d:href>
    <d:status>HTTP/1.1 404 Not Found</d:status>
  </d:response>
  <d:sync-token>token-2</d:sync-token>
</d:multistatus>"#;

    const NEXTCLOUD_MULTIGET: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/remote.php/dav/calendars/user/personal/abc.ics</d:href>
    <d:propstat>
      <d:prop>
        <d:getetag>"abc"</d:getetag>
        <cal:calendar-data>BEGIN:VCALENDAR
BEGIN:VEVENT
UID:abc
SUMMARY:Test &amp; Coffee
DTSTART:20261112T090000Z
DTEND:20261112T100000Z
END:VEVENT
END:VCALENDAR</cal:calendar-data>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;

    const PRINCIPAL: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/</d:href>
    <d:propstat>
      <d:prop><d:current-user-principal><d:href>/principals/users/jacky/</d:href></d:current-user-principal></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;

    #[test]
    fn parses_calendar_list() {
        let parsed = parse_multistatus(ICLOUD_LIST).expect("parse");
        assert_eq!(parsed.responses.len(), 2);
        let first = &parsed.responses[0];
        assert_eq!(first.prop.display_name.as_deref(), Some("Home"));
        assert_eq!(first.prop.color.as_deref(), Some("#FF2968"));
        assert!(first.prop.is_calendar);
        assert!(!first.prop.read_only);
        assert_eq!(first.prop.components, vec!["VEVENT"]);
        assert!(parsed.responses[1].prop.read_only);
    }

    #[test]
    fn parses_sync_collection() {
        let parsed = parse_multistatus(FASTMAIL_SYNC).expect("parse");
        assert_eq!(parsed.sync_token.as_deref(), Some("token-2"));
        assert_eq!(parsed.responses.len(), 2);
        assert_eq!(parsed.responses[0].prop.etag.as_deref(), Some("e1"));
        assert!(parsed.responses[1].not_found);
    }

    #[test]
    fn parses_multiget_with_entities() {
        let parsed = parse_multistatus(NEXTCLOUD_MULTIGET).expect("parse");
        let response = &parsed.responses[0];
        assert_eq!(response.prop.etag.as_deref(), Some("abc"));
        let data = response.prop.data.as_deref().expect("calendar data");
        assert!(data.contains("SUMMARY:Test & Coffee"));
    }

    #[test]
    fn numeric_character_references_decode() {
        assert_eq!(numeric_char_reference("#xD"), Some('\r'));
        assert_eq!(numeric_char_reference("#xd"), Some('\r'));
        assert_eq!(numeric_char_reference("#13"), Some('\r'));
        assert_eq!(numeric_char_reference("#10"), Some('\n'));
        assert_eq!(numeric_char_reference("#x4E2D"), Some('中'));
        // Not a reference, not a number, or not a valid scalar value.
        assert_eq!(numeric_char_reference("amp"), None);
        assert_eq!(numeric_char_reference("#"), None);
        assert_eq!(numeric_char_reference("#xZZ"), None);
        assert_eq!(numeric_char_reference("#xD800"), None);
        assert_eq!(numeric_char_reference("#x110000"), None);
    }

    #[test]
    fn carriage_return_references_in_address_data_do_not_leak_into_fields() {
        // The shape contacts.163.com returns: CRLF lines escaped as &#xD;.
        let xml = "<d:multistatus xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:carddav\">\
<d:response><d:href>/a.vcf</d:href><d:propstat><d:status>HTTP/1.1 200 OK</d:status><d:prop>\
<d:getetag>\"1\"</d:getetag>\
<c:address-data>BEGIN:VCARD&#xD;\nVERSION:3.0&#xD;\nFN:Alice&#xD;\nEMAIL:alice@example.com&#xD;\nEND:VCARD&#xD;\n</c:address-data>\
</d:prop></d:propstat></d:response></d:multistatus>";
        let parsed = parse_multistatus(xml).expect("parse");
        let data = parsed.responses[0].prop.data.as_deref().expect("data");
        assert!(!data.contains("&#"), "{data:?}");
        assert!(data.contains("FN:Alice\r\n"), "{data:?}");
    }

    #[test]
    fn extracts_principal() {
        let parsed = parse_multistatus(PRINCIPAL).expect("parse");
        assert_eq!(
            parsed.current_user_principal.as_deref(),
            Some("/principals/users/jacky/")
        );
    }

    #[test]
    fn request_bodies_are_well_formed() {
        assert!(multiget_body("calendar", &["/a&b.ics".to_string()]).contains("/a&amp;b.ics"));
        assert!(mkcalendar_body("Work", Some("#112233")).contains("Work"));
        assert!(sync_collection_body(Some("tok")).contains("<d:sync-token>tok</d:sync-token>"));
    }
}
