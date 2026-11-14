/**
 * Reading view ("readability") support.
 *
 * The mail backend sanitizes the HTML body and keeps it in an iframe. This
 * module extracts the main article from that sanitized document so the reader
 * can render it in a typographic column without the sender's layout, and
 * remembers the per-message choice. Extraction never executes scripts: the
 * document is parsed with DOMParser (inert) and only text nodes are rendered,
 * so the article view is safe by construction.
 */

export type ArticleBlockType = "heading" | "paragraph" | "listItem" | "quote" | "pre";

export interface ArticleBlock {
  type: ArticleBlockType;
  text: string;
  /** Heading level (1-6) when `type` is "heading". */
  level?: number;
  /** True when the list item belongs to an ordered list. */
  ordered?: boolean;
}

/* ─────────────────────────── Persistence ─────────────────────────── */

const STORAGE_KEY = "nuncio.mail.readingView.v1";
/** Cap the number of remembered messages so localStorage cannot grow forever. */
const MAX_PERSISTED_MESSAGES = 500;

interface ReadingViewEntry {
  enabled: boolean;
  /** Last update, used to evict the oldest entries. */
  at: number;
}

interface ReadingViewStore {
  version: 1;
  entries: Record<string, ReadingViewEntry>;
}

function storage(): Storage | null {
  try {
    return typeof localStorage === "undefined" ? null : localStorage;
  } catch {
    // Access can throw when storage is disabled by the webview.
    return null;
  }
}

function emptyStore(): ReadingViewStore {
  return { version: 1, entries: {} };
}

function loadStore(): ReadingViewStore {
  const target = storage();
  if (!target) return emptyStore();
  try {
    const raw = target.getItem(STORAGE_KEY);
    if (!raw) return emptyStore();
    const parsed: unknown = JSON.parse(raw);
    if (!parsed || typeof parsed !== "object") return emptyStore();
    const entries = (parsed as { entries?: unknown }).entries;
    if (!entries || typeof entries !== "object") return emptyStore();
    const store = emptyStore();
    for (const [id, value] of Object.entries(entries as Record<string, unknown>)) {
      if (!id || !value || typeof value !== "object") continue;
      const entry = value as { enabled?: unknown; at?: unknown };
      if (typeof entry.enabled !== "boolean") continue;
      store.entries[id] = {
        enabled: entry.enabled,
        at: typeof entry.at === "number" && Number.isFinite(entry.at) ? entry.at : 0,
      };
    }
    return store;
  } catch {
    return emptyStore();
  }
}

/** Whether the user asked for the reading view for this message. */
export function readReadingView(messageId: string | null | undefined): boolean {
  if (!messageId) return false;
  return loadStore().entries[messageId]?.enabled ?? false;
}

/** Persists the reading view toggle for a message (best effort). */
export function writeReadingView(messageId: string | null | undefined, enabled: boolean): void {
  if (!messageId) return;
  const target = storage();
  if (!target) return;
  const store = loadStore();
  store.entries[messageId] = { enabled, at: Date.now() };

  const ids = Object.keys(store.entries);
  if (ids.length > MAX_PERSISTED_MESSAGES) {
    ids
      .sort((a, b) => store.entries[a].at - store.entries[b].at)
      .slice(0, ids.length - MAX_PERSISTED_MESSAGES)
      .forEach((id) => {
        delete store.entries[id];
      });
  }

  try {
    target.setItem(STORAGE_KEY, JSON.stringify(store));
  } catch {
    // Quota exceeded or storage disabled: the toggle still works for this view.
  }
}

/* ─────────────────────────── Text helpers ─────────────────────────── */

const REMOVE_TAGS = new Set([
  "SCRIPT",
  "STYLE",
  "NOSCRIPT",
  "TEMPLATE",
  "HEAD",
  "IFRAME",
  "OBJECT",
  "EMBED",
  "SVG",
  "CANVAS",
  "VIDEO",
  "AUDIO",
  "LINK",
  "META",
  "FORM",
  "INPUT",
  "BUTTON",
  "SELECT",
  "TEXTAREA",
  "NAV",
  "ASIDE",
  "FOOTER",
]);

const REMOVE_ROLES = new Set([
  "navigation",
  "banner",
  "contentinfo",
  "complementary",
  "search",
  "menu",
  "menubar",
]);

