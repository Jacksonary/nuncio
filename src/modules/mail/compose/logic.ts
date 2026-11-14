/**
 * Pure helpers shared by the composer and its unit tests.
 *
 * This module must stay free of React and Tauri imports so it can run under
 * Vitest's node environment. HTML handling is intentionally string/regex based
 * because the input is always Tiptap's own serialization (well-formed, no
 * comments/CDATA) and DOM APIs are unavailable in node tests.
 */
import type {
  Address,
  Draft,
  DraftAttachment,
  Signature,
} from "@/ipc/types";
import { formatFileSize, isValidEmail, parseAddressInput } from "@/lib/format";

/** Total attachment warning threshold (Outlook/Gmail-ish). */
export const ATTACHMENT_SIZE_LIMIT = 25 * 1024 * 1024;

/* ───────────────────────────── Addresses ─────────────────────────── */

export function sameAddress(a: Address, b: Address): boolean {
  return a.email.trim().toLowerCase() === b.email.trim().toLowerCase();
}

export interface ParsedRecipients {
  valid: Address[];
  invalid: string[];
}

/**
 * Split a pasted "a@x.com; Ada <b@y.com>" blob into de-duplicated valid
 * addresses plus the raw strings that failed validation (for error display).
 */
export function parseRecipients(text: string): ParsedRecipients {
  const valid: Address[] = [];
  const invalid: string[] = [];
  for (const parsed of parseAddressInput(text)) {
    const email = parsed.email.trim();
    if (!email) continue;
    if (!isValidEmail(email)) {
      invalid.push(email);
      continue;
    }
    const candidate: Address = {
      name: parsed.name?.trim() || null,
      email,
    };
    if (!valid.some((existing) => sameAddress(existing, candidate))) {
      valid.push(candidate);
    }
  }
  return { valid, invalid };
}

/** E-mail addresses in a field that are not syntactically valid. */
export function invalidRecipients(addresses: Address[]): string[] {
  const seen = new Set<string>();
  const result: string[] = [];
  for (const address of addresses) {
    const email = address.email.trim();
    if (isValidEmail(email)) continue;
    const key = email.toLowerCase();
    if (seen.has(key)) continue;
    seen.add(key);
    result.push(email);
  }
  return result;
}

/** Move an address between To/Cc/Bcc lists without creating duplicates. */
export function moveRecipient(
  lists: { to: Address[]; cc: Address[]; bcc: Address[] },
  from: "to" | "cc" | "bcc",
  to: "to" | "cc" | "bcc",
  address: Address,
): { to: Address[]; cc: Address[]; bcc: Address[] } {
  const next = {
    to: [...lists.to],
    cc: [...lists.cc],
    bcc: [...lists.bcc],
  };
  if (from === to) return next;
  next[from] = next[from].filter((item) => !sameAddress(item, address));
  if (!next[to].some((item) => sameAddress(item, address))) {
    next[to] = [...next[to], address];
  }
  return next;
}

/* ───────────────────────────── Attachments ───────────────────────── */

export function mergeAttachments(
  current: DraftAttachment[],
  added: DraftAttachment[],
): DraftAttachment[] {
  const seen = new Set(current.map((item) => item.id));
  const result = [...current];
  for (const item of added) {
    if (seen.has(item.id)) continue;
    seen.add(item.id);
    result.push(item);
  }
  return result;
}

export function attachmentTotal(attachments: DraftAttachment[]): number {
  return attachments.reduce(
    (sum, item) => sum + (Number.isFinite(item.size) ? item.size : 0),
    0,
  );
}

export interface AttachmentSummary {
  total: number;
  formatted: string;
  tooLarge: boolean;
}

export function summarizeAttachments(
  attachments: DraftAttachment[],
  limit = ATTACHMENT_SIZE_LIMIT,
): AttachmentSummary {
  const total = attachmentTotal(attachments);
  return {
    total,
    formatted: formatFileSize(total),
    tooLarge: total > limit,
  };
}

/* ───────────────────────── Attachment keyword ────────────────────── */

const ATTACHMENT_WORDS =
  /(?:^|[^\p{L}])(?:attach(?:ment|ments|ed|ing)?|attached|enclos(?:ed|ure|ures)|enclosed)(?:[^\p{L}]|$)|附件|附加|随附|见附|附上/iu;

/**
 * True when the body mentions an attachment. Used to warn before sending a
 * message that talks about an attachment but has none.
 */
