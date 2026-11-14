/**
 * Pure helpers for the People module: alphabetical sectioning (including a
 * lightweight pinyin-initial heuristic for Chinese names), sorting according
 * to the contacts settings, contact name formatting, draft conversion and
 * validation. No React and no IPC here so everything is unit-testable.
 */
import type {
  Address,
  AddressBook,
  Contact,
  ContactField,
  ContactFieldType,
  ContactSummary,
  PostalAddress,
} from "@/ipc/types";

/** How many contacts are requested per page from `contact_list`. */
export const CONTACTS_PAGE_SIZE = 200;

export type ContactSortBy = "firstName" | "lastName";
export type ContactDisplayOrder = "firstLast" | "lastFirst";

export interface PeopleListOptions {
  sortBy: ContactSortBy;
  displayOrder: ContactDisplayOrder;
  locale: string;
}

/** Alias used by the list component. */
export type ContactListOptions = PeopleListOptions;

export const DEFAULT_PEOPLE_OPTIONS: PeopleListOptions = {
  sortBy: "firstName",
  displayOrder: "firstLast",
  locale: "zh-CN",
};

export const CONTACT_FIELD_TYPES: ContactFieldType[] = [
  "home",
  "work",
  "mobile",
  "fax",
  "pager",
  "main",
  "other",
];

/* ─────────────────────────────── Names ─────────────────────────────── */

const CJK_RE = /[㐀-䶿一-鿿豈-﫿]/u;

export function isCjk(value: string | null | undefined): boolean {
  return Boolean(value && CJK_RE.test(value));
}

/** Split a display name into words, respecting CJK (no spaces needed). */
function words(value: string): string[] {
  return value
    .trim()
    .split(/\s+/u)
    .filter(Boolean);
}

/**
 * name used for sorting and sectioning. `lastName` moves the last word to the
 * front ("John Smith" -> "Smith John"); CJK names have no spaces and are left
 * untouched (surname already comes first).
 */
export function nameSortKey(
  displayName: string,
  sortBy: ContactSortBy = "firstName",
): string {
  const parts = words(displayName);
  if (sortBy === "lastName" && parts.length > 1) {
    const last = parts[parts.length - 1];
    return [last, ...parts.slice(0, -1)].join(" ");
  }
  return displayName.trim();
}

/** Join name parts with a space, unless every part is CJK. */
function joinNameParts(parts: string[]): string {
  const filtered = parts.map((part) => part.trim()).filter(Boolean);
  if (filtered.length === 0) return "";
  if (filtered.every(isCjk)) return filtered.join("");
  return filtered.join(" ");
}

/** Compose a human name from the structured fields, honoring display order. */
function composeStructuredName(
  contact: { givenName: string; familyName: string; middleName?: string },
  displayOrder: ContactDisplayOrder,
): string {
  const given = contact.givenName.trim();
  const family = contact.familyName.trim();
  if (!given && !family) return "";
  const middle = (contact.middleName ?? "").trim();
  // Chinese names keep the surname first in both display orders.
  const cjkPair = isCjk(given) && isCjk(family);
  const parts = cjkPair
    ? [family, given]
    : displayOrder === "lastFirst"
      ? [family, given, middle]
      : [given, middle, family];
  return joinNameParts(parts);
}

/** True when `displayName` is a user override, not an auto-composed name. */
export function isCustomDisplayName(contact: {
  displayName: string;
  givenName: string;
  familyName: string;
  middleName?: string;
}): boolean {
  const display = contact.displayName.trim();
  if (!display) return false;
  if (!contact.givenName.trim() && !contact.familyName.trim()) return false;
  return (
    display !== composeStructuredName(contact, "firstLast") &&
    display !== composeStructuredName(contact, "lastFirst")
  );
}

export function formatContactName(
  contact: {
    displayName: string;
    givenName: string;
    familyName: string;
    middleName?: string;
    company?: string;
    emails?: ContactField[];
  },
  displayOrder: ContactDisplayOrder = "firstLast",
): string {
  const structured = composeStructuredName(contact, displayOrder);
  if (structured) {
    // An explicitly different display name wins over the composed one.
    return isCustomDisplayName(contact) ? contact.displayName.trim() : structured;
  }
  const display = contact.displayName.trim();
  if (display) return display;
  const company = (contact.company ?? "").trim();
  if (company) return company;
  const email = contact.emails?.find((field) => field.value.trim())?.value.trim();
  return email ?? "";
}