/** Class/id tokens that almost always identify chrome, not article content. */
const BOILERPLATE_HINT =
  /(?:^|[\s_-])(?:nav|navbar|navigation|menu|sidebar|footer|social|share|sharing|advert|advertisement|ads?|promo|banner|cookie|consent|subscribe|subscription|unsubscribe|preferences?|view-in-browser|email-footer)(?:$|[\s_-])/i;

const BLOCK_TAGS = new Set([
  "H1",
  "H2",
  "H3",
  "H4",
  "H5",
  "H6",
  "P",
  "LI",
  "BLOCKQUOTE",
  "PRE",
  "FIGCAPTION",
  "DT",
  "DD",
  "TD",
  "TH",
]);

const CONTAINER_TAGS = new Set([
  "DIV",
  "SECTION",
  "ARTICLE",
  "MAIN",
  "CENTER",
  "FONT",
  "FIGURE",
  "TABLE",
  "THEAD",
  "TBODY",
  "TFOOT",
  "TR",
  "UL",
  "OL",
  "DL",
]);

const BLOCK_DESCENDANT_SELECTOR =
  "p,div,section,article,main,li,ul,ol,table,tr,td,th,h1,h2,h3,h4,h5,h6,blockquote,pre,figure,figcaption,dl,dt,dd";

/** Copy an element's text, turning <br>, block boundaries and image alt text into readable text. */
function collectText(node: Node): string {
  if (node.nodeType === 3 /* Node.TEXT_NODE */) return node.nodeValue ?? "";
  if (node.nodeType !== 1 /* Node.ELEMENT_NODE */) return "";
  const element = node as Element;
  const tag = element.tagName;
  if (tag === "BR") return "\n";
  if (tag === "IMG") {
    const alt = element.getAttribute("alt")?.trim();
    return alt ? ` ${alt} ` : " ";
  }
  if (element.hasAttribute("hidden") || element.getAttribute("aria-hidden") === "true") return "";
  let text = "";
  for (const child of Array.from(element.childNodes)) text += collectText(child);
  return text;
}

function normalizeInline(value: string): string {
  return value
    .replace(/\u00a0/g, " ")
    .replace(/\r\n?/g, "\n")
    .replace(/[ \t]+/g, " ")
    .replace(/\s*\n\s*/g, " ")
    .trim();
}

function normalizeBlock(value: string): string {
  return value
    .replace(/\u00a0/g, " ")
    .replace(/\r\n?/g, "\n")
    .replace(/[ \t]+\n/g, "\n")
    .replace(/[ \t]{2,}/g, " ")
    .replace(/\n{3,}/g, "\n\n")
    .trim();
}

function elementText(element: Element, preserveLines: boolean): string {
  const raw = collectText(element);
  return preserveLines ? normalizeBlock(raw) : normalizeInline(raw);
}

/** Element scoring used to pick the main article container. */
function scoreCandidate(element: Element): number {
  const text = normalizeInline(element.textContent ?? "");
  if (text.length === 0) return 0;
  const links = Array.from(element.querySelectorAll("a"));
  let linkLength = 0;
  for (const link of links) linkLength += normalizeInline(link.textContent ?? "").length;
  const linkDensity = Math.min(1, linkLength / Math.max(1, text.length));
  const paragraphs = element.querySelectorAll("p").length;
  const punctuation = (text.match(/[,.!?;:，。！？；：]/g) ?? []).length;
  let score =
    text.length * (1 - linkDensity * 0.8) + paragraphs * 25 + Math.min(punctuation, 400) * 0.5;
  if (
    element.tagName === "ARTICLE" ||
    element.tagName === "MAIN" ||
    element.getAttribute("role") === "main"
  ) {
    score *= 1.15;
  }
  return score;
}

function shouldRemoveElement(element: Element): boolean {
  if (REMOVE_TAGS.has(element.tagName)) return true;
  const role = (element.getAttribute("role") ?? "").toLowerCase();
  if (REMOVE_ROLES.has(role)) return true;
  if (element.hasAttribute("hidden") || element.getAttribute("aria-hidden") === "true") return true;
  const inlineStyle = element.getAttribute("style") ?? "";
  if (/display\s*:\s*none|visibility\s*:\s*hidden/i.test(inlineStyle)) return true;
  const hint = `${element.getAttribute("class") ?? ""} ${element.getAttribute("id") ?? ""}`.trim();
  return hint.length > 0 && BOILERPLATE_HINT.test(hint);
}

