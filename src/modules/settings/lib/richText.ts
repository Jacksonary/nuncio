/**
 * Signature HTML sanitization.
 *
 * Signatures are authored in the settings editor (constrained formatting) and
 * come back from the local DB, which means they can also arrive from an
 * imported encrypted backup or a hand-edited database row. The value is written
 * into a contentEditable living in the privileged Tauri WebView, so it must be
 * treated as untrusted markup.
 *
 * The old implementation was a set of regular-expression blacklists. Those are
 * bypassable in ways the browser's HTML parser is not: an attribute glued to a
 * preceding quoted value (`alt="a"onerror=...`) is a new attribute for the
 * parser, and entity-encoded URL schemes (`jav&#97;script:`) are decoded before
 * the URL is resolved. This module therefore reparses the markup character by
 * character and rebuilds it from allowlists:
 *
 *   - only known formatting tags survive (unknown tags are dropped, their text
 *     is kept; raw-text/active elements are dropped together with their body);
 *   - only known attributes survive, with per-attribute value validation;
 *   - `href`/`src` values are entity-decoded before the scheme is checked, and
 *     whitespace/control characters are ignored exactly like a URL parser does;
 *   - CSS declarations are parsed and filtered against a property/value
 *     allowlist, so `url()`, `expression()` and friends never survive.
 *
 * The output is a fresh serialization of that allowlist. Text nodes are kept
 * as text (a literal `<` is escaped), attributes are always double quoted and
 * escaped, so the result cannot be re-tokenized as a different structure.
 */

const ALLOWED_TAGS = new Set([
  "a",
  "abbr",
  "address",
  "b",
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
  "div",
  "dl",
  "dt",
  "em",
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
  "tr",
  "u",
  "ul",
  "var",
  "wbr",
]);

/** HTML void elements: never get a closing tag. */
const VOID_TAGS = new Set(["br", "col", "hr", "img", "wbr"]);

/**
 * Elements that are dropped *with their content*. Script/style-like elements
 * can hide markup that becomes active when reparsed, and embedded documents /
 * plugin hosts have no place in a mail signature.
 */
const DROP_WITH_CONTENT = new Set([
  "applet",
  "base",
  "button",
  "embed",
  "frame",
  "frameset",
  "iframe",
  "input",
  "link",
  "math",
  "meta",
  "noembed",
  "noframes",
  "noscript",
  "object",
  "option",
  "plaintext",
  "script",
  "select",
  "style",
  "svg",
  "template",
  "textarea",
  "title",
  "xmp",
]);

const GLOBAL_ATTRS = new Set(["dir", "lang", "style", "title"]);

const TAG_ATTRS: Record<string, Set<string>> = {
  a: new Set(["href"]),
  font: new Set(["color", "face", "size"]),
  img: new Set(["alt", "height", "src", "width"]),
  ol: new Set(["start", "type"]),
  table: new Set(["align", "bgcolor", "border", "cellpadding", "cellspacing", "height", "width"]),
  td: new Set(["align", "bgcolor", "colspan", "height", "rowspan", "valign", "width"]),
  th: new Set(["align", "bgcolor", "colspan", "height", "rowspan", "valign", "width"]),
  tr: new Set(["align", "bgcolor", "height", "valign"]),
  ul: new Set(["type"]),
};

const ALLOWED_STYLE_PROPS = new Set([
  "background-color",
  "border",
  "border-bottom",
  "border-collapse",
  "border-color",
  "border-left",
  "border-radius",
  "border-right",
  "border-style",
  "border-top",
  "border-width",
  "color",
  "font-family",
  "font-size",
  "font-style",
  "font-weight",
  "height",
  "letter-spacing",
  "line-height",
  "margin",
  "margin-bottom",
  "margin-left",
  "margin-right",
  "margin-top",
  "max-width",
  "min-width",
  "padding",
  "padding-bottom",
  "padding-left",
  "padding-right",
  "padding-top",
  "text-align",
  "text-decoration",
  "text-decoration-color",
  "text-transform",
  "vertical-align",
  "white-space",
  "width",
  "word-break",
]);

/** Legacy named entities that are relevant for URL/scheme validation. */
const NAMED_ENTITIES: Record<string, string> = {
  amp: "&",
  apos: "'",
  bsol: "\\",
  colon: ":",
  comma: ",",
  commat: "@",
  equals: "=",
  excl: "!",
  gt: ">",
  hyphen: "-",
  lpar: "(",
  lowbar: "_",
  lt: "<",
  newline: "\n",
  nbsp: " ",
  num: "#",
  period: ".",
  plus: "+",
  quest: "?",
  quot: '"',
  rpar: ")",
  semi: ";",
  sol: "/",
  tab: "\t",
  verbar: "|",
};

const CONTROL_CHARS = /[\u0000-\u0008\u000B\u000C\u000E-\u001F\u007F]/g;

