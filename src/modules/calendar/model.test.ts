import { describe, expect, it } from "vitest";
import type { CalendarEvent } from "@/ipc/types";
import {
  eventLastDayInclusive,
  eventSpansDay,
  eventStartDate,
  formatEventTimeRange,
  rangeToInput,
  splitAttendees,
} from "@/modules/calendar/model";
import { toOffsetIsoString } from "@/modules/calendar/dates";

function makeEvent(overrides: Partial<CalendarEvent>): CalendarEvent {
  const start = new Date(2026, 10, 12, 9, 0);
  const end = new Date(2026, 10, 12, 10, 0);
  return {
    id: "e1",
    calendarId: "cal",
    accountId: "acc",
    uid: "uid",
    title: "Event",
    location: "",
    description: "",
    start: toOffsetIsoString(start),
    end: toOffsetIsoString(end),
    allDay: false,
    timeZone: "Asia/Shanghai",
    recurrence: null,
    seriesId: null,
    recurrenceId: null,
    status: "confirmed",
    showAs: "busy",
    isPrivate: false,
    organizer: null,
    attendees: [],
    reminders: [],
    isOrganizer: false,
    myResponse: null,
    onlineMeetingUrl: null,
    color: null,
    updatedAt: null,
    ...overrides,
  };
}

describe("event day spans", () => {
  it("uses the exclusive end for all-day events", () => {
    const event = makeEvent({ allDay: true, start: "2026-11-12", end: "2026-11-15" });
    expect(eventLastDayInclusive(event).getDate()).toBe(14);
    expect(eventSpansDay(event, new Date(2026, 10, 12))).toBe(true);
    expect(eventSpansDay(event, new Date(2026, 10, 14))).toBe(true);
    expect(eventSpansDay(event, new Date(2026, 10, 15))).toBe(false);
  });

  it("treats an end at midnight as the previous day", () => {
    const start = new Date(2026, 10, 12, 22, 0);
    const end = new Date(2026, 10, 13, 0, 0);
    const event = makeEvent({ start: toOffsetIsoString(start), end: toOffsetIsoString(end) });
    expect(eventLastDayInclusive(event).getDate()).toBe(12);
    expect(eventSpansDay(event, new Date(2026, 10, 13))).toBe(false);
  });

  it("keeps an event crossing midnight on both days", () => {
    const start = new Date(2026, 10, 12, 22, 0);
    const end = new Date(2026, 10, 13, 2, 0);
    const event = makeEvent({ start: toOffsetIsoString(start), end: toOffsetIsoString(end) });
    expect(eventSpansDay(event, new Date(2026, 10, 12))).toBe(true);
    expect(eventSpansDay(event, new Date(2026, 10, 13))).toBe(true);
  });
});

describe("rangeToInput", () => {
  it("emits date-only strings for all-day ranges", () => {
    const range = rangeToInput(new Date(2026, 10, 12), new Date(2026, 10, 13), true);
    expect(range).toEqual({ start: "2026-11-12", end: "2026-11-13" });
  });

  it("emits ISO strings with an offset for timed ranges", () => {
    const range = rangeToInput(new Date(2026, 10, 12, 9, 0), new Date(2026, 10, 12, 10, 0), false);
    expect(range.start.startsWith("2026-11-12T09:00:00")).toBe(true);
    expect(range.end.startsWith("2026-11-12T10:00:00")).toBe(true);
    expect(range.start).toMatch(/[+-]\d{2}:\d{2}$/);
  });
});

describe("formatting and attendees", () => {
  it("formats a single-day event range", () => {
    const event = makeEvent({});
    const text = formatEventTimeRange(event, "All day", "en-US");
    expect(text).toBe("09:00 – 10:00");
  });

  it("labels single-day all-day events with the all-day text only", () => {
    const event = makeEvent({ allDay: true, start: "2026-11-12", end: "2026-11-13" });
    expect(formatEventTimeRange(event, "All day", "en-US")).toBe("All day");
  });

  it("keeps multi-day all-day events labelled as all-day", () => {
    const event = makeEvent({ allDay: true, start: "2026-11-12", end: "2026-11-15" });
    // Dates come from the date-only values at local midnight (never a UTC
    // instant that could shift the day), and the label makes the span clear.
    expect(formatEventTimeRange(event, "All day", "en-US")).toBe(
      "All day · Nov 12 – Nov 14",
    );
    expect(formatEventTimeRange(event, "全天", "zh-CN")).toBe("全天 · 11月12日 – 11月14日");
  });

  it("splits required and optional attendees", () => {
    const buckets = splitAttendees([
      { email: "a@x.com", name: null, role: "required", status: "accepted" },
      { email: "b@x.com", name: null, role: "optional", status: "needsAction" },
      { email: "c@x.com", name: null, role: "resource", status: "accepted" },
    ]);
    expect(buckets.required.map((a) => a.email)).toEqual(["a@x.com"]);
    expect(buckets.optional.map((a) => a.email)).toEqual(["b@x.com", "c@x.com"]);
  });

  it("parses all-day starts at local midnight", () => {
    const event = makeEvent({ allDay: true, start: "2026-11-12", end: "2026-11-13" });
    const start = eventStartDate(event);
    expect(start.getHours()).toBe(0);
    expect(start.getDate()).toBe(12);
  });
});