/** List rows only know `displayName`; reorder the words when requested. */
export function formatSummaryName(
  displayName: string,
  displayOrder: ContactDisplayOrder = "firstLast",
): string {
  const trimmed = displayName.trim();
  if (displayOrder !== "lastFirst") return trimmed;
  const parts = words(trimmed);
  if (parts.length < 2) return trimmed;
  const joined = [parts[parts.length - 1], ...parts.slice(0, -1)].join(" ");
  return isCjk(trimmed) ? trimmed : joined;
}

/* ─────────────────────── Alphabetical sectioning ───────────────────── */

/**
 * Representative characters for the pinyin initials. Comparing a Chinese
 * character against these with `Intl.Collator("zh-CN")` yields its pinyin
 * initial as long as the runtime ships pinyin collation (V8/ICU does).
 * Lighter than a pinyin dictionary and good enough for A–Z grouping.
 */
const PINYIN_ANCHORS: ReadonlyArray<readonly [string, string]> = [
  ["A", "阿"],
  ["B", "八"],
  ["C", "擦"],
  ["D", "搭"],
  ["E", "蛾"],
  ["F", "发"],
  ["G", "嘎"],
  ["H", "哈"],
  ["J", "击"],
  ["K", "咖"],
  ["L", "拉"],
  ["M", "妈"],
  ["N", "拿"],
  ["O", "哦"],
  ["P", "趴"],
  ["Q", "七"],
  ["R", "然"],
  ["S", "撒"],
  ["T", "塌"],
  ["W", "挖"],
  ["X", "昔"],
  ["Y", "压"],
  ["Z", "匝"],
];

export const OTHER_SECTION = "#";
export const CJK_UNKNOWN_SECTION = "中";
export const LETTER_SECTIONS = "ABCDEFGHIJKLMNOPQRSTUVWXYZ".split("");

let pinyinCollator: Intl.Collator | null = null;
let pinyinSupported: boolean | null = null;

function getPinyinCollator(): Intl.Collator {
  if (!pinyinCollator) {
    pinyinCollator = new Intl.Collator("zh-CN", { usage: "sort" });
  }
  return pinyinCollator;
}

/** True when the ICU collator actually sorts Chinese by pinyin. */
export function supportsPinyinCollation(): boolean {
  if (pinyinSupported !== null) return pinyinSupported;
  try {
    const collator = getPinyinCollator();
    // Chen < Li < Wang < Zhang in pinyin order; codepoint order differs.
    const ordered =
      collator.compare("陈", "李") < 0 &&
      collator.compare("李", "王") < 0 &&
      collator.compare("王", "张") < 0;
    pinyinSupported = ordered;
  } catch {
    pinyinSupported = false;
  }
  return pinyinSupported;
}

/** Pinyin initial for a single Chinese character, or null when unknown. */
export function pinyinInitial(char: string): string | null {
  if (!isCjk(char)) return null;
  if (!supportsPinyinCollation()) return CJK_UNKNOWN_SECTION;
  const collator = getPinyinCollator();
  let found: string | null = null;
  for (const [letter, anchor] of PINYIN_ANCHORS) {
    if (collator.compare(char, anchor) >= 0) found = letter;
    else break;
  }
  return found ?? OTHER_SECTION;
}

/** First meaningful character of a name (skips spaces and punctuation). */
export function firstMeaningfulChar(value: string): string {
  const chars = Array.from(value.trim());
  for (const char of chars) {
    if (/[\p{L}\p{N}]/u.test(char)) return char;
  }
  return chars[0] ?? "";
}

/**
 * Section label for a contact name: A–Z for Latin and (when the runtime
 * supports it) pinyin initials of Chinese names, "#" for everything else.
 */
