/**
 * Calendar view math: local-time day/week/month boundaries, per-view ranges
 * and period navigation.
 *
 * Everything here is calendar-based (not millisecond arithmetic) so it stays
 * correct across DST transitions. Instants are converted with `Date` only at
 * the query boundary (`range.toISOString()`).
 */
import type { CalendarView } from "@/ipc/types";
import { parseTimeOfDay, toIsoDate } from "@/lib/format";

export interface CalendarSettings {
  defaultView: CalendarView;
  firstDayOfWeek: number;
  workDays: number[];
  workHoursStart: string;
  workHoursEnd: string;
  defaultReminderMinutes: number;
  defaultEventDurationMinutes: number;
  showWeekNumbers: boolean;
  pollMinutes: number;
}

export const DEFAULT_CALENDAR_SETTINGS: CalendarSettings = {
  defaultView: "workWeek",
  firstDayOfWeek: 1,
  workDays: [1, 2, 3, 4, 5],
  workHoursStart: "09:00",
  workHoursEnd: "18:00",
  defaultReminderMinutes: 15,
  defaultEventDurationMinutes: 30,
  showWeekNumbers: false,
  pollMinutes: 15,
};

export const HOUR_HEIGHT = 48;
export const SNAP_MINUTES = 15;

/** Start of the local day. */
export function startOfDayLocal(date: Date): Date {
  return new Date(date.getFullYear(), date.getMonth(), date.getDate(), 0, 0, 0, 0);
}

/** Local midnight of `date` shifted by `days` (calendar days, DST-safe). */
export function addDaysLocal(date: Date, days: number): Date {
  const next = new Date(date.getTime());
  next.setDate(next.getDate() + days);
  return next;
}

export function addMinutes(date: Date, minutes: number): Date {
  return new Date(date.getTime() + minutes * 60_000);
}

export function addMonthsLocal(date: Date, months: number): Date {
  const year = date.getFullYear();
  const month = date.getMonth() + months;
  const daysInTarget = new Date(year, month + 1, 0).getDate();
  return new Date(
    year,
    month,
    Math.min(date.getDate(), daysInTarget),
    date.getHours(),
    date.getMinutes(),
    0,
    0,
  );
}

/** Parse "YYYY-MM-DD" at local midnight (all-day semantics, not noon). */
export function parseDateOnly(value: string | null | undefined): Date | null {
  if (!value) return null;
  const match = /^(\d{4})-(\d{2})-(\d{2})/.exec(value);
  if (!match) {
    const parsed = new Date(value);
    if (Number.isNaN(parsed.getTime())) return null;
    return startOfDayLocal(parsed);
  }
  return new Date(Number(match[1]), Number(match[2]) - 1, Number(match[3]), 0, 0, 0, 0);
}

export function formatDateOnly(date: Date): string {
  return toIsoDate(date);
}

export function isSameDayLocal(a: Date, b: Date): boolean {
  return (
    a.getFullYear() === b.getFullYear() &&
    a.getMonth() === b.getMonth() &&
    a.getDate() === b.getDate()
  );
}

export function isSameMonth(a: Date, b: Date): boolean {
  return a.getFullYear() === b.getFullYear() && a.getMonth() === b.getMonth();
}

export function startOfWeekLocal(date: Date, weekStartsOn: number): Date {
  const base = startOfDayLocal(date);
  const diff = (base.getDay() - weekStartsOn + 7) % 7;
  return addDaysLocal(base, -diff);
}

export function startOfMonth(date: Date): Date {
  return new Date(date.getFullYear(), date.getMonth(), 1, 0, 0, 0, 0);
}

export function endOfMonthExclusive(date: Date): Date {
  return new Date(date.getFullYear(), date.getMonth() + 1, 1, 0, 0, 0, 0);
}

/** Whole calendar days produced from a [start, end-1] span (inclusive). */
export function calendarDays(start: Date, endExclusive: Date): Date[] {
  const days: Date[] = [];
  let cursor = startOfDayLocal(start);
  const limit = startOfDayLocal(endExclusive);
  // Guard against accidental infinite loops from bad input.
  let guard = 0;
  while (cursor < limit && guard < 400) {
    days.push(cursor);
    cursor = addDaysLocal(cursor, 1);
    guard += 1;
  }
  return days;
}

/** Difference in calendar days (a - b), immune to DST hour changes. */
export function calendarDayDiff(a: Date, b: Date): number {
  const ua = Date.UTC(a.getFullYear(), a.getMonth(), a.getDate());
  const ub = Date.UTC(b.getFullYear(), b.getMonth(), b.getDate());
  return Math.round((ua - ub) / 86_400_000);
}

