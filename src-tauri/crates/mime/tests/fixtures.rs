//! Fixture-driven integration tests for the public `nuncio-mime` API.

use base64::Engine;
use encoding_rs::GBK;
use nuncio_core::models::{Importance, ReplyMode};
use nuncio_mime::{
    build_message, build_reply, extract_part, format_address_list, html_to_text, parse,
    parse_headers_only, render_document, sanitize_fragment, OutgoingMessage, RenderInput,
};

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[test]
fn gbk_encoded_subject_body_and_display_name() {
    let subject = b64(GBK.encode("中文邮件测试").0.as_ref());
    let body = b64(GBK.encode("这是 GBK 编码的正文。\r\n第二行。").0.as_ref());
    let sender = b64(GBK.encode("张三").0.as_ref());
    let raw = format!(
        "From: =?GBK?B?{sender}?= <zhangsan@example.cn>\r\nTo: me@example.com\r\nSubject: =?GBK?B?{subject}?=\r\nDate: Tue, 12 Nov 2026 10:00:00 +0800\r\nMessage-ID: <gbk@example.cn>\r\nContent-Type: text/plain; charset=GBK\r\nContent-Transfer-Encoding: base64\r\n\r\n{body}"
    );
    let parsed = parse(raw.as_bytes()).expect("GBK message should parse");
    assert_eq!(parsed.subject(), "中文邮件测试");
    assert_eq!(
        parsed.from().and_then(|address| address.name.as_deref()),
        Some("张三")
    );
    let text = parsed.text.as_deref().expect("text body");
    assert!(text.contains("这是 GBK 编码的正文。"));
    assert!(text.contains("第二行。"));
}

#[test]
fn quoted_printable_and_base64_variants() {
    let quoted_printable = "From: a@b.com\r\nSubject: =?UTF-8?Q?=E4=BD=A0=E5=A5=BD?=\r\nContent-Type: text/plain; charset=UTF-8\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\n=E4=BD=A0=E5=A5=BD=EF=BC=8C=E4=B8=96=E7=95=8C\r\n";
    let parsed = parse(quoted_printable.as_bytes()).expect("QP message should parse");
    assert_eq!(parsed.subject(), "你好");
    assert!(parsed.text.as_deref().unwrap_or("").contains("你好，世界"));

    let base64_message = "From: a@b.com\r\nSubject: base64\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n5L2g5aW977yM5LiW55WM\r\n";
    let parsed = parse(base64_message.as_bytes()).expect("base64 message should parse");
    assert!(parsed.text.as_deref().unwrap_or("").contains("你好，世界"));
}

#[test]
fn rfc2231_filename_and_continuations() {
    let raw = "\
From: a@b.com
To: c@d.com
Subject: attachments
MIME-Version: 1.0
Content-Type: multipart/mixed; boundary=\"outer\"

--outer
Content-Type: text/plain; charset=utf-8

see attachments
--outer
Content-Type: application/pdf
Content-Transfer-Encoding: base64
Content-Disposition: attachment; filename*=UTF-8''%E4%B8%AD%E6%96%87%E6%8A%A5%E5%91%8A.pdf

JVBERi0xLjQK
--outer
Content-Type: text/plain
Content-Disposition: attachment; filename*0*=UTF-8''%E8%BF%9E; filename*1*=%E7%BB%AD.txt

continuation
--outer--
"
    .replace('\n', "\r\n");
    let parsed = parse(raw.as_bytes()).expect("RFC2231 message should parse");
    assert_eq!(parsed.attachments.len(), 2);
    assert_eq!(parsed.attachments[0].filename, "中文报告.pdf");
    assert_eq!(parsed.attachments[1].filename, "连续.txt");
    assert_eq!(parsed.attachments[0].mime_type, "application/pdf");
}