export function mentionsAttachment(text: string): boolean {
  if (!text) return false;
  return ATTACHMENT_WORDS.test(text);
}

/* ───────────────────────────── Signatures ────────────────────────── */

/**
 * Pick the signature for an identity:
 *  1. the identity's explicit signature (when it belongs to the account or is
 *     shared across accounts),
 *  2. an account-specific signature matching the new/reply flag,
 *  3. a global (accountId = null) signature matching the flag.
 */
export function findSignatureForIdentity(
  identity: { signatureId: string | null } | null | undefined,
  accountId: string,
  signatures: Signature[],
  isReply: boolean,
): Signature | null {
  const flag = isReply ? "useForReply" : "useForNew";
  if (identity?.signatureId) {
    const explicit = signatures.find((item) => item.id === identity.signatureId);
    const usable =
      explicit && (explicit.accountId === null || explicit.accountId === accountId);
    if (usable) return explicit;
  }
  const eligible = signatures.filter((item) => item[flag]);
  return (
    eligible.find((item) => item.accountId === accountId) ??
    eligible.find((item) => item.accountId === null) ??
    null
  );
}

/* ─────────────────────────────── HTML ────────────────────────────── */

const IMG_TAG = /<img\b[^>]*>/gi;

function getAttribute(tag: string, name: string): string | null {
  const escaped = name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const match = tag.match(
    new RegExp(`${escaped}\\s*=\\s*(?:"([^"]*)"|'([^']*)'|([^\\s>]+))`, "i"),
  );
  return match ? (match[1] ?? match[2] ?? match[3] ?? "") : null;
}

function stripAttributes(tag: string, pattern: RegExp): string {
  return tag.replace(pattern, "");
}

