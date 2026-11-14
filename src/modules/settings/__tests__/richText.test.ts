import { describe, expect, it } from "vitest";
import {
  htmlToPlainText,
  isSafeLinkHref,
  normalizeLinkHref,
  sanitizeSignatureHtml,
} from "../lib/richText";

describe("signature sanitization", () => {
  it("removes script and active markup", () => {
    const html =
      '<p onclick="steal()">Hello</p><script>alert(1)</script><iframe src="x"></iframe>';
    const safe = sanitizeSignatureHtml(html);
    expect(safe).not.toContain("script");
    expect(safe).not.toContain("iframe");
    expect(safe).not.toContain("onclick");
    expect(safe).toContain("Hello");
  });

  it("removes javascript: URLs and CSS expressions", () => {
    const html =
      '<a href="javascript:alert(1)">x</a><span style="background:expression(alert(1))">y</span>';
    const safe = sanitizeSignatureHtml(html);
    expect(safe).not.toContain("javascript:");
    expect(safe).not.toContain("expression(");
  });

  it("keeps ordinary formatting", () => {
    const html = '<p><strong>Regards,</strong><br><a href="https://example.com">Site</a></p>';
    expect(sanitizeSignatureHtml(html)).toBe(html);
  });

  it("drops event attributes attached without preceding whitespace", () => {
    // The HTML parser treats `alt="a"onerror="..."` as two attributes even
    // though there is no space, so a whitespace-based regex misses it.
    const safe = sanitizeSignatureHtml('<img src=x alt="a"onerror="alert(1)">');
    expect(safe).not.toContain("onerror");
    expect(safe).not.toContain("alert");
    expect(safe).toBe('<img alt="a">');
  });

  it("drops event handlers split by newlines or slashes", () => {
    expect(sanitizeSignatureHtml("<p\nonclick=alert(1)>x</p>")).toBe("<p>x</p>");
    expect(sanitizeSignatureHtml("<svg/onload=alert(1)>x</svg>")).toBe("");
  });

  it("decodes entity-encoded URL schemes before checking them", () => {
    const payloads = [
      '<a href="jav&#97;script:alert(1)">x</a>',
      '<a href="j&#x61;vascript&colon;alert(1)">x</a>',
      '<a href="java\tscript:alert(1)">x</a>',
      '<a href="data:text/html,<script>alert(1)</script>">x</a>',
      '<a href="mailto:me@example.com" onclick="steal()">mail</a>',
    ];
    for (const html of payloads) {
      const safe = sanitizeSignatureHtml(html);
      expect(safe).not.toMatch(/javascript|data:text|onclick|alert/i);
    }
    expect(sanitizeSignatureHtml('<a href="https://example.com/?a=1&amp;b=2">x</a>')).toBe(
      '<a href="https://example.com/?a=1&amp;b=2">x</a>',
    );
  });

  it("drops raw-content elements together with their bodies", () => {
    expect(sanitizeSignatureHtml("before<script>alert(1)")).toBe("before");
    expect(sanitizeSignatureHtml("<style>p{color:red}</style><p>ok</p>")).toBe("<p>ok</p>");
    expect(sanitizeSignatureHtml("<iframe src=x><p>hidden</p></iframe>visible")).toBe("visible");
  });

  it("filters style declarations against a property/value allowlist", () => {
    const safe = sanitizeSignatureHtml(
      '<span style="color: red; background: url(javascript:alert(1)); font-weight: bold">x</span>',
    );
    expect(safe).toBe('<span style="color: red; font-weight: bold">x</span>');
    const css = sanitizeSignatureHtml(
      '<span style="font-family: expression(alert(1)); width: 10px">x</span>',
    );
    expect(css).toBe('<span style="width: 10px">x</span>');
  });

  it("escapes literal angle brackets in text and preserves void tags", () => {
    expect(sanitizeSignatureHtml("<p>1 < 2</p><br/>")).toBe("<p>1 &lt; 2</p><br>");
  });
});

describe("link helpers", () => {
  it("allows only http(s), mailto, tel and bare emails", () => {
    expect(isSafeLinkHref("https://example.com")).toBe(true);
    expect(isSafeLinkHref("mailto:me@example.com")).toBe(true);
    expect(isSafeLinkHref("tel:+123")).toBe(true);
    expect(isSafeLinkHref("me@example.com")).toBe(true);
    expect(isSafeLinkHref("javascript:alert(1)")).toBe(false);
    expect(isSafeLinkHref("")).toBe(false);
  });

  it("normalizes bare domains and emails", () => {
    expect(normalizeLinkHref("example.com")).toBe("https://example.com");
    expect(normalizeLinkHref("me@example.com")).toBe("mailto:me@example.com");
    expect(normalizeLinkHref("https://example.com")).toBe("https://example.com");
  });
});

describe("plain text preview", () => {
  it("strips tags and entities", () => {
    expect(htmlToPlainText("<p>Hello&nbsp;<b>world</b></p><p>Line 2</p>")).toBe(
      "Hello world Line 2",
    );
  });
});