#[test]
fn nested_mixed_related_alternative_with_cid_image() {
    let png = b64(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0]);
    let raw = format!(
        "From: a@b.com\r\nTo: c@d.com\r\nSubject: nested\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"outer\"\r\n\r\n\
--outer\r\nContent-Type: multipart/related; boundary=\"rel\"; type=\"text/html\"\r\n\r\n\
--rel\r\nContent-Type: multipart/alternative; boundary=\"alt\"\r\n\r\n\
--alt\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nplain body\r\n\
--alt\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<html><body><p>Hello <img src=\"cid:img1@example.com\"></p></body></html>\r\n\
--alt--\r\n\
--rel\r\nContent-Type: image/png\r\nContent-Transfer-Encoding: base64\r\nContent-ID: <img1@example.com>\r\nContent-Disposition: inline; filename=\"pixel.png\"\r\n\r\n{png}\r\n\
--rel--\r\n\
--outer\r\nContent-Type: application/pdf\r\nContent-Transfer-Encoding: base64\r\nContent-Disposition: attachment; filename=\"doc.pdf\"\r\n\r\nJVBERi0xLjQK\r\n\
--outer--\r\n"
    );
    let parsed = parse(raw.as_bytes()).expect("nested message should parse");
    assert!(parsed
        .html
        .as_deref()
        .unwrap_or("")
        .contains("cid:img1@example.com"));
    assert_eq!(parsed.text.as_deref(), Some("plain body"));
    assert_eq!(parsed.attachments.len(), 2);

    let inline = parsed
        .attachments
        .iter()
        .find(|attachment| attachment.is_inline)
        .expect("inline image");
    assert_eq!(inline.mime_type, "image/png");
    assert_eq!(inline.content_id.as_deref(), Some("img1@example.com"));
    assert_eq!(inline.part.as_string(), "1.2");
    let bytes = extract_part(raw.as_bytes(), &inline.part).expect("extract inline image");
    assert!(bytes.starts_with(&[0x89, b'P', b'N', b'G']));

    let pdf = parsed
        .attachments
        .iter()
        .find(|attachment| !attachment.is_inline)
        .expect("pdf attachment");
    assert_eq!(pdf.filename, "doc.pdf");
    assert_eq!(pdf.part.as_string(), "2");

    let rendered = render_document(RenderInput {
        html: parsed.html.clone(),
        text: parsed.text.clone(),
        message_id: "m1".to_string(),
        allow_remote: false,
        cid_base: "nuncio://localhost/".to_string(),
        light_background: false,
    });
    assert!(rendered
        .document
        .contains("nuncio://localhost/cid/m1/img1%40example.com"));
}

#[test]
fn calendar_part_is_exposed_with_method() {
    let raw = "From: a@b.com\r\nTo: c@d.com\r\nSubject: invite\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"outer\"\r\n\r\n--outer\r\nContent-Type: text/plain\r\n\r\nSee invitation\r\n--outer\r\nContent-Type: text/calendar; charset=utf-8; method=REQUEST\r\n\r\nBEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:evt-1\r\nSUMMARY:Meeting\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n--outer--\r\n";
    let parsed = parse(raw.as_bytes()).expect("calendar message should parse");
    assert_eq!(parsed.calendar_parts.len(), 1);
    assert_eq!(parsed.calendar_parts[0].method, "REQUEST");
    assert!(parsed.calendar_parts[0].text.contains("BEGIN:VCALENDAR"));
    assert!(
        parsed.attachments.is_empty(),
        "calendar parts are not regular attachments"
    );
    let ics = extract_part(raw.as_bytes(), &parsed.calendar_parts[0].part).expect("extract ICS");
    assert!(String::from_utf8_lossy(&ics).contains("SUMMARY:Meeting"));
}

#[test]
fn calendar_method_falls_back_to_ics_content() {
    let raw = "From: a@b.com\r\nSubject: invite\r\nMIME-Version: 1.0\r\nContent-Type: text/calendar; charset=utf-8\r\n\r\nBEGIN:VCALENDAR\r\nMETHOD:CANCEL\r\nEND:VCALENDAR\r\n";
    let parsed = parse(raw.as_bytes()).expect("calendar message should parse");
    assert_eq!(parsed.calendar_parts.len(), 1);
    assert_eq!(parsed.calendar_parts[0].method, "CANCEL");
}

