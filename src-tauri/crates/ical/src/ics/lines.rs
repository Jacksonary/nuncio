//! Physical-line unfolding and content-line parsing (RFC 5545 §3.1).
//!
//! The same parser is used for iCalendar and vCard. vCard 2.1 allows bare
//! parameters (`TEL;HOME;VOICE:`) which are preserved as parameter names with
//! an empty value list.

use crate::value::decode_caret;

/// A parsed `NAME;PARAM=value:VALUE` content line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentLine {
    /// Upper-cased property name, group prefix removed.
    pub name: String,
    /// Upper-cased parameter name and its (unquoted) values.
    pub params: Vec<(String, Vec<String>)>,
    /// Raw property value (escapes are *not* removed).
    pub value: String,
}

impl ContentLine {
    /// Parse one logical line. Returns `None` for blank lines.
    pub fn parse(line: &str) -> Option<Self> {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.trim().is_empty() {
            return None;
        }
        let colon = find_unquoted(line, ':')?;
        let head = &line[..colon];
        let value = line[colon + 1..].to_string();
        let mut segments = split_unquoted(head, ';').into_iter();
        let first_owned = segments.next()?;
        let first = first_owned.trim();
        if first.is_empty() {
            return None;
        }
        let name = first
            .rsplit('.')
            .next()
            .unwrap_or(first)
            .trim()
            .to_ascii_uppercase();

        let mut params = Vec::new();
        for segment in segments {
            let segment = segment.trim();
            if segment.is_empty() {
                continue;
            }
            match find_unquoted(segment, '=') {
                Some(eq) => {
                    let key = segment[..eq].trim().to_ascii_uppercase();
                    if key.is_empty() {
                        continue;
                    }
                    let raw = segment[eq + 1..].trim();
                    let values = split_unquoted(raw, ',')
                        .into_iter()
                        .map(|v| unquote_param(v.trim()))
                        .collect();
                    params.push((key, values));
                }
                None => {
                    // vCard 2.1 bare parameter, e.g. `TEL;HOME;VOICE`.
                    let key = unquote_param(segment).trim().to_ascii_uppercase();
                    if !key.is_empty() {
                        params.push((key, Vec::new()));
                    }
                }
            }
        }

        Some(Self {
            name,
            params,
            value,
        })
    }

    /// First value of a parameter.
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .and_then(|(_, values)| values.first())
            .map(|value| value.as_str())
    }

    /// True when the parameter exists (even without a value).
    pub fn has_param(&self, name: &str) -> bool {
        self.params
            .iter()
            .any(|(key, _)| key.eq_ignore_ascii_case(name))
    }

    /// vCard type values: `TYPE=` values plus bare 2.1 parameter names.
    pub fn type_values(&self) -> Vec<String> {
        const NON_TYPE: &[&str] = &[
            "TYPE",
            "VALUE",
            "ENCODING",
            "CHARSET",
            "PREF",
            "LANGUAGE",
            "ALTID",
            "PID",
            "LABEL",
            "MEDIATYPE",
            "CALSCALE",
            "SORT-AS",
            "GEO",
            "TZ",
            "INDEX",
            "LEVEL",
            "GROUP",
        ];
        let mut out = Vec::new();
        for (key, values) in &self.params {
            if key.eq_ignore_ascii_case("TYPE") {
                for value in values {
                    for part in value.split(',') {
                        if !part.trim().is_empty() {
                            out.push(part.trim().to_string());
                        }
                    }
                }
            } else if !NON_TYPE.iter().any(|n| n.eq_ignore_ascii_case(key)) {
                out.push(key.clone());
            }
        }
        out
    }
}

/// A component tree (`BEGIN:VEVENT … END:VEVENT`).
#[derive(Debug, Clone, Default)]
pub struct Component {
    pub name: String,
    pub props: Vec<ContentLine>,
    pub children: Vec<Component>,
}

impl Component {
    pub fn prop(&self, name: &str) -> Option<&ContentLine> {
        self.props
            .iter()
            .find(|line| line.name.eq_ignore_ascii_case(name))
    }

    pub fn props_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a ContentLine> + 'a {
        self.props
            .iter()
            .filter(move |line| line.name.eq_ignore_ascii_case(name))
    }

    pub fn value(&self, name: &str) -> Option<&str> {
        self.prop(name).map(|line| line.value.as_str())
    }
}

/// Unfold a document's physical lines.
///
/// Handles three things at once:
/// * CRLF / CR / LF line endings;
/// * standard folding (a continuation line begins with space or tab);
/// * quoted-printable soft line breaks (a line ends with `=`), which vCard 2.1
///   producers use without an indented continuation line.
pub fn unfold(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let mut line = raw.trim_end_matches('\r').to_string();
        if out.is_empty() {
            line = line.trim_start_matches('\u{feff}').to_string();
        }
        if let Some(last) = out.last_mut() {
            if line.starts_with(' ') || line.starts_with('\t') {
                let continuation = line[1..].to_string();
                if last.ends_with('=') && is_qp(last) {
                    last.pop();
                }
                last.push_str(&continuation);
                continue;
            }
            if last.ends_with('=') && is_qp(last) {
                last.pop();
                last.push_str(&line);
                continue;
            }
        }
        out.push(line);
    }
    out.retain(|line| !line.trim().is_empty());
    out
}