function isNoiseLine(text: string): boolean {
  if (text.length > 80) return false;
  return /^(?:unsubscribe|manage (?:your )?(?:subscription|preferences)|view (?:this (?:email|message) )?in (?:your )?browser|update (?:your )?preferences|copyright\b|all rights reserved)/i.test(
    text,
  );
}

function pushBlock(element: Element, blocks: ArticleBlock[]): void {
  const tag = element.tagName;
  if (tag === "H1" || tag === "H2" || tag === "H3" || tag === "H4" || tag === "H5" || tag === "H6") {
    const text = elementText(element, false);
    if (text) blocks.push({ type: "heading", level: Number(tag[1]), text });
    return;
  }
  if (tag === "LI") {
    const clone = element.cloneNode(true) as Element;
    clone.querySelectorAll("ul, ol").forEach((nested) => nested.remove());
    const text = elementText(clone, false);
    if (text) {
      blocks.push({
        type: "listItem",
        text,
        ordered: element.parentElement?.tagName === "OL",
      });
    }
    return;
  }
  if (tag === "BLOCKQUOTE") {
    const text = elementText(element, true);
    if (text) blocks.push({ type: "quote", text });
    return;
  }
  if (tag === "PRE") {
    // Code and ASCII art need their leading whitespace.
    const text = collectText(element)
      .replace(/\u00a0/g, " ")
      .replace(/\r\n?/g, "\n")
      .replace(/\n{3,}/g, "\n\n")
      .trim();
    if (text) blocks.push({ type: "pre", text });
    return;
  }
  const text = elementText(element, true);
  if (text) blocks.push({ type: "paragraph", text });
}

function hasBlockDescendant(element: Element): boolean {
  return Boolean(element.querySelector(BLOCK_DESCENDANT_SELECTOR));
}

function visitContainer(element: Element, blocks: ArticleBlock[]): void {
  for (const child of Array.from(element.children)) {
    const tag = child.tagName;

    if (BLOCK_TAGS.has(tag)) {
      // Table cells often wrap the real paragraph; descend instead of taking
      // the whole cell when it contains block-level markup.
      const isCell = tag === "TD" || tag === "TH";
      if (isCell && hasBlockDescendant(child)) {
        visitContainer(child, blocks);
      } else {
        pushBlock(child, blocks);
      }
      continue;
    }

    if (CONTAINER_TAGS.has(tag)) {
      if (hasBlockDescendant(child)) {
        visitContainer(child, blocks);
      } else {
        const text = elementText(child, true);
        if (text) blocks.push({ type: "paragraph", text });
      }
      continue;
    }

    // Unknown wrappers (e.g. <span> around a <p>) are transparent.
    if (hasBlockDescendant(child)) visitContainer(child, blocks);
  }
}

function dedupe(blocks: ArticleBlock[]): ArticleBlock[] {
  const result: ArticleBlock[] = [];
  let previous = "";
  for (const block of blocks) {
    const text = block.text.trim();
    if (!text) continue;
    const key = `${block.type}:${text}`;
    if (key === previous) continue;
    previous = key;
    if (isNoiseLine(text)) continue;
    result.push({ ...block, text });
  }
  return result;
}

function totalLength(blocks: ArticleBlock[]): number {
  let length = 0;
  for (const block of blocks) length += block.text.length;
  return length;
}

/**
 * Extracts the main article from a sanitized HTML document.
 * Returns an empty array when nothing useful could be found.
 */
export function extractArticleFromHtml(html: string): ArticleBlock[] {
  if (!html.trim()) return [];
  if (typeof DOMParser === "undefined") return extractArticleFallback(html);

  let document: Document;
  try {
    document = new DOMParser().parseFromString(html, "text/html");
  } catch {
    return extractArticleFallback(html);
  }
  if (!document.body) return [];
  if (document.querySelector("parsererror")) return [];

  // Drop scripts, page chrome and marketing furniture before scoring.
  for (const element of Array.from(document.body.querySelectorAll("*"))) {
    if (shouldRemoveElement(element)) element.remove();
  }

  const candidates: Element[] = [];
  const seen = new Set<Element>();
  const add = (element: Element | null) => {
    if (!element || seen.has(element)) return;
    seen.add(element);
    candidates.push(element);
  };
  add(document.body);
  for (const element of Array.from(
    document.body.querySelectorAll("article, main, [role='main'], section, div, td"),
  )) {
    add(element);
  }

  candidates.sort((a, b) => scoreCandidate(b) - scoreCandidate(a));

  // Try the best few containers: a nested wrapper sometimes scores higher
  // while missing most of the text.
  for (const candidate of candidates.slice(0, 8)) {
    const blocks = dedupe(extractBlocks(candidate));
    if (totalLength(blocks) >= 40) return blocks;
  }

  const fromBody = dedupe(extractBlocks(document.body));
  if (fromBody.length > 0) return fromBody;

  // Last resort: the scorer may have rejected everything, but plain text is
  // still preferable to an unreadable layout.
  const bodyText = normalizeBlock(collectText(document.body));
  return bodyText ? blocksFromText(bodyText) : [];
}