const DATA_ATTRIBUTES =
  /\sdata-[\w-]+(?:\s*=\s*(?:"[^"]*"|'[^']*'|[^\s>]+))?/gi;
const EDITOR_ATTRIBUTES =
  /\s(?:contenteditable|spellcheck)(?:\s*=\s*(?:"[^"]*"|'[^']*'|[^\s>]+))?/gi;

/** Remove editor-only metadata so it never reaches the recipient. */
export function stripEditorArtifacts(html: string): string {
  return html
    .replace(DATA_ATTRIBUTES, "")
    .replace(EDITOR_ATTRIBUTES, "");
}

/**
 * Convert `<img src="data:…" data-content-id="x">` into the wire format
 * `src="cid:x"` and drop the temporary attribute. Already-cid images are
 * returned unchanged, so the function is idempotent.
 */
export function serializeInlineImages(html: string): string {
  return html.replace(IMG_TAG, (tag) => {
    const contentId = getAttribute(tag, "data-content-id");
    if (!contentId) return tag;
    const withCid = tag.replace(
      /\bsrc\s*=\s*(?:"[^"]*"|'[^']*'|[^\s>]+)/i,
      `src="cid:${escapeAttribute(contentId)}"`,
    );
    return stripAttributes(withCid, DATA_ATTRIBUTES);
  });
}

function cssFontFamily(fontFamily: string): string {
  const cleaned = fontFamily.replace(/["'<>{}\\]/g, "").trim();
  if (!cleaned) return "sans-serif";
  return `'${cleaned}'`;
}

export interface FinalizeOptions {
  fontFamily?: string | null;
  fontSize?: number | null;
}

/**
 * Produce the HTML stored in a Draft / sent over SMTP:
 *  - inline images referenced as cid:<contentId>,
 *  - no editor-only data attributes,
 *  - the default font applied with an inline style so recipients see it.
 */
export function finalizeHtml(html: string, options: FinalizeOptions = {}): string {
  const cleaned = stripEditorArtifacts(serializeInlineImages(html));
  const family = options.fontFamily?.trim();
  const size =
    options.fontSize && Number.isFinite(options.fontSize) && options.fontSize > 0
      ? Math.round(options.fontSize)
      : null;
  const styles: string[] = [];
  if (family) styles.push(`font-family:${cssFontFamily(family)}`);
  if (size) styles.push(`font-size:${size}px`);
  if (styles.length === 0) return cleaned;
  return `<div style="${styles.join(";")};">${cleaned}</div>`;
}

const PLACEHOLDER_SVG =
  'data:image/svg+xml;charset=utf-8,' +
  encodeURIComponent(
    '<svg xmlns="http://www.w3.org/2000/svg" width="360" height="120" viewBox="0 0 360 120">' +
      '<rect width="360" height="120" rx="8" fill="#EDEBE9"/>' +
      '<path d="M168 44h24a4 4 0 0 1 4 4v24a4 4 0 0 1-4 4h-24a4 4 0 0 1-4-4V48a4 4 0 0 1 4-4Zm0 24 8-8 6 6 4-4 10 10h-28Z" fill="#8A8886"/>' +
      "</svg>",
  );

/**
 * The default-font wrapper is a composer implementation detail; strip it when
 * a draft is reopened so saving does not nest wrappers indefinitely.
 */
const DEFAULT_FONT_WRAPPER =
  /^\s*<div style="font-family:'[^"']*'(?:;font-size:\d+px)?;">([\s\S]*)<\/div>\s*$/i;

export function stripDefaultFontWrapper(html: string): string {
  const match = html.match(DEFAULT_FONT_WRAPPER);
  return match ? match[1] : html;
}

/**
 * Rewrite cid images in a stored draft into a visible placeholder while the
 * editor is open (the staged file cannot be loaded directly: the asset
 * protocol is disabled). The `data-content-id` attribute lets the serializer
 * restore `cid:` on save/send.
 */
export function prepareEditorHtml(
  html: string,
  attachments: DraftAttachment[],
): string {
  if (!html) return "";
  const unwrapped = stripDefaultFontWrapper(html);
  const byContentId = new Map(
    attachments
      .filter((item) => item.contentId)
      .map((item) => [item.contentId as string, item]),
  );
  return unwrapped.replace(IMG_TAG, (tag) => {
    const src = getAttribute(tag, "src");
    if (!src || !/^cid:/i.test(src)) return tag;
    let contentId = src.slice(4);
    try {
      contentId = decodeURIComponent(contentId);
    } catch {
      // keep the raw value
    }
    const attachment = byContentId.get(contentId);
    let next = tag.replace(
      /\bsrc\s*=\s*(?:"[^"]*"|'[^']*'|[^\s>]+)/i,
      `src="${PLACEHOLDER_SVG}"`,
    );
    next = next.replace(/\salt\s*=\s*(?:"[^"]*"|'[^']*'|[^\s>]+)/i, "");
    next = next.replace(
      /<img\b/i,
      `<img alt="${escapeAttribute(attachment?.filename ?? "inline image")}"`,
    );
    next = next.replace(/^<img\b/i, `<img data-content-id="${escapeAttribute(contentId)}"`);
    return next;
  });
}

export function escapeHtml(value: string): string {
  return value
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

export function escapeAttribute(value: string): string {
  return escapeHtml(value).replace(/'/g, "&#39;");
}

/**
 * Normalize a link typed into the link dialog. Returns null for dangerous or
 * empty input. Bare hosts get an https:// prefix.
 */
export function normalizeLinkUrl(raw: string): string | null {
  const value = raw.trim();
  if (!value) return null;
  if (/^(?:javascript|data|vbscript|file):/i.test(value)) return null;
  if (/^(?:https?|mailto|tel):/i.test(value)) return value;
  if (/^[a-z][a-z0-9+.-]*:\/\//i.test(value)) return value;
  return `https://${value}`;
}

/** Plain-text body used for the attachment keyword check. */
export function plainTextFromHtml(html: string): string {
  return html
    .replace(/<(?:br|p|div|li|tr|h[1-6])\b[^>]*>/gi, "\n")
    .replace(/<[^>]+>/g, " ")
    .replace(/&nbsp;/gi, " ")
    .replace(/&amp;/gi, "&")
    .replace(/&lt;/gi, "<")
    .replace(/&gt;/gi, ">")
    .replace(/&quot;/gi, '"')
    .replace(/&#39;/gi, "'")
    .replace(/\s+/g, " ")
    .trim();
}

export type RecipientField = "to" | "cc" | "bcc";

/** All non-empty recipient fields of a draft. */
export function allRecipients(draft: Pick<Draft, "to" | "cc" | "bcc">): Address[] {
  return [...draft.to, ...draft.cc, ...draft.bcc];
}

export function recipientCount(draft: Pick<Draft, "to" | "cc" | "bcc">): number {
  return draft.to.length + draft.cc.length + draft.bcc.length;
}
