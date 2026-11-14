//! Safe rendering of e-mail HTML for the reader iframe.
//!
//! The sanitizer follows ARCHITECTURE §7: an ammonia allow-list that keeps
//! tables and filtered inline styles, strips active content, blocks remote
//! resources unless explicitly allowed, rewrites `cid:` URLs to the backend
//! protocol, and wraps the fragment in a complete document with a CSP.

use std::collections::HashSet;

use ammonia::{Builder, UrlRelative};
use regex::{Captures, Regex};

use crate::util::{escape_attr, percent_encode, strip_angle_brackets};

/// Placeholder scheme used to survive ammonia's URL-scheme allow-list while
/// only permitting `data:image/*` data URLs.
const DATA_IMAGE_SCHEME: &str = "nuncio-img-data";
const DATA_IMAGE_PREFIX: &str = "nuncio-img-data:image/";
const BLANK_IMAGE: &str = "data:image/gif;base64,R0lGODlhAQABAIAAAAAAAP///ywAAAAAAQABAAACAUwAOw==";

/// Input for [`render_document`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderInput {
    pub html: Option<String>,
    pub text: Option<String>,
    pub message_id: String,
    pub allow_remote: bool,
    pub cid_base: String,
    pub light_background: bool,
}

/// Result of [`render_document`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedHtml {
    /// Complete HTML document for `<iframe sandbox srcdoc>`.
    pub document: String,
    /// The message references remote images, backgrounds or CSS `url()`s.
    pub has_remote_content: bool,
}

impl RenderedHtml {
    pub fn is_empty(&self) -> bool {
        self.document.is_empty()
    }
}

/// Sanitize and wrap an HTML or plain-text body into a complete document.
pub fn render_document(input: RenderInput) -> RenderedHtml {
    let raw_html = match input.html {
        Some(html) if !html.trim().is_empty() => html,
        _ => crate::text::plain_text_to_html(input.text.as_deref().unwrap_or("")),
    };

    let has_remote_content = has_remote_content(&raw_html);
    let scrubbed = scrub_style_attributes(&raw_html);
    let mut body = sanitize_fragment(&scrubbed);
    if !input.allow_remote {
        body = block_remote_resources(&body);
    }
    body = rewrite_cid_urls(&body, &input.message_id, &input.cid_base);

    let has_own_background = message_sets_background(&body);
    let document = wrap_document(
        &body,
        &input.cid_base,
        input.allow_remote,
        input.light_background,
        has_own_background,
    );

    RenderedHtml {
        document,
        has_remote_content,
    }
}

/// Sanitize an HTML fragment with the same allow-list used for rendering.
///
/// This is the entry point for signature HTML (write path and backup import):
/// active content (`script`, event handlers, forms, iframes, `javascript:`) is
/// removed while basic formatting, links and images are kept — including
/// remote `https:` images and `data:image/*` URLs.  Remote resources are kept
/// intact; use [`render_document`] for content that will be displayed in the
/// reader iframe with the reader's remote-content policy.
pub fn sanitize_fragment(html: &str) -> String {
    let protected = protect_data_images(&scrub_style_attributes(html));
    let sanitizer = build_sanitizer();
    let cleaned = sanitizer.clean(&protected).to_string();
    restore_data_images(&cleaned)
}

/// True when the HTML references remote images, backgrounds or CSS `url()`s.
pub fn has_remote_content(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    remote_attr_pattern().is_match(html)
        || remote_css_pattern().is_match(html)
        || lower.contains("<link") && remote_link_pattern().is_match(html)
}

/// Build a `cid:` resource URL served by the app crate.
pub fn cid_resource_url(cid_base: &str, message_id: &str, content_id: &str) -> String {
    let base = if cid_base.trim().is_empty() {
        "nuncio://localhost/"
    } else {
        cid_base
    };
    let base = base.trim_end_matches('/');
    let message_id = if message_id.trim().is_empty() {
        "message"
    } else {
        message_id.trim()
    };
    let content_id = strip_angle_brackets(content_id);
    let content_id = content_id.strip_prefix("cid:").unwrap_or(&content_id);
    format!(
        "{base}/cid/{}/{}",
        percent_encode(message_id),
        percent_encode(content_id)
    )
}

