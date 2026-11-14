import { describe, expect, it } from "vitest";
import {
  blocksFromText,
  buildArticle,
  extractArticleFromHtml,
  readReadingView,
  writeReadingView,
} from "./readingView";

describe("blocksFromText", () => {
  it("splits paragraphs on blank lines and keeps internal line breaks", () => {
    const blocks = blocksFromText("Hello team,\n\nLine one\nLine two\n\nThanks,\nAda");
    expect(blocks).toHaveLength(3);
    expect(blocks[0]).toMatchObject({ type: "paragraph", text: "Hello team," });
    expect(blocks[1]).toMatchObject({ type: "paragraph", text: "Line one\nLine two" });
    expect(blocks[2].type).toBe("paragraph");
  });

  it("recognises quoted replies", () => {
    const blocks = blocksFromText("My answer\n\n> Previous message\n> second line");
    expect(blocks[1]).toMatchObject({
      type: "quote",
      text: "Previous message\nsecond line",
    });
  });

  it("recognises bullet and ordered lists", () => {
    const blocks = blocksFromText("• first\n• second\n\n1. one\n2. two");
    expect(blocks.map((block) => [block.type, block.ordered, block.text])).toEqual([
      ["listItem", false, "first"],
      ["listItem", false, "second"],
      ["listItem", true, "one"],
      ["listItem", true, "two"],
    ]);
  });

  it("ignores empty input", () => {
    expect(blocksFromText(null)).toEqual([]);
    expect(blocksFromText("   \n\n ")).toEqual([]);
  });
});

describe("extractArticleFromHtml (DOM-less fallback)", () => {
  it("drops scripts, styles and chrome", () => {
    const html = `<!doctype html><html><head><style>.x{color:red}</style></head><body>
      <nav>Home | About</nav>
      <div class="content"><p>First paragraph of the article.</p>
      <p>Second paragraph with <strong>important</strong> details.</p></div>
      <footer>Unsubscribe · Copyright 2026</footer>
      <script>alert("not executed")</script>
    </body></html>`;
    const blocks = extractArticleFromHtml(html);
    const text = blocks.map((block) => block.text).join("\n");
    expect(text).toContain("First paragraph of the article.");
    expect(text).toContain("Second paragraph with important details.");
    expect(text).not.toContain("alert");
    expect(text).not.toContain("color:red");
  });

  it("decodes basic entities", () => {
    const blocks = extractArticleFromHtml("<p>Tom &amp; Jerry &lt;3</p>");
    expect(blocks[0]?.text).toContain("Tom & Jerry <3");
  });
});

describe("buildArticle", () => {
  it("falls back to the plain-text body when HTML has no usable content", () => {
    const blocks = buildArticle("<html><body>   </body></html>", "Plain body text");
    expect(blocks).toEqual([{ type: "paragraph", text: "Plain body text" }]);
  });

  it("returns an empty list when neither body is available", () => {
    expect(buildArticle("", null)).toEqual([]);
  });
});

describe("reading view preference", () => {
  it("is safe without localStorage and defaults to off", () => {
    expect(readReadingView("message-1")).toBe(false);
    expect(() => writeReadingView("message-1", true)).not.toThrow();
    expect(readReadingView(null)).toBe(false);
  });
});