/** ISO-8601 week number (1..53). */
export function isoWeekNumber(date: Date): number {
  const target = new Date(Date.UTC(date.getFullYear(), date.getMonth(), date.getDate()));
  const dayNumber = target.getUTCDay() || 7;
  target.setUTCDate(target.getUTCDate() + 4 - dayNumber);
  const yearStart = new Date(Date.UTC(target.getUTCFullYear(), 0, 1));
  return Math.ceil(((target.getTime() - yearStart.getTime()) / 86_400_000 + 1) / 7);
}

/** 42-day grid (6 weeks) for a month view, starting on the configured weekday. */
export function monthGridDays(month: Date, weekStartsOn: number): Date[] {
  const first = startOfMonth(month);
  const gridStart = startOfWeekLocal(first, weekStartsOn);
  return Array.from({ length: 42 }, (_, index) => addDaysLocal(gridStart, index));
}

export function normalizeWeekDays(workDays: number[]): number[] {
  const unique = Array.from(new Set(workDays.filter((day) => day >= 0 && day <= 6)));
  unique.sort((a, b) => a - b);
  return unique.length > 0 ? unique : [1, 2, 3, 4, 5];
}

export interface ViewRange {
  /** First instant of the range (inclusive). */
  from: Date;
  /** End instant of the range (exclusive). */
  to: Date;
  /** Concrete days for day/week/work-week (and month grid) views. */
  days: Date[];
  /** True when `days` covers a full grid whose edges may be outside the month. */
  grid?: boolean;
}

export function mainDaysForView(
  view: CalendarView,
  anchor: Date,
  settings: CalendarSettings,
): Date[] | null {
  switch (view) {
    case "day":
      return [startOfDayLocal(anchor)];
    case "week": {
      const start = startOfWeekLocal(anchor, settings.firstDayOfWeek);
      return Array.from({ length: 7 }, (_, index) => addDaysLocal(start, index));
    }
    case "workWeek": {
      const weekStart = startOfWeekLocal(anchor, settings.firstDayOfWeek);
      const workDays = normalizeWeekDays(settings.workDays);
      return workDays
        .map((weekday) => {
          const offset = (weekday - settings.firstDayOfWeek + 7) % 7;
          return addDaysLocal(weekStart, offset);
        })
        .sort((a, b) => a.getTime() - b.getTime());
    }
    default:
      return null;
  }
}

export function rangeForView(
  view: CalendarView,
  anchor: Date,
  settings: CalendarSettings,
): ViewRange {
  const day = startOfDayLocal(anchor);
  switch (view) {
    case "day":
      return { from: day, to: addDaysLocal(day, 1), days: [day] };
    case "week":
    case "workWeek": {
      const days = mainDaysForView(view, anchor, settings) ?? [day];
      const from = days[0] ?? day;
      const last = days[days.length - 1] ?? day;
      return { from, to: addDaysLocal(last, 1), days };
    }
    case "month": {
      const gridStart = monthGridDays(anchor, settings.firstDayOfWeek);
      const from = gridStart[0] ?? day;
      const last = gridStart[gridStart.length - 1] ?? day;
      return { from, to: addDaysLocal(last, 1), days: gridStart, grid: true };
    }
    case "year": {
      const from = new Date(anchor.getFullYear(), 0, 1, 0, 0, 0, 0);
      const to = new Date(anchor.getFullYear() + 1, 0, 1, 0, 0, 0, 0);
      return { from, to, days: [] };
    }
    case "agenda":
    default:
      return { from: day, to: addDaysLocal(day, 30), days: [] };
  }
}

/** Move the anchor one period in the given direction. */
export function shiftView(
  view: CalendarView,
  anchor: Date,
  direction: 1 | -1,
  _settings: CalendarSettings,
): Date {
  switch (view) {
    case "day":
      return addDaysLocal(startOfDayLocal(anchor), direction);
    case "workWeek":
    case "week":
      return addDaysLocal(startOfDayLocal(anchor), 7 * direction);
    case "month":
      return startOfMonth(addMonthsLocal(anchor, direction));
    case "year":
      return new Date(anchor.getFullYear() + direction, anchor.getMonth(), 1, 0, 0, 0, 0);
    case "agenda":
    default:
      return addDaysLocal(startOfDayLocal(anchor), 30 * direction);
  }
}