fn build_sanitizer() -> Builder<'static> {
    let mut builder = Builder::new();
    builder
        .add_tags(&[
            "font", "big", "dir", "menu", "nobr", "table", "thead", "tbody", "tfoot", "tr", "td",
            "th", "colgroup", "col", "caption",
        ])
        .add_generic_attributes(&[
            "lang", "title", "dir", "align", "valign", "style", "width", "height", "bgcolor",
        ])
        .add_tag_attributes("a", &["href", "hreflang", "name", "target", "title"])
        .add_tag_attributes(
            "img",
            &[
                "src", "alt", "width", "height", "align", "border", "hspace", "vspace", "title",
            ],
        )
        .add_tag_attributes(
            "table",
            &[
                "border",
                "cellpadding",
                "cellspacing",
                "summary",
                "frame",
                "rules",
            ],
        )
        .add_tag_attributes("font", &["color", "face", "size"])
        .add_tag_attributes("hr", &["size", "noshade"])
        .add_tag_attributes("ol", &["start", "type"])
        .add_tag_attributes("ul", &["type"])
        .add_tag_attributes("blockquote", &["cite"])
        .add_tag_attributes("col", &["span"])
        .add_tag_attributes("colgroup", &["span"])
        .add_url_schemes(&["cid", DATA_IMAGE_SCHEME, "webcal"])
        .url_relative(UrlRelative::Deny)
        .link_rel(Some("noopener noreferrer"))
        .set_tag_attribute_value("a", "target", "_blank")
        .filter_style_properties(safe_style_properties());
    builder
}

fn safe_style_properties() -> HashSet<&'static str> {
    [
        "align-content",
        "align-items",
        "align-self",
        "background",
        "background-color",
        "background-image",
        "background-position",
        "background-repeat",
        "background-size",
        "border",
        "border-bottom",
        "border-bottom-color",
        "border-bottom-style",
        "border-bottom-width",
        "border-collapse",
        "border-color",
        "border-left",
        "border-left-color",
        "border-left-style",
        "border-left-width",
        "border-radius",
        "border-right",
        "border-right-color",
        "border-right-style",
        "border-right-width",
        "border-spacing",
        "border-style",
        "border-top",
        "border-top-color",
        "border-top-style",
        "border-top-width",
        "border-width",
        "box-sizing",
        "caption-side",
        "clear",
        "color",
        "direction",
        "display",
        "float",
        "font",
        "font-family",
        "font-size",
        "font-style",
        "font-variant",
        "font-weight",
        "height",
        "letter-spacing",
        "line-height",
        "list-style",
        "list-style-image",
        "list-style-position",
        "list-style-type",
        "margin",
        "margin-bottom",
        "margin-left",
        "margin-right",
        "margin-top",
        "max-height",
        "max-width",
        "min-height",
        "min-width",
        "opacity",
        "overflow",
        "overflow-wrap",
        "overflow-x",
        "overflow-y",
        "padding",
        "padding-bottom",
        "padding-left",
        "padding-right",
        "padding-top",
        "table-layout",
        "text-align",
        "text-decoration",
        "text-decoration-color",
        "text-indent",
        "text-transform",
        "vertical-align",
        "visibility",
        "white-space",
        "width",
        "word-break",
        "word-spacing",
        "word-wrap",
    ]
    .into_iter()
    .collect()
}

fn style_attribute_pattern() -> &'static Regex {
    static PATTERN: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r#"(?is)(\bstyle\s*=\s*)("([^"]*)"|'([^']*)'|([^\s>]+))"#)
            .unwrap_or_else(|_| unreachable!("static style regex"))
    })
}

fn scrub_style_attributes(html: &str) -> String {
    style_attribute_pattern()
        .replace_all(html, |captures: &Captures<'_>| {
            let prefix = captures
                .get(1)
                .map(|value| value.as_str())
                .unwrap_or("style=");
            let (value, quote) = if let Some(value) = captures.get(3) {
                (value.as_str(), "\"")
            } else if let Some(value) = captures.get(4) {
                (value.as_str(), "'")
            } else {
                (
                    captures.get(5).map(|value| value.as_str()).unwrap_or(""),
                    "\"",
                )
            };
            format!("{prefix}{quote}{}{quote}", filter_css_declarations(value))
        })
        .into_owned()
}

