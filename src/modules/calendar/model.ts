/**
 * Mapping between contract types and what the views need: local day spans,
 * display labels, and event -> EventInput round-tripping for drag & drop.
 */
import type {
  Attendee,
  CalendarEvent,
  EventInput,
  EventInstance,
  IsoDateTime,
  ShowAs,
} from "@/ipc/types";
import { formatTime } from "@/lib/format";
import {
  addDaysLocal,
  combineDateAndMinutes,
  formatDateOnly,
  formatIsoWithZoneOffset,
  isValidTimeZone,
  minutesOfDay,
  parseDateOnly,
  startOfDayLocal,
  toOffsetIsoString,
} from "./dates";

export type CalendarEventLike = CalendarEvent | EventInstance;

export function isAllDay(event: CalendarEventLike): boolean {
  return event.allDay;
}

/** Local Date of the event start (all-day → local midnight). */
export function eventStartDate(event: CalendarEventLike): Date {
  if (event.allDay) {
    return parseDateOnly(event.start) ?? startOfDayLocal(new Date(event.start));
  }
  const parsed = new Date(event.start);
  return Number.isNaN(parsed.getTime()) ? new Date() : parsed;
}

/** Local Date of the exclusive end (all-day → local midnight of the exclusive date). */
export function eventEndDateExclusive(event: CalendarEventLike): Date {
  if (event.allDay) {
    const date = parseDateOnly(event.end) ?? parseDateOnly(event.start);
    if (!date) return eventStartDate(event);
    return date;
  }
  const parsed = new Date(event.end);
  if (Number.isNaN(parsed.getTime())) return eventStartDate(event);
  return parsed;
}

export function eventDurationMinutes(event: CalendarEventLike): number {
  const start = eventStartDate(event);
  const end = eventEndDateExclusive(event);
  return Math.max(0, Math.round((end.getTime() - start.getTime()) / 60_000));
}

/** Inclusive last day an event occupies. */
export function eventLastDayInclusive(event: CalendarEventLike): Date {
  const start = eventStartDate(event);
  if (event.allDay) {
    return addDaysLocal(eventEndDateExclusive(event), -1);
  }
  const end = eventEndDateExclusive(event);
  const justBeforeEnd = new Date(end.getTime() - 1);
  const last = startOfDayLocal(justBeforeEnd);
  return last < startOfDayLocal(start) ? startOfDayLocal(start) : last;
}

export function eventSpansDay(event: CalendarEventLike, day: Date): boolean {
  const first = startOfDayLocal(eventStartDate(event));
  const last = startOfDayLocal(eventLastDayInclusive(event));
  const target = startOfDayLocal(day);
  return target >= first && target <= last;
}

export function eventStartsOnDay(event: CalendarEventLike, day: Date): boolean {
  const start = startOfDayLocal(eventStartDate(event));
  return start.getTime() === startOfDayLocal(day).getTime();
}

export function isEventDeclined(event: CalendarEventLike): boolean {
  return event.myResponse === "declined";
}

export function isEventCancelled(event: CalendarEventLike): boolean {
  return event.status === "cancelled";
}

/** Key used to compare the selected row with an instance. */
export function instanceSelectionKey(eventId: string, instanceStart: string | null): string {
  return `${eventId}::${instanceStart ?? ""}`;
}

/** Backend instance key is `${eventId}:${start}`. */
export function instanceSelectionKeyForInstance(event: EventInstance): string {
  return instanceSelectionKey(event.eventId, event.start);
}

export function isSameShowAs(a: ShowAs, b: ShowAs): boolean {
  return a === b;
}

export function eventSpanDays(event: CalendarEventLike): number {
  const start = startOfDayLocal(eventStartDate(event));
  const last = startOfDayLocal(eventLastDayInclusive(event));
  return Math.max(1, Math.round((last.getTime() - start.getTime()) / 86_400_000) + 1);
}

function sameDay(a: Date, b: Date): boolean {
  return (
    a.getFullYear() === b.getFullYear() &&
    a.getMonth() === b.getMonth() &&
    a.getDate() === b.getDate()
  );
}