export function minutesOfDay(date: Date): number {
  return date.getHours() * 60 + date.getMinutes();
}

/** Snap minutes to the nearest step, clamped to [0, 1440]. */
export function snapMinutes(minutes: number, step = SNAP_MINUTES): number {
  const snapped = Math.round(minutes / step) * step;
  return Math.max(0, Math.min(1440, snapped));
}

/** Snap a signed minute delta (drag gestures); no clamping. */
export function snapDelta(minutes: number, step = SNAP_MINUTES): number {
  if (!Number.isFinite(minutes)) return 0;
  return Math.round(minutes / step) * step;
}

export function workHourBounds(settings: CalendarSettings): { start: number; end: number } {
  const start = parseTimeOfDay(settings.workHoursStart) ?? 9 * 60;
  const end = parseTimeOfDay(settings.workHoursEnd) ?? 18 * 60;
  return { start: Math.min(start, end), end: Math.max(start, end) };
}

/** True when the weekday participates in the working week. */
export function isWorkDay(date: Date, settings: CalendarSettings): boolean {
  return normalizeWeekDays(settings.workDays).includes(date.getDay());
}

/** Combine a local day with minutes since midnight. */
export function combineDateAndMinutes(day: Date, minutes: number): Date {
  const base = startOfDayLocal(day);
  return new Date(
    base.getFullYear(),
    base.getMonth(),
    base.getDate(),
    Math.floor(minutes / 60),
    minutes % 60,
    0,
    0,
  );
}

/** Default IANA zone of the client. */
export function localTimeZone(): string {
  try {
    const zone = Intl.DateTimeFormat().resolvedOptions().timeZone;
    if (zone) return zone;
  } catch {
    // Ignore unsupported Intl and fall back below.
  }
  return "UTC";
}

/** Serialize a local Date as ISO-8601 with its numeric UTC offset. */
export function toOffsetIsoString(date: Date): string {
  const pad = (value: number, length = 2) => String(Math.abs(value)).padStart(length, "0");
  const year = date.getFullYear();
  const month = pad(date.getMonth() + 1);
  const day = pad(date.getDate());
  const hours = pad(date.getHours());
  const minutes = pad(date.getMinutes());
  const seconds = pad(date.getSeconds());
  const offset = -date.getTimezoneOffset();
  const sign = offset >= 0 ? "+" : "-";
  const offsetHours = pad(Math.floor(Math.abs(offset) / 60));
  const offsetMinutes = pad(Math.abs(offset) % 60);
  return `${year}-${month}-${day}T${hours}:${minutes}:${seconds}${sign}${offsetHours}:${offsetMinutes}`;
}

/* ─────────────────── IANA time zone helpers ───────────────────
 * Events carry an IANA zone id and store start/end as ISO-8601 with the
 * offset in effect at that instant. The editor lets users pick the zone;
 * these helpers convert between wall-clock values in a zone and real
 * instants without pulling in a time zone library (Intl already knows the
 * tz database). Invalid zone ids fall back to the browser's local zone.
 */

export interface ZonedDateTimeParts {
  year: number;
  /** 1..12 */
  month: number;
  day: number;
  hour: number;
  minute: number;
  second: number;
}

const zoneFormatters = new Map<string, Intl.DateTimeFormat | null>();

function zoneFormatter(zone: string): Intl.DateTimeFormat | null {
  const cached = zoneFormatters.get(zone);
  if (cached !== undefined) return cached;
  let formatter: Intl.DateTimeFormat | null = null;
  try {
    formatter = new Intl.DateTimeFormat("en-US", {
      timeZone: zone,
      hourCycle: "h23",
      year: "numeric",
      month: "2-digit",
      day: "2-digit",
      hour: "2-digit",
      minute: "2-digit",
      second: "2-digit",
    });
  } catch {
    // Unknown zone id (or an engine without full ICU): use local time.
    formatter = null;
  }
  zoneFormatters.set(zone, formatter);
  return formatter;
}

/** True when the runtime understands this IANA zone id. */
export function isValidTimeZone(zone: string | null | undefined): zone is string {
  if (!zone) return false;
  return zoneFormatter(zone) !== null;
}