fn filter_css_declarations(value: &str) -> String {
    split_css_declarations(value)
        .into_iter()
        .filter_map(|declaration| {
            let declaration = declaration.trim();
            if declaration.is_empty() {
                return None;
            }
            let lower = declaration.to_ascii_lowercase();
            let unsafe_css = lower.contains("expression")
                || lower.contains("javascript:")
                || lower.contains("vbscript:")
                || lower.contains("-moz-binding")
                || lower.contains("behavior:")
                || lower.contains("@import");
            (!unsafe_css).then(|| declaration.to_string())
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Split a declaration list on semicolons that are outside quotes and
/// parentheses, so `url(data:image/png;base64,...)` stays intact.
fn split_css_declarations(value: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let mut start = 0usize;
    let mut depth = 0usize;
    let mut in_quote: Option<char> = None;
    let mut escaped = false;
    for (index, character) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if let Some(quote) = in_quote {
            if character == '\\' {
                escaped = true;
            } else if character == quote {
                in_quote = None;
            }
            continue;
        }
        match character {
            '"' | '\'' => in_quote = Some(character),
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ';' if depth == 0 => {
                result.push(&value[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    result.push(&value[start..]);
    result
}

fn data_image_pattern() -> &'static Regex {
    static PATTERN: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"(?i)data:image/").unwrap_or_else(|_| unreachable!("static data image regex"))
    })
}

fn protect_data_images(html: &str) -> String {
    data_image_pattern()
        .replace_all(html, DATA_IMAGE_PREFIX)
        .into_owned()
}

fn restore_data_images(html: &str) -> String {
    html.replace(DATA_IMAGE_PREFIX, "data:image/")
        .replace(DATA_IMAGE_SCHEME, "data")
}

fn remote_attr_pattern() -> &'static Regex {
    static PATTERN: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r#"(?i)(?:^|\s)(?:src|background|poster)\s*=\s*(?:"\s*|'\s*)?(?:https?:)?//"#)
            .unwrap_or_else(|_| unreachable!("static remote attr regex"))
    })
}

fn remote_css_pattern() -> &'static Regex {
    static PATTERN: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r#"(?i)url\(\s*(?:"|')?\s*(?:https?:)?//"#)
            .unwrap_or_else(|_| unreachable!("static remote css regex"))
    })
}

fn remote_link_pattern() -> &'static Regex {
    static PATTERN: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r#"(?i)<link\b[^>]*\bhref\s*=\s*(?:"|')?\s*(?:https?:)?//"#)
            .unwrap_or_else(|_| unreachable!("static remote link regex"))
    })
}

fn remote_img_tag_pattern() -> &'static Regex {
    static PATTERN: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"(?is)<img\b[^>]*>").unwrap_or_else(|_| unreachable!("static img regex"))
    })
}

fn src_attribute_pattern() -> &'static Regex {
    static PATTERN: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r#"(?is)\bsrc\s*=\s*(?:"([^"]*)"|'([^']*)')"#)
            .unwrap_or_else(|_| unreachable!("static src regex"))
    })
}

fn css_url_pattern() -> &'static Regex {
    static PATTERN: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"(?is)url\(\s*([^)]*)\)").unwrap_or_else(|_| unreachable!("static url regex"))
    })
}

fn block_remote_resources(html: &str) -> String {
    let blocked_images = remote_img_tag_pattern()
        .replace_all(html, |captures: &Captures<'_>| {
            let tag = captures.get(0).map(|value| value.as_str()).unwrap_or("");
            block_remote_img(tag)
        })
        .into_owned();
    css_url_pattern()
        .replace_all(&blocked_images, |captures: &Captures<'_>| {
            let whole = captures.get(0).map(|value| value.as_str()).unwrap_or("");
            let inner = captures.get(1).map(|value| value.as_str()).unwrap_or("");
            if is_remote_url(inner) {
                "none".to_string()
            } else {
                whole.to_string()
            }
        })
        .into_owned()
}

