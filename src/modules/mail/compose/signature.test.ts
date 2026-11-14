import { describe, expect, it } from "vitest";
import { getSchema } from "@tiptap/core";
import type { Node as PMNode } from "@tiptap/pm/model";
import { EditorState } from "@tiptap/pm/state";
import type { Signature } from "@/ipc/types";
import { createComposeExtensions } from "./extensions";
import {
  SIGNATURE_CLASS,
  SIGNATURE_ID_ATTRIBUTE,
  SIGNATURE_NODE_NAME,
  buildSignatureTransaction,
  findSignatureBlockBounds,
  findSignatureBlocks,
  isSafeUrl,
  planSignatureEdit,
  sanitizeSignatureBlocksHtml,
  sanitizeSignatureHtml,
  sanitizeSignatureHtmlWithoutDom,
  sanitizeStyle,
  signatureBlockHtml,
  signatureInsertPosition,
} from "./signature";
import { escapeAttribute } from "./logic";

const tokenizer = (html: string) => sanitizeSignatureHtml(html, { mode: "tokenizer" });

const schema = getSchema(
  createComposeExtensions({ placeholder: "Write…", isLocked: () => false }),
);

function docFromJson(content: unknown[]): PMNode {
  return schema.nodeFromJSON({ type: "doc", content });
}

const paragraph = (text: string) => ({
  type: "paragraph",
  content: text ? [{ type: "text", text }] : [],
});

const signatureNode = (signatureId: string | null, text = "-- Ada") => ({
  type: SIGNATURE_NODE_NAME,
  attrs: { signatureId },
  content: [paragraph(text)],
});

const signature = (overrides: Partial<Signature> = {}): Signature => ({
  id: "sig-1",
  accountId: "acc-1",
  name: "Work",
  html: "<p>Ada Lovelace</p><p>Analyst</p>",
  useForNew: true,
  useForReply: true,
  ...overrides,
});

