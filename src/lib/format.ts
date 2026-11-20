/**
 * Formatting helpers shared by every module: dates in the Windows Mail list
 * style (today -> time, this week -> weekday, older -> date), sizes, addresses
 * and avatars. All Intl formatters are cached per locale/style.
 */
import type { Address } from "@/ipc/types";
import { getLocale } from "@/lib/i18n";

export interface DateFormatOptions {
  /** Reference point for relative formats (testable). */
  now?: Date;
  /** BCP-47 locale; defaults to the active UI language. */
  locale?: string;
  /** 0 = Sunday, 1 = Monday (default). */
  weekStartsOn?: number;
}

const timeFormatters = new Map<string, Intl.DateTimeFormat>();
const dateFormatters = new Map<string, Intl.DateTimeFormat>();
const dateTimeFormatters = new Map<string, Intl.DateTimeFormat>();
const weekdayFormatters = new Map<string, Intl.DateTimeFormat>();
const monthYearFormatters = new Map<string, Intl.DateTimeFormat>();

function timeFormatter(locale: string): Intl.DateTimeFormat {
  let f = timeFormatters.get(locale);
  if (!f) {
    f = new Intl.DateTimeFormat(locale, {
      hour: "2-digit",
      minute: "2-digit",
      hourCycle: "h23",
    });
    timeFormatters.set(locale, f);
  }
  return f;
}

function dateFormatter(locale: string, withYear: boolean): Intl.DateTimeFormat {
  const key = `${locale}:${withYear}`;
  let f = dateFormatters.get(key);
  if (!f) {
    f = new Intl.DateTimeFormat(locale, {
      month: "short",
      day: "numeric",
      ...(withYear ? { year: "numeric" } : {}),
    });
    dateFormatters.set(key, f);
  }
  return f;
}

function dateTimeFormatter(locale: string): Intl.DateTimeFormat {
  let f = dateTimeFormatters.get(locale);
  if (!f) {
    f = new Intl.DateTimeFormat(locale, {
      year: "numeric",
      month: "short",
      day: "numeric",
      hour: "2-digit",
      minute: "2-digit",
      hourCycle: "h23",
    });
    dateTimeFormatters.set(locale, f);
  }
  return f;
}

function weekdayFormatter(locale: string, style: "short" | "long"): Intl.DateTimeFormat {
  const key = `${locale}:${style}`;
  let f = weekdayFormatters.get(key);
  if (!f) {
    f = new Intl.DateTimeFormat(locale, { weekday: style });
    weekdayFormatters.set(key, f);
  }
  return f;
}

function monthYearFormatter(locale: string, withYear: boolean): Intl.DateTimeFormat {
  const key = `${locale}:${withYear}`;
  let f = monthYearFormatters.get(key);
  if (!f) {
    f = new Intl.DateTimeFormat(locale, {
      month: "long",
      ...(withYear ? { year: "numeric" } : {}),
    });
    monthYearFormatters.set(key, f);
  }
  return f;
}

function toDate(value: string | Date): Date | null {
  if (value instanceof Date) return Number.isNaN(value.getTime()) ? null : value;
  if (!value) return null;
  const parsed = new Date(value);
  return Number.isNaN(parsed.getTime()) ? null : parsed;
}

export function isSameDay(a: Date, b: Date): boolean {
  return (
    a.getFullYear() === b.getFullYear() &&
    a.getMonth() === b.getMonth() &&
    a.getDate() === b.getDate()
  );
}

/** Start of the calendar week containing `date`, in local time. */
export function startOfWeek(date: Date, weekStartsOn = 1): Date {
  const result = new Date(date.getFullYear(), date.getMonth(), date.getDate());
  const day = result.getDay();
  const diff = (day - weekStartsOn + 7) % 7;
  result.setDate(result.getDate() - diff);
  return result;
}

export function addDays(date: Date, days: number): Date {
  const result = new Date(date.getTime());
  result.setDate(result.getDate() + days);
  return result;
}

/** True when `date` is inside the calendar week containing `now`. */
export function isThisWeek(date: Date, now: Date, weekStartsOn = 1): boolean {
  const start = startOfWeek(now, weekStartsOn);
  const end = addDays(start, 7);
  return date >= start && date < end;
}

/** "14:05" — 24-hour clock, locale ordered. */
export function formatTime(value: string | Date, opts?: DateFormatOptions): string {
  const date = toDate(value);
  if (!date) return typeof value === "string" ? value : "";
  return timeFormatter(opts?.locale ?? getLocale()).format(date);
}