fn block_remote_img(tag: &str) -> String {
    let remote_src = src_attribute_pattern().captures(tag).and_then(|captures| {
        let value = captures
            .get(1)
            .or_else(|| captures.get(2))
            .map(|value| value.as_str())?;
        is_remote_url(value).then(|| value.to_string())
    });
    let remote_src = match remote_src {
        Some(remote_src) => remote_src,
        None => return tag.to_string(),
    };
    let replaced = src_attribute_pattern()
        .replace(tag, |captures: &Captures<'_>| {
            let prefix = captures.get(0).map(|value| value.as_str()).unwrap_or("");
            let attribute_name = prefix.split('=').next().unwrap_or("src").trim();
            format!("{attribute_name}=\"{BLANK_IMAGE}\"")
        })
        .into_owned();
    let data_src = format!(" data-src=\"{}\"", escape_attr(&remote_src));
    match replaced.rfind('>') {
        Some(position) => {
            let mut output = String::with_capacity(replaced.len() + data_src.len());
            output.push_str(&replaced[..position]);
            output.push_str(&data_src);
            output.push_str(&replaced[position..]);
            output
        }
        None => replaced,
    }
}

fn is_remote_url(value: &str) -> bool {
    let value = css_url_value(value);
    let lower = value.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("//")
}

/// Return the URL inside a CSS `url(...)` token, decoding the HTML entities
/// ammonia uses for quotes in style attributes.
fn css_url_value(value: &str) -> String {
    let mut value = value.trim().to_string();
    for _ in 0..2 {
        value = value
            .trim()
            .trim_matches(['"', '\''])
            .trim_start_matches("&quot;")
            .trim_end_matches("&quot;")
            .trim_start_matches("&#39;")
            .trim_end_matches("&#39;")
            .trim()
            .to_string();
    }
    value.trim_matches(['"', '\'']).trim().to_string()
}

fn cid_attr_pattern() -> &'static Regex {
    static PATTERN: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r#"(?is)(\b(?:src|href|background)\s*=\s*)("|')cid:([^"']*)("|')"#)
            .unwrap_or_else(|_| unreachable!("static cid attr regex"))
    })
}

fn rewrite_cid_urls(html: &str, message_id: &str, cid_base: &str) -> String {
    let rewritten = cid_attr_pattern()
        .replace_all(html, |captures: &Captures<'_>| {
            let prefix = captures
                .get(1)
                .map(|value| value.as_str())
                .unwrap_or("src=");
            let quote = captures.get(2).map(|value| value.as_str()).unwrap_or("\"");
            let content_id = captures.get(3).map(|value| value.as_str()).unwrap_or("");
            let url = cid_resource_url(cid_base, message_id, content_id);
            format!("{prefix}{quote}{url}{quote}")
        })
        .into_owned();
    css_url_pattern()
        .replace_all(&rewritten, |captures: &Captures<'_>| {
            let whole = captures.get(0).map(|value| value.as_str()).unwrap_or("");
            let inner = captures.get(1).map(|value| value.as_str()).unwrap_or("");
            let url = css_url_value(inner);
            if let Some(content_id) = url.strip_prefix("cid:") {
                format!(
                    "url(&quot;{}&quot;)",
                    cid_resource_url(cid_base, message_id, content_id)
                )
            } else {
                whole.to_string()
            }
        })
        .into_owned()
}