/** Decode numeric and legacy named character references (browser-like). */
function decodeEntities(value: string): string {
  return value.replace(
    /&(?:#x([0-9a-fA-F]+);?|#([0-9]+);?|([a-zA-Z][a-zA-Z0-9]+);?)/g,
    (match: string, hex: string | undefined, decimal: string | undefined, name: string | undefined) => {
      if (hex !== undefined || decimal !== undefined) {
        const code = Number.parseInt(hex ?? decimal ?? "", hex !== undefined ? 16 : 10);
        if (!Number.isFinite(code) || code <= 0 || code > 0x10ffff) return "�";
        // Lone surrogates cannot be encoded; treat them as invalid.
        if (code >= 0xd800 && code <= 0xdfff) return "�";
        try {
          return String.fromCodePoint(code);
        } catch {
          return "�";
        }
      }
      const named = NAMED_ENTITIES[(name ?? "").toLowerCase()];
      return named ?? match;
    },
  );
}

/**
 * Normalize a URL the way attribute consumers do before deciding whether it is
 * allowed: decode character references, then drop ASCII whitespace/control
 * characters that URL parsers ignore. Returns lowercase for scheme checks.
 */
function compactUrl(value: string): string {
  return decodeEntities(value)
    .replace(/[\u0000-\u0020\u007F\u00A0]/g, "")
    .toLowerCase();
}

/** `http(s)`, `mailto` and `tel` links only. Relative URLs are meaningless here. */
function isSafeHref(value: string): boolean {
  return /^(?:https?:|mailto:|tel:)/.test(compactUrl(value));
}

/** Images may be remote (http(s)) or inline raster data URLs. No SVG/data HTML. */
function isSafeSrc(value: string): boolean {
  const compact = compactUrl(value);
  if (/^https?:/.test(compact)) return true;
  return /^data:image\/(?:png|jpe?g|gif|webp|bmp|avif);/.test(compact);
}

function stripControlChars(value: string): string {
  return value.replace(CONTROL_CHARS, "");
}

function escapeText(value: string): string {
  // `&` is intentionally left alone so existing entities such as `&nbsp;`
  // round-trip. A literal `<` must be escaped or it could start a tag when the
  // sanitized string is written back into innerHTML.
  return stripControlChars(value).replace(/</g, "&lt;");
}

function escapeAttribute(value: string): string {
  return value
    .replace(/&/g, "&amp;")
    .replace(/"/g, "&quot;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
}

/** Split a style attribute into declarations without breaking quoted values. */
function splitDeclarations(style: string): string[] {
  const declarations: string[] = [];
  let buffer = "";
  let quote: string | null = null;
  let depth = 0;
  for (const char of style) {
    if (quote) {
      buffer += char;
      if (char === quote) quote = null;
      continue;
    }
    if (char === '"' || char === "'") {
      quote = char;
      buffer += char;
      continue;
    }
    if (char === "(") depth += 1;
    if (char === ")" && depth > 0) depth -= 1;
    if (char === ";" && depth === 0) {
      declarations.push(buffer);
      buffer = "";
      continue;
    }
    buffer += char;
  }
  if (buffer.trim()) declarations.push(buffer);
  return declarations;
}

function findDeclarationColon(declaration: string): number {
  let quote: string | null = null;
  let depth = 0;
  for (let index = 0; index < declaration.length; index += 1) {
    const char = declaration[index];
    if (quote) {
      if (char === quote) quote = null;
      continue;
    }
    if (char === '"' || char === "'") {
      quote = char;
      continue;
    }
    if (char === "(") depth += 1;
    if (char === ")" && depth > 0) depth -= 1;
    if (char === ":" && depth === 0) return index;
  }
  return -1;
}

function isSafeStyleValue(value: string): boolean {
  if (!value || value.length > 240) return false;
  if (/[<>{}@\\]/.test(value)) return false;
  if (/[\u0000-\u001F\u007F]/.test(value)) return false;
  if (/expression\s*\(/i.test(value)) return false;
  if (/url\s*\(/i.test(value)) return false;
  if (/javascript\s*:/i.test(value)) return false;
  if (/vbscript\s*:/i.test(value)) return false;
  if (/behavior\s*:/i.test(value)) return false;
  if (/-moz-binding/i.test(value)) return false;
  return true;
}

function sanitizeStyle(value: string): string {
  const kept: string[] = [];
  for (const declaration of splitDeclarations(decodeEntities(value))) {
    const colon = findDeclarationColon(declaration);
    if (colon < 0) continue;
    const property = declaration.slice(0, colon).trim().toLowerCase();
    const propertyValue = declaration.slice(colon + 1).trim();
    if (!/^[a-z-]+$/.test(property)) continue;
    if (!ALLOWED_STYLE_PROPS.has(property)) continue;
    if (!isSafeStyleValue(propertyValue)) continue;
    kept.push(`${property}: ${propertyValue}`);
  }
  return kept.join("; ");
}

function isAllowedAttribute(tag: string, attribute: string): boolean {
  if (GLOBAL_ATTRS.has(attribute)) return true;
  return TAG_ATTRS[tag]?.has(attribute) ?? false;
}

/**
 * Validate and normalize one attribute value. Returns null when the attribute
 * must be dropped.
 */
function sanitizeAttribute(attribute: string, rawValue: string): string | null {
  const decoded = stripControlChars(decodeEntities(rawValue)).trim();
  if (attribute === "href") return isSafeHref(decoded) ? decoded : null;
  if (attribute === "src") return isSafeSrc(decoded) ? decoded : null;
  if (attribute === "style") {
    const style = sanitizeStyle(decoded);
    return style || null;
  }
  if (!decoded) return null;

  if (attribute === "width" || attribute === "height") {
    return /^(?:\d{1,4}(?:px|%)?|auto)$/i.test(decoded) ? decoded : null;
  }
  if (attribute === "colspan" || attribute === "rowspan") {
    return /^\d{1,3}$/.test(decoded) ? decoded : null;
  }
  if (attribute === "border" || attribute === "cellpadding" || attribute === "cellspacing") {
    return /^\d{1,3}$/.test(decoded) ? decoded : null;
  }
  if (attribute === "align") {
    return /^(?:left|center|right|justify)$/i.test(decoded) ? decoded : null;
  }
  if (attribute === "valign") {
    return /^(?:top|middle|bottom|baseline)$/i.test(decoded) ? decoded : null;
  }
  if (attribute === "dir") {
    return /^(?:ltr|rtl|auto)$/i.test(decoded) ? decoded : null;
  }
  if (attribute === "lang") {
    return /^[a-z]{2,8}(?:-[a-z0-9]{1,8})*$/i.test(decoded) ? decoded : null;
  }
  if (attribute === "color" || attribute === "bgcolor") {
    return /^(?:#[0-9a-f]{3,8}|[a-z]{1,24}|rgba?\([0-9.,%\s]+\))$/i.test(decoded)
      ? decoded
      : null;
  }
  if (attribute === "face") {
    return /^[a-z0-9 ,'"-]{1,120}$/i.test(decoded) ? decoded : null;
  }
  if (attribute === "size") {
    return /^[1-7]$/.test(decoded) ? decoded : null;
  }
  if (attribute === "start") {
    return /^-?\d{1,6}$/.test(decoded) ? decoded : null;
  }
  if (attribute === "type") {
    return /^[a-z0-9]{1,8}$/i.test(decoded) ? decoded : null;
  }
  return decoded.slice(0, 1024);
}

interface ParsedTag {
  name: string;
  attributes: Map<string, string>;
  selfClosing: boolean;
  /** Index just past the closing `>`. */
  end: number;
  /** False when the input ended before the tag was closed. */
  closed: boolean;
}

/**
 * Parse a start tag. Attribute parsing mirrors the HTML tokenizer closely
 * enough to catch `alt="a"onerror="..."`: after a quoted value the next
 * character may begin a new attribute even without whitespace.
 */
function parseStartTag(html: string, start: number): ParsedTag | null {
  const length = html.length;
  let index = start + 1;
  const nameStart = index;
  while (index < length && /[a-zA-Z0-9:-]/.test(html[index])) index += 1;
  if (index === nameStart) return null;
  const name = html.slice(nameStart, index).toLowerCase();
  const attributes = new Map<string, string>();
  let selfClosing = false;
  let closed = false;

  while (index < length) {
    while (index < length && /\s/.test(html[index])) index += 1;
    if (index >= length) break;
    const char = html[index];
    if (char === ">") {
      closed = true;
      index += 1;
      break;
    }
    if (char === "/") {
      if (html[index + 1] === ">") {
        selfClosing = true;
        closed = true;
        index += 2;
        break;
      }
      index += 1;
      continue;
    }

    const attributeStart = index;
    while (index < length && !/[\s=/>]/.test(html[index])) index += 1;
    const attributeName = html.slice(attributeStart, index).toLowerCase();
    if (!attributeName) {
      // Stray `=` or quote; advance so the loop always makes progress.
      index += 1;
      continue;
    }

    while (index < length && /\s/.test(html[index])) index += 1;
    let value = "";
    if (html[index] === "=") {
      index += 1;
      while (index < length && /\s/.test(html[index])) index += 1;
      const quote = html[index];
      if (quote === '"' || quote === "'") {
        index += 1;
        const valueStart = index;
        while (index < length && html[index] !== quote) index += 1;
        value = html.slice(valueStart, index);
        if (index < length) index += 1;
      } else {
        const valueStart = index;
        while (index < length && !/[\s>]/.test(html[index])) index += 1;
        value = html.slice(valueStart, index);
      }
    }
    // The HTML tokenizer keeps the first occurrence of a duplicate attribute.
    if (!attributes.has(attributeName)) attributes.set(attributeName, value);
  }

  return { name, attributes, selfClosing, end: index, closed };
}

function serializeStartTag(tag: ParsedTag): string {
  let output = `<${tag.name}`;
  for (const [attribute, rawValue] of tag.attributes) {
    if (!isAllowedAttribute(tag.name, attribute)) continue;
    const value = sanitizeAttribute(attribute, rawValue);
    if (value === null) continue;
    output += ` ${attribute}="${escapeAttribute(value)}"`;
  }
  output += ">";
  if (!VOID_TAGS.has(tag.name) && tag.selfClosing) output += `</${tag.name}>`;
  return output;
}

/** Move past the closing tag of an element whose whole body is dropped. */
function skipDroppedElement(html: string, name: string, from: number): number {
  const lower = html.toLowerCase();
  const closeStart = lower.indexOf(`</${name}`, from);
  if (closeStart < 0) return html.length;
  const closeEnd = html.indexOf(">", closeStart + 2);
  return closeEnd < 0 ? html.length : closeEnd + 1;
}

export function sanitizeSignatureHtml(html: string): string {
  if (!html) return "";
  const length = html.length;
  const output: string[] = [];
  let index = 0;

  while (index < length) {
    if (html[index] !== "<") {
      const next = html.indexOf("<", index);
      const end = next < 0 ? length : next;
      output.push(escapeText(html.slice(index, end)));
      index = end;
      continue;
    }

    if (html.startsWith("<!--", index)) {
      const commentEnd = html.indexOf("-->", index + 4);
      index = commentEnd < 0 ? length : commentEnd + 3;
      continue;
    }

    if (html.startsWith("</", index)) {
      const closeEnd = html.indexOf(">", index + 2);
      if (closeEnd < 0) break;
      const closingName = html
        .slice(index + 2, closeEnd)
        .trim()
        .split(/\s/, 1)[0]
        .toLowerCase();
      if (ALLOWED_TAGS.has(closingName) && !VOID_TAGS.has(closingName)) {
        output.push(`</${closingName}>`);
      }
      index = closeEnd + 1;
      continue;
    }

    if (html.startsWith("<!", index) || html.startsWith("<?", index)) {
      const declarationEnd = html.indexOf(">", index + 2);
      index = declarationEnd < 0 ? length : declarationEnd + 1;
      continue;
    }

    const tag = parseStartTag(html, index);
    if (!tag) {
      // A `<` that does not begin a tag is text.
      output.push("&lt;");
      index += 1;
      continue;
    }
    if (!tag.closed) break; // Incomplete tag at EOF: drop it and the remainder.
    index = tag.end;

    if (DROP_WITH_CONTENT.has(tag.name)) {
      index = skipDroppedElement(html, tag.name, index);
      continue;
    }
    // Unknown tags are removed but their text content is preserved. Their
    // attributes disappear with the tag, including any event handlers.
    if (!ALLOWED_TAGS.has(tag.name)) continue;

    output.push(serializeStartTag(tag));
  }

  return output.join("").trim();
}

/** Plain text is what we insert on paste, so quote it as HTML. */
export function escapeHtml(value: string): string {
  return value
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

/** Only http(s), mailto and tel links are allowed in signatures. */
export function isSafeLinkHref(value: string): boolean {
  const trimmed = value.trim();
  if (!trimmed) return false;
  if (/^(https?:|mailto:|tel:)/i.test(trimmed)) return true;
  return /^[^\s:@]+@[^\s@]+\.[^\s@]+$/.test(trimmed);
}

export function normalizeLinkHref(value: string): string {
  const trimmed = value.trim();
  if (!trimmed) return "";
  if (/^[^\s:@]+@[^\s@]+\.[^\s@]+$/.test(trimmed)) return `mailto:${trimmed}`;
  if (/^(https?:|mailto:|tel:)/i.test(trimmed)) return trimmed;
  return `https://${trimmed}`;
}

/** Strip tags for compact list previews. */
export function htmlToPlainText(html: string): string {
  return html
    .replace(/<\s*br\s*\/?>/gi, " ")
    .replace(/<\s*\/\s*(p|div|li|tr|h[1-6])\s*>/gi, " ")
    .replace(/<[^>]+>/g, "")
    .replace(/&nbsp;/gi, " ")
    .replace(/&amp;/gi, "&")
    .replace(/&lt;/gi, "<")
    .replace(/&gt;/gi, ">")
    .replace(/&quot;/gi, '"')
    .replace(/&#39;/gi, "'")
    .replace(/\s+/g, " ")
    .trim();
}