/** Wall-clock parts of an instant in `zone` (falls back to the local zone). */
export function zonedParts(date: Date, zone: string): ZonedDateTimeParts {
  const formatter = zoneFormatter(zone);
  if (formatter && !Number.isNaN(date.getTime())) {
    const values: Record<string, string> = {};
    for (const part of formatter.formatToParts(date)) {
      if (part.type !== "literal") values[part.type] = part.value;
    }
    const year = Number.parseInt(values.year ?? "", 10);
    const month = Number.parseInt(values.month ?? "", 10);
    const day = Number.parseInt(values.day ?? "", 10);
    const hour = Number.parseInt(values.hour ?? "", 10);
    const minute = Number.parseInt(values.minute ?? "", 10);
    const second = Number.parseInt(values.second ?? "", 10);
    if (
      [year, month, day, hour, minute, second].every((value) => Number.isFinite(value))
    ) {
      // Some engines report midnight as "24" even with hourCycle h23.
      return { year, month, day, hour: hour % 24, minute, second };
    }
  }
  return {
    year: date.getFullYear(),
    month: date.getMonth() + 1,
    day: date.getDate(),
    hour: date.getHours(),
    minute: date.getMinutes(),
    second: date.getSeconds(),
  };
}

/** UTC offset (milliseconds) of `zone` at the given instant; east of UTC is positive. */
export function zoneOffsetMs(date: Date, zone: string): number {
  const parts = zonedParts(date, zone);
  const wallAsUtc = Date.UTC(
    parts.year,
    parts.month - 1,
    parts.day,
    parts.hour,
    parts.minute,
    parts.second,
  );
  return wallAsUtc - (date.getTime() - date.getMilliseconds());
}

/** Convert a wall-clock time in `zone` to the matching instant. */
export function zonedDateTimeToDate(
  zone: string,
  year: number,
  month: number,
  day: number,
  hour: number,
  minute: number,
): Date {
  const wallAsUtc = Date.UTC(year, month - 1, day, hour, minute, 0, 0);
  // Two passes settle the offset even when the first guess lands on the
  // other side of a DST transition.
  let instant = wallAsUtc - zoneOffsetMs(new Date(wallAsUtc), zone);
  for (let pass = 0; pass < 2; pass += 1) {
    instant = wallAsUtc - zoneOffsetMs(new Date(instant), zone);
  }
  return new Date(instant);
}

/** Serialize an instant as ISO-8601 with the offset of `zone` at that instant. */
export function formatIsoWithZoneOffset(date: Date, zone: string): string {
  const parts = zonedParts(date, zone);
  const pad = (value: number) => String(Math.abs(value)).padStart(2, "0");
  const offsetMinutes = Math.round(zoneOffsetMs(date, zone) / 60_000);
  const sign = offsetMinutes >= 0 ? "+" : "-";
  return (
    `${parts.year}-${pad(parts.month)}-${pad(parts.day)}` +
    `T${pad(parts.hour)}:${pad(parts.minute)}:${pad(parts.second)}` +
    `${sign}${pad(Math.floor(Math.abs(offsetMinutes) / 60))}:${pad(Math.abs(offsetMinutes) % 60)}`
  );
}

/** Local `Date` whose fields mirror the wall clock in `zone` (recurrence math). */
export function wallClockDate(date: Date, zone: string): Date {
  const parts = zonedParts(date, zone);
  return new Date(
    parts.year,
    parts.month - 1,
    parts.day,
    parts.hour,
    parts.minute,
    parts.second,
    0,
  );
}

/** "UTC+08:00" label for a zone at a given instant. */
export function formatZoneOffset(zone: string, at: Date): string {
  const pad = (value: number) => String(Math.abs(value)).padStart(2, "0");
  const offsetMinutes = Math.round(zoneOffsetMs(at, zone) / 60_000);
  const sign = offsetMinutes >= 0 ? "+" : "-";
  return `UTC${sign}${pad(Math.floor(Math.abs(offsetMinutes) / 60))}:${pad(Math.abs(offsetMinutes) % 60)}`;
}

const FALLBACK_TIME_ZONES = [
  "UTC",
  "Asia/Shanghai",
  "Asia/Hong_Kong",
  "Asia/Taipei",
  "Asia/Tokyo",
  "Asia/Seoul",
  "Asia/Singapore",
  "Asia/Bangkok",
  "Asia/Kolkata",
  "Asia/Dubai",
  "Europe/London",
  "Europe/Paris",
  "Europe/Berlin",
  "Europe/Moscow",
  "America/New_York",
  "America/Chicago",
  "America/Denver",
  "America/Los_Angeles",
  "America/Sao_Paulo",
  "Australia/Sydney",
  "Pacific/Auckland",
];