function extractBlocks(container: Element): ArticleBlock[] {
  const blocks: ArticleBlock[] = [];
  visitContainer(container, blocks);
  if (blocks.length === 0) {
    const text = elementText(container, true);
    if (text) blocks.push({ type: "paragraph", text });
  }
  return blocks;
}

/** Regex-only extraction used when DOMParser is unavailable (tests / non-DOM hosts). */
function extractArticleFallback(html: string): ArticleBlock[] {
  const withoutChrome = html
    .replace(/<!--[\s\S]*?-->/g, " ")
    .replace(/<(script|style|noscript|template|head|svg)\b[\s\S]*?<\/\1\s*>/gi, " ")
    .replace(/<(nav|aside|footer|form)\b[\s\S]*?<\/\1\s*>/gi, " ");
  const withBreaks = withoutChrome
    .replace(/<br\s*\/?>/gi, "\n")
    .replace(/<\/(?:p|div|section|article|li|ul|ol|h[1-6]|blockquote|pre|tr|table)\s*>/gi, "\n\n")
    .replace(/<li\b[^>]*>/gi, "\n• ");
  const decoded = withBreaks
    .replace(/&nbsp;/gi, " ")
    .replace(/&amp;/gi, "&")
    .replace(/&lt;/gi, "<")
    .replace(/&gt;/gi, ">")
    .replace(/&quot;/gi, '"')
    .replace(/&#(?:39|x27);/gi, "'")
    .replace(/&[a-z]+;/gi, " ")
    .replace(/<[^>]*>/g, " ")
    // Inline tags leave padding spaces where they were removed.
    .replace(/[ \t]{2,}/g, " ");
  return blocksFromText(decoded);
}

/* ───────────────────────── Plain-text articles ───────────────────────── */

/** Converts the backend's `MessageDetail.text` into article blocks. */
export function blocksFromText(text: string | null | undefined): ArticleBlock[] {
  if (!text) return [];
  const normalized = text.replace(/\r\n?/g, "\n").replace(/\u00a0/g, " ").trim();
  if (!normalized) return [];
  const blocks: ArticleBlock[] = [];

  for (const chunk of normalized.split(/\n{2,}/)) {
    const lines = chunk.split("\n").map((line) => line.replace(/[ \t]+$/g, ""));
    const nonEmpty = lines.filter((line) => line.trim().length > 0);
    if (nonEmpty.length === 0) continue;

    if (nonEmpty.every((line) => /^\s*>/.test(line))) {
      const quoted = lines.map((line) => line.replace(/^\s*>\s?/, "")).join("\n").trim();
      if (quoted) blocks.push({ type: "quote", text: quoted });
      continue;
    }

    const bulleted = nonEmpty.every((line) => /^\s*(?:[-*•·]|\d+[.)])\s+/.test(line));
    if (bulleted) {
      for (const line of nonEmpty) {
        const ordered = /^\s*\d/.test(line);
        const item = line.replace(/^\s*(?:[-*•·]|\d+[.)])\s+/, "").trim();
        if (item) blocks.push({ type: "listItem", text: item, ordered });
      }
      continue;
    }

    const paragraph = lines.join("\n").trim();
    if (paragraph) blocks.push({ type: "paragraph", text: paragraph });
  }

  return dedupe(blocks);
}

/**
 * Builds the article for a message. Uses a readability-style extraction over
 * the sanitized HTML first and falls back to the backend's plain-text body.
 * Returns an empty array when neither yields content (the caller then keeps
 * rendering the original HTML).
 */
export function buildArticle(
  html: string | null | undefined,
  text: string | null | undefined,
): ArticleBlock[] {
  if (html && html.trim()) {
    const fromHtml = extractArticleFromHtml(html);
    if (fromHtml.length > 0) return fromHtml;
  }
  return blocksFromText(text);
}
