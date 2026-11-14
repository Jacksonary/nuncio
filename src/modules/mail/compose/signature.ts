/**
 * Signature handling for the composer.
 *
 * The backend wraps every signature in `<div class="nuncio-signature">…</div>`
 * (see `signature_block` in `crates/mail/src/service/compose.rs`). This module
 * owns the three pieces the composer needs around that wrapper:
 *
 *  1. `sanitizeSignatureHtml` — allow-list sanitizer applied to every signature
 *     before it reaches the editor. The browser path uses `DOMParser`; a
 *     tokenizer fallback with the same policy is used where no DOM exists
 *     (unit tests, non-DOM runtimes).
 *  2. `signatureBlockHtml` / `sanitizeSignatureBlocksHtml` — build and repair
 *     the wrapper markup.
 *  3. `planSignatureEdit` / `applySignatureToEditor` — replace the existing
 *     signature block in place when the From identity changes, insert one when
 *     missing, and drop it when the new identity has no signature.
 *
 * The wrapper is preserved inside Tiptap by the `SignatureBlock` node declared
 * in `extensions.ts`, which parses `div.nuncio-signature` and renders it back
 * with the same class.
 */
import type { Editor } from "@tiptap/core";
import type { Node as PMNode } from "@tiptap/pm/model";
import { DOMParser as PMDOMParser } from "@tiptap/pm/model";
import type { EditorState, Transaction } from "@tiptap/pm/state";
import type { Signature } from "@/ipc/types";
import { escapeAttribute } from "./logic";

/** CSS class of the signature wrapper shared with the Rust backend. */
export const SIGNATURE_CLASS = "nuncio-signature";
/** ProseMirror node name of the signature wrapper (see extensions.ts). */
export const SIGNATURE_NODE_NAME = "signature";
/** Attribute the editor keeps on the wrapper to remember which signature it holds. */
export const SIGNATURE_ID_ATTRIBUTE = "data-nuncio-signature-id";

/* ─────────────────────────── HTML entities ───────────────────────── */

const NAMED_ENTITIES: Record<string, string> = {
  amp: "&",
  apos: "'",
  colon: ":",
  gt: ">",
  lt: "<",
  nbsp: " ",
  newline: "\n",
  quot: '"',
  tab: "\t",
};