/** "Nov 12" / "2025年11月12日". */
export function formatDate(value: string | Date, opts?: DateFormatOptions): string {
  const date = toDate(value);
  if (!date) return typeof value === "string" ? value : "";
  const locale = opts?.locale ?? getLocale();
  const now = opts?.now ?? new Date();
  const withYear = date.getFullYear() !== now.getFullYear();
  return dateFormatter(locale, withYear).format(date);
}

/** Date + time, always with the year. */
export function formatDateTime(value: string | Date, opts?: DateFormatOptions): string {
  const date = toDate(value);
  if (!date) return typeof value === "string" ? value : "";
  return dateTimeFormatter(opts?.locale ?? getLocale()).format(date);
}

/** "Mon" / "周一". */
export function formatWeekday(
  value: string | Date,
  opts?: DateFormatOptions & { style?: "short" | "long" },
): string {
  const date = toDate(value);
  if (!date) return "";
  return weekdayFormatter(opts?.locale ?? getLocale(), opts?.style ?? "short").format(date);
}

/** "November 2026" / "2026年11月". */
export function formatMonthYear(
  value: string | Date,
  opts?: DateFormatOptions & { withYear?: boolean },
): string {
  const date = toDate(value);
  if (!date) return "";
  return monthYearFormatter(opts?.locale ?? getLocale(), opts?.withYear !== false).format(date);
}

/**
 * Windows Mail list timestamp:
 *   today          -> "14:05"
 *   this week      -> "Mon"
 *   this year      -> "Nov 2"
 *   other years    -> "Nov 2, 2025"
 */
export function formatListDate(
  value: string | Date,
  opts?: DateFormatOptions,
): string {
  const date = toDate(value);
  if (!date) return typeof value === "string" ? value : "";
  const now = opts?.now ?? new Date();
  const locale = opts?.locale ?? getLocale();
  const weekStartsOn = opts?.weekStartsOn ?? 1;

  if (isSameDay(date, now)) return timeFormatter(locale).format(date);
  if (isThisWeek(date, now, weekStartsOn)) {
    return weekdayFormatter(locale, "short").format(date);
  }
  const withYear = date.getFullYear() !== now.getFullYear();
  return dateFormatter(locale, withYear).format(date);
}

/** "刚刚" / "5 分钟前" / "3 小时前" / date, for sync status and toasts. */
export function formatRelativeTime(
  value: string | Date,
  opts?: DateFormatOptions,
): string {
  const date = toDate(value);
  if (!date) return typeof value === "string" ? value : "";
  const now = opts?.now ?? new Date();
  const locale = opts?.locale ?? getLocale();
  const diffMs = now.getTime() - date.getTime();
  const diffMinutes = Math.floor(diffMs / 60_000);
  if (diffMinutes < 1) return labels(locale).justNow;
  if (diffMinutes < 60) return labels(locale).minutes(diffMinutes);
  const diffHours = Math.floor(diffMinutes / 60);
  if (diffHours < 24) return labels(locale).hours(diffHours);
  return formatListDate(date, opts);
}

function labels(locale: string): {
  justNow: string;
  minutes: (n: number) => string;
  hours: (n: number) => string;
} {
  const zh = locale.toLowerCase().startsWith("zh");
  return zh
    ? {
        justNow: "刚刚",
        minutes: (n) => `${n} 分钟前`,
        hours: (n) => `${n} 小时前`,
      }
    : {
        justNow: "Just now",
        minutes: (n) => `${n} min ago`,
        hours: (n) => `${n} h ago`,
      };
}

/** Human readable byte size: "512 B", "1.5 KB", "12 MB". */
export function formatFileSize(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  if (unit === 0) return `${Math.round(value)} B`;
  const rounded = value >= 10 ? Math.round(value) : Math.round(value * 10) / 10;
  return `${rounded} ${units[unit]}`;
}

/** "Ada Lovelace" for display; falls back to the address. */
export function formatAddress(address: Address | null | undefined): string {
  if (!address) return "";
  const name = address.name?.trim();
  return name ? name : address.email;
}

/** `Ada Lovelace <ada@example.com>` */
export function formatAddressFull(address: Address | null | undefined): string {
  if (!address) return "";
  const name = address.name?.trim();
  return name ? `${name} <${address.email}>` : address.email;
}

export function formatAddressList(
  addresses: Address[] | null | undefined,
  separator = ", ",
): string {
  return (addresses ?? []).map(formatAddress).filter(Boolean).join(separator);
}