/**
 * Zone ids for the editor dropdown: the client zone first, then the full IANA
 * database when the engine exposes it, plus any ids the current event uses.
 */
export function listTimeZones(extra: string[] = []): string[] {
  const local = localTimeZone();
  const zones = new Set<string>([local]);
  const intl = Intl as unknown as { supportedValuesOf?: (key: string) => string[] };
  if (typeof intl.supportedValuesOf === "function") {
    try {
      for (const zone of intl.supportedValuesOf("timeZone")) zones.add(zone);
    } catch {
      // Older engine: the curated list below is enough.
    }
  }
  for (const zone of FALLBACK_TIME_ZONES) zones.add(zone);
  for (const zone of extra) {
    if (zone) zones.add(zone);
  }
  const rest = Array.from(zones)
    .filter((zone) => zone !== local)
    .sort((a, b) => a.localeCompare(b));
  return [local, ...rest];
}

/** Narrow weekday headers in display order. */
export function weekdayHeaders(locale: string, weekStartsOn: number): string[] {
  const formatter = new Intl.DateTimeFormat(locale, { weekday: "narrow" });
  // 2024-01-01 is a Monday.
  const monday = new Date(2024, 0, 1, 12, 0, 0, 0);
  return Array.from({ length: 7 }, (_, index) => {
    const date = addDaysLocal(monday, (weekStartsOn - 1 + 7 + index) % 7);
    return formatter.format(date);
  });
}

/** Short weekday label for a concrete date. */
export function shortWeekday(date: Date, locale: string): string {
  return new Intl.DateTimeFormat(locale, { weekday: "short" }).format(date);
}

/** Long weekday label for a concrete date. */
export function longWeekday(date: Date, locale: string): string {
  return new Intl.DateTimeFormat(locale, { weekday: "long" }).format(date);
}

export function formatMonthDay(date: Date, locale: string): string {
  return new Intl.DateTimeFormat(locale, { month: "short", day: "numeric" }).format(date);
}

export function formatMonthLong(date: Date, locale: string): string {
  return new Intl.DateTimeFormat(locale, { month: "long", year: "numeric" }).format(date);
}

/** Toolbar title for the current view. */
export function formatRangeTitle(
  view: CalendarView,
  anchor: Date,
  range: ViewRange,
  locale: string,
): string {
  const longDate = new Intl.DateTimeFormat(locale, {
    year: "numeric",
    month: "long",
    day: "numeric",
    weekday: "long",
  });
  const fullMonth = new Intl.DateTimeFormat(locale, { month: "long", year: "numeric" });
  switch (view) {
    case "day":
      return longDate.format(anchor);
    case "workWeek":
    case "week": {
      const days = range.days;
      const start = days[0] ?? anchor;
      const end = days[days.length - 1] ?? anchor;
      const zh = locale.toLowerCase().startsWith("zh");
      if (isSameMonth(start, end)) {
        if (zh) {
          return `${start.getFullYear()}年${start.getMonth() + 1}月${start.getDate()}日 – ${end.getDate()}日`;
        }
        const monthName = new Intl.DateTimeFormat(locale, { month: "long" }).format(start);
        return `${monthName} ${start.getDate()} – ${end.getDate()}, ${end.getFullYear()}`;
      }
      return zh
        ? `${start.getFullYear()}年${formatMonthDay(start, locale)} – ${formatMonthDay(end, locale)}`
        : `${formatMonthDay(start, locale)} – ${formatMonthDay(end, locale)}, ${end.getFullYear()}`;
    }
    case "month":
      return fullMonth.format(anchor);
    case "year":
      return String(anchor.getFullYear());
    case "agenda":
    default: {
      const lastDay = addDaysLocal(range.to, -1);
      return locale.toLowerCase().startsWith("zh")
        ? `${range.from.getFullYear()}年${formatMonthDay(range.from, locale)} – ${formatMonthDay(lastDay, locale)}`
        : `${formatMonthDay(range.from, locale)} – ${formatMonthDay(lastDay, locale)}, ${lastDay.getFullYear()}`;
    }
  }
}

/** Heading for an agenda day group ("Today", "Tomorrow", otherwise a date). */
export function formatAgendaDay(date: Date, locale: string, today: Date): string {
  const diff = calendarDayDiff(date, today);
  if (diff === 0) return "__today__";
  if (diff === 1) return "__tomorrow__";
  return new Intl.DateTimeFormat(locale, {
    weekday: "long",
    month: "long",
    day: "numeric",
  }).format(date);
}
