import { describe, expect, it } from "vitest";
import { normalizeExternalUrl } from "@/modules/calendar/openExternal";

describe("normalizeExternalUrl", () => {
  it("adds HTTPS to bare hosts, including www links", () => {
    expect(normalizeExternalUrl("www.example.com")).toBe("https://www.example.com/");
    expect(normalizeExternalUrl("example.com/path?q=1")).toBe(
      "https://example.com/path?q=1",
    );
    expect(normalizeExternalUrl("  www.example.com/meet  ")).toBe(
      "https://www.example.com/meet",
    );
  });

  it("accepts http, https and mailto", () => {
    expect(normalizeExternalUrl("https://example.com/join?id=1")).toBe(
      "https://example.com/join?id=1",
    );
    expect(normalizeExternalUrl("http://example.com")).toBe("http://example.com/");
    expect(normalizeExternalUrl("mailto:someone@example.com")).toBe(
      "mailto:someone@example.com",
    );
    expect(normalizeExternalUrl("MAILTO:someone@example.com")).toBe(
      "mailto:someone@example.com",
    );
  });

  it("rejects every other scheme and malformed input", () => {
    expect(normalizeExternalUrl("javascript:alert(1)")).toBeNull();
    expect(normalizeExternalUrl("JavaScript:alert(1)")).toBeNull();
    expect(normalizeExternalUrl("file:///etc/passwd")).toBeNull();
    expect(normalizeExternalUrl("data:text/html,<script>alert(1)</script>")).toBeNull();
    expect(normalizeExternalUrl("ftp://example.com")).toBeNull();
    expect(normalizeExternalUrl("tel:+15551234567")).toBeNull();
    expect(normalizeExternalUrl("mailto:")).toBeNull();
    expect(normalizeExternalUrl("https://")).toBeNull();
    expect(normalizeExternalUrl("")).toBeNull();
    expect(normalizeExternalUrl(null)).toBeNull();
    expect(normalizeExternalUrl(undefined)).toBeNull();
  });
});