export function sectionLetter(
  displayName: string,
  sortBy: ContactSortBy = "firstName",
): string {
  const key = nameSortKey(displayName, sortBy);
  const char = firstMeaningfulChar(key);
  if (!char) return OTHER_SECTION;
  // Strip diacritics so "Émile" lands under E, like Windows Contacts.
  const base = char.normalize("NFD").replace(/[\u0300-\u036f]/gu, "");
  if (/[a-z]/iu.test(base)) return base.toUpperCase();
  if (isCjk(char)) return pinyinInitial(char) ?? CJK_UNKNOWN_SECTION;
  return OTHER_SECTION;
}

export function sectionRank(label: string): number {
  if (label === OTHER_SECTION) return 1000;
  if (label === CJK_UNKNOWN_SECTION) return 990;
  if (/^[A-Z]$/.test(label)) return label.charCodeAt(0);
  return 995;
}

export function getCollator(locale: string): Intl.Collator {
  try {
    return new Intl.Collator(locale, { usage: "sort", sensitivity: "base", numeric: true });
  } catch {
    return new Intl.Collator("en", { usage: "sort", sensitivity: "base", numeric: true });
  }
}

export function compareContacts(
  a: ContactSummary,
  b: ContactSummary,
  options: Pick<PeopleListOptions, "sortBy" | "locale">,
): number {
  const collator = getCollator(options.locale);
  const keyA = nameSortKey(a.displayName, options.sortBy);
  const keyB = nameSortKey(b.displayName, options.sortBy);
  const primary = collator.compare(keyA, keyB);
  if (primary !== 0) return primary;
  return collator.compare(a.displayName, b.displayName);
}

export type PeopleRow =
  | { kind: "header"; key: string; label: string; count: number }
  | { kind: "contact"; key: string; contact: ContactSummary; index: number };

/** Sort contacts and interleave A–Z / # section headers. */
export function buildPeopleRows(
  contacts: ContactSummary[],
  options: PeopleListOptions,
): PeopleRow[] {
  const labels = new Map<string, string>();
  for (const contact of contacts) {
    labels.set(contact.id, sectionLetter(contact.displayName, options.sortBy));
  }
  const sorted = [...contacts].sort((a, b) => {
    const rankA = sectionRank(labels.get(a.id) ?? OTHER_SECTION);
    const rankB = sectionRank(labels.get(b.id) ?? OTHER_SECTION);
    if (rankA !== rankB) return rankA - rankB;
    return compareContacts(a, b, options);
  });
  const rows: PeopleRow[] = [];
  let currentLabel: string | null = null;
  let header: Extract<PeopleRow, { kind: "header" }> | null = null;
  let contactIndex = 0;
  for (const contact of sorted) {
    const label = sectionLetter(contact.displayName, options.sortBy);
    if (label !== currentLabel) {
      currentLabel = label;
      header = { kind: "header", key: `h:${label}`, label, count: 0 };
      rows.push(header);
    }
    header!.count += 1;
    rows.push({
      kind: "contact",
      key: `c:${contact.id}`,
      contact,
      index: contactIndex,
    });
    contactIndex += 1;
  }
  return rows;
}

/* ─────────────────────────── Primary fields ─────────────────────────── */

export function primaryField(fields: ContactField[] | null | undefined): ContactField | null {
  const values = (fields ?? []).filter((field) => field.value.trim());
  if (values.length === 0) return null;
  return values.find((field) => field.isPrimary) ?? values[0];
}

export function primaryEmail(contact: Pick<Contact, "emails">): string | null {
  return primaryField(contact.emails)?.value.trim() ?? null;
}

export function primaryPhone(contact: Pick<Contact, "phones">): string | null {
  return primaryField(contact.phones)?.value.trim() ?? null;
}

/** Recipient for composing a mail to this contact, or null without e-mail. */
export function contactRecipient(
  contact: Pick<Contact, "displayName" | "givenName" | "familyName" | "emails">,
  displayOrder: ContactDisplayOrder = "firstLast",
): Address | null {
  const email = primaryEmail(contact);
  if (!email) return null;
  const name = formatContactName(contact, displayOrder).trim();
  return { name: name || null, email };
}

