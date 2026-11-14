import { describe, expect, it } from "vitest";
import type { DraftAttachment, Signature } from "@/ipc/types";
import {
  attachmentTotal,
  finalizeHtml,
  findSignatureForIdentity,
  invalidRecipients,
  mentionsAttachment,
  moveRecipient,
  normalizeLinkUrl,
  parseRecipients,
  prepareEditorHtml,
  serializeInlineImages,
  summarizeAttachments,
} from "./logic";

const inlineImage: DraftAttachment = {
  id: "a1",
  filename: "logo.png",
  mimeType: "image/png",
  size: 2048,
  path: "/tmp/staging/a1/logo.png",
  contentId: "logo@nuncio",
};

describe("recipient parsing", () => {
  it("splits a pasted list into valid addresses and rejects invalid ones", () => {
    const result = parseRecipients(
      "Ada Lovelace <ada@example.com>, bob@example.org; not-an-email\ncarl@example.net",
    );
    expect(result.valid.map((item) => item.email)).toEqual([
      "ada@example.com",
      "bob@example.org",
      "carl@example.net",
    ]);
    expect(result.valid[0].name).toBe("Ada Lovelace");
    expect(result.invalid).toEqual(["not-an-email"]);
  });

  it("de-duplicates case-insensitively", () => {
    const result = parseRecipients("a@example.com, A@Example.com");
    expect(result.valid).toHaveLength(1);
  });

  it("reports invalid recipients already in a field", () => {
    expect(
      invalidRecipients([
        { name: null, email: "ok@example.com" },
        { name: null, email: "broken@" },
        { name: null, email: "" },
      ]),
    ).toEqual(["broken@", ""]);
  });

  it("moves an address between fields without duplicating", () => {
    const address = { name: "Ada", email: "ada@example.com" };
    const moved = moveRecipient(
      {
        to: [address],
        cc: [{ name: null, email: "b@example.com" }],
        bcc: [],
      },
      "to",
      "cc",
      address,
    );
    expect(moved.to).toHaveLength(0);
    expect(moved.cc.map((item) => item.email)).toEqual([
      "b@example.com",
      "ada@example.com",
    ]);
  });
});

describe("attachment keyword detection", () => {
  it("detects Chinese and English attachment mentions", () => {
    expect(mentionsAttachment("详情请见附件。")).toBe(true);
    expect(mentionsAttachment("Please see the attached file")).toBe(true);
    expect(mentionsAttachment("I have attached the report")).toBe(true);
    expect(mentionsAttachment("enclosed is the invoice")).toBe(true);
    expect(mentionsAttachment("attachments: two files")).toBe(true);
  });

  it("does not fire on ordinary text", () => {
    expect(mentionsAttachment("Thanks, see you tomorrow.")).toBe(false);
    expect(mentionsAttachment("")).toBe(false);
  });

  it("computes the total attachment size", () => {
    expect(attachmentTotal([inlineImage])).toBe(2048);
    const summary = summarizeAttachments([
      { ...inlineImage, size: 20 * 1024 * 1024 },
      { ...inlineImage, id: "a2", size: 6 * 1024 * 1024 },
    ]);
    expect(summary.total).toBe(26 * 1024 * 1024);
    expect(summary.tooLarge).toBe(true);
  });
});