describe("signature sanitizer", () => {
  it("drops dangerous elements together with their content", () => {
    const html =
      '<p>Hello</p><script>alert(1)</script><style>body{color:red}</style>' +
      '<iframe src="https://evil.example"></iframe><object data="x"></object>' +
      '<embed src="x"><form action="/x"><input value="1"><button>Go</button></form>' +
      '<meta http-equiv="refresh" content="0"><link rel="stylesheet" href="x">' +
      '<p>Bye</p>';
    const clean = tokenizer(html);
    expect(clean).toBe("<p>Hello</p><p>Bye</p>");
    expect(clean).not.toMatch(/script|iframe|object|embed|form|input|meta|link|style/i);
  });

  it("keeps the text of unknown wrapper elements but drops the tag", () => {
    expect(tokenizer("<custom-tag><b>Bold</b> text</custom-tag>")).toBe(
      "<b>Bold</b> text",
    );
  });

  it("removes every on* attribute", () => {
    const clean = tokenizer(
      '<p onclick="alert(1)" onmouseover="alert(2)">Hi</p>' +
        '<img src="https://example.com/a.png" onerror="alert(3)" alt="a">',
    );
    expect(clean).toBe(
      '<p>Hi</p><img src="https://example.com/a.png" alt="a">',
    );
  });

  it("drops javascript:, vbscript: and data: URLs", () => {
    expect(tokenizer('<a href="javascript:alert(1)">x</a>')).toBe("<a>x</a>");
    expect(tokenizer('<a href="vbscript:msgbox(1)">x</a>')).toBe("<a>x</a>");
    expect(tokenizer('<a href="data:text/html,<script>x</script>">x</a>')).toBe(
      "<a>x</a>",
    );
    expect(tokenizer('<img src="data:image/png;base64,AAAA">')).toBe(
      '<img src="data:image/png;base64,AAAA">',
    );
    expect(tokenizer('<img src="data:text/html;base64,AAAA">')).toBe("<img>");
  });

  it("defeats entity and control-character obfuscation of schemes", () => {
    expect(tokenizer('<a href="jav&#x61;script:alert(1)">x</a>')).toBe("<a>x</a>");
    expect(tokenizer('<a href="jav&#97;script&colon;alert(1)">x</a>')).toBe(
      "<a>x</a>",
    );
    expect(tokenizer('<a href="java\tscript:alert(1)">x</a>')).toBe("<a>x</a>");
    expect(tokenizer('<a href="JaVaScRiPt:alert(1)">x</a>')).toBe("<a>x</a>");
  });

  it("keeps http(s), mailto, tel, cid and relative URLs", () => {
    expect(tokenizer('<a href="https://example.com/x?a=1&amp;b=2">x</a>')).toBe(
      '<a href="https://example.com/x?a=1&amp;b=2">x</a>',
    );
    expect(tokenizer('<a href="mailto:ada@example.com">x</a>')).toBe(
      '<a href="mailto:ada@example.com">x</a>',
    );
    expect(tokenizer('<a href="/inbox">x</a>')).toBe('<a href="/inbox">x</a>');
    expect(tokenizer('<img src="cid:logo@nuncio">')).toBe(
      '<img src="cid:logo@nuncio">',
    );
  });

  it("strips executable or remote CSS from style attributes", () => {
    expect(
      tokenizer(
        '<p style="color:red;background:url(https://evil.example/x.png);width:expression(alert(1))">x</p>',
      ),
    ).toBe('<p style="color:red">x</p>');
    expect(sanitizeStyle("color: red; behavior: url(x.htc)")).toBe("color: red");
    expect(sanitizeStyle("position:fixed;color:red")).toBe("color:red");
    expect(sanitizeStyle("background:url(data:image/png;base64,AA)")).toBe(
      "background:url(data:image/png;base64,AA)",
    );
  });

  it("removes attributes that are not on the allow-list", () => {
    expect(
      tokenizer('<p id="x" data-track="1" formaction="/x" contenteditable="true">Hi</p>'),
    ).toBe("<p>Hi</p>");
  });

  it("drops comments and neutralizes nested signature wrappers", () => {
    expect(tokenizer("<!-- note --><p>Hi</p>")).toBe("<p>Hi</p>");
    expect(
      tokenizer(`<div class="${SIGNATURE_CLASS} evil"><p>x</p></div>`),
    ).toBe('<div class="evil"><p>x</p></div>');
  });

  it("closes elements left open by the input", () => {
    expect(tokenizer("<p><b>Hello")).toBe("<p><b>Hello</b></p>");
  });

  it("escapes a stray '<' in text", () => {
    expect(tokenizer("<p>a < b</p>")).toBe("<p>a &lt; b</p>");
  });

  it("falls back to the tokenizer when no DOM is available", () => {
    const html = '<p onclick="x()">Hi</p><script>alert(1)</script>';
    expect(sanitizeSignatureHtml(html)).toBe(tokenizer(html));
    expect(sanitizeSignatureHtmlWithoutDom(html)).toBe("<p>Hi</p>");
  });

  it("exposes a URL policy helper", () => {
    expect(isSafeUrl("https://example.com")).toBe(true);
    expect(isSafeUrl("javascript:alert(1)")).toBe(false);
    expect(isSafeUrl("data:image/png;base64,AA", { allowDataImage: true })).toBe(true);
    expect(isSafeUrl("data:image/png;base64,AA")).toBe(false);
    expect(isSafeUrl("")).toBe(false);
  });
});

describe("signature wrapper", () => {
  it("builds a sanitized wrapper with the shared class", () => {
    const html = signatureBlockHtml(
      signature({ html: '<p>Ada</p><img src="x" onerror="alert(1)">' }),
    );
    expect(html).toBe(
      `<div class="${SIGNATURE_CLASS}" ${SIGNATURE_ID_ATTRIBUTE}="sig-1"><p>Ada</p><img src="x"></div>`,
    );
    expect(html).not.toContain("onerror");
  });

  it("returns an empty string for an empty signature", () => {
    expect(signatureBlockHtml(signature({ html: "   " }))).toBe("");
  });

  it("omits the id attribute when the signature has no id", () => {
    expect(signatureBlockHtml(signature({ id: "" }))).toBe(
      `<div class="${SIGNATURE_CLASS}"><p>Ada Lovelace</p><p>Analyst</p></div>`,
    );
  });

  it("finds wrappers including nested divs", () => {
    const html = `<p>Hi</p><div class="nuncio-signature"><div><p>A</p></div></div><p>Bye</p>`;
    const bounds = findSignatureBlockBounds(html);
    expect(bounds).toHaveLength(1);
    expect(html.slice(bounds[0].start, bounds[0].end)).toBe(
      '<div class="nuncio-signature"><div><p>A</p></div></div>',
    );
    expect(findSignatureBlockBounds("<p>Hi</p>")).toHaveLength(0);
  });

  it("sanitizes only the wrapper content, not the rest of the body", () => {
    const html =
      '<p>Body <a href="javascript:alert(1)">link</a></p>' +
      '<div class="nuncio-signature"><img src="x" onerror="alert(1)"><script>x</script>Ada</div>';
    const cleaned = sanitizeSignatureBlocksHtml(html);
    expect(cleaned).toBe(
      '<p>Body <a href="javascript:alert(1)">link</a></p>' +
        '<div class="nuncio-signature"><img src="x">Ada</div>',
    );
  });

  it("leaves html without a signature untouched", () => {
    expect(sanitizeSignatureBlocksHtml("<p>Hi</p>")).toBe("<p>Hi</p>");
  });
});