fn is_qp(line: &str) -> bool {
    let head = line.split_once(':').map(|(head, _)| head).unwrap_or(line);
    head.to_ascii_uppercase().contains("QUOTED-PRINTABLE")
}

/// Parse a document into a forest of components. Top-level lines that are not
/// inside `BEGIN`/`END` are collected in a synthetic component named `""`.
pub fn parse_components(text: &str) -> Vec<Component> {
    let lines = unfold(text);
    let mut roots: Vec<Component> = Vec::new();
    let mut stack: Vec<Component> = Vec::new();

    for line in lines {
        let Some(parsed) = ContentLine::parse(&line) else {
            continue;
        };
        if parsed.name.eq_ignore_ascii_case("BEGIN") {
            let name = parsed.value.trim().to_ascii_uppercase();
            stack.push(Component {
                name,
                props: Vec::new(),
                children: Vec::new(),
            });
            continue;
        }
        if parsed.name.eq_ignore_ascii_case("END") {
            let name = parsed.value.trim().to_ascii_uppercase();
            if let Some(component) = stack.pop() {
                if !component.name.eq_ignore_ascii_case(&name) {
                    tracing::debug!(
                        expected = %component.name,
                        found = %name,
                        "mismatched END in iCalendar/vCard data"
                    );
                }
                attach(&mut stack, &mut roots, component);
            }
            continue;
        }
        if let Some(current) = stack.last_mut() {
            current.props.push(parsed);
        } else {
            // Loose lines before BEGIN: keep them in a synthetic root.
            if roots.is_empty() || roots.last().map(|c| !c.name.is_empty()).unwrap_or(true) {
                roots.push(Component::default());
            }
            if let Some(root) = roots.last_mut() {
                root.props.push(parsed);
            }
        }
    }

    // Close unterminated components rather than dropping them.
    while let Some(component) = stack.pop() {
        tracing::debug!(name = %component.name, "unterminated component in input");
        attach(&mut stack, &mut roots, component);
    }

    roots
}

fn attach(stack: &mut [Component], roots: &mut Vec<Component>, component: Component) {
    if let Some(parent) = stack.last_mut() {
        parent.children.push(component);
    } else {
        roots.push(component);
    }
}

fn find_unquoted(value: &str, needle: char) -> Option<usize> {
    let mut quoted = false;
    let mut escaped = false;
    for (idx, ch) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            c if c == needle && !quoted => return Some(idx),
            _ => {}
        }
    }
    None
}

fn split_unquoted(value: &str, delimiter: char) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for ch in value.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if quoted => {
                current.push(ch);
                escaped = true;
            }
            '"' => {
                quoted = !quoted;
                current.push(ch);
            }
            c if c == delimiter && !quoted => {
                parts.push(std::mem::take(&mut current));
            }
            c => current.push(c),
        }
    }
    parts.push(current);
    parts
}

fn unquote_param(value: &str) -> String {
    let trimmed = value.trim();
    let unquoted = if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };
    let mut out = String::with_capacity(unquoted.len());
    let mut chars = unquoted.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(next) = chars.next() {
                if next == '"' || next == '\\' {
                    out.push(next);
                } else {
                    out.push('\\');
                    out.push(next);
                }
            } else {
                out.push('\\');
            }
        } else {
            out.push(ch);
        }
    }
    decode_caret(&out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_params_and_quoted_colons() {
        let line = ContentLine::parse(
            r#"ATTENDEE;CN="Doe, John";ROLE=REQ-PARTICIPANT;PARTSTAT=ACCEPTED:mailto:john@example.com"#,
        )
        .unwrap();
        assert_eq!(line.name, "ATTENDEE");
        assert_eq!(line.param("CN"), Some("Doe, John"));
        assert_eq!(line.param("ROLE"), Some("REQ-PARTICIPANT"));
        assert_eq!(line.value, "mailto:john@example.com");
    }

    #[test]
    fn unfolds_and_joins_qp() {
        let text = "SUMMARY:Hello\r\n World\r\nNOTE;ENCODING=QUOTED-PRINTABLE:=E4=\r\n=BD=A0\r\n";
        let lines = unfold(text);
        assert_eq!(lines[0], "SUMMARY:HelloWorld");
        assert_eq!(lines[1], "NOTE;ENCODING=QUOTED-PRINTABLE:=E4=BD=A0");
    }

    #[test]
    fn parses_component_tree() {
        let text = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:1\nBEGIN:VALARM\nTRIGGER:-PT15M\nEND:VALARM\nEND:VEVENT\nEND:VCALENDAR";
        let roots = parse_components(text);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].name, "VCALENDAR");
        assert_eq!(roots[0].children[0].name, "VEVENT");
        assert_eq!(roots[0].children[0].children[0].name, "VALARM");
    }

    #[test]
    fn vcard21_bare_type_params() {
        let line = ContentLine::parse("TEL;HOME;VOICE:+1 555 0100").unwrap();
        assert!(line.has_param("HOME"));
        assert!(line.has_param("VOICE"));
        assert_eq!(line.value, "+1 555 0100");
    }
}