#[test]
fn text_only_message() {
    let raw = b"From: a@b.com\r\nTo: c@d.com\r\nSubject: plain\r\nContent-Type: text/plain; charset=us-ascii\r\n\r\njust text\r\n";
    let parsed = parse(raw).expect("text message should parse");
    assert_eq!(
        parsed.html, None,
        "text-only messages must not synthesize HTML"
    );
    assert_eq!(parsed.text.as_deref(), Some("just text"));
    assert!(parsed.attachments.is_empty());
    assert_eq!(parsed.preview, "just text");
}

#[test]
fn malformed_boundaries_do_not_panic() {
    let raw = b"From: a@b.com\r\nSubject: broken\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"missing\"\r\n\r\n--missing\r\nContent-Type: text/plain\r\n\r\nHello despite the broken boundary\r\n";
    let parsed = parse(raw).expect("malformed message should still parse");
    let text = parsed.text.as_deref().unwrap_or("");
    assert!(
        text.contains("Hello despite the broken boundary"),
        "salvaged text: {text:?}"
    );
    assert_eq!(parsed.size as usize, raw.len());
}

#[test]
fn headers_only_decoding() {
    let gbk_name = b64(GBK.encode("张三").0.as_ref());
    let raw = format!("From: =?GBK?B?{gbk_name}?= <zhangsan@example.cn>\r\nTo: \"Doe, John\" <john@example.com>, 张三 <zhang@example.cn>\r\nCc: cc@example.com\r\nReply-To: reply@example.com\r\nSubject: =?UTF-8?B?5L2g5aW9?=\r\nDate: Tue, 12 Nov 2026 10:00:00 +0800\r\nMessage-ID: <id@example.com>\r\nIn-Reply-To: <parent@example.com>\r\nReferences: <root@example.com> <parent@example.com>\r\nImportance: High\r\nList-Unsubscribe: <mailto:unsub@example.com>\r\nDisposition-Notification-To: read@example.com\r\n");
    let headers = parse_headers_only(raw.as_bytes()).expect("headers should parse");
    assert_eq!(headers.subject, "你好");
    assert_eq!(
        headers.from.and_then(|address| address.name),
        Some("张三".to_string())
    );
    assert_eq!(headers.to.len(), 2);
    assert_eq!(headers.to[0].name.as_deref(), Some("Doe, John"));
    assert_eq!(headers.cc[0].email, "cc@example.com");
    assert_eq!(headers.reply_to[0].email, "reply@example.com");
    assert_eq!(headers.importance, Importance::High);
    assert_eq!(
        headers.list_unsubscribe.as_deref(),
        Some("mailto:unsub@example.com")
    );
    assert_eq!(headers.message_id.as_deref(), Some("id@example.com"));
    assert_eq!(headers.in_reply_to.as_deref(), Some("parent@example.com"));
    assert_eq!(
        headers.references,
        vec!["root@example.com", "parent@example.com"]
    );
    assert_eq!(
        headers.date.map(|date| date.to_rfc3339()),
        Some("2026-11-12T02:00:00+00:00".to_string())
    );
}

#[test]
fn xss_corpus_is_neutralized() {
    let html = r##"
        <script>alert(1)</script>
        <img src="https://evil.example/x.png" onerror="alert(2)">
        <a href="javascript:alert(3)">x</a>
        <svg onload="alert(4)"><script>alert(5)</script></svg>
        <p style="background: url(https://evil.example/bg.png); behavior: url(x.htc); color: #123456">p</p>
        <meta http-equiv="refresh" content="0;url=https://evil.example">
        <iframe src="https://evil.example"></iframe>
        <form action="https://evil.example"><input autofocus onfocus="alert(6)"></form>
        <object data="https://evil.example"></object>
        <a href="vbscript:msgbox(7)">v</a>
        <table bgcolor="#ffffff"><tr><td style="padding: 4px">cell</td></tr></table>
    "##;
    let document = sanitize_fragment(html).to_ascii_lowercase();
    assert!(!document.contains("alert("));
    assert!(!document.contains("<script"));
    assert!(!document.contains("<iframe"));
    assert!(!document.contains("<form"));
    assert!(!document.contains("<object"));
    assert!(!document.contains("javascript:"));
    assert!(!document.contains("vbscript:"));
    assert!(!document.contains("onerror"));
    assert!(!document.contains("onload"));
    assert!(!document.contains("onfocus"));
    assert!(!document.contains("behavior:"));
    assert!(!document.contains("http-equiv"));
    assert!(document.contains("<table"));
    assert!(document.contains("padding:") && document.contains("4px"));
    assert!(document.contains("color:") && document.contains("#123456"));
}