describe("signature node in the editor schema", () => {
  it("parses the backend wrapper and renders it back with the class", () => {
    const spec = schema.nodes[SIGNATURE_NODE_NAME].spec;
    const selectors = (spec.parseDOM ?? []).map((rule) => rule.tag);
    expect(selectors).toContain(`div.${SIGNATURE_CLASS}`);

    const node = schema.nodes[SIGNATURE_NODE_NAME].create(
      { signatureId: "sig-1" },
      [schema.nodes.paragraph.create(null, schema.text("Ada"))],
    );
    const toDom = spec.toDOM as unknown as (node: PMNode) => unknown;
    const output = toDom(node) as [string, Record<string, string>, number];
    expect(output[0]).toBe("div");
    expect(output[1].class).toBe(SIGNATURE_CLASS);
    expect(output[1][SIGNATURE_ID_ATTRIBUTE]).toBe("sig-1");
    expect(output[2]).toBe(0);
  });

  it("round-trips through JSON without flattening the wrapper", () => {
    const doc = docFromJson([signatureNode(null)]);
    expect(doc.toJSON().content?.[0].type).toBe(SIGNATURE_NODE_NAME);
    expect(schema.nodeFromJSON(doc.toJSON()).eq(doc)).toBe(true);
  });

  it("finds every signature block with its remembered id", () => {
    const doc = docFromJson([
      paragraph("Hi"),
      signatureNode("sig-1"),
      paragraph("Tail"),
      signatureNode(null),
    ]);
    // paragraph("Hi"): 4, signature ("-- Ada"): 10, paragraph("Tail"): 6.
    expect(findSignatureBlocks(doc)).toEqual([
      { pos: 4, size: 10, signatureId: "sig-1" },
      { pos: 20, size: 10, signatureId: null },
    ]);
  });
});

describe("signature replacement plan", () => {
  it("replaces the first block and removes duplicates", () => {
    const doc = docFromJson([
      paragraph("Hi"),
      signatureNode(null),
      paragraph("Tail"),
      signatureNode("sig-old"),
    ]);
    const edits = planSignatureEdit(doc, true);
    expect(edits).toEqual([
      { kind: "replace", from: 4, to: 14 },
      { kind: "remove", from: 20, to: 30 },
    ]);
  });

  it("inserts at the end when no block exists", () => {
    const doc = docFromJson([paragraph("Hi"), paragraph("Tail")]);
    expect(signatureInsertPosition(doc)).toBe(doc.content.size);
    expect(planSignatureEdit(doc, true)).toEqual([
      { kind: "insert", at: doc.content.size },
    ]);
  });

  it("inserts above a quoted reply", () => {
    const doc = docFromJson([
      paragraph("My answer"),
      {
        type: "blockquote",
        content: [paragraph("Quoted")],
      },
    ]);
    // paragraph("My answer") is 11 wide, so the blockquote starts at 11.
    expect(signatureInsertPosition(doc)).toBe(11);
  });

  it("removes every block when the identity has no signature", () => {
    const doc = docFromJson([paragraph("Hi"), signatureNode(null)]);
    expect(planSignatureEdit(doc, false)).toEqual([
      { kind: "remove", from: 4, to: 14 },
    ]);
  });
});