describe("inline image serialization", () => {
  it("writes data-content-id images as cid: sources", () => {
    const html =
      '<p>Hi</p><img src="data:image/png;base64,AAAA" data-content-id="logo@nuncio" alt="logo.png">';
    const serialized = serializeInlineImages(html);
    expect(serialized).toContain('src="cid:logo@nuncio"');
    expect(serialized).not.toContain("data-content-id");
    expect(serialized).toContain('alt="logo.png"');
  });

  it("is idempotent for already-serialized cid images", () => {
    const html = '<img src="cid:logo@nuncio" alt="logo.png">';
    expect(serializeInlineImages(html)).toBe(html);
  });

  it("restores stored cid images to editable placeholders and back", () => {
    const stored = '<p>Body</p><img src="cid:logo@nuncio" alt="logo.png">';
    const editable = prepareEditorHtml(stored, [inlineImage]);
    expect(editable).toContain('data-content-id="logo@nuncio"');
    expect(editable).toContain('src="data:image/svg+xml');
    expect(editable).toContain('alt="logo.png"');
    const roundTrip = serializeInlineImages(editable);
    expect(roundTrip).toContain('src="cid:logo@nuncio"');
    expect(roundTrip).not.toContain("data-content-id");
  });

  it("applies the default font and removes editor metadata", () => {
    const html =
      '<p contenteditable="true">Hello</p><div data-nuncio-signature="s1">-- Ada</div>';
    const finalized = finalizeHtml(html, {
      fontFamily: "Segoe UI",
      fontSize: 14,
    });
    expect(finalized).toBe(
      "<div style=\"font-family:'Segoe UI';font-size:14px;\"><p>Hello</p><div>-- Ada</div></div>",
    );
  });

  it("unwraps its own default-font wrapper when reopening a draft", () => {
    const stored =
      '<div style="font-family:\'Segoe UI\';font-size:14px;"><p>Hello</p></div>';
    expect(prepareEditorHtml(stored, [])).toBe("<p>Hello</p>");
  });

  it("leaves html untouched when no font is configured", () => {
    expect(finalizeHtml("<p>Hi</p>")).toBe("<p>Hi</p>");
  });
});

describe("link normalization", () => {
  it("adds https to bare hosts and keeps known schemes", () => {
    expect(normalizeLinkUrl("example.com")).toBe("https://example.com");
    expect(normalizeLinkUrl("https://example.com/a?b=1")).toBe(
      "https://example.com/a?b=1",
    );
    expect(normalizeLinkUrl("mailto:ada@example.com")).toBe("mailto:ada@example.com");
    expect(normalizeLinkUrl("tel:+123456")).toBe("tel:+123456");
  });

  it("rejects dangerous or empty values", () => {
    expect(normalizeLinkUrl("javascript:alert(1)")).toBeNull();
    expect(normalizeLinkUrl("data:text/html,<script>")).toBeNull();
    expect(normalizeLinkUrl("   ")).toBeNull();
  });
});

describe("signature selection", () => {
  const signatures: Signature[] = [
    {
      id: "new-global",
      accountId: null,
      name: "Global new",
      html: "<p>global new</p>",
      useForNew: true,
      useForReply: false,
    },
    {
      id: "reply-account",
      accountId: "acc1",
      name: "Account reply",
      html: "<p>reply</p>",
      useForNew: false,
      useForReply: true,
    },
    {
      id: "explicit",
      accountId: "acc1",
      name: "Explicit",
      html: "<p>explicit</p>",
      useForNew: false,
      useForReply: true,
    },
    {
      id: "foreign",
      accountId: "acc2",
      name: "Other account",
      html: "<p>other</p>",
      useForNew: true,
      useForReply: true,
    },
  ];

  it("prefers the identity's explicit signature", () => {
    expect(
      findSignatureForIdentity({ signatureId: "explicit" }, "acc1", signatures, false)?.id,
    ).toBe("explicit");
  });

  it("uses the account signature matching the new/reply flag", () => {
    expect(findSignatureForIdentity(null, "acc1", signatures, false)?.id).toBe(
      "new-global",
    );
    expect(findSignatureForIdentity(null, "acc1", signatures, true)?.id).toBe(
      "reply-account",
    );
  });

  it("ignores an explicit signature owned by another account", () => {
    expect(
      findSignatureForIdentity({ signatureId: "foreign" }, "acc1", signatures, false)?.id,
    ).toBe("new-global");
    expect(
      findSignatureForIdentity({ signatureId: "foreign" }, "acc2", signatures, false)?.id,
    ).toBe("foreign");
  });

  it("returns null when nothing matches", () => {
    expect(findSignatureForIdentity(null, "acc3", signatures, true)).toBeNull();
  });
});