const ENTITY_PATTERN =
  /&(?:#x([0-9a-fA-F]{1,6})|#([0-9]{1,7})|([a-zA-Z][a-zA-Z0-9]{1,31}));/g;

/**
 * Decode the character references that matter for URL scheme checks. Unknown
 * names are left untouched (they are re-escaped when serialized).
 */
export function decodeEntities(value: string): string {
  if (!value || value.indexOf("&") < 0) return value;
  return value.replace(
    ENTITY_PATTERN,
    (match, hex: string | undefined, decimal: string | undefined, name: string | undefined) => {
      if (hex) {
        const code = Number.parseInt(hex, 16);
        return code > 0 && code <= 0x10ffff ? String.fromCodePoint(code) : match;
      }
      if (decimal) {
        const code = Number.parseInt(decimal, 10);
        return code > 0 && code <= 0x10ffff ? String.fromCodePoint(code) : match;
      }
      if (name) {
        const decoded = NAMED_ENTITIES[name.toLowerCase()];
        return decoded ?? match;
      }
      return match;
    },
  );
}

/* ────────────────────────────── Policy ───────────────────────────── */

/** Elements removed together with their content. */
const FORBIDDEN_TAGS = new Set([
  "applet",
  "audio",
  "base",
  "button",
  "canvas",
  "embed",
  "form",
  "frame",
  "frameset",
  "iframe",
  "input",
  "link",
  "math",
  "meta",
  "noscript",
  "object",
  "optgroup",
  "option",
  "param",
  "script",
  "select",
  "source",
  "style",
  "svg",
  "template",
  "textarea",
  "track",
  "video",
]);

/** Elements kept as-is (attributes are filtered separately). */
const ALLOWED_TAGS = new Set([
  "a",
  "abbr",
  "address",
  "b",
  "big",
  "blockquote",
  "br",
  "caption",
  "center",
  "cite",
  "code",
  "col",
  "colgroup",
  "dd",
  "del",
  "dfn",
  "div",
  "dl",
  "dt",
  "em",
  "figcaption",
  "figure",
  "font",
  "h1",
  "h2",
  "h3",
  "h4",
  "h5",
  "h6",
  "hr",
  "i",
  "img",
  "ins",
  "kbd",
  "li",
  "mark",
  "ol",
  "p",
  "pre",
  "q",
  "s",
  "samp",
  "small",
  "span",
  "strike",
  "strong",
  "sub",
  "sup",
  "table",
  "tbody",
  "td",
  "tfoot",
  "th",
  "thead",
  "time",
  "tr",
  "tt",
  "u",
  "ul",
  "var",
]);

/** Attributes allowed on every element. */
const GLOBAL_ATTRIBUTES = new Set([
  "align",
  "aria-hidden",
  "aria-label",
  "class",
  "dir",
  "lang",
  "role",
  "style",
  "title",
  "valign",
]);

/** Extra attributes per element. */
const TAG_ATTRIBUTES: Record<string, readonly string[]> = {
  a: ["href", "name", "rel", "target", "title"],
  blockquote: ["cite"],
  col: ["span", "width"],
  colgroup: ["span", "width"],
  font: ["color", "face", "size"],
  h1: ["id"],
  h2: ["id"],
  h3: ["id"],
  h4: ["id"],
  h5: ["id"],
  h6: ["id"],
  img: ["alt", "border", "height", "src", "width"],
  li: ["type", "value"],
  ol: ["start", "type"],
  q: ["cite"],
  table: ["bgcolor", "border", "cellpadding", "cellspacing", "height", "summary", "width"],
  td: ["bgcolor", "colspan", "headers", "height", "nowrap", "rowspan", "width"],
  th: ["bgcolor", "colspan", "headers", "height", "nowrap", "rowspan", "scope", "width"],
  tr: ["bgcolor", "height"],
  ul: ["type"],
};

/** Attributes whose value is a URL and must pass `isSafeUrl`. */
const URL_ATTRIBUTES = new Set(["background", "cite", "href", "longdesc", "src"]);

const ALLOWED_SCHEMES = new Set(["http", "https", "mailto", "sms", "tel", "cid"]);

const DATA_IMAGE_PATTERN =
  /^data:image\/(?:avif|bmp|gif|jpe?g|png|svg\+xml|vnd\.microsoft\.icon|webp|x-icon)\b/i;

/** Elements that never take a closing tag. */
const VOID_TAGS = new Set([
  "area",
  "br",
  "col",
  "embed",
  "hr",
  "img",
  "input",
  "link",
  "meta",
  "source",
  "track",
  "wbr",
]);

export function isForbiddenTag(tag: string): boolean {
  return FORBIDDEN_TAGS.has(tag.toLowerCase());
}

export function isAllowedTag(tag: string): boolean {
  return ALLOWED_TAGS.has(tag.toLowerCase());
}

export function isAllowedAttribute(tag: string, attribute: string): boolean {
  const name = attribute.toLowerCase();
  if (!name || name.startsWith("on")) return false;
  if (GLOBAL_ATTRIBUTES.has(name)) return true;
  const extra = TAG_ATTRIBUTES[tag.toLowerCase()];
  return extra ? extra.includes(name) : false;
}

/**
 * True when a URL is safe to keep in a signature. Relative URLs are allowed;
 * schemes are limited to http(s)/mailto/tel/sms/cid, and `data:` only for
 * `data:image/*` when `allowDataImage` is set (`<img src>`).
 */
export function isSafeUrl(
  raw: string,
  options: { allowDataImage?: boolean } = {},
): boolean {
  const decoded = decodeEntities(String(raw ?? "")).trim();
  if (!decoded) return false;
  // Browsers ignore control characters and whitespace inside a scheme
  // (`java\tscript:`), so strip them before the scheme check.
  const normalized = decoded.replace(/[\u0000- \u007f-\u009f]/g, "");
  if (!normalized) return false;
  const match = /^([a-zA-Z][a-zA-Z0-9+.-]*):/.exec(normalized);
  if (!match) return true; // relative or protocol-relative: allowed
  const scheme = match[1].toLowerCase();
  if (scheme === "data") {
    return Boolean(options.allowDataImage) && DATA_IMAGE_PATTERN.test(normalized);
  }
  return ALLOWED_SCHEMES.has(scheme);
}

/** Split a `style` value into declarations, honouring parentheses/quotes. */
function splitDeclarations(value: string): string[] {
  const parts: string[] = [];
  let current = "";
  let depth = 0;
  let quote: string | null = null;
  for (const char of value) {
    if (quote) {
      current += char;
      if (char === quote) quote = null;
      continue;
    }
    if (char === '"' || char === "'") {
      quote = char;
      current += char;
      continue;
    }
    if (char === "(") depth += 1;
    else if (char === ")") depth = Math.max(0, depth - 1);
    if (char === ";" && depth === 0) {
      parts.push(current);
      current = "";
      continue;
    }
    current += char;
  }
  parts.push(current);
  return parts;
}

/** Drop CSS declarations that could execute code or fetch remote resources. */
export function sanitizeStyle(value: string): string {
  const kept: string[] = [];
  for (const declaration of splitDeclarations(value)) {
    const trimmed = declaration.trim();
    if (!trimmed) continue;
    if (/(?:expression\s*\(|javascript:|vbscript:|-moz-binding|behavior\s*:|@import|<\/)/i.test(trimmed)) {
      continue;
    }
    // Positioning could overlay the surrounding app chrome (clickjacking).
    if (/^\s*position\s*:/i.test(trimmed)) continue;
    if (/url\s*\(/i.test(trimmed) && !/url\s*\(\s*["']?data:image\//i.test(trimmed)) {
      continue;
    }
    kept.push(trimmed);
  }
  return kept.join("; ");
}

/** Remove nested wrapper classes so a signature cannot inject extra blocks. */
export function stripSignatureClasses(value: string): string {
  return value
    .split(/\s+/)
    .filter((token) => token && token.toLowerCase() !== SIGNATURE_CLASS)
    .join(" ");
}

/* ─────────────────────── DOM based sanitizer ─────────────────────── */

function sanitizeElementAttributes(element: Element, tag: string): void {
  for (const attribute of Array.from(element.attributes)) {
    const name = attribute.name.toLowerCase();
    if (!isAllowedAttribute(tag, name)) {
      element.removeAttribute(attribute.name);
      continue;
    }
    if (URL_ATTRIBUTES.has(name)) {
      if (!isSafeUrl(attribute.value, {
        allowDataImage: tag === "img" && name === "src",
      })) {
        element.removeAttribute(attribute.name);
      }
      continue;
    }
    if (name === "style") {
      const cleaned = sanitizeStyle(attribute.value);
      if (cleaned) element.setAttribute("style", cleaned);
      else element.removeAttribute("style");
      continue;
    }
    if (name === "class") {
      const cleaned = stripSignatureClasses(attribute.value);
      if (cleaned) element.setAttribute("class", cleaned);
      else element.removeAttribute("class");
    }
  }
}

function sanitizeDomChildren(parent: ParentNode, doc: Document): void {
  for (const child of Array.from(parent.childNodes)) {
    if (child.nodeType === 3 /* Node.TEXT_NODE */) continue;
    if (child.nodeType !== 1 /* Node.ELEMENT_NODE */) {
      child.parentNode?.removeChild(child);
      continue;
    }
    const element = child as Element;
    const tag = element.tagName.toLowerCase();
    if (isForbiddenTag(tag)) {
      element.remove();
      continue;
    }
    if (!isAllowedTag(tag)) {
      // Unknown wrapper: keep the text, drop the tag.
      const fragment = doc.createDocumentFragment();
      while (element.firstChild) fragment.appendChild(element.firstChild);
      sanitizeDomChildren(fragment, doc);
      element.replaceWith(fragment);
      continue;
    }
    sanitizeElementAttributes(element, tag);
    sanitizeDomChildren(element, doc);
  }
}

function sanitizeWithDom(html: string): string {
  const doc = new DOMParser().parseFromString(html, "text/html");
  const body = doc.body;
  if (!body) return "";
  sanitizeDomChildren(body, doc);
  return body.innerHTML;
}

/* ───────────────────── Tokenizer based sanitizer ─────────────────── */

interface ParsedTag {
  name: string;
  closing: boolean;
  selfClosing: boolean;
  attributes: string;
}

function readTag(html: string, start: number): { raw: string; end: number } | null {
  let quote: string | null = null;
  for (let index = start + 1; index < html.length; index += 1) {
    const char = html[index];
    if (quote) {
      if (char === quote) quote = null;
      continue;
    }
    if (char === '"' || char === "'") {
      quote = char;
      continue;
    }
    if (char === ">") return { raw: html.slice(start, index + 1), end: index + 1 };
  }
  return null;
}

/** `<` outside a quoted attribute value means the token is not a real tag. */
function hasUnquotedLt(source: string): boolean {
  let quote: string | null = null;
  for (const char of source) {
    if (quote) {
      if (char === quote) quote = null;
      continue;
    }
    if (char === '"' || char === "'") {
      quote = char;
      continue;
    }
    if (char === "<") return true;
  }
  return false;
}

function parseTagToken(raw: string): ParsedTag | null {
  if (raw.startsWith("<!") || raw.startsWith("<?")) return null;
  const match = /^<(\/?)([a-zA-Z][a-zA-Z0-9:_-]*)([\s\S]*?)\/?\s*>$/.exec(raw);
  if (!match) return null;
  const attributes = match[3] ?? "";
  // A `<` outside quotes means this is not a well-formed tag (e.g. the text
  // "a < b</p>"); the caller treats the `<` as text instead.
  if (hasUnquotedLt(attributes)) return null;
  return {
    name: match[2].toLowerCase(),
    closing: match[1] === "/",
    selfClosing: /\/\s*>$/.test(raw),
    attributes,
  };
}

const ATTRIBUTE_PATTERN =
  /([a-zA-Z_:][-a-zA-Z0-9_:.]*)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'=<>`]+)))?/g;

function serializeAttributes(tag: string, source: string): string {
  let result = "";
  ATTRIBUTE_PATTERN.lastIndex = 0;
  let match: RegExpExecArray | null;
  while ((match = ATTRIBUTE_PATTERN.exec(source)) !== null) {
    const name = match[1].toLowerCase();
    const rawValue = match[2] ?? match[3] ?? match[4] ?? "";
    if (!isAllowedAttribute(tag, name)) continue;
    let value = decodeEntities(rawValue);
    if (URL_ATTRIBUTES.has(name)) {
      if (!isSafeUrl(value, { allowDataImage: tag === "img" && name === "src" })) {
        continue;
      }
    } else if (name === "style") {
      value = sanitizeStyle(value);
      if (!value) continue;
    } else if (name === "class") {
      value = stripSignatureClasses(value);
      if (!value) continue;
    }
    result += ` ${name}="${escapeAttribute(value)}"`;
  }
  return result;
}

/** Skip a forbidden element, honouring nested elements with the same name. */
function skipElement(html: string, from: number, name: string): number {
  let depth = 1;
  let index = from;
  while (index < html.length) {
    const start = html.indexOf("<", index);
    if (start < 0) return html.length;
    index = start;
    if (html.startsWith("<!--", index)) {
      const commentEnd = html.indexOf("-->", index + 4);
      if (commentEnd < 0) return html.length;
      index = commentEnd + 3;
      continue;
    }
    const token = readTag(html, index);
    if (!token) {
      index += 1;
      continue;
    }
    index = token.end;
    const parsed = parseTagToken(token.raw);
    if (!parsed || parsed.name !== name) continue;
    if (parsed.closing) {
      depth -= 1;
      if (depth === 0) return index;
    } else if (!parsed.selfClosing && !VOID_TAGS.has(parsed.name)) {
      depth += 1;
    }
  }
  return html.length;
}

/**
 * Allow-list sanitizer without a DOM. Exposed for tests and for runtimes that
 * do not provide `DOMParser`.
 */
export function sanitizeSignatureHtmlWithoutDom(html: string): string {
  if (!html) return "";
  let output = "";
  const stack: string[] = [];
  let index = 0;

  while (index < html.length) {
    const start = html.indexOf("<", index);
    if (start < 0) {
      output += html.slice(index);
      break;
    }
    if (start > index) output += html.slice(index, start);
    index = start;

    if (html.startsWith("<!--", index)) {
      const commentEnd = html.indexOf("-->", index + 4);
      index = commentEnd < 0 ? html.length : commentEnd + 3;
      continue;
    }

    const token = readTag(html, index);
    if (!token) {
      output += "&lt;";
      index += 1;
      continue;
    }
    index = token.end;

    const parsed = parseTagToken(token.raw);
    if (!parsed) {
      if (token.raw.startsWith("<!") || token.raw.startsWith("<?")) continue;
      // Not a tag after all: keep the character as text.
      output += "&lt;";
      index = start + 1;
      continue;
    }

    if (parsed.closing) {
      const stackIndex = stack.lastIndexOf(parsed.name);
      if (stackIndex >= 0) {
        while (stack.length > stackIndex) {
          output += `</${stack.pop() as string}>`;
        }
      }
      continue;
    }

    if (isForbiddenTag(parsed.name)) {
      if (!parsed.selfClosing && !VOID_TAGS.has(parsed.name)) {
        index = skipElement(html, index, parsed.name);
      }
      continue;
    }

    if (!isAllowedTag(parsed.name)) {
      // Unknown wrapper: keep the text, drop the tag.
      continue;
    }

    const attributes = serializeAttributes(parsed.name, parsed.attributes);
    output += `<${parsed.name}${attributes}>`;
    if (!VOID_TAGS.has(parsed.name) && !parsed.selfClosing) {
      stack.push(parsed.name);
    }
  }

  while (stack.length > 0) output += `</${stack.pop() as string}>`;
  return output;
}

export interface SanitizeSignatureOptions {
  /**
   * Force a specific implementation. Production callers omit it and get the
   * DOM sanitizer when available.
   */
  mode?: "dom" | "tokenizer";
}

/**
 * Sanitize signature HTML with a strict allow-list: script/style/iframe/object/
 * embed/form/input/meta/link and friends are removed, `on*` attributes are
 * removed, and `javascript:`/`vbscript:`/`data:` URLs are dropped (except
 * `data:image/*` on `<img src>`).
 */
export function sanitizeSignatureHtml(
  html: string,
  options: SanitizeSignatureOptions = {},
): string {
  if (!html) return "";
  const mode = options.mode ?? (typeof DOMParser === "function" ? "dom" : "tokenizer");
  if (mode === "dom") {
    try {
      return sanitizeWithDom(html);
    } catch {
      // A broken document implementation must never let raw HTML through:
      // fall back to the tokenizer, which shares the same policy.
      return sanitizeSignatureHtmlWithoutDom(html);
    }
  }
  return sanitizeSignatureHtmlWithoutDom(html);
}

/* ───────────────────────── Wrapper building ──────────────────────── */

function isSignatureWrapperTag(tag: string): boolean {
  const withoutName = tag.replace(/^<\s*div\b/i, "");
  if (/data-nuncio-signature/i.test(withoutName)) return true;
  const classMatch = /\bclass\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))/i.exec(withoutName);
  if (!classMatch) return false;
  const value = classMatch[1] ?? classMatch[2] ?? classMatch[3] ?? "";
  return value
    .split(/\s+/)
    .some((token) => token.toLowerCase() === SIGNATURE_CLASS);
}

export interface SignatureBlockBounds {
  /** Start of the opening `<div …>`. */
  start: number;
  /** End of the opening tag (start of the inner HTML). */
  contentStart: number;
  /** Start of the closing `</div>` (or end of input when unclosed). */
  contentEnd: number;
  /** End of the closing `</div>` (or end of input when unclosed). */
  end: number;
}

const DIV_TOKEN = /<div\b[^>]*>|<\/div\s*>/gi;

/** Locate every top-level `.nuncio-signature` wrapper in an HTML string. */
export function findSignatureBlockBounds(html: string): SignatureBlockBounds[] {
  const bounds: SignatureBlockBounds[] = [];
  let cursor = 0;
  while (cursor < html.length) {
    DIV_TOKEN.lastIndex = cursor;
    const open = DIV_TOKEN.exec(html);
    if (!open) break;
    if (!open[0].toLowerCase().startsWith("<div") || !isSignatureWrapperTag(open[0])) {
      cursor = open.index + open[0].length;
      continue;
    }
    const contentStart = open.index + open[0].length;
    const nested = /<div\b[^>]*>|<\/div\s*>/gi;
    nested.lastIndex = contentStart;
    let depth = 1;
    let contentEnd = html.length;
    let end = html.length;
    let match: RegExpExecArray | null;
    while ((match = nested.exec(html)) !== null) {
      if (/^<div/i.test(match[0])) {
        depth += 1;
      } else {
        depth -= 1;
        if (depth === 0) {
          contentEnd = match.index;
          end = match.index + match[0].length;
          break;
        }
      }
    }
    bounds.push({ start: open.index, contentStart, contentEnd, end });
    cursor = end;
  }
  return bounds;
}

/**
 * Sanitize the content of every signature wrapper found in `html`, leaving the
 * rest of the body untouched. Used when a draft is opened so a stored (or
 * imported) signature cannot carry scripts into the editor.
 */
export function sanitizeSignatureBlocksHtml(html: string): string {
  if (!html) return "";
  const bounds = findSignatureBlockBounds(html);
  if (bounds.length === 0) return html;
  let result = "";
  let cursor = 0;
  for (const block of bounds) {
    result += html.slice(cursor, block.contentStart);
    result += sanitizeSignatureHtml(html.slice(block.contentStart, block.contentEnd));
    result += html.slice(block.contentEnd, block.end);
    cursor = block.end;
  }
  return result + html.slice(cursor);
}

/** Markup inserted into the editor for a signature ("" when it is empty). */
export function signatureBlockHtml(signature: Signature): string {
  const inner = sanitizeSignatureHtml(signature.html);
  if (!inner.trim()) return "";
  const id = signature.id?.trim();
  const idAttribute = id
    ? ` ${SIGNATURE_ID_ATTRIBUTE}="${escapeAttribute(id)}"`
    : "";
  return `<div class="${SIGNATURE_CLASS}"${idAttribute}>${inner}</div>`;
}

/* ──────────────────────── Editor integration ─────────────────────── */

export interface SignatureBlockRef {
  /** Position of the node inside the document. */
  pos: number;
  /** `node.nodeSize` of the wrapper. */
  size: number;
  /** Signature id remembered by the editor, when known. */
  signatureId: string | null;
}

export type SignatureEdit =
  | { kind: "replace"; from: number; to: number }
  | { kind: "insert"; at: number }
  | { kind: "remove"; from: number; to: number };

function attributeString(node: PMNode, name: string): string | null {
  const value = (node.attrs as Record<string, unknown>)[name];
  return typeof value === "string" && value ? value : null;
}

/**
 * Every signature wrapper in the document, in document order. Wrappers nested
 * inside another wrapper (a signature pasted into a signature) are reported as
 * part of the outer one, so replacement edits never overlap.
 */
export function findSignatureBlocks(doc: PMNode): SignatureBlockRef[] {
  const blocks: SignatureBlockRef[] = [];
  let coveredUntil = -1;
  doc.descendants((node, pos) => {
    if (node.type.name !== SIGNATURE_NODE_NAME) return;
    if (pos < coveredUntil) return;
    blocks.push({
      pos,
      size: node.nodeSize,
      signatureId: attributeString(node, "signatureId"),
    });
    coveredUntil = pos + node.nodeSize;
  });
  return blocks;
}

/**
 * Where a new signature block belongs when the document has none:
 * directly above the quoted message (replies/forwards quote the original in a
 * top-level blockquote), otherwise at the end of the body.
 */
export function signatureInsertPosition(doc: PMNode): number {
  let offset = 0;
  let position = doc.content.size;
  let found = false;
  doc.forEach((node) => {
    if (!found && node.type.name === "blockquote") {
      position = offset;
      found = true;
    }
    offset += node.nodeSize;
  });
  return position;
}

/**
 * Compute the edits that make the document hold exactly one signature block
 * (the given one) or none at all.
 */
export function planSignatureEdit(doc: PMNode, hasSignature: boolean): SignatureEdit[] {
  const blocks = findSignatureBlocks(doc);
  if (!hasSignature) {
    return blocks.map((block) => ({
      kind: "remove" as const,
      from: block.pos,
      to: block.pos + block.size,
    }));
  }
  if (blocks.length === 0) {
    return [{ kind: "insert", at: signatureInsertPosition(doc) }];
  }
  const [first, ...rest] = blocks;
  const edits: SignatureEdit[] = [
    { kind: "replace", from: first.pos, to: first.pos + first.size },
  ];
  for (const block of rest) {
    edits.push({ kind: "remove", from: block.pos, to: block.pos + block.size });
  }
  return edits;
}

function editPosition(edit: SignatureEdit): number {
  return edit.kind === "insert" ? edit.at : edit.from;
}

function parseSignatureNode(editor: Editor, html: string): PMNode | null {
  try {
    const wrapper = document.createElement("div");
    wrapper.innerHTML = html;
    const parsed = PMDOMParser.fromSchema(editor.schema).parse(wrapper);
    let found: PMNode | null = null;
    parsed.forEach((node) => {
      if (!found && node.type.name === SIGNATURE_NODE_NAME) found = node;
    });
    return found;
  } catch (error) {
    console.warn("[compose] could not parse the signature block", error);
    return null;
  }
}

/**
 * Build the transaction that puts `node` into the document (or removes the
 * signature when `node` is null). Returns null when nothing would change —
 * either there is no block to touch or the document already holds exactly this
 * signature (same id, single block), so an identity re-selection does not
 * throw away inline edits.
 */
export function buildSignatureTransaction(
  state: EditorState,
  node: PMNode | null,
): Transaction | null {
  const signatureId = node ? attributeString(node, "signatureId") : null;
  const blocks = findSignatureBlocks(state.doc);
  if (node && signatureId && blocks.length === 1 && blocks[0].signatureId === signatureId) {
    return null;
  }
  const edits = planSignatureEdit(state.doc, Boolean(node));
  if (edits.length === 0) return null;
  let transaction = state.tr;
  const ordered = [...edits].sort((a, b) => editPosition(b) - editPosition(a));
  for (const edit of ordered) {
    switch (edit.kind) {
      case "remove":
        transaction = transaction.delete(edit.from, edit.to);
        break;
      case "replace":
        if (node) transaction = transaction.replaceWith(edit.from, edit.to, node);
        break;
      case "insert":
        if (node) transaction = transaction.insert(edit.at, node);
        break;
    }
  }
  return transaction.docChanged ? transaction : null;
}

/**
 * Replace (or insert / remove) the signature block in the editor.
 *
 * Returns true when the document changed. Existing wrapper nodes are replaced
 * in place so the signature stays where it was (above a quote in replies); a
 * missing one is inserted above the quote or at the end of the body.
 */
export function applySignatureToEditor(
  editor: Editor,
  signature: Signature | null,
): boolean {
  if (editor.isDestroyed) return false;
  const html = signature ? signatureBlockHtml(signature) : "";
  const node = html ? parseSignatureNode(editor, html) : null;
  if (signature && !node) return false;
  const transaction = buildSignatureTransaction(editor.state, node);
  if (!transaction) return false;
  editor.view.dispatch(transaction);
  return true;
}