/** "09:00 – 10:00", "全天" or a multi-day range label. */
export function formatEventTimeRange(
  event: CalendarEventLike,
  allDayLabel: string,
  locale: string,
): string {
  if (event.allDay) {
    const start = eventStartDate(event);
    const last = eventLastDayInclusive(event);
    if (sameDay(start, last)) return allDayLabel;
    // Multi-day all-day events must still be recognisable as all-day: a bare
    // date range would read like a timed span. Dates come from calendar-day
    // helpers (local midnight), never from a UTC instant.
    const formatter = new Intl.DateTimeFormat(locale, { month: "short", day: "numeric" });
    return `${allDayLabel} · ${formatter.format(start)} – ${formatter.format(last)}`;
  }
  const start = eventStartDate(event);
  const end = eventEndDateExclusive(event);
  const startText = formatTime(start, { locale });
  if (sameDay(start, end) || (end.getHours() === 0 && end.getMinutes() === 0 && sameDay(start, addDaysLocal(end, -1)))) {
    return `${startText} – ${formatTime(end, { locale })}`;
  }
  const formatter = new Intl.DateTimeFormat(locale, {
    month: "short",
    day: "numeric",
  });
  return `${formatter.format(start)} ${startText} – ${formatter.format(end)} ${formatTime(end, { locale })}`;
}

/** Compact time text for chips: "09:00" or "全天". */
export function formatEventChipTime(
  event: CalendarEventLike,
  allDayLabel: string,
  locale: string,
): string {
  if (event.allDay) return allDayLabel;
  return formatTime(eventStartDate(event), { locale });
}

/** Convert a stored event into a full EventInput (drag, editor scope changes). */
export function buildEventInput(event: CalendarEvent): EventInput {
  return {
    calendarId: event.calendarId,
    title: event.title,
    location: event.location,
    description: event.description,
    start: event.start,
    end: event.end,
    allDay: event.allDay,
    timeZone: event.timeZone,
    recurrence: event.recurrence,
    showAs: event.showAs,
    isPrivate: event.isPrivate,
    attendees: event.attendees,
    reminders: event.reminders,
    onlineMeetingUrl: event.onlineMeetingUrl,
    color: event.color,
    notifyAttendees: false,
  };
}

/** New start/end strings for a local Date range, honoring all-day semantics. */
export function rangeToInput(
  start: Date,
  end: Date,
  allDay: boolean,
): { start: string; end: string } {
  if (allDay) {
    return {
      start: formatDateOnly(startOfDayLocal(start)),
      end: formatDateOnly(startOfDayLocal(end)),
    };
  }
  return { start: toOffsetIsoString(start), end: toOffsetIsoString(end) };
}

/**
 * Like `rangeToInput`, but timed events are serialized with the offset of
 * their own IANA zone so the stored strings stay consistent with
 * `CalendarEvent.timeZone`. Unknown zones fall back to the local offset.
 */
export function rangeToInputInZone(
  start: Date,
  end: Date,
  allDay: boolean,
  timeZone: string,
): { start: string; end: string } {
  if (allDay || !isValidTimeZone(timeZone)) {
    return rangeToInput(start, end, allDay);
  }
  return {
    start: formatIsoWithZoneOffset(start, timeZone),
    end: formatIsoWithZoneOffset(end, timeZone),
  };
}

/** Start Date for a quick-create slot at the next half hour. */
export function nextHalfHour(now: Date): Date {
  const base = new Date(now.getFullYear(), now.getMonth(), now.getDate(), now.getHours(), 0, 0, 0);
  if (now.getMinutes() < 30) {
    base.setMinutes(30);
  } else {
    base.setHours(base.getHours() + 1);
    base.setMinutes(0);
  }
  return base;
}

/** Build a local Date from an event's start plus an offset in minutes. */
export function shiftEventStart(event: CalendarEventLike, minutes: number): Date {
  return new Date(eventStartDate(event).getTime() + minutes * 60_000);
}

export { combineDateAndMinutes, minutesOfDay };

/** Attendees split for the editor. */
export interface AttendeeBuckets {
  required: Attendee[];
  optional: Attendee[];
}

export function splitAttendees(attendees: Attendee[]): AttendeeBuckets {
  const required: Attendee[] = [];
  const optional: Attendee[] = [];
  for (const attendee of attendees) {
    if (attendee.role === "optional" || attendee.role === "resource") {
      optional.push(attendee);
    } else {
      required.push(attendee);
    }
  }
  return { required, optional };
}

/** ISO start/end used when the editor combines pickers. */
export function eventInstantForResponse(event: CalendarEventLike): IsoDateTime {
  return event.start;
}