/** Split a pasted recipient string into addresses. */
export function parseAddressInput(text: string): Address[] {
  return text
    .split(/[,;\n]+/)
    .map((part) => part.trim())
    .filter(Boolean)
    .map((part) => {
      const match = part.match(/^(.*?)\s*<([^<>]+)>$/);
      if (match) {
        return { name: match[1].trim() || null, email: match[2].trim() };
      }
      return { name: null, email: part };
    });
}

const EMAIL_RE = /^[^\s@]+@[^\s@]+\.[^\s@]+$/;

export function isValidEmail(value: string): boolean {
  return EMAIL_RE.test(value.trim());
}

/** Two initials for an avatar; handles CJK names and bare e-mail addresses. */
export function getInitials(name?: string | null, email?: string | null): string {
  const source = (name ?? "").trim();
  if (source) {
    const words = source.split(/\s+/).filter(Boolean);
    if (words.length >= 2) {
      return (
        firstGrapheme(words[0]) + firstGrapheme(words[words.length - 1])
      ).toUpperCase();
    }
    const word = words[0];
    if (/^[㐀-鿿぀-ヿ가-힯]/.test(word)) {
      return Array.from(word).slice(0, 2).join("");
    }
    return Array.from(word).slice(0, 2).join("").toUpperCase();
  }
  const local = (email ?? "").split("@")[0].replace(/[._-]+/g, " ").trim();
  if (!local) return "?";
  const pieces = local.split(/\s+/);
  if (pieces.length >= 2) {
    return (firstGrapheme(pieces[0]) + firstGrapheme(pieces[1])).toUpperCase();
  }
  return Array.from(local).slice(0, 2).join("").toUpperCase();
}

function firstGrapheme(value: string): string {
  return Array.from(value)[0] ?? "";
}

/** Parse "YYYY-MM-DD" into a local Date at noon (avoids DST edges). */
export function parseIsoDate(value: string | null | undefined): Date | null {
  if (!value) return null;
  const match = value.match(/^(\d{4})-(\d{2})-(\d{2})/);
  if (!match) return null;
  const date = new Date(Number(match[1]), Number(match[2]) - 1, Number(match[3]), 12, 0, 0, 0);
  return Number.isNaN(date.getTime()) ? null : date;
}

/** Format a local Date as "YYYY-MM-DD". */
export function toIsoDate(date: Date): string {
  const year = date.getFullYear();
  const month = String(date.getMonth() + 1).padStart(2, "0");
  const day = String(date.getDate()).padStart(2, "0");
  return `${year}-${month}-${day}`;
}

/** Parse "HH:mm" into minutes since midnight. */
export function parseTimeOfDay(value: string): number | null {
  const match = /^(\d{1,2}):(\d{2})$/.exec(value.trim());
  if (!match) return null;
  const hours = Number(match[1]);
  const minutes = Number(match[2]);
  if (hours > 23 || minutes > 59) return null;
  return hours * 60 + minutes;
}

/**
 * Parse a time the user typed, tolerating the shorthand people actually use:
 * `9:05`, `09:05`, `9：05`, `905`, `0905`, or a bare hour `9`.
 *
 * Returns the normalized `HH:mm` form, or `null` when the text is not a valid
 * time of day.
 */
export function parseLooseTime(value: string): string | null {
  const text = value.trim().replace(/：/g, ":");
  if (text.length === 0) return null;
  const withSeparator = /^(\d{1,2}):(\d{1,2})$/.exec(text);
  if (withSeparator) {
    const hours = Number(withSeparator[1]);
    const minutes = Number(withSeparator[2]);
    return hours <= 23 && minutes <= 59
      ? formatTimeOfDay(hours * 60 + minutes)
      : null;
  }
  const digits = /^(\d{1,4})$/.exec(text);
  if (!digits) return null;
  const raw = digits[1];
  if (raw.length <= 2) {
    const hours = Number(raw);
    return hours <= 23 ? formatTimeOfDay(hours * 60) : null;
  }
  const hours = Number(raw.slice(0, raw.length - 2));
  const minutes = Number(raw.slice(-2));
  return hours <= 23 && minutes <= 59
    ? formatTimeOfDay(hours * 60 + minutes)
    : null;
}

/** Format minutes since midnight as "HH:mm". */
export function formatTimeOfDay(minutesSinceMidnight: number): string {
  const total = ((Math.round(minutesSinceMidnight) % 1440) + 1440) % 1440;
  const hours = Math.floor(total / 60);
  const minutes = total % 60;
  return `${String(hours).padStart(2, "0")}:${String(minutes).padStart(2, "0")}`;
}
