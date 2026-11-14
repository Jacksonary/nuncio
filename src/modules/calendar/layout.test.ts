import { afterAll, beforeAll, describe, expect, it } from "vitest";
import type { EventInstance } from "@/ipc/types";
import {
  allDayEventsForDay,
  layoutSegments,
  segmentsForDay,
  segmentForDay,
} from "@/modules/calendar/layout";
import { toOffsetIsoString } from "@/modules/calendar/dates";

function instance(
  id: string,
  startHour: number,
  endHour: number,
  overrides: Partial<EventInstance> = {},
): EventInstance {
  const day = new Date(2026, 10, 12);
  const start = new Date(day.getFullYear(), day.getMonth(), day.getDate(), 0, startHour * 60);
  const end = new Date(day.getFullYear(), day.getMonth(), day.getDate(), 0, endHour * 60);
  return {
    eventId: id,
    instanceKey: `${id}:${start.toISOString()}`,
    calendarId: "cal",
    accountId: "acc",
    title: id,
    location: "",
    start: toOffsetIsoString(start),
    end: toOffsetIsoString(end),
    allDay: false,
    color: "#0f6cbd",
    isRecurring: false,
    showAs: "busy",
    status: "confirmed",
    myResponse: null,
    hasAttendees: false,
    hasReminder: false,
    ...overrides,
  };
}

const DAY = new Date(2026, 10, 12);

describe("segmentForDay", () => {
  it("clips an event to the visible day", () => {
    const event = instance("a", 9, 10);
    const segment = segmentForDay(event, DAY);
    expect(segment?.startMin).toBe(540);
    expect(segment?.endMin).toBe(600);
    expect(segment?.continuesBefore).toBe(false);
    expect(segment?.continuesAfter).toBe(false);
  });

  it("marks multi-day timed events as continuing", () => {
    const start = new Date(2026, 10, 11, 22, 0);
    const end = new Date(2026, 10, 13, 2, 0);
    const event = instance("a", 0, 0, {
      start: toOffsetIsoString(start),
      end: toOffsetIsoString(end),
    });
    const segment = segmentForDay(event, DAY);
    expect(segment?.startMin).toBe(0);
    expect(segment?.endMin).toBe(1440);
    expect(segment?.continuesBefore).toBe(true);
    expect(segment?.continuesAfter).toBe(true);
  });

  it("skips all-day and non-overlapping events", () => {
    expect(segmentForDay(instance("a", 9, 10, { allDay: true }), DAY)).toBeNull();
    expect(segmentForDay(instance("b", 9, 10), new Date(2026, 10, 13))).toBeNull();
  });
});

describe("layoutSegments", () => {
  it("gives back-to-back events separate clusters", () => {
    const placed = layoutSegments(segmentsForDay([instance("a", 9, 10), instance("b", 10, 11)], DAY));
    expect(placed).toHaveLength(2);
    expect(placed.every((item) => item.columns === 1)).toBe(true);
    expect(placed.every((item) => item.column === 0)).toBe(true);
  });

  it("splits two overlapping events into two columns", () => {
    const placed = layoutSegments(
      segmentsForDay([instance("a", 9, 11), instance("b", 10, 12)], DAY),
    );
    const byId = new Map(placed.map((item) => [item.segment.event.eventId, item]));
    expect(byId.get("a")?.columns).toBe(2);
    expect(byId.get("b")?.columns).toBe(2);
    expect(byId.get("a")?.column).toBe(0);
    expect(byId.get("b")?.column).toBe(1);
  });

  it("reuses a freed column inside a cluster", () => {
    const placed = layoutSegments(
      segmentsForDay(
        [instance("a", 9, 11), instance("b", 9.5, 10.5), instance("c", 10.75, 12)],
        DAY,
      ),
    );
    const byId = new Map(placed.map((item) => [item.segment.event.eventId, item]));
    // a spans the whole cluster; b and c can share the second column.
    expect(byId.get("a")?.column).toBe(0);
    expect(byId.get("b")?.column).toBe(1);
    expect(byId.get("c")?.column).toBe(1);
    expect(byId.get("a")?.columns).toBe(2);
  });

  it("uses three columns with three concurrent events", () => {
    const placed = layoutSegments(
      segmentsForDay(
        [instance("a", 9, 12), instance("b", 9.5, 12), instance("c", 10, 12)],
        DAY,
      ),
    );
    expect(new Set(placed.map((item) => item.column))).toEqual(new Set([0, 1, 2]));
    expect(placed.every((item) => item.columns === 3)).toBe(true);
  });

  it("returns events sorted by start", () => {
    const placed = layoutSegments(
      segmentsForDay([instance("b", 11, 12), instance("a", 9, 10)], DAY),
    );
    expect(placed.map((item) => item.segment.event.eventId)).toEqual(["a", "b"]);
  });
});

function allDayInstance(id: string, start: string, end: string): EventInstance {
  return {
    eventId: id,
    instanceKey: `${id}:${start}`,
    calendarId: "cal",
    accountId: "acc",
    title: id,
    location: "",
    start,
    end,
    allDay: true,
    color: "#0f6cbd",
    isRecurring: false,
    showAs: "busy",
    status: "confirmed",
    myResponse: null,
    hasAttendees: false,
    hasReminder: false,
  };
}

type NodeProcess = { env: Record<string, string | undefined> };

const nodeProcess = (globalThis as unknown as { process?: NodeProcess }).process;
const originalTimeZone = nodeProcess?.env.TZ;

function setProcessTimeZone(zone: string | undefined): void {
  if (!nodeProcess) return;
  if (zone === undefined) delete nodeProcess.env.TZ;
  else nodeProcess.env.TZ = zone;
}

describe("allDayEventsForDay", () => {
  beforeAll(() => {
    // The regression only reproduces east of UTC: `new Date("2026-11-02")`
    // is 08:00 local in Shanghai, which used to include the next day.
    setProcessTimeZone("Asia/Shanghai");
  });

  afterAll(() => {
    setProcessTimeZone(originalTimeZone);
  });

  it("treats a single-day all-day end as an exclusive calendar date (UTC+8)", () => {
    expect(new Date(2026, 10, 1).getTimezoneOffset()).toBe(-480);
    expect(new Date("2026-11-02").getHours()).toBe(8); // the buggy parse

    const event = allDayInstance("a", "2026-11-01", "2026-11-02");
    expect(allDayEventsForDay([event], new Date(2026, 10, 1))).toHaveLength(1);
    expect(allDayEventsForDay([event], new Date(2026, 10, 2))).toHaveLength(0);
  });

  it("shows multi-day all-day events on every covered day only", () => {
    const event = allDayInstance("trip", "2026-11-01", "2026-11-04");
    const counts = [1, 2, 3, 4].map(
      (dayOfMonth) => allDayEventsForDay([event], new Date(2026, 10, dayOfMonth)).length,
    );
    expect(counts).toEqual([1, 1, 1, 0]);
  });

  it("still shows invalid or zero-length all-day events for one day", () => {
    const broken = allDayInstance("broken", "2026-11-05", "not-a-date");
    expect(allDayEventsForDay([broken], new Date(2026, 10, 5))).toHaveLength(1);
    expect(allDayEventsForDay([broken], new Date(2026, 10, 6))).toHaveLength(0);
    const zeroLength = allDayInstance("zero", "2026-11-05", "2026-11-05");
    expect(allDayEventsForDay([zeroLength], new Date(2026, 10, 5))).toHaveLength(1);
  });
});