describe("signature replacement transaction", () => {
  function stateFor(content: unknown[]): EditorState {
    return EditorState.create({ schema, doc: docFromJson(content) });
  }

  function signatureNodeFromJson(signatureId: string | null, text: string): PMNode {
    const doc = docFromJson([signatureNode(signatureId, text)]);
    const node = doc.child(0);
    if (node.type.name !== SIGNATURE_NODE_NAME) {
      throw new Error("expected a signature node");
    }
    return node;
  }

  it("ignores signature blocks nested inside another block", () => {
    const doc = docFromJson([
      paragraph("Hi"),
      {
        type: SIGNATURE_NODE_NAME,
        attrs: { signatureId: null },
        content: [
          signatureNode("nested", "-- Inner"),
          paragraph("-- Outer"),
        ],
      },
    ]);
    const blocks = findSignatureBlocks(doc);
    expect(blocks).toHaveLength(1);
    expect(blocks[0].signatureId).toBeNull();
    // The outer block is replaced as a whole, so the plan never overlaps.
    expect(planSignatureEdit(doc, true)).toEqual([
      { kind: "replace", from: 4, to: doc.content.size },
    ]);
    const transaction = buildSignatureTransaction(
      stateFor([
        paragraph("Hi"),
        {
          type: SIGNATURE_NODE_NAME,
          attrs: { signatureId: null },
          content: [signatureNode("nested", "-- Inner")],
        },
      ]),
      signatureNodeFromJson("sig-2", "-- New"),
    );
    expect(transaction?.doc.textContent).toBe("Hi-- New");
  });

  it("replaces the block in place and drops duplicates", () => {
    const state = stateFor([
      paragraph("Hi"),
      signatureNode(null, "-- Old"),
      paragraph("Tail"),
      signatureNode("sig-old", "-- Extra"),
    ]);
    const next = signatureNodeFromJson("sig-2", "-- New");
    const transaction = buildSignatureTransaction(state, next);
    expect(transaction).not.toBeNull();
    const doc = transaction?.doc;
    expect(doc?.toJSON().content?.map((node: { type: string }) => node.type)).toEqual([
      "paragraph",
      SIGNATURE_NODE_NAME,
      "paragraph",
    ]);
    const blocks = findSignatureBlocks(doc as PMNode);
    expect(blocks).toEqual([{ pos: 4, size: 10, signatureId: "sig-2" }]);
    expect(doc?.textContent).toContain("-- New");
    expect(doc?.textContent).not.toContain("-- Old");
    expect(doc?.textContent).not.toContain("-- Extra");
  });

  it("inserts above the quote when the body has no signature", () => {
    const state = stateFor([
      paragraph("My answer"),
      { type: "blockquote", content: [paragraph("Quoted")] },
    ]);
    const transaction = buildSignatureTransaction(
      state,
      signatureNodeFromJson("sig-2", "-- New"),
    );
    expect(transaction).not.toBeNull();
    const doc = transaction?.doc;
    expect(doc?.toJSON().content?.map((node: { type: string }) => node.type)).toEqual([
      "paragraph",
      SIGNATURE_NODE_NAME,
      "blockquote",
    ]);
    expect(findSignatureBlocks(doc as PMNode)[0].pos).toBe(11);
  });

  it("removes the block when the identity has no signature", () => {
    const state = stateFor([paragraph("Hi"), signatureNode(null, "-- Old")]);
    const transaction = buildSignatureTransaction(state, null);
    expect(transaction?.doc.toJSON().content?.map((node: { type: string }) => node.type)).toEqual([
      "paragraph",
    ]);
  });

  it("is a no-op when the document already holds that signature", () => {
    const state = stateFor([paragraph("Hi"), signatureNode("sig-1", "-- New")]);
    expect(
      buildSignatureTransaction(state, signatureNodeFromJson("sig-1", "-- New")),
    ).toBeNull();
  });

  it("is a no-op when there is nothing to remove", () => {
    const state = stateFor([paragraph("Hi")]);
    expect(buildSignatureTransaction(state, null)).toBeNull();
  });
});

/**
 * `applySignatureToEditor` needs a live editor (and therefore a DOM), which the
 * node test environment does not provide. Everything it composes — the sanitizer,
 * the wrapper markup, the edit plan and the transaction against a real
 * `EditorState` — is covered above; the DOM only enters through
 * `escapeAttribute`, verified here.
 */
describe("signature markup escaping", () => {
  it("escapes attribute values", () => {
    expect(escapeAttribute('" onmouseover="alert(1)')).toBe(
      "&quot; onmouseover=&quot;alert(1)",
    );
  });
});