fn message_sets_background(html: &str) -> bool {
    static PATTERN: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let pattern = PATTERN.get_or_init(|| {
        Regex::new(r#"(?is)(bgcolor\s*=|style\s*=\s*"[^"]*\bbackground(?:-color|-image)?\s*:)"#)
            .unwrap_or_else(|_| unreachable!("static background regex"))
    });
    pattern.is_match(html)
}

fn wrap_document(
    body: &str,
    cid_base: &str,
    allow_remote: bool,
    light_background: bool,
    has_own_background: bool,
) -> String {
    let csp = content_security_policy(cid_base, allow_remote);
    let dark_css = if light_background {
        String::new()
    } else {
        let body_rule = if has_own_background {
            ""
        } else {
            "body{background:#1f1f1f;color:#e6e6e6;}"
        };
        format!(
            "@media (prefers-color-scheme: dark){{{body_rule}blockquote{{border-left-color:#555;color:#bdbdbd;}}a{{color:#6cb6ff;}}}}"
        )
    };
    let color_scheme = if light_background {
        "light"
    } else {
        "light dark"
    };
    let base_background = "#ffffff";
    format!(
        "<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\"><meta http-equiv=\"Content-Security-Policy\" content=\"{csp}\"><meta name=\"referrer\" content=\"no-referrer\"><base target=\"_blank\"><style>{base_css}</style><style>{dark_css}</style></head><body>{body}</body></html>",
        base_css = base_css(color_scheme, base_background)
    )
}

fn base_css(color_scheme: &str, background: &str) -> String {
    format!(
        ":root{{color-scheme:{color_scheme};}}html,body{{margin:0;padding:0;}}body{{font-family:-apple-system,BlinkMacSystemFont,\"Segoe UI Variable\",\"Segoe UI\",\"Microsoft YaHei UI\",\"Microsoft YaHei\",\"PingFang SC\",\"Hiragino Sans GB\",\"Noto Sans CJK SC\",Roboto,Helvetica,Arial,sans-serif;font-size:15px;line-height:1.5;color:#1a1a1a;background:{background};padding:12px 16px;word-wrap:break-word;overflow-wrap:anywhere;}}img{{max-width:100%!important;height:auto;}}table{{max-width:100%;border-collapse:collapse;}}td,th{{vertical-align:top;}}blockquote{{margin:0.6em 0;padding:0 0 0 12px;border-left:3px solid #c8c8c8;color:#555;}}a{{color:#0b6cbf;}}pre{{white-space:pre-wrap;word-wrap:break-word;}}code,pre{{font-family:ui-monospace,SFMono-Regular,Consolas,\"Liberation Mono\",monospace;}}.nuncio-plain{{white-space:pre-wrap;word-wrap:break-word;}}.nuncio-quote-header{{margin:0 0 8px 0;padding:8px 10px;border-left:3px solid #8a8a8a;background:rgba(127,127,127,0.08);font-size:13px;line-height:1.5;}}.nuncio-quote-header b{{font-weight:600;}}.nuncio-quote{{margin:0;padding:0 0 0 12px;border-left:3px solid #c8c8c8;}}"
    )
}

fn content_security_policy(cid_base: &str, allow_remote: bool) -> String {
    let base = if cid_base.trim().is_empty() {
        "nuncio://localhost/"
    } else {
        cid_base
    };
    let base = base.trim_end_matches('/');
    let scheme = base.split(':').next().unwrap_or("nuncio");
    let mut sources = Vec::new();
    let origin = base.to_string();
    if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") {
        // An `http:` scheme source would whitelist the whole web; use the exact
        // local origin instead.
        if !origin.is_empty() {
            sources.push(origin);
        }
    } else {
        let scheme_source = format!("{scheme}:");
        if !sources.contains(&scheme_source) {
            sources.push(scheme_source);
        }
        if !origin.is_empty() && !sources.contains(&origin) {
            sources.push(origin);
        }
    }
    sources.push("data:".to_string());
    if allow_remote {
        sources.push("https:".to_string());
        sources.push("http:".to_string());
    }
    format!(
        "default-src 'none'; img-src {}; style-src 'unsafe-inline'; font-src data:;",
        sources.join(" ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(html: &str, allow_remote: bool) -> RenderedHtml {
        render_document(RenderInput {
            html: Some(html.to_string()),
            text: None,
            message_id: "msg-1".to_string(),
            allow_remote,
            cid_base: "nuncio://localhost/".to_string(),
            light_background: false,
        })
    }

    #[test]
    fn sanitizes_xss_corpus() {
        let html = r##"
            <script>alert(1)</script>
            <img src="x" onerror="alert(2)">
            <a href="javascript:alert(3)">bad</a>
            <svg onload="alert(4)"></svg>
            <p style="width: expression(alert(5)); color: red">styled</p>
            <meta http-equiv="refresh" content="0;url=https://evil.example">
            <iframe src="https://evil.example"></iframe>
            <form action="https://evil.example"><input name="x"></form>
            <a href="data:text/html,<script>alert(6)</script>">data</a>
            <img src="data:image/png;base64,iVBORw0KGgo=">
        "##;
        let rendered = render(html, false);
        let document = rendered.document.to_ascii_lowercase();
        assert!(!document.contains("alert("));
        assert!(!document.contains("<script"));
        assert!(!document.contains("<iframe"));
        assert!(!document.contains("<form"));
        assert!(!document.contains("onerror"));
        assert!(!document.contains("onload"));
        assert!(!document.contains("expression("));
        assert!(!document.contains("javascript:"));
        assert!(!document.contains("http-equiv=\"refresh\""));
        assert!(!document.contains("data:text/html"));
        assert!(document.contains("data:image/png;base64"));
        assert!(document.contains("content-security-policy"));
    }

    #[test]
    fn links_get_blank_target_and_rel() {
        let rendered = render(r#"<a href="https://example.com/x">link</a>"#, false);
        assert!(rendered.document.contains("target=\"_blank\""));
        assert!(rendered.document.contains("rel=\"noopener noreferrer\""));
        assert!(rendered.document.contains("https://example.com/x"));
    }

    #[test]
    fn blocks_remote_images_and_keeps_original_in_data_src() {
        let html = r#"<p>Hello</p><img src="https://tracker.example/pixel.gif" alt="x">"#;
        let rendered = render(html, false);
        assert!(rendered.has_remote_content);
        assert!(rendered
            .document
            .contains("data-src=\"https://tracker.example/pixel.gif\""));
        assert!(rendered.document.contains("data:image/gif;base64"));
        assert!(!rendered
            .document
            .contains("<img src=\"https://tracker.example"));
        assert!(!rendered.document.contains("https: http:"));
    }

    #[test]
    fn allows_remote_images_when_requested() {
        let html = r#"<img src="https://cdn.example/a.png">"#;
        let rendered = render(html, true);
        assert!(rendered.has_remote_content);
        assert!(rendered
            .document
            .contains("src=\"https://cdn.example/a.png\""));
        assert!(rendered.document.contains("https:"));
    }

    #[test]
    fn rewrites_cid_images() {
        let html = r#"<img src="cid:image001@example.com">"#;
        let rendered = render(html, false);
        assert!(rendered
            .document
            .contains("nuncio://localhost/cid/msg-1/image001%40example.com"));
    }

    #[test]
    fn converts_text_body() {
        let rendered = render_document(RenderInput {
            html: None,
            text: Some("Hello <world>\nhttps://example.com".to_string()),
            message_id: "m".to_string(),
            allow_remote: false,
            cid_base: "http://nuncio.localhost/".to_string(),
            light_background: true,
        });
        assert!(rendered.document.contains("Hello &lt;world&gt;"));
        assert!(rendered.document.contains("href=\"https://example.com\""));
        assert!(rendered.document.contains("color-scheme:light"));
    }

    #[test]
    fn escapes_quotes_in_cid_urls() {
        let url = cid_resource_url("nuncio://localhost/", "a b", "<x/y@z>");
        assert!(!url.contains(' '));
        assert!(!url.contains('<'));
    }

    #[test]
    fn sanitize_fragment_is_safe_for_signatures() {
        let html = r##"<p style="color:#123456">Regards,<br><b>Ada</b></p>
            <a href="https://example.com/">Example</a>
            <img src="https://cdn.example/logo.png" alt="logo">
            <img src="data:image/png;base64,iVBORw0KGgo=" alt="inline">
            <script>alert(1)</script>
            <iframe src="https://evil.example"></iframe>
            <form action="https://evil.example"><input name="x"></form>
            <a href="javascript:alert(2)" onclick="alert(3)">bad</a>"##;
        let cleaned = sanitize_fragment(html);
        let lower = cleaned.to_ascii_lowercase();
        assert!(!lower.contains("<script"));
        assert!(!lower.contains("<iframe"));
        assert!(!lower.contains("<form"));
        assert!(!lower.contains("javascript:"));
        assert!(!lower.contains("onclick"));
        assert!(cleaned.contains("href=\"https://example.com/\""));
        assert!(cleaned.contains("src=\"https://cdn.example/logo.png\""));
        assert!(cleaned.contains("data:image/png;base64"));
        assert!(cleaned.contains("<b>Ada</b>"));
        assert!(cleaned.contains("color"));
        assert!(cleaned.contains("#123456"));
    }
}