#[test]
fn remote_content_blocking_and_allow() {
    let html = r#"<p>Hi</p><img src="https://cdn.example/a.png"><div style="background-image:url('https://cdn.example/bg.png')">x</div>"#;
    let blocked = render_document(RenderInput {
        html: Some(html.to_string()),
        text: None,
        message_id: "m".to_string(),
        allow_remote: false,
        cid_base: "nuncio://localhost/".to_string(),
        light_background: false,
    });
    assert!(blocked.has_remote_content);
    assert!(blocked
        .document
        .contains("data-src=\"https://cdn.example/a.png\""));
    assert!(!blocked.document.contains("<img src=\"https://cdn.example"));
    assert!(blocked.document.contains("background-image:none"));

    let allowed = render_document(RenderInput {
        html: Some(html.to_string()),
        text: None,
        message_id: "m".to_string(),
        allow_remote: true,
        cid_base: "http://nuncio.localhost/".to_string(),
        light_background: true,
    });
    assert!(allowed.has_remote_content);
    assert!(allowed
        .document
        .contains("src=\"https://cdn.example/a.png\""));
    assert!(allowed.document.contains("https:"));
    assert!(allowed.document.contains("color-scheme:light"));
}

#[test]
fn reply_and_build_round_trip() {
    let raw = b"From: Alice <alice@example.com>\r\nTo: Me <me@example.com>\r\nCc: Bob <bob@example.com>\r\nSubject: Report\r\nMessage-ID: <report@example.com>\r\nReferences: <thread@example.com>\r\nDate: Tue, 12 Nov 2026 10:00:00 +0800\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>Original <b>message</b></p>";
    let parsed = parse(raw).expect("source message should parse");
    let reply = build_reply(
        &parsed,
        ReplyMode::ReplyAll,
        &["me@example.com".to_string()],
        "zh-CN",
        true,
    );
    assert_eq!(reply.to[0].email, "alice@example.com");
    assert_eq!(format_address_list(&reply.cc), "Bob <bob@example.com>");
    assert_eq!(reply.subject, "Re: Report");
    assert!(reply.html.contains("nuncio-quote-header"));

    let mut message = OutgoingMessage::new(
        nuncio_core::models::Address::new("me@example.com"),
        reply.subject.clone(),
        reply.html.clone(),
    );
    message.to = reply.to;
    message.cc = reply.cc;
    message.in_reply_to = reply.in_reply_to;
    message.references = reply.references;
    let (built, message_id) = build_message(message).expect("build reply");
    let reparsed = parse(&built).expect("rebuilt reply should parse");
    assert_eq!(reparsed.subject(), "Re: Report");
    assert_eq!(
        reparsed.headers.message_id.as_deref(),
        Some(message_id.as_str())
    );
    assert_eq!(
        reparsed.headers.in_reply_to.as_deref(),
        Some("report@example.com")
    );
    assert_eq!(
        reparsed.headers.references,
        vec!["thread@example.com", "report@example.com"]
    );
    assert!(reparsed
        .html
        .as_deref()
        .unwrap_or("")
        .contains("<blockquote"));
}

#[test]
fn html_to_text_is_decorated_free() {
    let text = html_to_text("<p>Hello <b>world</b></p><ul><li>one</li><li>two</li></ul>");
    assert!(text.contains("Hello world"));
    assert!(!text.contains("**"));
    assert!(text.contains("one"));
    assert!(text.contains("two"));
}