/* ────────────────────────────── Addresses ──────────────────────────── */

export function addressLines(address: PostalAddress): string[] {
  const street = address.street.trim();
  const locality = [address.city, address.region].map((part) => part.trim()).filter(Boolean).join(", ");
  const postal = [address.postalCode.trim(), address.country.trim()].filter(Boolean).join(" ");
  return [street, locality, postal].filter(Boolean);
}

export function addressSingleLine(address: PostalAddress): string {
  return addressLines(address).join(", ");
}

export function addressMapUrl(address: PostalAddress): string | null {
  const query = addressSingleLine(address);
  if (!query) return null;
  return `https://www.bing.com/maps?q=${encodeURIComponent(query)}`;
}

/** Explicit scheme prefix, e.g. `https:`, `file:`, `javascript:`. */
const SCHEME_RE = /^([a-z][a-z0-9+.-]*):/iu;
/** Host with an explicit port, e.g. `example.com:8080` or `localhost:3000`. */
const HOST_PORT_RE =
  /^(?:(?:[a-z0-9-]+\.)+[a-z]{2,}|localhost|\d{1,3}(?:\.\d{1,3}){3}):\d+(?:[/?#].*)?$/iu;
/** Characters that can never appear in a URL we hand to the OS handler. */
const UNSAFE_URL_CHARS_RE = /[\s\u0000-\u001f\u007f]/u;

/**
 * Normalize an http(s) URL: bare domains get `https://`, protocol-relative
 * values get an `https:` prefix and explicit http(s) schemes are kept.
 * Everything else (`file:`, `javascript:`, `data:`, custom app schemes, …)
 * returns null so a synced vCard can never launch an arbitrary handler.
 */
function normalizeHttpUrl(value: string): string | null {
  const trimmed = value.trim();
  if (!trimmed || UNSAFE_URL_CHARS_RE.test(trimmed)) return null;

  let candidate: string;
  const schemeMatch = SCHEME_RE.exec(trimmed);
  if (schemeMatch) {
    const scheme = schemeMatch[1]?.toLowerCase() ?? "";
    if (scheme === "http" || scheme === "https") {
      const rest = trimmed.slice(scheme.length + 1).replace(/^\/+/u, "");
      if (!rest) return null;
      candidate = `${scheme}://${rest}`;
    } else if (HOST_PORT_RE.test(trimmed)) {
      // "example.com:8080" parses as a scheme prefix, but is a host + port.
      candidate = `https://${trimmed}`;
    } else {
      return null;
    }
  } else {
    candidate = trimmed.startsWith("//") ? `https:${trimmed}` : `https://${trimmed}`;
  }

  try {
    const parsed = new URL(candidate);
    if (parsed.protocol !== "http:" && parsed.protocol !== "https:") return null;
    if (!parsed.hostname) return null;
    return candidate;
  } catch {
    return null;
  }
}

/**
 * Normalize a URL for the OS opener, restricted to http(s) (and optionally
 * `mailto:`). Returns null for anything else.
 */
export function safeExternalUrl(
  value: string | null | undefined,
  options: { allowMailto?: boolean } = {},
): string | null {
  const trimmed = (value ?? "").trim();
  if (!trimmed || UNSAFE_URL_CHARS_RE.test(trimmed)) return null;
  if (/^mailto:/iu.test(trimmed)) {
    if (!options.allowMailto) return null;
    try {
      const parsed = new URL(trimmed);
      return parsed.protocol === "mailto:" ? trimmed : null;
    } catch {
      return null;
    }
  }
  return normalizeHttpUrl(trimmed);
}

/**
 * Href for a contact website. Only http(s) is accepted; unsupported or
 * malformed values return "" so callers can render plain text instead.
 */
export function websiteHref(value: string): string {
  return normalizeHttpUrl(value) ?? "";
}

export function isValidWebsite(value: string): boolean {
  const trimmed = value.trim();
  if (!trimmed) return true;
  if (/\s/.test(trimmed)) return false;
  return /^(https?:\/\/)?[\w-]+(\.[\w-]+)+([/?#][^\s]*)?$/iu.test(trimmed);
}

/* ────────────────────────────── Birthday ───────────────────────────── */

export interface BirthdayParts {
  /** null when the birth year is unknown. */
  year: number | null;
  month: number;
  day: number;
}

/**
 * Parse "YYYY-MM-DD" or "--MM-DD". Returns null for empty or invalid values.
 * A year is optional by design (Outlook permits year-less birthdays).
 */
export function parseBirthday(value: string | null | undefined): BirthdayParts | null {
  if (!value) return null;
  const full = value.match(/^(\d{4})-(\d{2})-(\d{2})$/u);
  const withoutYear = value.match(/^--(\d{2})-(\d{2})$/u);
  let year: number | null;
  let month: number;
  let day: number;
  if (full) {
    year = Number(full[1]);
    month = Number(full[2]);
    day = Number(full[3]);
  } else if (withoutYear) {
    year = null;
    month = Number(withoutYear[1]);
    day = Number(withoutYear[2]);
  } else {
    return null;
  }
  if (month < 1 || month > 12) return null;
  if (day < 1 || day > daysInMonth(year, month)) return null;
  if (year !== null && (year < 1000 || year > 9999)) return null;
  return { year, month, day };
}

export function isValidBirthday(value: string | null | undefined): boolean {
  if (!value) return true;
  return parseBirthday(value) !== null;
}

/** Days in a month; a null year uses a leap year so Feb 29 is allowed. */
export function daysInMonth(year: number | null, month: number): number {
  if (month < 1 || month > 12) return 0;
  const resolvedYear = year ?? 2000;
  return new Date(resolvedYear, month, 0).getDate();
}

export function joinBirthday(
  year: string,
  month: string,
  day: string,
): string | null {
  const monthNumber = Number(month);
  const dayNumber = Number(day);
  if (!month || !day || !Number.isInteger(monthNumber) || !Number.isInteger(dayNumber)) {
    return null;
  }
  if (monthNumber < 1 || monthNumber > 12) return null;
  if (dayNumber < 1 || dayNumber > daysInMonth(year ? Number(year) : null, monthNumber)) {
    return null;
  }
  const mm = String(monthNumber).padStart(2, "0");
  const dd = String(dayNumber).padStart(2, "0");
  if (!year) return `--${mm}-${dd}`;
  const yearNumber = Number(year);
  if (!Number.isInteger(yearNumber) || yearNumber < 1000 || yearNumber > 9999) return null;
  return `${yearNumber}-${mm}-${dd}`;
}

/** Localized birthday, omitting the year when it is unknown. */
export function formatBirthday(
  value: string | null | undefined,
  locale: string,
): string {
  const parts = parseBirthday(value);
  if (!parts) return "";
  try {
    const date = new Date(parts.year ?? 2004, parts.month - 1, parts.day, 12, 0, 0, 0);
    const formatter = new Intl.DateTimeFormat(locale, {
      ...(parts.year !== null ? { year: "numeric" as const } : {}),
      month: "long",
      day: "numeric",
    });
    return formatter.format(date);
  } catch {
    return value ?? "";
  }
}

/* ─────────────────────────────── Drafts ────────────────────────────── */

/**
 * Editable form state. Mirrors `Contact` but tolerates empty rows while the
 * user is typing; `draftToContact` normalizes it for the backend.
 */
export interface ContactDraft {
  id: string;
  addressBookId: string;
  accountId: string;
  displayName: string;
  givenName: string;
  familyName: string;
  middleName: string;
  nickname: string;
  company: string;
  jobTitle: string;
  department: string;
  emails: ContactField[];
  phones: ContactField[];
  addresses: PostalAddress[];
  websites: string[];
  birthday: string | null;
  notes: string;
  photo: string | null;
  isFavorite: boolean;
  groupIds: string[];
  updatedAt: string | null;
}

export function emptyField(type: ContactFieldType = "home"): ContactField {
  return { type, value: "", isPrimary: false };
}

export function emptyAddress(type: ContactFieldType = "home"): PostalAddress {
  return { type, street: "", city: "", region: "", postalCode: "", country: "" };
}

export function emptyDraft(
  addressBook: Pick<AddressBook, "id" | "accountId"> | null,
): ContactDraft {
  return {
    id: "",
    addressBookId: addressBook?.id ?? "",
    accountId: addressBook?.accountId ?? "",
    displayName: "",
    givenName: "",
    familyName: "",
    middleName: "",
    nickname: "",
    company: "",
    jobTitle: "",
    department: "",
    emails: [{ ...emptyField("home"), isPrimary: true }],
    phones: [emptyField("mobile")],
    addresses: [],
    websites: [],
    birthday: null,
    notes: "",
    photo: null,
    isFavorite: false,
    groupIds: [],
    updatedAt: null,
  };
}

export function draftFromContact(contact: Contact): ContactDraft {
  return {
    id: contact.id,
    addressBookId: contact.addressBookId,
    accountId: contact.accountId,
    displayName: isCustomDisplayName(contact) ? contact.displayName : "",
    givenName: contact.givenName,
    familyName: contact.familyName,
    middleName: contact.middleName,
    nickname: contact.nickname,
    company: contact.company,
    jobTitle: contact.jobTitle,
    department: contact.department,
    emails: contact.emails.map((field) => ({ ...field })),
    phones: contact.phones.map((field) => ({ ...field })),
    addresses: contact.addresses.map((address) => ({ ...address })),
    websites: [...contact.websites],
    birthday: contact.birthday,
    notes: contact.notes,
    photo: contact.photo,
    isFavorite: contact.isFavorite,
    groupIds: [...contact.groupIds],
    updatedAt: contact.updatedAt,
  };
}

function cleanFields(fields: ContactField[]): ContactField[] {
  const cleaned = fields
    .map((field) => ({ ...field, value: field.value.trim() }))
    .filter((field) => field.value.length > 0);
  let primaries = cleaned.filter((field) => field.isPrimary);
  if (primaries.length === 0 && cleaned.length > 0) cleaned[0].isPrimary = true;
  if (primaries.length > 1) {
    const keep = primaries[0];
    for (const field of cleaned) field.isPrimary = field === keep;
  }
  return cleaned;
}

function cleanAddresses(addresses: PostalAddress[]): PostalAddress[] {
  return addresses
    .map((address) => ({
      type: address.type,
      street: address.street.trim(),
      city: address.city.trim(),
      region: address.region.trim(),
      postalCode: address.postalCode.trim(),
      country: address.country.trim(),
    }))
    .filter(
      (address) =>
        address.street ||
        address.city ||
        address.region ||
        address.postalCode ||
        address.country,
    );
}

/** Normalize the form state into a `Contact` ready for `contact_create/update`. */
export function draftToContact(draft: ContactDraft): Contact {
  const emails = cleanFields(draft.emails);
  const phones = cleanFields(draft.phones);
  const websites = draft.websites.map((value) => value.trim()).filter(Boolean);
  const composed = formatContactName(
    {
      displayName: draft.displayName,
      givenName: draft.givenName,
      familyName: draft.familyName,
      middleName: draft.middleName,
      company: draft.company,
    },
    "firstLast",
  );
  const displayName =
    draft.displayName.trim() ||
    composed ||
    draft.nickname.trim() ||
    emails[0]?.value ||
    "";
  return {
    id: draft.id,
    addressBookId: draft.addressBookId,
    accountId: draft.accountId,
    displayName,
    givenName: draft.givenName.trim(),
    familyName: draft.familyName.trim(),
    middleName: draft.middleName.trim(),
    nickname: draft.nickname.trim(),
    company: draft.company.trim(),
    jobTitle: draft.jobTitle.trim(),
    department: draft.department.trim(),
    emails,
    phones,
    addresses: cleanAddresses(draft.addresses),
    websites,
    birthday: draft.birthday,
    notes: draft.notes.trim(),
    photo: draft.photo,
    isFavorite: draft.isFavorite,
    groupIds: [...new Set(draft.groupIds)],
    updatedAt: draft.updatedAt,
  };
}

export interface DraftValidation {
  /** Name fields, display name or at least one e-mail is present. */
  hasIdentity: boolean;
  invalidEmailIndexes: number[];
  invalidWebsiteIndexes: number[];
  birthdayInvalid: boolean;
  valid: boolean;
}

const EMAIL_RE = /^[^\s@<>()",;:]+@[^\s@<>()",;:]+\.[^\s@<>()",;:]+$/u;

export function isValidEmailAddress(value: string): boolean {
  return EMAIL_RE.test(value.trim());
}

export function validateDraft(draft: ContactDraft): DraftValidation {
  const hasName = Boolean(
    draft.displayName.trim() ||
      draft.givenName.trim() ||
      draft.familyName.trim() ||
      draft.middleName.trim() ||
      draft.nickname.trim(),
  );
  const invalidEmailIndexes: number[] = [];
  draft.emails.forEach((field, index) => {
    const value = field.value.trim();
    if (value && !isValidEmailAddress(value)) invalidEmailIndexes.push(index);
  });
  const hasEmail = draft.emails.some((field) => field.value.trim());
  const invalidWebsiteIndexes: number[] = [];
  draft.websites.forEach((value, index) => {
    if (!isValidWebsite(value)) invalidWebsiteIndexes.push(index);
  });
  const birthdayInvalid = !isValidBirthday(draft.birthday);
  return {
    hasIdentity: hasName || hasEmail,
    invalidEmailIndexes,
    invalidWebsiteIndexes,
    birthdayInvalid,
    valid:
      (hasName || hasEmail) &&
      invalidEmailIndexes.length === 0 &&
      invalidWebsiteIndexes.length === 0 &&
      !birthdayInvalid,
  };
}

/* ────────────────────────────── Groups ─────────────────────────────── */

/** Address book owning `addressBookId`, or null when it is unknown. */
export function findAddressBook(
  books: ReadonlyArray<Pick<AddressBook, "id" | "isReadOnly">>,
  addressBookId: string | null | undefined,
): Pick<AddressBook, "id" | "isReadOnly"> | null {
  if (!addressBookId) return null;
  return books.find((book) => book.id === addressBookId) ?? null;
}

/**
 * True when the given address book is read-only. Unknown books are treated as
 * writable so a stale list cannot silently block editing.
 */
export function isReadOnlyBook(
  books: ReadonlyArray<Pick<AddressBook, "id" | "isReadOnly">>,
  addressBookId: string | null | undefined,
): boolean {
  return findAddressBook(books, addressBookId)?.isReadOnly === true;
}

/**
 * Split ids into the ones that may be modified (their address book is
 * writable) and the ones that belong to read-only address books.
 */
export function partitionWritable<T extends { id: string; addressBookId: string }>(
  items: ReadonlyArray<T>,
  books: ReadonlyArray<Pick<AddressBook, "id" | "isReadOnly">>,
  ids: ReadonlyArray<string>,
): { writable: string[]; readOnly: string[] } {
  const readOnlyBooks = new Set(
    books.filter((book) => book.isReadOnly).map((book) => book.id),
  );
  const byId = new Map(items.map((item) => [item.id, item]));
  const writable: string[] = [];
  const readOnly: string[] = [];
  for (const id of ids) {
    const item = byId.get(id);
    if (!item || readOnlyBooks.has(item.addressBookId)) readOnly.push(id);
    else writable.push(id);
  }
  return { writable, readOnly };
}

export function groupNameFor(
  groupIds: string[],
  groups: ReadonlyArray<{ id: string; name: string }>,
): string[] {
  const names: string[] = [];
  for (const id of groupIds) {
    const group = groups.find((candidate) => candidate.id === id);
    if (group) names.push(group.name);
  }
  return names;
}

export function toggleId(ids: string[], id: string): string[] {
  return ids.includes(id) ? ids.filter((candidate) => candidate !== id) : [...ids, id];
}

/* ────────────────────────────── Sect. order ────────────────────────── */

/** Ordered section labels produced by `buildPeopleRows`, for the rail. */
export function sectionsOf(rows: PeopleRow[]): string[] {
  return rows
    .filter((row): row is Extract<PeopleRow, { kind: "header" }> => row.kind === "header")
    .map((row) => row.label);
}
